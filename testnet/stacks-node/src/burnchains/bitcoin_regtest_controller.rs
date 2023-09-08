use std::cmp;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_h1::client;
use async_std::io::ReadExt;
use async_std::net::TcpStream;
use base64::encode;
use http_types::{Method, Request, Url};
use serde::Serialize;
use serde_json::value::RawValue;
use stacks::burnchains::bitcoin::address::{
    BitcoinAddress, LegacyBitcoinAddress, LegacyBitcoinAddressType, SegwitBitcoinAddress,
};
use stacks::burnchains::bitcoin::indexer::{
    BitcoinIndexer, BitcoinIndexerConfig, BitcoinIndexerRuntime,
};
use stacks::burnchains::bitcoin::spv::SpvClient;
use stacks::burnchains::bitcoin::BitcoinNetworkType;
use stacks::burnchains::db::BurnchainDB;
use stacks::burnchains::indexer::BurnchainIndexer;
use stacks::burnchains::{
    Burnchain, BurnchainParameters, BurnchainStateTransitionOps, Error as burnchain_error,
    PoxConstants, PublicKey, Txid,
};
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::burn::operations::{
    BlockstackOperationType, DelegateStxOp, LeaderBlockCommitOp, LeaderKeyRegisterOp, PreStxOp,
    TransferStxOp, UserBurnSupportOp,
};
#[cfg(test)]
use stacks::chainstate::burn::Opcodes;
use stacks::chainstate::coordinator::comm::CoordinatorChannels;
#[cfg(test)]
use stacks::chainstate::stacks::address::PoxAddress;
use stacks::codec::StacksMessageCodec;
use stacks::core::{StacksEpoch, StacksEpochId};
use stacks::monitoring::{increment_btc_blocks_received_counter, increment_btc_ops_sent_counter};
use stacks::types::chainstate::BurnchainHeaderHash;
use stacks::util::hash::{hex_bytes, Hash160};
use stacks::util::secp256k1::Secp256k1PublicKey;
use stacks::util::sleep_ms;
use stacks_common::deps_common::bitcoin::blockdata::opcodes;
use stacks_common::deps_common::bitcoin::blockdata::script::{Builder, Script};
use stacks_common::deps_common::bitcoin::blockdata::transaction::{
    OutPoint, Transaction, TxIn, TxOut,
};
use stacks_common::deps_common::bitcoin::network::encodable::ConsensusEncodable;
#[cfg(test)]
use stacks_common::deps_common::bitcoin::network::serialize::deserialize as btc_deserialize;
use stacks_common::deps_common::bitcoin::network::serialize::RawEncoder;
use stacks_common::deps_common::bitcoin::util::hash::Sha256dHash;

use super::super::operations::BurnchainOpSigner;
use super::super::Config;
use super::{BurnchainController, BurnchainTip, Error as BurnchainControllerError};

/// The number of bitcoin blocks that can have
///  passed since the UTXO cache was last refreshed before
///  the cache is force-reset.
const UTXO_CACHE_STALENESS_LIMIT: u64 = 6;
const DUST_UTXO_LIMIT: u64 = 5500;

pub struct BitcoinRegtestController {
    config: Config,
    indexer: BitcoinIndexer,
    db: Option<SortitionDB>,
    burnchain_db: Option<BurnchainDB>,
    chain_tip: Option<BurnchainTip>,
    use_coordinator: Option<CoordinatorChannels>,
    burnchain_config: Option<Burnchain>,
    ongoing_block_commit: Option<OngoingBlockCommit>,
    should_keep_running: Option<Arc<AtomicBool>>,
    allow_rbf: bool,
}

#[derive(Clone)]
pub struct OngoingBlockCommit {
    payload: LeaderBlockCommitOp,
    utxos: UTXOSet,
    fees: LeaderBlockCommitFees,
    txids: Vec<Txid>,
}

impl OngoingBlockCommit {
    fn sum_utxos(&self) -> u64 {
        self.utxos.total_available()
    }
}

#[derive(Clone)]
struct LeaderBlockCommitFees {
    sunset_fee: u64,
    fee_rate: u64,
    sortition_fee: u64,
    outputs_len: u64,
    default_tx_size: u64,
    spent_in_attempts: u64,
    is_rbf_enabled: bool,
    final_size: u64,
}

#[cfg(test)]
pub fn addr2str(btc_addr: &BitcoinAddress) -> String {
    if let BitcoinAddress::Segwit(segwit_addr) = btc_addr {
        // regtest segwit addresses use a different hrp
        let s = segwit_addr.to_bech32_hrp("bcrt");
        warn!("Re-encoding {} to {}", &segwit_addr, &s);
        s
    } else {
        format!("{}", &btc_addr)
    }
}

#[cfg(not(test))]
pub fn addr2str(btc_addr: &BitcoinAddress) -> String {
    format!("{}", &btc_addr)
}

/// Helper method to create a BitcoinIndexer
pub fn make_bitcoin_indexer(
    config: &Config,
    should_keep_running: Option<Arc<AtomicBool>>,
) -> BitcoinIndexer {
    let (network, _) = config.burnchain.get_bitcoin_network();
    let burnchain_params = BurnchainParameters::from_params(&config.burnchain.chain, &network)
        .expect("Bitcoin network unsupported");
    let indexer_config = {
        let burnchain_config = config.burnchain.clone();
        BitcoinIndexerConfig {
            peer_host: burnchain_config.peer_host,
            peer_port: burnchain_config.peer_port,
            rpc_port: burnchain_config.rpc_port,
            rpc_ssl: burnchain_config.rpc_ssl,
            username: burnchain_config.username,
            password: burnchain_config.password,
            timeout: burnchain_config.timeout,
            spv_headers_path: config.get_spv_headers_file_path(),
            first_block: burnchain_params.first_block_height,
            magic_bytes: burnchain_config.magic_bytes,
            epochs: burnchain_config.epochs,
        }
    };

    let (_, network_type) = config.burnchain.get_bitcoin_network();
    let indexer_runtime = BitcoinIndexerRuntime::new(network_type);
    let burnchain_indexer = BitcoinIndexer {
        config: indexer_config.clone(),
        runtime: indexer_runtime,
        should_keep_running: should_keep_running,
    };
    burnchain_indexer
}

impl LeaderBlockCommitFees {
    pub fn fees_from_previous_tx(
        &self,
        payload: &LeaderBlockCommitOp,
        config: &Config,
    ) -> LeaderBlockCommitFees {
        let mut fees = LeaderBlockCommitFees::estimated_fees_from_payload(payload, config);
        fees.spent_in_attempts = cmp::max(1, self.spent_in_attempts);
        fees.final_size = self.final_size;
        fees.fee_rate = self.fee_rate + config.burnchain.rbf_fee_increment;
        fees.is_rbf_enabled = true;
        fees
    }

    pub fn estimated_fees_from_payload(
        payload: &LeaderBlockCommitOp,
        config: &Config,
    ) -> LeaderBlockCommitFees {
        let sunset_fee = if payload.sunset_burn > 0 {
            cmp::max(payload.sunset_burn, DUST_UTXO_LIMIT)
        } else {
            0
        };

        let number_of_transfers = payload.commit_outs.len() as u64;
        let value_per_transfer = payload.burn_fee / number_of_transfers;
        let sortition_fee = value_per_transfer * number_of_transfers;
        let spent_in_attempts = 0;
        let fee_rate = config.burnchain.satoshis_per_byte;
        let default_tx_size = config.burnchain.block_commit_tx_estimated_size;

        LeaderBlockCommitFees {
            sunset_fee,
            fee_rate,
            sortition_fee,
            outputs_len: number_of_transfers,
            default_tx_size,
            spent_in_attempts,
            is_rbf_enabled: false,
            final_size: 0,
        }
    }

    pub fn estimated_miner_fee(&self) -> u64 {
        self.fee_rate * self.default_tx_size
    }

    pub fn rbf_fee(&self) -> u64 {
        if self.is_rbf_enabled {
            self.spent_in_attempts + self.default_tx_size
        } else {
            0
        }
    }

    pub fn estimated_amount_required(&self) -> u64 {
        self.estimated_miner_fee() + self.rbf_fee() + self.sunset_fee + self.sortition_fee
    }

    pub fn total_spent(&self) -> u64 {
        self.fee_rate * self.final_size
            + self.spent_in_attempts
            + self.sunset_fee
            + self.sortition_fee
    }

    pub fn amount_per_output(&self) -> u64 {
        self.sortition_fee / self.outputs_len
    }

    pub fn total_spent_in_outputs(&self) -> u64 {
        self.sunset_fee + self.sortition_fee
    }

    pub fn min_tx_size(&self) -> u64 {
        cmp::max(self.final_size, self.default_tx_size)
    }

    pub fn register_replacement(&mut self, tx_size: u64) {
        let new_size = cmp::max(tx_size, self.final_size);
        if self.is_rbf_enabled {
            self.spent_in_attempts += new_size;
        }
        self.final_size = new_size;
    }
}

impl BitcoinRegtestController {
    pub fn new(config: Config, coordinator_channel: Option<CoordinatorChannels>) -> Self {
        BitcoinRegtestController::with_burnchain(config, coordinator_channel, None, None)
    }

