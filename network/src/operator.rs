// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{LedgerRequest, PeersRequest, State};
use snarkos_environment::{
    helpers::NodeType,
    network::{Data, Message},
    Environment,
};
use snarkos_storage::{storage::Storage, OperatorState};
use snarkvm::dpc::{prelude::*, PoSWProof};

use anyhow::Result;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, RwLock};

/// Shorthand for the parent half of the `Operator` message channel.
pub type OperatorRouter<N> = mpsc::Sender<OperatorRequest<N>>;
/// Shorthand for the child half of the `Operator` message channel.
pub type OperatorHandler<N> = mpsc::Receiver<OperatorRequest<N>>;

///
/// An enum of requests that the `Operator` struct processes.
///
#[derive(Debug)]
pub enum OperatorRequest<N: Network> {
    /// PoolRegister := (peer_ip, prover_address)
    PoolRegister(SocketAddr, Address<N>),
    /// PoolResponse := (peer_ip, prover_address, nonce, proof)
    PoolResponse(SocketAddr, Address<N>, N::PoSWNonce, PoSWProof<N>),
    /// PoolBlock := (nonce, proof)
    PoolBlock(N::PoSWNonce, PoSWProof<N>),
}

/// The predefined base share difficulty.
const BASE_SHARE_DIFFICULTY: u64 = u64::MAX / 5;
/// The operator heartbeat in seconds.
const HEARTBEAT_IN_SECONDS: Duration = Duration::from_millis(100);

///
/// An operator for a program on a specific network in the node server.
///
pub struct Operator<N: Network, E: Environment> {
    /// The state storage of the operator.
    operator_state: Arc<OperatorState<N>>,
    /// The current block template that is being mined on by the operator.
    block_template: RwLock<Option<BlockTemplate<N>>>,
    /// A list of provers and their associated state := (last_submitted, share_difficulty)
    provers: RwLock<HashMap<Address<N>, (Instant, u64)>>,
    /// A list of the known nonces for the current round.
    known_nonces: RwLock<HashSet<N::PoSWNonce>>,
    /// The operator router of the node.
    operator_router: OperatorRouter<N>,
    /// The shared state of the owning node.
    state: Arc<State<N, E>>,
}

impl<N: Network, E: Environment> Operator<N, E> {
    /// Initializes a new instance of the operator, paired with its handler.
    #[allow(clippy::too_many_arguments)]
    pub async fn open<S: Storage, P: AsRef<Path> + Copy>(
        path: P,
        state: Arc<State<N, E>>,
    ) -> Result<(Self, mpsc::Receiver<OperatorRequest<N>>)> {
        // Initialize an mpsc channel for sending requests to the `Operator` struct.
        let (operator_router, operator_handler) = mpsc::channel(1024);
        // Initialize the operator.
        let operator = Self {
            operator_state: Arc::new(OperatorState::open_writer::<S, P>(path)?),
            block_template: RwLock::new(None),
            provers: Default::default(),
            known_nonces: Default::default(),
            operator_router,
            state,
        };

        Ok((operator, operator_handler))
    }

