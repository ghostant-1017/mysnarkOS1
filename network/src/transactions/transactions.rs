// Copyright (C) 2019-2020 Aleo Systems Inc.
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

use crate::{external::message::*, peers::PeerInfo, Environment, NetworkError, Outbound};
use snarkos_consensus::memory_pool::Entry;
use snarkvm_dpc::base_dpc::instantiated::Tx;
use snarkvm_utilities::{
    bytes::{FromBytes, ToBytes},
    to_bytes,
};

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

/// A stateful component for managing the transactions for the ledger on this node server.
#[derive(Clone)]
pub struct Transactions {
    /// The parameters and settings of this node server.
    pub(crate) environment: Environment,
    /// The outbound handler of this node server.
    outbound: Arc<Outbound>,
}

impl Transactions {
    ///
    /// Creates a new instance of `Transactions`.
    ///
    pub fn new(environment: Environment, outbound: Arc<Outbound>) -> Self {
        trace!("Instantiating the transaction service");
        Self { environment, outbound }
    }

    ///
    /// Triggers the transaction sync with a selected peer.
    ///
    pub fn update(&self, sync_node: Option<SocketAddr>) -> Result<(), NetworkError> {
        if !self.environment.is_bootnode() {
            if let Some(sync_node) = sync_node {
                self.outbound
                    .send_request(Message::new(Direction::Outbound(sync_node), Payload::GetMemoryPool));

                trace!("Send from update to {:?}", sync_node);
            } else {
                info!("No sync node is registered, transactions could not be synced");
            }
        }

        Ok(())
    }

    /// Broadcast transaction to connected peers
    pub(crate) async fn propagate_transaction(
        &self,
        transaction_bytes: Vec<u8>,
        transaction_sender: SocketAddr,
        connected_peers: &HashMap<SocketAddr, PeerInfo>,
    ) -> Result<(), NetworkError> {
        debug!("Propagating a transaction to peers");

        let local_address = self.environment.local_address().unwrap();

        for remote_address in connected_peers.keys() {
            if *remote_address != transaction_sender && *remote_address != local_address {
                // Send a `Transaction` message to the connected peer.
                self.outbound.send_request(Message::new(
                    Direction::Outbound(*remote_address),
                    Payload::Transaction(transaction_bytes.clone()),
                ));
            }
        }

        Ok(())
    }

    /// Verify a transaction, add it to the memory pool, propagate it to peers.
    pub(crate) async fn received_transaction(
        &self,
        source: SocketAddr,
        transaction: Vec<u8>,
        connected_peers: HashMap<SocketAddr, PeerInfo>,
    ) -> Result<(), NetworkError> {
        if let Ok(tx) = Tx::read(&*transaction) {
            let parameters = self.environment.dpc_parameters();
            let storage = self.environment.storage();
            let consensus = self.environment.consensus_parameters();

            if !consensus.verify_transaction(parameters, &tx, &*storage.read())? {
                error!("Received a transaction that was invalid");
                return Ok(());
            }

            if tx.value_balance.is_negative() {
                error!("Received a transaction that was a coinbase transaction");
                return Ok(());
            }

            let entry = Entry::<Tx> {
                size_in_bytes: transaction.len(),
                transaction: tx,
            };

            let insertion = self.environment.memory_pool().lock().insert(&storage.read(), entry);

            if let Ok(inserted) = insertion {
                if inserted.is_some() {
                    info!("Transaction added to memory pool.");
                    self.propagate_transaction(transaction, source, &connected_peers)
                        .await?;
                }
            }
        }

        Ok(())
    }

    /// A peer has requested our memory pool transactions.
    pub(crate) async fn received_get_memory_pool(&self, remote_address: SocketAddr) -> Result<(), NetworkError> {
        // TODO (howardwu): This should have been written with Rayon - it is easily parallelizable.
        let transactions = {
            let mut txs = vec![];

            let memory_pool = self.environment.memory_pool().lock();
            for entry in memory_pool.transactions.values() {
                if let Ok(transaction_bytes) = to_bytes![entry.transaction] {
                    txs.push(transaction_bytes);
                }
            }

            txs
        };

        if !transactions.is_empty() {
            // Send a `MemoryPool` message to the connected peer.
            self.outbound.send_request(Message::new(
                Direction::Outbound(remote_address),
                Payload::MemoryPool(transactions),
            ));
        }

        Ok(())
    }

    /// A peer has sent us their memory pool transactions.
    pub(crate) fn received_memory_pool(&self, transactions: Vec<Vec<u8>>) -> Result<(), NetworkError> {
        let mut memory_pool = self.environment.memory_pool().lock();

        for transaction_bytes in transactions {
            let transaction: Tx = Tx::read(&transaction_bytes[..])?;
            let entry = Entry::<Tx> {
                size_in_bytes: transaction_bytes.len(),
                transaction,
            };

            if let Ok(Some(txid)) = memory_pool.insert(&*self.environment.storage().read(), entry) {
                debug!(
                    "Transaction added to memory pool with txid: {:?}",
                    hex::encode(txid.clone())
                );
            }
        }

        //  Cleanse and store transactions once batch has been received.
        debug!("Cleansing memory pool transactions in database");
        memory_pool
            .cleanse(&self.environment.storage().read())
            .unwrap_or_else(|error| debug!("Failed to cleanse memory pool transactions in database {}", error));
        debug!("Storing memory pool transactions in database");
        memory_pool
            .store(&self.environment.storage().read())
            .unwrap_or_else(|error| debug!("Failed to store memory pool transaction in database {}", error));

        Ok(())
    }
}