    pub fn with_burnchain(
        config: Config,
        coordinator_channel: Option<CoordinatorChannels>,
        burnchain: Option<Burnchain>,
        should_keep_running: Option<Arc<AtomicBool>>,
    ) -> Self {
        std::fs::create_dir_all(&config.get_burnchain_path_str())
            .expect("Unable to create workdir");
        let (network, network_id) = config.burnchain.get_bitcoin_network();

        let res = SpvClient::new(
            &config.get_spv_headers_file_path(),
            0,
            None,
            network_id,
            true,
            false,
        );
        if let Err(err) = res {
            error!("Unable to init block headers: {}", err);
            panic!()
        }

        let burnchain_params = BurnchainParameters::from_params(&config.burnchain.chain, &network)
            .expect("Bitcoin network unsupported");

        if network_id == BitcoinNetworkType::Mainnet && config.burnchain.epochs.is_some() {
            panic!("It is an error to set custom epochs while running on Mainnet: network_id {:?} config.burnchain {:#?}",
                   &network_id, &config.burnchain);
        }

        let indexer_config = {
            let burnchain_config = config.burnchain.clone();
            BitcoinIndexerConfig {
                peer_host: burnchain_config.peer_host,
                peer_port: burnchain_config.peer_port,
                rpc_port: burnchain_config.rpc_port,
                rpc_ssl: burnchain_config.rpc_ssl,
                username: burnchain_config.username,
                password: burnchain_config.password,
                timeout: burnchain_config.timeout,
                spv_headers_path: config.get_spv_headers_file_path(),
                first_block: burnchain_params.first_block_height,
                magic_bytes: burnchain_config.magic_bytes,
                epochs: burnchain_config.epochs,
            }
        };

        let (_, network_type) = config.burnchain.get_bitcoin_network();
        let indexer_runtime = BitcoinIndexerRuntime::new(network_type);
        let burnchain_indexer = BitcoinIndexer {
            config: indexer_config.clone(),
            runtime: indexer_runtime,
            should_keep_running: should_keep_running.clone(),
        };

        Self {
            use_coordinator: coordinator_channel,
            config,
            indexer: burnchain_indexer,
            db: None,
            burnchain_db: None,
            chain_tip: None,
            burnchain_config: burnchain,
            ongoing_block_commit: None,
            should_keep_running,
            allow_rbf: true,
        }
    }

    /// create a dummy bitcoin regtest controller.
    ///   used just for submitting bitcoin ops.
    pub fn new_dummy(config: Config) -> Self {
        let (network, _) = config.burnchain.get_bitcoin_network();
        let burnchain_params = BurnchainParameters::from_params(&config.burnchain.chain, &network)
            .expect("Bitcoin network unsupported");

        let indexer_config = {
            let burnchain_config = config.burnchain.clone();
            BitcoinIndexerConfig {
                peer_host: burnchain_config.peer_host,
                peer_port: burnchain_config.peer_port,
                rpc_port: burnchain_config.rpc_port,
                rpc_ssl: burnchain_config.rpc_ssl,
                username: burnchain_config.username,
                password: burnchain_config.password,
                timeout: burnchain_config.timeout,
                spv_headers_path: config.get_spv_headers_file_path(),
                first_block: burnchain_params.first_block_height,
                magic_bytes: burnchain_config.magic_bytes,
                epochs: burnchain_config.epochs,
            }
        };

        let (_, network_type) = config.burnchain.get_bitcoin_network();
        let indexer_runtime = BitcoinIndexerRuntime::new(network_type);
        let burnchain_indexer = BitcoinIndexer {
            config: indexer_config.clone(),
            runtime: indexer_runtime,
            should_keep_running: None,
        };

        Self {
            use_coordinator: None,
            config,
            indexer: burnchain_indexer,
            db: None,
            burnchain_db: None,
            chain_tip: None,
            burnchain_config: None,
            ongoing_block_commit: None,
            should_keep_running: None,
            allow_rbf: true,
        }
    }

    /// Creates a dummy bitcoin regtest controller, with the given ongoing block-commits
    pub fn new_ongoing_dummy(config: Config, ongoing: Option<OngoingBlockCommit>) -> Self {
        let mut ret = Self::new_dummy(config);
        ret.ongoing_block_commit = ongoing;
        ret
    }

    /// Get an owned copy of the ongoing block commit state
    pub fn get_ongoing_commit(&self) -> Option<OngoingBlockCommit> {
        self.ongoing_block_commit.clone()
    }

    /// Set the ongoing block commit state
    pub fn set_ongoing_commit(&mut self, ongoing: Option<OngoingBlockCommit>) {
        self.ongoing_block_commit = ongoing;
    }

    /// Get the default Burnchain instance from our config
    fn default_burnchain(&self) -> Burnchain {
        let burnchain = match &self.burnchain_config {
            Some(burnchain) => burnchain.clone(),
            None => self.config.get_burnchain(),
        };
        burnchain
    }

    /// Get the PoX constants in use
    pub fn get_pox_constants(&self) -> PoxConstants {
        let burnchain = self.get_burnchain();
        burnchain.pox_constants
    }

    /// Get the Burnchain in use
    pub fn get_burnchain(&self) -> Burnchain {
        match self.burnchain_config {
            Some(ref burnchain) => burnchain.clone(),
            None => self.default_burnchain(),
        }
    }

    /// Helium (devnet) blocks receiver.  Returns the new burnchain tip.
    fn receive_blocks_helium(&mut self) -> BurnchainTip {
        let mut burnchain = self.get_burnchain();
        let (block_snapshot, state_transition) = loop {
            match burnchain.sync_with_indexer_deprecated(&mut self.indexer) {
                Ok(x) => {
                    break x;
                }
                Err(e) => {
                    // keep trying
                    error!("Unable to sync with burnchain: {}", e);
                    match e {
                        burnchain_error::TrySyncAgain => {
                            // try again immediately
                            continue;
                        }
                        burnchain_error::BurnchainPeerBroken => {
                            // remote burnchain peer broke, and produced a shorter blockchain fork.
                            // just keep trying
                            sleep_ms(5000);
                            continue;
                        }
                        _ => {
                            // delay and try again
                            sleep_ms(5000);
                            continue;
                        }
                    }
                }
            }
        };

        let rest = match (state_transition, &self.chain_tip) {
            (None, Some(chain_tip)) => chain_tip.clone(),
            (Some(state_transition), _) => {
                let burnchain_tip = BurnchainTip {
                    block_snapshot: block_snapshot,
                    state_transition: BurnchainStateTransitionOps::from(state_transition),
                    received_at: Instant::now(),
                };
                self.chain_tip = Some(burnchain_tip.clone());
                burnchain_tip
            }
            (None, None) => {
                // can happen at genesis
                let burnchain_tip = BurnchainTip {
                    block_snapshot: block_snapshot,
                    state_transition: BurnchainStateTransitionOps::noop(),
                    received_at: Instant::now(),
                };
                self.chain_tip = Some(burnchain_tip.clone());
                burnchain_tip
            }
        };

        debug!("Done receiving blocks");
        rest
    }

    fn receive_blocks(
        &mut self,
        block_for_sortitions: bool,
        target_block_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), BurnchainControllerError> {
        let coordinator_comms = match self.use_coordinator.as_ref() {
            Some(x) => x.clone(),
            None => {
                // pre-PoX helium node
                let tip = self.receive_blocks_helium();
                let height = tip.block_snapshot.block_height;
                return Ok((tip, height));
            }
        };

        let mut burnchain = self.get_burnchain();
        let (block_snapshot, burnchain_height, state_transition) = loop {
            if !self.should_keep_running() {
                return Err(BurnchainControllerError::CoordinatorClosed);
            }

            match burnchain.sync_with_indexer(
                &mut self.indexer,
                coordinator_comms.clone(),
                target_block_height_opt,
                Some(burnchain.pox_constants.reward_cycle_length as u64),
                self.should_keep_running.clone(),
            ) {
                Ok(x) => {
                    increment_btc_blocks_received_counter();

                    // initialize the dbs...
                    self.sortdb_mut();

                    // wait for the chains coordinator to catch up with us.
                    // don't wait for heights beyond the burnchain tip.
                    if block_for_sortitions {
                        self.wait_for_sortitions(
                            coordinator_comms.clone(),
                            target_block_height_opt.unwrap_or(x.block_height),
                        )?;
                    }

                    // NOTE: This is the latest _sortition_ on the canonical sortition history, not the latest burnchain block!
                    let sort_tip =
                        SortitionDB::get_canonical_burn_chain_tip(self.sortdb_ref().conn())
                            .expect("Sortition DB error.");

                    let (snapshot, state_transition) = self
                        .sortdb_ref()
                        .get_sortition_result(&sort_tip.sortition_id)
                        .expect("Sortition DB error.")
                        .expect("BUG: no data for the canonical chain tip");

                    let burnchain_height = self
                        .indexer
                        .get_highest_header_height()
                        .map_err(BurnchainControllerError::IndexerError)?;
                    break (snapshot, burnchain_height, state_transition);
                }
                Err(e) => {
                    // keep trying
                    error!("Unable to sync with burnchain: {}", e);
                    match e {
                        burnchain_error::CoordinatorClosed => {
                            return Err(BurnchainControllerError::CoordinatorClosed)
                        }
                        burnchain_error::TrySyncAgain => {
                            // try again immediately
                            continue;
                        }
                        burnchain_error::BurnchainPeerBroken => {
                            // remote burnchain peer broke, and produced a shorter blockchain fork.
                            // just keep trying
                            sleep_ms(5000);
                            continue;
                        }
                        _ => {
                            // delay and try again
                            sleep_ms(5000);
                            continue;
                        }
                    }
                }
            }
        };

        let burnchain_tip = BurnchainTip {
            block_snapshot: block_snapshot,
            state_transition: state_transition,
            received_at: Instant::now(),
        };

        self.chain_tip = Some(burnchain_tip.clone());
        debug!("Done receiving blocks");

        Ok((burnchain_tip, burnchain_height))
    }

    fn should_keep_running(&self) -> bool {
        match self.should_keep_running {
            Some(ref should_keep_running) => should_keep_running.load(Ordering::SeqCst),
            _ => true,
        }
    }