    pub async fn initialize(&self) {
        if E::NODE_TYPE == NodeType::Operator {
            if let Some(recipient) = self.state.address {
                // Initialize an update loop for the block template.
                let state = self.state.clone();
                let (router, handler) = oneshot::channel();
                E::resources().register_task(
                    None, // No need to provide an id, as the task will run indefinitely.
                    tokio::spawn(async move {
                        let operator = &state.operator();
                        // Notify the outer function that the task is ready.
                        let _ = router.send(());
                        // TODO (julesdesmit): Add logic to the loop to retarget share difficulty.
                        loop {
                            if !E::status().is_ready() {
                                tokio::time::sleep(HEARTBEAT_IN_SECONDS).await;
                                continue;
                            }
                            // Determine if the current block template is stale.
                            let is_block_template_stale = match &*operator.block_template.read().await {
                                Some(template) => {
                                    operator.state.ledger().reader().latest_block_height().saturating_add(1) != template.block_height()
                                }
                                None => true,
                            };

                            // Update the block template if it is stale.
                            if is_block_template_stale {
                                // Construct a new block template.
                                let transactions = operator.state.prover().memory_pool().read().await.transactions();
                                let ledger_reader = operator.state.ledger().reader().clone();
                                let result = tokio::task::spawn_blocking(move || {
                                    E::thread_pool().install(move || {
                                        match ledger_reader.get_block_template(
                                            recipient,
                                            E::COINBASE_IS_PUBLIC,
                                            &transactions,
                                            &mut rand::thread_rng(),
                                        ) {
                                            Ok(block_template) => Ok(block_template),
                                            Err(error) => Err(format!("Failed to produce a new block template: {}", error)),
                                        }
                                    })
                                })
                                .await;

                                // Update the block template.
                                match result {
                                    Ok(Ok(block_template)) => {
                                        // Acquire the write lock to update the block template.
                                        *operator.block_template.write().await = Some(block_template.clone());
                                        // Clear the set of known nonces.
                                        operator.known_nonces.write().await.clear();

                                        let pool_message = Message::NewBlockTemplate(Data::Object(block_template));
                                        if let Err(error) = state
                                            .peers()
                                            .router()
                                            .send(PeersRequest::MessagePropagatePoolServer(pool_message))
                                            .await
                                        {
                                            warn!("Failed to propagate PoolRequest: {}", error);
                                        }
                                    }
                                    Ok(Err(error_message)) => error!("{}", error_message),
                                    Err(error) => error!("{}", error),
                                };
                            }

                            // Proceed to sleep for a preset amount of time.
                            tokio::time::sleep(HEARTBEAT_IN_SECONDS).await;
                        }
                    }),
                );

                // Wait until the operator handler is ready.
                let _ = handler.await;
            } else {
                error!("Missing operator address. Please specify an Aleo address in order to operate a pool");
            }
        }
    }

    /// Returns an instance of the operator router.
    pub fn router(&self) -> &OperatorRouter<N> {
        &self.operator_router
    }

    /// Returns all the shares in storage.
    pub fn to_shares(&self) -> Vec<((u32, Record<N>), HashMap<Address<N>, u64>)> {
        self.operator_state.to_shares()
    }

    /// Returns the shares for a specific block, given the block height and coinbase record commitment.
    pub fn get_shares_for_block(&self, block_height: u32, coinbase_record: Record<N>) -> Result<HashMap<Address<N>, u64>> {
        self.operator_state.get_shares_for_block(block_height, coinbase_record)
    }

    /// Returns the shares for a specific prover, given the prover address.
    pub fn get_shares_for_prover(&self, prover: &Address<N>) -> u64 {
        self.operator_state.get_shares_for_prover(prover)
    }

    ///
    /// Returns a list of all provers which have submitted shares to this operator.
    ///
    pub fn get_provers(&self) -> Vec<Address<N>> {
        self.operator_state.get_provers()
    }