    #[cfg(test)]
    pub fn get_all_utxos(&self, public_key: &Secp256k1PublicKey) -> Vec<UTXO> {
        // Configure UTXO filter, disregard what epoch we're in
        let address = self.get_miner_address(StacksEpochId::Epoch21, public_key);
        let filter_addresses = vec![addr2str(&address)];

        let pubk = if self.config.miner.segwit {
            let mut p = public_key.clone();
            p.set_compressed(true);
            p
        } else {
            public_key.clone()
        };

        test_debug!("Import public key '{}'", &pubk.to_hex());
        let _result = BitcoinRPCRequest::import_public_key(&self.config, &pubk);

        sleep_ms(1000);

        let min_conf = 0i64;
        let max_conf = 9999999i64;
        let minimum_amount = ParsedUTXO::sat_to_serialized_btc(1);

        test_debug!(
            "List unspent for '{}' ('{}')",
            &addr2str(&address),
            pubk.to_hex()
        );
        let payload = BitcoinRPCRequest {
            method: "listunspent".to_string(),
            params: vec![
                min_conf.into(),
                max_conf.into(),
                filter_addresses.clone().into(),
                true.into(),
                json!({ "minimumAmount": minimum_amount }),
            ],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };

        let mut res = BitcoinRPCRequest::send(&self.config, payload).unwrap();
        let mut result_vec = vec![];

        if let Some(ref mut object) = res.as_object_mut() {
            match object.get_mut("result") {
                Some(serde_json::Value::Array(entries)) => {
                    while let Some(entry) = entries.pop() {
                        let parsed_utxo: ParsedUTXO = match serde_json::from_value(entry) {
                            Ok(utxo) => utxo,
                            Err(err) => {
                                warn!("Failed parsing UTXO: {}", err);
                                continue;
                            }
                        };
                        let amount = match parsed_utxo.get_sat_amount() {
                            Some(amount) => amount,
                            None => continue,
                        };

                        if amount < 1 {
                            continue;
                        }

                        let script_pub_key = match parsed_utxo.get_script_pub_key() {
                            Some(script_pub_key) => script_pub_key,
                            None => {
                                continue;
                            }
                        };

                        let txid = match parsed_utxo.get_txid() {
                            Some(amount) => amount,
                            None => continue,
                        };

                        result_vec.push(UTXO {
                            txid,
                            vout: parsed_utxo.vout,
                            script_pub_key,
                            amount,
                            confirmations: parsed_utxo.confirmations,
                        });
                    }
                }
                _ => {
                    warn!("Failed to get UTXOs");
                }
            }
        }

        result_vec
    }

    /// Checks if the config-supplied wallet exists.
    /// If it does not exist, this function creates it.
    pub fn create_wallet_if_dne(&self) -> RPCResult<()> {
        let wallets = BitcoinRPCRequest::list_wallets(&self.config)?;

        if !wallets.contains(&self.config.burnchain.wallet_name) {
            BitcoinRPCRequest::create_wallet(&self.config, &self.config.burnchain.wallet_name)?;
        }
        Ok(())
    }

    pub fn get_utxos(
        &self,
        epoch_id: StacksEpochId,
        public_key: &Secp256k1PublicKey,
        total_required: u64,
        utxos_to_exclude: Option<UTXOSet>,
        block_height: u64,
    ) -> Option<UTXOSet> {
        // if mock mining, do not even both requesting UTXOs
        if self.config.node.mock_mining {
            return None;
        }

        let pubk = if self.config.miner.segwit && epoch_id >= StacksEpochId::Epoch21 {
            let mut p = public_key.clone();
            p.set_compressed(true);
            p
        } else {
            public_key.clone()
        };

        // Configure UTXO filter
        let address = self.get_miner_address(epoch_id, &pubk);
        test_debug!(
            "Get UTXOs for {} ({}) rbf={}",
            pubk.to_hex(),
            addr2str(&address),
            self.allow_rbf
        );
        let filter_addresses = vec![addr2str(&address)];

        let mut utxos = loop {
            let result = BitcoinRPCRequest::list_unspent(
                &self.config,
                filter_addresses.clone(),
                !self.allow_rbf, // if RBF is disabled, then we can use 0-conf txs
                total_required,
                &utxos_to_exclude,
                block_height,
            );

            // Perform request
            match result {
                Ok(utxos) => {
                    break utxos;
                }
                Err(e) => {
                    error!("Bitcoin RPC failure: error listing utxos {:?}", e);
                    sleep_ms(5000);
                    continue;
                }
            };
        };

        let utxos = if utxos.is_empty() {
            let (_, network) = self.config.burnchain.get_bitcoin_network();
            loop {
                if let BitcoinNetworkType::Regtest = network {
                    // Performing this operation on Mainnet / Testnet is very expensive, and can be longer than bitcoin block time.
                    // Assuming that miners are in charge of correctly operating their bitcoind nodes sounds
                    // reasonable to me.
                    // $ bitcoin-cli importaddress mxVFsFW5N4mu1HPkxPttorvocvzeZ7KZyk
                    let _result = BitcoinRPCRequest::import_public_key(&self.config, &pubk);
                    sleep_ms(1000);
                }

                let result = BitcoinRPCRequest::list_unspent(
                    &self.config,
                    filter_addresses.clone(),
                    !self.allow_rbf, // if RBF is disabled, then we can use 0-conf txs
                    total_required,
                    &utxos_to_exclude,
                    block_height,
                );

                utxos = match result {
                    Ok(utxos) => utxos,
                    Err(e) => {
                        error!("Bitcoin RPC failure: error listing utxos {:?}", e);
                        sleep_ms(5000);
                        continue;
                    }
                };

                test_debug!("Unspent for {:?}: {:?}", &filter_addresses, &utxos);

                if utxos.is_empty() {
                    return None;
                } else {
                    break utxos;
                }
            }
        } else {
            debug!(
                "Got {} UTXOs for {:?}",
                utxos.utxos.len(),
                &filter_addresses
            );
            utxos
        };

        let total_unspent = utxos.total_available();
        if total_unspent < total_required {
            warn!(
                "Total unspent {} < {} for {:?}",
                total_unspent,
                total_required,
                &pubk.to_hex()
            );
            return None;
        }

        Some(utxos)
    }

    fn build_leader_key_register_tx(
        &mut self,
        epoch_id: StacksEpochId,
        payload: LeaderKeyRegisterOp,
        signer: &mut BurnchainOpSigner,
        _attempt: u64,
    ) -> Option<Transaction> {
        let public_key = signer.get_public_key();

        let btc_miner_fee = self.config.burnchain.leader_key_tx_estimated_size
            * self.config.burnchain.satoshis_per_byte;
        let budget_for_outputs = DUST_UTXO_LIMIT;
        let total_required = btc_miner_fee + budget_for_outputs;

        let (mut tx, mut utxos) =
            self.prepare_tx(epoch_id, &public_key, total_required, None, None, 0)?;

        // Serialize the payload
        let op_bytes = {
            let mut buffer = vec![];
            let mut magic_bytes = self.config.burnchain.magic_bytes.as_bytes().to_vec();
            buffer.append(&mut magic_bytes);
            payload
                .consensus_serialize(&mut buffer)
                .expect("FATAL: invalid operation");
            buffer
        };

        let consensus_output = TxOut {
            value: 0,
            script_pubkey: Builder::new()
                .push_opcode(opcodes::All::OP_RETURN)
                .push_slice(&op_bytes)
                .into_script(),
        };

        tx.output = vec![consensus_output];

        let fee_rate = self.config.burnchain.satoshis_per_byte;

        self.finalize_tx(
            epoch_id,
            &mut tx,
            budget_for_outputs,
            0,
            self.config.burnchain.leader_key_tx_estimated_size,
            fee_rate,
            &mut utxos,
            signer,
        )?;

        increment_btc_ops_sent_counter();

        info!(
            "Miner node: submitting leader_key_register op - {}, waiting for its inclusion in the next Bitcoin block",
            public_key.to_hex()
        );

        Some(tx)
    }

    #[cfg(not(test))]
    fn build_transfer_stacks_tx(
        &mut self,
        _epoch_id: StacksEpochId,
        _payload: TransferStxOp,
        _signer: &mut BurnchainOpSigner,
        _utxo: Option<UTXO>,
    ) -> Option<Transaction> {
        unimplemented!()
    }

    #[cfg(not(test))]
    fn build_delegate_stacks_tx(
        &mut self,
        _epoch_id: StacksEpochId,
        _payload: DelegateStxOp,
        _signer: &mut BurnchainOpSigner,
        _utxo: Option<UTXO>,
    ) -> Option<Transaction> {
        unimplemented!()
    }

    #[cfg(test)]
    pub fn submit_manual(
        &mut self,
        epoch_id: StacksEpochId,
        operation: BlockstackOperationType,
        op_signer: &mut BurnchainOpSigner,
        utxo: Option<UTXO>,
    ) -> Option<Transaction> {
        let transaction = match operation {
            BlockstackOperationType::LeaderBlockCommit(_)
            | BlockstackOperationType::LeaderKeyRegister(_)
            | BlockstackOperationType::StackStx(_)
            | BlockstackOperationType::DelegateStx(_)
            | BlockstackOperationType::UserBurnSupport(_) => {
                unimplemented!();
            }
            BlockstackOperationType::PreStx(payload) => {
                self.build_pre_stacks_tx(epoch_id, payload, op_signer)
            }
            BlockstackOperationType::TransferStx(payload) => {
                self.build_transfer_stacks_tx(epoch_id, payload, op_signer, utxo)
            }
        }?;

        let ser_transaction = SerializedTx::new(transaction.clone());

        if self.send_transaction(ser_transaction).is_some() {
            Some(transaction)
        } else {
            None
        }
    }

    #[cfg(test)]
    /// Build a transfer stacks tx.
    ///   this *only* works if the only existant UTXO is from a PreStx Op
    ///   this is okay for testing, but obviously not okay for actual use.
    ///   The reason for this constraint is that the bitcoin_regtest_controller's UTXO
    ///     and signing logic are fairly intertwined, and untangling the two seems excessive
    ///     for a functionality that won't be implemented for production via this controller.
    fn build_transfer_stacks_tx(
        &mut self,
        epoch_id: StacksEpochId,
        payload: TransferStxOp,
        signer: &mut BurnchainOpSigner,
        utxo_to_use: Option<UTXO>,
    ) -> Option<Transaction> {
        let public_key = signer.get_public_key();
        let max_tx_size = 230;

        let (mut tx, mut utxos) = if let Some(utxo) = utxo_to_use {
            (
                Transaction {
                    input: vec![],
                    output: vec![],
                    version: 1,
                    lock_time: 0,
                },
                UTXOSet {
                    bhh: BurnchainHeaderHash::zero(),
                    utxos: vec![utxo],
                },
            )
        } else {
            self.prepare_tx(
                epoch_id,
                &public_key,
                DUST_UTXO_LIMIT + max_tx_size * self.config.burnchain.satoshis_per_byte,
                None,
                None,
                0,
            )?
        };

        // Serialize the payload
        let op_bytes = {
            let mut bytes = self.config.burnchain.magic_bytes.as_bytes().to_vec();
            payload.consensus_serialize(&mut bytes).ok()?;
            bytes
        };

        let consensus_output = TxOut {
            value: 0,
            script_pubkey: Builder::new()
                .push_opcode(opcodes::All::OP_RETURN)
                .push_slice(&op_bytes)
                .into_script(),
        };

        tx.output = vec![consensus_output];
        tx.output.push(
            PoxAddress::Standard(payload.recipient.clone(), None)
                .to_bitcoin_tx_out(DUST_UTXO_LIMIT),
        );

        self.finalize_tx(
            epoch_id,
            &mut tx,
            DUST_UTXO_LIMIT,
            0,
            max_tx_size,
            self.config.burnchain.satoshis_per_byte,
            &mut utxos,
            signer,
        )?;

        increment_btc_ops_sent_counter();

        info!(
            "Miner node: submitting stacks transfer op - {}",
            public_key.to_hex()
        );

        Some(tx)
    }

    #[cfg(test)]
    /// Build a delegate stacks tx.
    ///   this *only* works if the only existant UTXO is from a PreStx Op
    ///   this is okay for testing, but obviously not okay for actual use.
    ///   The reason for this constraint is that the bitcoin_regtest_controller's UTXO
    ///     and signing logic are fairly intertwined, and untangling the two seems excessive
    ///     for a functionality that won't be implemented for production via this controller.
    fn build_delegate_stacks_tx(
        &mut self,
        epoch_id: StacksEpochId,
        payload: DelegateStxOp,
        signer: &mut BurnchainOpSigner,
        utxo_to_use: Option<UTXO>,
    ) -> Option<Transaction> {
        let public_key = signer.get_public_key();
        let max_tx_size = 230;

        let (mut tx, mut utxos) = if let Some(utxo) = utxo_to_use {
            (
                Transaction {
                    input: vec![],
                    output: vec![],
                    version: 1,
                    lock_time: 0,
                },
                UTXOSet {
                    bhh: BurnchainHeaderHash::zero(),
                    utxos: vec![utxo],
                },
            )
        } else {
            self.prepare_tx(
                epoch_id,
                &public_key,
                DUST_UTXO_LIMIT + max_tx_size * self.config.burnchain.satoshis_per_byte,
                None,
                None,
                0,
            )?
        };

        // Serialize the payload
        let op_bytes = {
            let mut bytes = self.config.burnchain.magic_bytes.as_bytes().to_vec();
            payload.consensus_serialize(&mut bytes).ok()?;
            bytes
        };

        let consensus_output = TxOut {
            value: 0,
            script_pubkey: Builder::new()
                .push_opcode(opcodes::All::OP_RETURN)
                .push_slice(&op_bytes)
                .into_script(),
        };

        tx.output = vec![consensus_output];
        tx.output.push(
            PoxAddress::Standard(payload.delegate_to.clone(), None)
                .to_bitcoin_tx_out(DUST_UTXO_LIMIT),
        );

        self.finalize_tx(
            epoch_id,
            &mut tx,
            DUST_UTXO_LIMIT,
            0,
            max_tx_size,
            self.config.burnchain.satoshis_per_byte,
            &mut utxos,
            signer,
        )?;

        increment_btc_ops_sent_counter();

        info!(
            "Miner node: submitting stacks delegate op - {}",
            public_key.to_hex()
        );

        Some(tx)
    }

    #[cfg(not(test))]
    fn build_pre_stacks_tx(
        &mut self,
        _epoch_id: StacksEpochId,
        _payload: PreStxOp,
        _signer: &mut BurnchainOpSigner,
    ) -> Option<Transaction> {
        unimplemented!()
    }

    #[cfg(test)]
    fn build_pre_stacks_tx(
        &mut self,
        epoch_id: StacksEpochId,
        payload: PreStxOp,
        signer: &mut BurnchainOpSigner,
    ) -> Option<Transaction> {
        let public_key = signer.get_public_key();
        let max_tx_size = 280;

        let output_amt = DUST_UTXO_LIMIT + max_tx_size * self.config.burnchain.satoshis_per_byte;
        let (mut tx, mut utxos) =
            self.prepare_tx(epoch_id, &public_key, output_amt, None, None, 0)?;

        // Serialize the payload
        let op_bytes = {
            let mut bytes = self.config.burnchain.magic_bytes.as_bytes().to_vec();
            bytes.push(Opcodes::PreStx as u8);
            bytes
        };

        let consensus_output = TxOut {
            value: 0,
            script_pubkey: Builder::new()
                .push_opcode(opcodes::All::OP_RETURN)
                .push_slice(&op_bytes)
                .into_script(),
        };

        tx.output = vec![consensus_output];
        tx.output
            .push(PoxAddress::Standard(payload.output.clone(), None).to_bitcoin_tx_out(output_amt));

        self.finalize_tx(
            epoch_id,
            &mut tx,
            output_amt,
            0,
            max_tx_size,
            self.config.burnchain.satoshis_per_byte,
            &mut utxos,
            signer,
        )?;

        increment_btc_ops_sent_counter();

        info!(
            "Miner node: submitting pre_stacks op - {}",
            public_key.to_hex()
        );

        Some(tx)
    }

    fn send_block_commit_operation(
        &mut self,
        epoch_id: StacksEpochId,
        payload: LeaderBlockCommitOp,
        signer: &mut BurnchainOpSigner,
        utxos_to_include: Option<UTXOSet>,
        utxos_to_exclude: Option<UTXOSet>,
        previous_fees: Option<LeaderBlockCommitFees>,
        previous_txids: &Vec<Txid>,
    ) -> Option<Transaction> {
        let mut estimated_fees = match previous_fees {
            Some(fees) => fees.fees_from_previous_tx(&payload, &self.config),
            None => LeaderBlockCommitFees::estimated_fees_from_payload(&payload, &self.config),
        };

        let _ = self.sortdb_mut();
        let burn_chain_tip = self.burnchain_db.as_ref()?.get_canonical_chain_tip().ok()?;

        let public_key = signer.get_public_key();
        let (mut tx, mut utxos) = self.prepare_tx(
            epoch_id,
            &public_key,
            estimated_fees.estimated_amount_required(),
            utxos_to_include,
            utxos_to_exclude,
            burn_chain_tip.block_height,
        )?;

        // Serialize the payload
        let op_bytes = {
            let mut buffer = vec![];
            let mut magic_bytes = self.config.burnchain.magic_bytes.as_bytes().to_vec();
            buffer.append(&mut magic_bytes);
            payload
                .consensus_serialize(&mut buffer)
                .expect("FATAL: invalid operation");
            buffer
        };

        let consensus_output = TxOut {
            value: estimated_fees.sunset_fee,
            script_pubkey: Builder::new()
                .push_opcode(opcodes::All::OP_RETURN)
                .push_slice(&op_bytes)
                .into_script(),
        };

        tx.output = vec![consensus_output];

        for commit_to in payload.commit_outs.iter() {
            tx.output
                .push(commit_to.to_bitcoin_tx_out(estimated_fees.amount_per_output()));
        }

        let fee_rate = estimated_fees.fee_rate;
        self.finalize_tx(
            epoch_id,
            &mut tx,
            estimated_fees.total_spent_in_outputs(),
            estimated_fees.spent_in_attempts,
            estimated_fees.min_tx_size(),
            fee_rate,
            &mut utxos,
            signer,
        )?;

        let serialized_tx = SerializedTx::new(tx.clone());

        let tx_size = serialized_tx.bytes.len() as u64;
        estimated_fees.register_replacement(tx_size);
        let mut txid = tx.txid().as_bytes().to_vec();
        txid.reverse();

        debug!("Transaction relying on UTXOs: {:?}", utxos);
        let txid = Txid::from_bytes(&txid[..]).unwrap();
        let mut txids = previous_txids.clone();
        txids.push(txid.clone());
        let ongoing_block_commit = OngoingBlockCommit {
            payload,
            utxos,
            fees: estimated_fees,
            txids,
        };

        info!(
            "Miner node: submitting leader_block_commit (txid: {}, rbf: {}, total spent: {}, size: {}, fee_rate: {})",
            txid.to_hex(),
            ongoing_block_commit.fees.is_rbf_enabled,
            ongoing_block_commit.fees.total_spent(),
            ongoing_block_commit.fees.final_size,
            fee_rate,
        );

        self.ongoing_block_commit = Some(ongoing_block_commit);

        increment_btc_ops_sent_counter();

        Some(tx)
    }