    ///
    /// Performs the given `request` to the operator.
    /// All requests must go through this `update`, so that a unified view is preserved.
    ///
    pub(super) async fn update(&self, request: OperatorRequest<N>) {
        match request {
            OperatorRequest::PoolRegister(peer_ip, address) => {
                if let Some(block_template) = self.block_template.read().await.clone() {
                    // Ensure this prover exists in the list first, and retrieve their share difficulty.
                    let share_difficulty = self
                        .provers
                        .write()
                        .await
                        .entry(address)
                        .or_insert((Instant::now(), BASE_SHARE_DIFFICULTY))
                        .1;

                    // Route a `PoolRequest` to the peer.
                    let message = Message::PoolRequest(share_difficulty, Data::Object(block_template));
                    if let Err(error) = self.state.peers().router().send(PeersRequest::MessageSend(peer_ip, message)).await {
                        warn!("[PoolRequest] {}", error);
                    }
                } else {
                    warn!("[PoolRegister] No current block template exists");
                }
            }
            OperatorRequest::PoolResponse(peer_ip, prover, nonce, proof) => {
                if let Some(block_template) = self.block_template.read().await.clone() {
                    // Ensure the given nonce from the prover is new.
                    if self.known_nonces.read().await.contains(&nonce) {
                        warn!("[PoolResponse] Peer {} sent a duplicate share", peer_ip);
                        // TODO (julesdesmit): punish?
                        return;
                    }

                    // Update known nonces.
                    self.known_nonces.write().await.insert(nonce);

                    // Retrieve the share difficulty for the given prover.
                    let share_difficulty = {
                        let provers = self.provers.read().await.clone();
                        match provers.get(&prover) {
                            Some((_, share_difficulty)) => *share_difficulty,
                            None => {
                                self.provers.write().await.insert(prover, (Instant::now(), BASE_SHARE_DIFFICULTY));
                                BASE_SHARE_DIFFICULTY
                            }
                        }
                    };

                    // Ensure the share difficulty target is met, and the PoSW proof is valid.
                    let block_height = block_template.block_height();
                    if !N::posw().verify(
                        block_height,
                        share_difficulty,
                        &[*block_template.to_header_root().unwrap(), *nonce],
                        &proof,
                    ) {
                        warn!("[PoolResponse] PoSW proof verification failed");
                        return;
                    }

                    // Update the internal state for this prover.
                    if let Some(ref mut prover) = self.provers.write().await.get_mut(&prover) {
                        prover.0 = Instant::now();
                    } else {
                        error!("Prover should have existing info");
                        return;
                    }

                    // Increment the share count for the prover.
                    let coinbase_record = block_template.coinbase_record().clone();
                    match self.operator_state.increment_share(block_height, coinbase_record, &prover) {
                        Ok(..) => info!(
                            "Operator has received a valid share from {} ({}) for block {}",
                            prover, peer_ip, block_height,
                        ),
                        Err(error) => error!("{}", error),
                    }

                    // If the block has satisfactory difficulty and is valid, proceed to broadcast it.
                    let previous_block_hash = block_template.previous_block_hash();
                    let transactions = block_template.transactions().clone();
                    if let Ok(block_header) = BlockHeader::<N>::from(
                        block_template.previous_ledger_root(),
                        block_template.transactions().transactions_root(),
                        BlockHeaderMetadata::new(&block_template),
                        nonce,
                        proof,
                    ) {
                        if let Ok(block) = Block::from(previous_block_hash, block_header, transactions) {
                            info!("Operator has found unconfirmed block {} ({})", block.height(), block.hash());
                            self.state.ledger().reader().invalidate_coinbase_cache();
                            let request = LedgerRequest::UnconfirmedBlock(self.state.local_ip, block);
                            if let Err(error) = self.state.ledger().router().send(request).await {
                                warn!("Failed to broadcast mined block - {}", error);
                            }
                        }
                    }
                } else {
                    warn!("[PoolResponse] No current block template exists");
                }
            }
            OperatorRequest::PoolBlock(nonce, proof) => {
                if let Some(block_template) = self.block_template.read().await.clone() {
                    let previous_block_hash = block_template.previous_block_hash();
                    let transactions = block_template.transactions().clone();
                    if let Ok(block_header) = BlockHeader::<N>::from(
                        block_template.previous_ledger_root(),
                        block_template.transactions().transactions_root(),
                        BlockHeaderMetadata::new(&block_template),
                        nonce,
                        proof,
                    ) {
                        if let Ok(block) = Block::from(previous_block_hash, block_header, transactions) {
                            info!("Operator has found unconfirmed block {} ({})", block.height(), block.hash());
                            let request = LedgerRequest::UnconfirmedBlock(self.state.local_ip, block);
                            self.state.ledger().reader().invalidate_coinbase_cache();
                            if let Err(error) = self.state.ledger().router().send(request).await {
                                warn!("Failed to broadcast mined block - {}", error);
                            }
                        }
                    }
                } else {
                    warn!("[PoolBlock] No current block template exists");
                }
            }
        }
    }
}