    fn build_leader_block_commit_tx(
        &mut self,
        epoch_id: StacksEpochId,
        payload: LeaderBlockCommitOp,
        signer: &mut BurnchainOpSigner,
        _attempt: u64,
    ) -> Option<Transaction> {
        // Are we currently tracking an operation?
        if self.ongoing_block_commit.is_none() || !self.allow_rbf {
            // Good to go, let's build the transaction and send it.
            let res = self.send_block_commit_operation(
                epoch_id,
                payload,
                signer,
                None,
                None,
                None,
                &vec![],
            );
            return res;
        }

        let ongoing_op = self.ongoing_block_commit.take().unwrap();

        let _ = self.sortdb_mut();
        let burnchain_db = self.burnchain_db.as_ref().expect("BurnchainDB not opened");

        for txid in ongoing_op.txids.iter() {
            let mined_op = burnchain_db.find_burnchain_op(&self.indexer, txid);
            if mined_op.is_some() {
                // Good to go, the transaction in progress was mined
                debug!("Was able to retrieve ongoing TXID - {}", txid);
                let res = self.send_block_commit_operation(
                    epoch_id,
                    payload,
                    signer,
                    None,
                    None,
                    None,
                    &vec![],
                );
                return res;
            } else {
                debug!("Was unable to retrieve ongoing TXID - {}", txid);
            }
        }

        // Did a re-org occur since we fetched our UTXOs, or are the UTXOs so stale that they should be abandoned?
        let mut traversal_depth = 0;
        let mut burn_chain_tip = burnchain_db.get_canonical_chain_tip().ok()?;
        let mut found_last_mined_at = false;
        while traversal_depth < UTXO_CACHE_STALENESS_LIMIT {
            if &burn_chain_tip.block_hash == &ongoing_op.utxos.bhh {
                found_last_mined_at = true;
                break;
            }

            let parent = BurnchainDB::get_burnchain_block(
                &burnchain_db.conn(),
                &burn_chain_tip.parent_block_hash,
            )
            .ok()?;

            burn_chain_tip = parent.header;
            traversal_depth += 1;
        }

        if !found_last_mined_at {
            info!(
                "Possible presence of fork or stale UTXO cache, invalidating cached set of UTXOs.";
                "cached_burn_block_hash" => %ongoing_op.utxos.bhh,
            );
            let res = self.send_block_commit_operation(
                epoch_id,
                payload,
                signer,
                None,
                None,
                None,
                &vec![],
            );
            return res;
        }

        // Stop as soon as the fee_rate is ${self.config.burnchain.max_rbf} percent higher, stop RBF
        if ongoing_op.fees.fee_rate
            > (self.config.burnchain.satoshis_per_byte * self.config.burnchain.max_rbf / 100)
        {
            warn!(
                "RBF'd block commits reached {}% satoshi per byte fee rate, not resubmitting",
                self.config.burnchain.max_rbf
            );
            self.ongoing_block_commit = Some(ongoing_op);
            return None;
        }

        // An ongoing operation is in the mempool and we received a new block. The desired behaviour is the following:
        // 1) If the ongoing and the incoming operation are **strictly** identical, we will be idempotent and discard the incoming.
        // 2) If the 2 operations are different, we will try to avoid wasting UTXOs, and attempt to RBF the outgoing transaction:
        //  i) If UTXOs are insufficient,
        //    a) If no other UTXOs, we'll have to wait on the ongoing operation to be mined before resuming operation.
        //    b) If we have some other UTXOs, drop the ongoing operation, and track the new one.
        //  ii) If UTXOs initially used are sufficient for paying for a fee bump, then RBF

        // Let's start by early returning 1)
        if payload == ongoing_op.payload {
            info!("Abort attempt to re-submit identical LeaderBlockCommit");
            self.ongoing_block_commit = Some(ongoing_op);
            return None;
        }

        // Let's proceed and early return 2) i)
        let res = if ongoing_op.fees.estimated_amount_required() > ongoing_op.sum_utxos() {
            // Try to build and submit op, excluding UTXOs currently used
            info!("Attempt to submit another leader_block_commit, despite an ongoing (outdated) commit");
            self.send_block_commit_operation(
                epoch_id,
                payload,
                signer,
                None,
                Some(ongoing_op.utxos.clone()),
                None,
                &vec![],
            )
        } else {
            // Case 2) ii): Attempt to RBF
            info!("Attempt to replace by fee an outdated leader block commit");
            self.send_block_commit_operation(
                epoch_id,
                payload,
                signer,
                Some(ongoing_op.utxos.clone()),
                None,
                Some(ongoing_op.fees.clone()),
                &ongoing_op.txids,
            )
        };

        if res.is_none() {
            self.ongoing_block_commit = Some(ongoing_op);
        }

        res
    }

    pub(crate) fn get_miner_address(
        &self,
        epoch_id: StacksEpochId,
        public_key: &Secp256k1PublicKey,
    ) -> BitcoinAddress {
        let (_, network_id) = self.config.burnchain.get_bitcoin_network();

        if self.config.miner.segwit && epoch_id >= StacksEpochId::Epoch21 {
            let hash160 = Hash160::from_data(&public_key.to_bytes_compressed());
            BitcoinAddress::from_bytes_segwit_p2wpkh(network_id, &hash160.0)
                .expect("Public key incorrect")
        } else {
            let hash160 = Hash160::from_data(&public_key.to_bytes());
            BitcoinAddress::from_bytes_legacy(
                network_id,
                LegacyBitcoinAddressType::PublicKeyHash,
                &hash160.0,
            )
            .expect("Public key incorrect")
        }
    }

    fn prepare_tx(
        &mut self,
        epoch_id: StacksEpochId,
        public_key: &Secp256k1PublicKey,
        total_required: u64,
        utxos_to_include: Option<UTXOSet>,
        utxos_to_exclude: Option<UTXOSet>,
        block_height: u64,
    ) -> Option<(Transaction, UTXOSet)> {
        let utxos = if let Some(utxos) = utxos_to_include {
            // in RBF, you have to consume the same UTXOs
            utxos
        } else {
            // Fetch some UTXOs
            let addr = self.get_miner_address(epoch_id, public_key);
            let utxos = match self.get_utxos(
                epoch_id,
                &public_key,
                total_required,
                utxos_to_exclude,
                block_height,
            ) {
                Some(utxos) => utxos,
                None => {
                    debug!(
                        "No UTXOs for {} ({}) in epoch {}",
                        &public_key.to_hex(),
                        &addr2str(&addr),
                        epoch_id
                    );
                    return None;
                }
            };
            utxos
        };

        // Prepare a backbone for the tx
        let transaction = Transaction {
            input: vec![],
            output: vec![],
            version: 1,
            lock_time: 0,
        };

        Some((transaction, utxos))
    }

    fn finalize_tx(
        &mut self,
        epoch_id: StacksEpochId,
        tx: &mut Transaction,
        spent_in_outputs: u64,
        spent_in_rbf: u64,
        min_tx_size: u64,
        fee_rate: u64,
        utxos_set: &mut UTXOSet,
        signer: &mut BurnchainOpSigner,
    ) -> Option<()> {
        // spend UTXOs in order by confirmations.  Spend the least-confirmed UTXO first, and in the
        // event of a tie, spend the smallest-value UTXO first.
        utxos_set.utxos.sort_by(|u1, u2| {
            if u1.confirmations != u2.confirmations {
                u1.confirmations.cmp(&u2.confirmations)
            } else {
                // for block-commits, the smaller value is likely the UTXO-chained value, so
                // continue to prioritize it as the first spend in order to avoid breaking the
                // miner commit chain.
                u1.amount.cmp(&u2.amount)
            }
        });

        let tx_size = {
            // We will be calling 2 times serialize_tx, the first time with an estimated size,
            // Second time with the actual size, computed thanks to the 1st attempt.
            let estimated_rbf = if spent_in_rbf == 0 {
                0
            } else {
                spent_in_rbf + min_tx_size // we're spending 1 sat / byte in RBF
            };
            let mut tx_cloned = tx.clone();
            let mut utxos_cloned = utxos_set.clone();
            self.serialize_tx(
                epoch_id,
                &mut tx_cloned,
                spent_in_outputs + min_tx_size * fee_rate + estimated_rbf,
                &mut utxos_cloned,
                signer,
            );
            let serialized_tx = SerializedTx::new(tx_cloned);
            cmp::max(min_tx_size, serialized_tx.bytes.len() as u64)
        };

        let rbf_fee = if spent_in_rbf == 0 {
            0
        } else {
            spent_in_rbf + tx_size // we're spending 1 sat / byte in RBF
        };
        self.serialize_tx(
            epoch_id,
            tx,
            spent_in_outputs + tx_size * fee_rate + rbf_fee,
            utxos_set,
            signer,
        );
        signer.dispose();
        Some(())
    }

    /// Sign and serialize a tx, consuming the UTXOs in utxo_set and spending total_to_spend
    /// satoshis.  Uses the key in signer.
    /// If self.config.miner.segwit is true, the transaction's change address will be a p2wpkh
    /// output. Otherwise, it will be a p2pkh output.
    fn serialize_tx(
        &mut self,
        epoch_id: StacksEpochId,
        tx: &mut Transaction,
        total_to_spend: u64,
        utxos_set: &mut UTXOSet,
        signer: &mut BurnchainOpSigner,
    ) -> bool {
        let mut public_key = signer.get_public_key();
        let mut total_consumed = 0;

        // select UTXOs until we have enough to cover the cost
        let mut available_utxos = vec![];
        available_utxos.append(&mut utxos_set.utxos);
        for utxo in available_utxos.into_iter() {
            total_consumed += utxo.amount;
            utxos_set.utxos.push(utxo);

            if total_consumed >= total_to_spend {
                break;
            }
        }

        if total_consumed < total_to_spend {
            warn!(
                "Consumed total {} is less than intended spend: {}",
                total_consumed, total_to_spend
            );
            return false;
        }

        // Append the change output
        let value = total_consumed - total_to_spend;
        debug!(
            "Payments value: {:?}, total_consumed: {:?}, total_spent: {:?}",
            value, total_consumed, total_to_spend
        );
        if value >= DUST_UTXO_LIMIT {
            let change_output = if self.config.miner.segwit && epoch_id >= StacksEpochId::Epoch21 {
                // p2wpkh
                public_key.set_compressed(true);
                let change_address_hash = Hash160::from_data(&public_key.to_bytes());
                SegwitBitcoinAddress::to_p2wpkh_tx_out(&change_address_hash.0, value)
            } else {
                // p2pkh
                let change_address_hash = Hash160::from_data(&public_key.to_bytes());
                LegacyBitcoinAddress::to_p2pkh_tx_out(&change_address_hash, value)
            };
            tx.output.push(change_output);
        } else {
            // Instead of leaving that change to the BTC miner, we could / should bump the sortition fee
            debug!("Not enough change to clear dust limit. Not adding change address.");
        }

        for (i, utxo) in utxos_set.utxos.iter().enumerate() {
            let input = TxIn {
                previous_output: OutPoint {
                    txid: utxo.txid,
                    vout: utxo.vout,
                },
                script_sig: Script::new(),
                sequence: 0xFFFFFFFD, // allow RBF
                witness: vec![],
            };
            tx.input.push(input);

            let script_pub_key = utxo.script_pub_key.clone();
            let sig_hash_all = 0x01;

            let (sig_hash, is_segwit) = if script_pub_key.as_bytes().len() == 22
                && script_pub_key.as_bytes()[0..2] == [0x00, 0x14]
            {
                // p2wpkh
                (
                    tx.segwit_signature_hash(i, &script_pub_key, utxo.amount, sig_hash_all),
                    true,
                )
            } else {
                // p2pkh
                (tx.signature_hash(i, &script_pub_key, sig_hash_all), false)
            };

            let sig1_der = {
                let message = signer
                    .sign_message(sig_hash.as_bytes())
                    .expect("Unable to sign message");
                message
                    .to_secp256k1_recoverable()
                    .expect("Unable to get recoverable signature")
                    .to_standard()
                    .serialize_der()
            };

            if is_segwit {
                // segwit
                public_key.set_compressed(true);
                tx.input[i].script_sig = Script::from(vec![]);
                tx.input[i].witness = vec![
                    [&*sig1_der, &[sig_hash_all as u8][..]].concat().to_vec(),
                    public_key.to_bytes(),
                ];
            } else {
                // legacy scriptSig
                tx.input[i].script_sig = Builder::new()
                    .push_slice(&[&*sig1_der, &[sig_hash_all as u8][..]].concat())
                    .push_slice(&public_key.to_bytes())
                    .into_script();
                tx.input[i].witness.clear();
            }
        }
        true
    }

    fn build_user_burn_support_tx(
        &mut self,
        _epoch_id: StacksEpochId,
        _payload: UserBurnSupportOp,
        _signer: &mut BurnchainOpSigner,
        _attempt: u64,
    ) -> Option<Transaction> {
        unimplemented!()
    }

    /// Send a serialized tx to the Bitcoin node.  Return Some(txid) on successful send; None on
    /// failure.
    pub fn send_transaction(&self, transaction: SerializedTx) -> Option<Txid> {
        test_debug!("Send raw transaction: {}", transaction.to_hex());
        let result = BitcoinRPCRequest::send_raw_transaction(&self.config, transaction.to_hex());
        match result {
            Ok(_) => {
                test_debug!("Sent transaction {}", &transaction.txid);
                Some(transaction.txid())
            }
            Err(e) => {
                error!(
                    "Bitcoin RPC failure: transaction submission failed - {:?}",
                    e
                );
                None
            }
        }
    }

    /// wait until the ChainsCoordinator has processed sortitions up to
    /// height_to_wait
    pub fn wait_for_sortitions(
        &self,
        coord_comms: CoordinatorChannels,
        height_to_wait: u64,
    ) -> Result<BurnchainTip, BurnchainControllerError> {
        let mut debug_ctr = 0;
        loop {
            let canonical_sortition_tip =
                SortitionDB::get_canonical_burn_chain_tip(self.sortdb_ref().conn()).unwrap();

            if debug_ctr % 10 == 0 {
                debug!(
                    "Waiting until canonical sortition height reaches {} (currently {})",
                    height_to_wait, canonical_sortition_tip.block_height
                );
            }
            debug_ctr += 1;

            if canonical_sortition_tip.block_height >= height_to_wait {
                let (_, state_transition) = self
                    .sortdb_ref()
                    .get_sortition_result(&canonical_sortition_tip.sortition_id)
                    .expect("Sortition DB error.")
                    .expect("BUG: no data for the canonical chain tip");

                return Ok(BurnchainTip {
                    block_snapshot: canonical_sortition_tip,
                    received_at: Instant::now(),
                    state_transition,
                });
            }

            if !self.should_keep_running() {
                return Err(BurnchainControllerError::CoordinatorClosed);
            }

            // help the chains coordinator along
            coord_comms.announce_new_burn_block();
            coord_comms.announce_new_stacks_block();

            // yield some time
            sleep_ms(1000);
        }
    }

    /// Instruct a regtest Bitcoin node to build the next block.
    pub fn build_next_block(&self, num_blocks: u64) {
        debug!("Generate {} block(s)", num_blocks);
        let public_key_bytes = match &self.config.burnchain.local_mining_public_key {
            Some(public_key) => hex_bytes(public_key).expect("Invalid byte sequence"),
            None => panic!("Unable to make new block, mining public key"),
        };

        // NOTE: miner address is whatever the configured segwit setting is
        let public_key = Secp256k1PublicKey::from_slice(&public_key_bytes)
            .expect("FATAL: invalid public key bytes");
        let address = self.get_miner_address(StacksEpochId::Epoch21, &public_key);
        let result =
            BitcoinRPCRequest::generate_to_address(&self.config, num_blocks, addr2str(&address));

        match result {
            Ok(_) => {}
            Err(e) => {
                error!("Bitcoin RPC failure: error generating block {:?}", e);
                panic!();
            }
        }
    }

    #[cfg(test)]
    pub fn invalidate_block(&self, block: &BurnchainHeaderHash) {
        info!("Invalidating block {}", &block);
        let request = BitcoinRPCRequest {
            method: "invalidateblock".into(),
            params: vec![json!(&block.to_string())],
            id: "stacks-forker".into(),
            jsonrpc: "2.0".into(),
        };
        if let Err(e) = BitcoinRPCRequest::send(&self.config, request) {
            error!("Bitcoin RPC failure: error invalidating block {:?}", e);
            panic!();
        }
    }

    #[cfg(test)]
    pub fn get_block_hash(&self, height: u64) -> BurnchainHeaderHash {
        let request = BitcoinRPCRequest {
            method: "getblockhash".into(),
            params: vec![json!(height)],
            id: "stacks-forker".into(),
            jsonrpc: "2.0".into(),
        };
        match BitcoinRPCRequest::send(&self.config, request) {
            Ok(v) => {
                BurnchainHeaderHash::from_hex(v.get("result").unwrap().as_str().unwrap()).unwrap()
            }
            Err(e) => {
                error!("Bitcoin RPC failure: error invalidating block {:?}", e);
                panic!();
            }
        }
    }

    #[cfg(test)]
    pub fn get_mining_pubkey(&self) -> Option<String> {
        self.config.burnchain.local_mining_public_key.clone()
    }

    #[cfg(test)]
    pub fn set_mining_pubkey(&mut self, pubkey: String) -> Option<String> {
        let old_key = self.config.burnchain.local_mining_public_key.take();
        self.config.burnchain.local_mining_public_key = Some(pubkey);
        old_key
    }

    #[cfg(test)]
    pub fn set_use_segwit(&mut self, segwit: bool) {
        self.config.miner.segwit = segwit;
    }

    #[cfg(test)]
    pub fn set_allow_rbf(&mut self, val: bool) {
        self.allow_rbf = val;
    }

    #[cfg(not(test))]
    pub fn set_allow_rbf(&mut self, _val: bool) {}

    pub fn make_operation_tx(
        &mut self,
        epoch_id: StacksEpochId,
        operation: BlockstackOperationType,
        op_signer: &mut BurnchainOpSigner,
        attempt: u64,
    ) -> Option<SerializedTx> {
        let transaction = match operation {
            BlockstackOperationType::LeaderBlockCommit(payload) => {
                self.build_leader_block_commit_tx(epoch_id, payload, op_signer, attempt)
            }
            BlockstackOperationType::LeaderKeyRegister(payload) => {
                self.build_leader_key_register_tx(epoch_id, payload, op_signer, attempt)
            }
            BlockstackOperationType::UserBurnSupport(payload) => {
                self.build_user_burn_support_tx(epoch_id, payload, op_signer, attempt)
            }
            BlockstackOperationType::PreStx(payload) => {
                self.build_pre_stacks_tx(epoch_id, payload, op_signer)
            }
            BlockstackOperationType::TransferStx(payload) => {
                self.build_transfer_stacks_tx(epoch_id, payload, op_signer, None)
            }
            BlockstackOperationType::StackStx(_payload) => unimplemented!(),
            BlockstackOperationType::DelegateStx(payload) => {
                self.build_delegate_stacks_tx(epoch_id, payload, op_signer, None)
            }
        };

        transaction.map(|tx| SerializedTx::new(tx))
    }

    #[cfg(test)]
    pub fn get_raw_transaction(&self, txid: &Txid) -> Transaction {
        let txstr = BitcoinRPCRequest::get_raw_transaction(&self.config, txid).unwrap();
        let tx: Transaction = btc_deserialize(&hex_bytes(&txstr).unwrap()).unwrap();
        tx
    }
}

impl BurnchainController for BitcoinRegtestController {
    fn sortdb_ref(&self) -> &SortitionDB {
        self.db
            .as_ref()
            .expect("BUG: did not instantiate the burn DB")
    }

    fn sortdb_mut(&mut self) -> &mut SortitionDB {
        let burnchain = self.get_burnchain();

        let (db, burnchain_db) = burnchain.open_db(true).unwrap();
        self.db = Some(db);
        self.burnchain_db = Some(burnchain_db);

        match self.db {
            Some(ref mut sortdb) => sortdb,
            None => unreachable!(),
        }
    }

    fn get_chain_tip(&self) -> BurnchainTip {
        match &self.chain_tip {
            Some(chain_tip) => chain_tip.clone(),
            None => {
                unreachable!();
            }
        }
    }

    fn get_headers_height(&self) -> u64 {
        let (_, network_id) = self.config.burnchain.get_bitcoin_network();
        let spv_client = SpvClient::new(
            &self.config.get_spv_headers_file_path(),
            0,
            None,
            network_id,
            false,
            false,
        )
        .expect("Unable to open burnchain headers DB");
        spv_client
            .get_headers_height()
            .expect("Unable to query number of burnchain headers")
    }

    fn connect_dbs(&mut self) -> Result<(), BurnchainControllerError> {
        let burnchain = self.get_burnchain();
        burnchain.connect_db(
            true,
            self.indexer.get_first_block_header_hash()?,
            self.indexer.get_first_block_header_timestamp()?,
            self.indexer.get_stacks_epochs(),
        )?;
        Ok(())
    }

    fn get_stacks_epochs(&self) -> Vec<StacksEpoch> {
        self.indexer.get_stacks_epochs()
    }

    fn start(
        &mut self,
        target_block_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), BurnchainControllerError> {
        // if no target block height is given, just fetch the first burnchain block.
        self.receive_blocks(
            false,
            target_block_height_opt.map_or_else(|| Some(1), |x| Some(x)),
        )
    }

    fn sync(
        &mut self,
        target_block_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), BurnchainControllerError> {
        let (burnchain_tip, burnchain_height) = if self.config.burnchain.mode == "helium" {
            // Helium: this node is responsible for mining new burnchain blocks
            self.build_next_block(1);
            self.receive_blocks(true, None)?
        } else {
            // Neon: this node is waiting on a block to be produced
            self.receive_blocks(true, target_block_height_opt)?
        };

        // Evaluate process_exit_at_block_height setting
        if let Some(cap) = self.config.burnchain.process_exit_at_block_height {
            if burnchain_tip.block_snapshot.block_height >= cap {
                info!(
                    "Node succesfully reached the end of the ongoing {} blocks epoch!",
                    cap
                );
                info!("This process will automatically terminate in 30s, restart your node for participating in the next epoch.");
                sleep_ms(30000);
                std::process::exit(0);
            }
        }
        Ok((burnchain_tip, burnchain_height))
    }

    // returns true if the operation was submitted successfully, false otherwise
    fn submit_operation(
        &mut self,
        epoch_id: StacksEpochId,
        operation: BlockstackOperationType,
        op_signer: &mut BurnchainOpSigner,
        attempt: u64,
    ) -> Option<Txid> {
        let transaction = self.make_operation_tx(epoch_id, operation, op_signer, attempt)?;
        self.send_transaction(transaction)
    }

    #[cfg(test)]
    fn bootstrap_chain(&mut self, num_blocks: u64) {
        if let Some(ref local_mining_pubkey) = &self.config.burnchain.local_mining_public_key {
            // NOTE: miner address is whatever the miner's segwit setting says it is here
            let mut local_mining_pubkey =
                Secp256k1PublicKey::from_hex(local_mining_pubkey).unwrap();
            let address = self.get_miner_address(StacksEpochId::Epoch21, &local_mining_pubkey);

            if self.config.miner.segwit {
                local_mining_pubkey.set_compressed(true);
            }

            info!("Creating wallet if it does not exist");
            match self.create_wallet_if_dne() {
                Err(e) => warn!("Error when creating wallet: {:?}", e),
                _ => {}
            }

            test_debug!("Import public key '{}'", &local_mining_pubkey.to_hex());

            let _result = BitcoinRPCRequest::import_public_key(&self.config, &local_mining_pubkey);

            test_debug!(
                "Generate to address '{}' for public key '{}'",
                &addr2str(&address),
                &local_mining_pubkey.to_hex()
            );
            let result = BitcoinRPCRequest::generate_to_address(
                &self.config,
                num_blocks,
                addr2str(&address),
            );

            match result {
                Ok(_) => {}
                Err(e) => {
                    error!("Bitcoin RPC failure: error generating block {:?}", e);
                    panic!();
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct UTXOSet {
    bhh: BurnchainHeaderHash,
    utxos: Vec<UTXO>,
}

impl UTXOSet {
    pub fn is_empty(&self) -> bool {
        self.utxos.len() == 0
    }

    pub fn total_available(&self) -> u64 {
        self.utxos.iter().map(|o| o.amount).sum()
    }
}

#[derive(Debug, Clone)]
pub struct SerializedTx {
    pub bytes: Vec<u8>,
    pub txid: Txid,
}

impl SerializedTx {
    pub fn new(tx: Transaction) -> SerializedTx {
        let txid = Txid::from_vec_be(&tx.txid().as_bytes().to_vec()).unwrap();
        let mut encoder = RawEncoder::new(Cursor::new(vec![]));
        tx.consensus_encode(&mut encoder)
            .expect("BUG: failed to serialize to a vec");
        let bytes: Vec<u8> = encoder.into_inner().into_inner();

        SerializedTx { txid, bytes }
    }

    pub fn txid(&self) -> Txid {
        self.txid.clone()
    }

    pub fn to_hex(&self) -> String {
        let formatted_bytes: Vec<String> =
            self.bytes.iter().map(|b| format!("{:02x}", b)).collect();
        format!("{}", formatted_bytes.join(""))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ParsedUTXO {
    txid: String,
    vout: u32,
    script_pub_key: String,
    amount: Box<RawValue>,
    confirmations: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UTXO {
    pub txid: Sha256dHash,
    pub vout: u32,
    pub script_pub_key: Script,
    pub amount: u64,
    pub confirmations: u32,
}

impl ParsedUTXO {
    pub fn get_txid(&self) -> Option<Sha256dHash> {
        match hex_bytes(&self.txid) {
            Ok(ref mut txid) => {
                txid.reverse();
                Some(Sha256dHash::from(&txid[..]))
            }
            Err(err) => {
                warn!("Unable to get txid from UTXO {}", err);
                None
            }
        }
    }

    pub fn get_sat_amount(&self) -> Option<u64> {
        ParsedUTXO::serialized_btc_to_sat(self.amount.get())
    }

    pub fn serialized_btc_to_sat(amount: &str) -> Option<u64> {
        let comps: Vec<&str> = amount.split(".").collect();
        match comps[..] {
            [lhs, rhs] => {
                if rhs.len() > 8 {
                    warn!("Unexpected amount of decimals");
                    return None;
                }

                match (lhs.parse::<u64>(), rhs.parse::<u64>()) {
                    (Ok(btc), Ok(frac_part)) => {
                        let base: u64 = 10;
                        let btc_to_sat = base.pow(8);
                        let mut amount = btc * btc_to_sat;
                        let sat = frac_part * base.pow(8 - rhs.len() as u32);
                        amount += sat;
                        Some(amount)
                    }
                    (lhs, rhs) => {
                        warn!("Error while converting BTC to sat {:?} - {:?}", lhs, rhs);
                        return None;
                    }
                }
            }
            _ => None,
        }
    }

    pub fn sat_to_serialized_btc(amount: u64) -> String {
        let base: u64 = 10;
        let int_part = amount / base.pow(8);
        let frac_part = amount % base.pow(8);
        let amount = format!("{}.{:08}", int_part, frac_part);
        amount
    }

    pub fn get_script_pub_key(&self) -> Option<Script> {
        match hex_bytes(&self.script_pub_key) {
            Ok(bytes) => Some(bytes.into()),
            Err(_) => {
                warn!("Unable to get script pub key");
                None
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BitcoinRPCRequest {
    /// The name of the RPC call
    pub method: String,
    /// Parameters to the RPC call
    pub params: Vec<serde_json::Value>,
    /// Identifier for this Request, which should appear in the response
    pub id: String,
    /// jsonrpc field, MUST be "2.0"
    pub jsonrpc: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum RPCError {
    Network(String),
    Parsing(String),
    Bitcoind(String),
}

type RPCResult<T> = Result<T, RPCError>;

impl BitcoinRPCRequest {
    fn build_rpc_request(config: &Config, payload: &BitcoinRPCRequest) -> Request {
        let url = {
            // some methods require a wallet ID
            let wallet_id = match payload.method.as_str() {
                "importaddress" | "listunspent" => Some(config.burnchain.wallet_name.clone()),
                _ => None,
            };
            let url = config.burnchain.get_rpc_url(wallet_id);
            Url::parse(&url).expect(&format!("Unable to parse {} as a URL", url))
        };
        debug!(
            "BitcoinRPC builder '{}': {:?}:{:?}@{}",
            &payload.method, &config.burnchain.username, &config.burnchain.password, &url
        );

        let mut req = Request::new(Method::Post, url);

        match (&config.burnchain.username, &config.burnchain.password) {
            (Some(username), Some(password)) => {
                let auth_token = format!("Basic {}", encode(format!("{}:{}", username, password)));
                req.append_header("Authorization", auth_token);
            }
            (_, _) => {}
        };
        req
    }

    #[cfg(test)]
    pub fn get_raw_transaction(config: &Config, txid: &Txid) -> RPCResult<String> {
        debug!("Get raw transaction {}", txid);
        let payload = BitcoinRPCRequest {
            method: "getrawtransaction".to_string(),
            params: vec![format!("{}", txid).into()],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };
        let res = BitcoinRPCRequest::send(&config, payload)?;
        debug!("Got raw transaction {}: {:?}", txid, &res);
        Ok(res.get("result").unwrap().as_str().unwrap().to_string())
    }

    pub fn generate_to_address(config: &Config, num_blocks: u64, address: String) -> RPCResult<()> {
        debug!("Generate {} blocks to {}", num_blocks, &address);
        let payload = BitcoinRPCRequest {
            method: "generatetoaddress".to_string(),
            params: vec![num_blocks.into(), address.clone().into()],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };

        let res = BitcoinRPCRequest::send(&config, payload)?;
        debug!(
            "Generated {} blocks to {}: {:?}",
            num_blocks, &address, &res
        );
        Ok(())
    }

    pub fn list_unspent(
        config: &Config,
        addresses: Vec<String>,
        include_unsafe: bool,
        minimum_sum_amount: u64,
        utxos_to_exclude: &Option<UTXOSet>,
        block_height: u64,
    ) -> RPCResult<UTXOSet> {
        let payload = BitcoinRPCRequest {
            method: "getblockhash".to_string(),
            params: vec![block_height.into()],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };

        let mut res = BitcoinRPCRequest::send(&config, payload)?;
        let bhh = match res.as_object_mut() {
            Some(res) => {
                let res = res
                    .get("result")
                    .ok_or(RPCError::Parsing("Failed to get bestblockhash".to_string()))?;
                let bhh: String = serde_json::from_value(res.to_owned())
                    .map_err(|_| RPCError::Parsing("Failed to get bestblockhash".to_string()))?;
                let bhh = BurnchainHeaderHash::from_hex(&bhh)
                    .map_err(|_| RPCError::Parsing("Failed to get bestblockhash".to_string()))?;
                Ok(bhh)
            }
            _ => return Err(RPCError::Parsing("Failed to get UTXOs".to_string())),
        }?;

        let min_conf = 0i64;
        let max_conf = 9999999i64;
        let minimum_amount = ParsedUTXO::sat_to_serialized_btc(minimum_sum_amount);

        let payload = BitcoinRPCRequest {
            method: "listunspent".to_string(),
            params: vec![
                min_conf.into(),
                max_conf.into(),
                addresses.into(),
                include_unsafe.into(),
                json!({ "minimumAmount": minimum_amount }),
            ],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };

        let mut res = BitcoinRPCRequest::send(&config, payload)?;
        let txids_to_filter = if let Some(utxos_to_exclude) = utxos_to_exclude {
            utxos_to_exclude
                .utxos
                .iter()
                .map(|utxo| utxo.txid)
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        let mut utxos = vec![];

        match res.as_object_mut() {
            Some(ref mut object) => match object.get_mut("result") {
                Some(serde_json::Value::Array(entries)) => {
                    while let Some(entry) = entries.pop() {
                        let parsed_utxo: ParsedUTXO = match serde_json::from_value(entry) {
                            Ok(utxo) => utxo,
                            Err(err) => {
                                warn!("Failed parsing UTXO: {}", err);
                                continue;
                            }
                        };
                        let amount = match parsed_utxo.get_sat_amount() {
                            Some(amount) => amount,
                            None => continue,
                        };

                        if amount < minimum_sum_amount {
                            continue;
                        }

                        let script_pub_key = match parsed_utxo.get_script_pub_key() {
                            Some(script_pub_key) => script_pub_key,
                            None => {
                                continue;
                            }
                        };

                        let txid = match parsed_utxo.get_txid() {
                            Some(amount) => amount,
                            None => continue,
                        };

                        // Exclude UTXOs that we want to filter
                        if txids_to_filter.contains(&txid) {
                            continue;
                        }

                        utxos.push(UTXO {
                            txid,
                            vout: parsed_utxo.vout,
                            script_pub_key,
                            amount,
                            confirmations: parsed_utxo.confirmations,
                        });
                    }
                }
                _ => {
                    warn!("Failed to get UTXOs");
                }
            },
            _ => {
                warn!("Failed to get UTXOs");
            }
        };

        Ok(UTXOSet { bhh, utxos })
    }

    pub fn send_raw_transaction(config: &Config, tx: String) -> RPCResult<()> {
        let payload = BitcoinRPCRequest {
            method: "sendrawtransaction".to_string(),
            params: vec![tx.into()],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };

        let json_resp = BitcoinRPCRequest::send(&config, payload)?;

        if let Some(e) = json_resp.get("error") {
            if !e.is_null() {
                error!("Error submitting transaction: {}", json_resp);
                return Err(RPCError::Bitcoind(json_resp.to_string()));
            }
        }
        Ok(())
    }

    pub fn import_public_key(config: &Config, public_key: &Secp256k1PublicKey) -> RPCResult<()> {
        let pkh = Hash160::from_data(&public_key.to_bytes())
            .to_bytes()
            .to_vec();
        let (_, network_id) = config.burnchain.get_bitcoin_network();

        // import both the legacy and segwit variants of this public key
        let mut addresses = vec![BitcoinAddress::from_bytes_legacy(
            network_id,
            LegacyBitcoinAddressType::PublicKeyHash,
            &pkh,
        )
        .expect("Public key incorrect")];

        if config.miner.segwit {
            addresses.push(
                BitcoinAddress::from_bytes_segwit_p2wpkh(network_id, &pkh)
                    .expect("Public key incorrect"),
            );
        }

        for address in addresses.into_iter() {
            debug!(
                "Import address {} for public key {}",
                addr2str(&address),
                public_key.to_hex()
            );

            let payload = BitcoinRPCRequest {
                method: "getdescriptorinfo".to_string(),
                params: vec![format!("addr({})", &addr2str(&address)).into()],
                id: "stacks".to_string(),
                jsonrpc: "2.0".to_string(),
            };

            let result = BitcoinRPCRequest::send(&config, payload)?;
            let checksum = result
                .get(&"result".to_string())
                .and_then(|res| res.as_object())
                .and_then(|obj| obj.get("checksum"))
                .and_then(|checksum_val| checksum_val.as_str())
                .ok_or(RPCError::Bitcoind(format!(
                    "Did not receive an object with `checksum` from `getdescriptorinfo \"{}\"`",
                    &addr2str(&address)
                )))?;

            let payload = BitcoinRPCRequest {
                method: "importdescriptors".to_string(),
                params: vec![
                    json!([{ "desc": format!("addr({})#{}", &addr2str(&address), &checksum), "timestamp": 0, "internal": true }]),
                ],
                id: "stacks".to_string(),
                jsonrpc: "2.0".to_string(),
            };

            BitcoinRPCRequest::send(&config, payload)?;
        }
        Ok(())
    }

    /// Calls `listwallets` method through RPC call and returns wallet names as a vector of Strings
    pub fn list_wallets(config: &Config) -> RPCResult<Vec<String>> {
        let payload = BitcoinRPCRequest {
            method: "listwallets".to_string(),
            params: vec![],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };

        let mut res = BitcoinRPCRequest::send(&config, payload)?;
        let mut wallets = Vec::new();
        match res.as_object_mut() {
            Some(ref mut object) => match object.get_mut("result") {
                Some(serde_json::Value::Array(entries)) => {
                    while let Some(entry) = entries.pop() {
                        let parsed_wallet_name: String = match serde_json::from_value(entry) {
                            Ok(wallet_name) => wallet_name,
                            Err(err) => {
                                warn!("Failed parsing wallet name: {}", err);
                                continue;
                            }
                        };

                        wallets.push(parsed_wallet_name);
                    }
                }
                _ => {
                    warn!("Failed to get wallets");
                }
            },
            _ => {
                warn!("Failed to get wallets");
            }
        };

        Ok(wallets)
    }

    /// Tries to create a wallet with the given name
    pub fn create_wallet(config: &Config, wallet_name: &str) -> RPCResult<()> {
        let payload = BitcoinRPCRequest {
            method: "createwallet".to_string(),
            params: vec![wallet_name.into(), true.into()],
            id: "stacks".to_string(),
            jsonrpc: "2.0".to_string(),
        };

        BitcoinRPCRequest::send(&config, payload)?;
        Ok(())
    }

    fn send(config: &Config, payload: BitcoinRPCRequest) -> RPCResult<serde_json::Value> {
        let mut request = BitcoinRPCRequest::build_rpc_request(&config, &payload);

        let body = match serde_json::to_vec(&json!(payload)) {
            Ok(body) => body,
            Err(err) => {
                return Err(RPCError::Network(format!("RPC Error: {}", err)));
            }
        };

        request.append_header("Content-Type", "application/json");
        request.set_body(body);

        let mut response = async_std::task::block_on(async move {
            let stream = match TcpStream::connect(config.burnchain.get_rpc_socket_addr()).await {
                Ok(stream) => stream,
                Err(err) => {
                    return Err(RPCError::Network(format!(
                        "Bitcoin RPC: connection failed - {:?}",
                        err
                    )))
                }
            };

            match client::connect(stream, request).await {
                Ok(response) => Ok(response),
                Err(err) => {
                    return Err(RPCError::Network(format!(
                        "Bitcoin RPC: invoking procedure failed - {:?}",
                        err
                    )))
                }
            }
        })?;

        let status = response.status();

        let (res, buffer) = async_std::task::block_on(async move {
            let mut buffer = Vec::new();
            let mut body = response.take_body();
            let res = body.read_to_end(&mut buffer).await;
            (res, buffer)
        });

        if !status.is_success() {
            return Err(RPCError::Network(format!(
                "Bitcoin RPC: status({}) != success, body is '{:?}'",
                status,
                match serde_json::from_slice::<serde_json::Value>(&buffer[..]) {
                    Ok(v) => v,
                    Err(_e) => serde_json::from_str("\"(unparseable)\"")
                        .expect("Failed to parse JSON literal"),
                }
            )));
        }

        if res.is_err() {
            return Err(RPCError::Network(format!(
                "Bitcoin RPC: unable to read body - {:?}",
                res
            )));
        }

        let payload = serde_json::from_slice::<serde_json::Value>(&buffer[..])
            .map_err(|e| RPCError::Parsing(format!("Bitcoin RPC: {}", e)))?;
        Ok(payload)
    }
}
