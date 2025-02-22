// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::genesis_config::{ValidatorGenesisConfig, ValidatorGenesisConfigBuilder};
use crate::network_config::NetworkConfig;
use fastcrypto::encoding::{Encoding, Hex};
use fastcrypto::traits::KeyPair;
use narwhal_config::{NetworkAdminServerParameters, PrometheusMetricsParameters};
use std::net::SocketAddr;
use std::path::PathBuf;
use sui_config::node::{
    default_enable_index_processing, default_end_of_epoch_broadcast_channel_capacity,
    AuthorityKeyPairWithPath, AuthorityStorePruningConfig, DBCheckpointConfig,
    ExpensiveSafetyCheckConfig, KeyPairWithPath, DEFAULT_GRPC_CONCURRENCY_LIMIT,
};
use sui_config::p2p::{P2pConfig, SeedPeer};
use sui_config::{
    local_ip_utils, ConsensusConfig, NodeConfig, AUTHORITIES_DB_NAME, CONSENSUS_DB_NAME,
    FULL_NODE_DB_PATH,
};
use sui_protocol_config::SupportedProtocolVersions;
use sui_types::crypto::{AuthorityKeyPair, AuthorityPublicKeyBytes, SuiKeyPair};

/// This builder contains information that's not included in ValidatorGenesisConfig for building
/// a validator NodeConfig. It can be used to build either a genesis validator or a new validator.
#[derive(Clone)]
pub struct ValidatorConfigBuilder {
    config_directory: PathBuf,
    supported_protocol_versions: Option<SupportedProtocolVersions>,
}

impl ValidatorConfigBuilder {
    pub fn new(config_directory: PathBuf) -> Self {
        Self {
            config_directory,
            supported_protocol_versions: None,
        }
    }

    pub fn with_supported_protocol_versions(
        mut self,
        supported_protocol_versions: SupportedProtocolVersions,
    ) -> Self {
        assert!(self.supported_protocol_versions.is_none());
        self.supported_protocol_versions = Some(supported_protocol_versions);
        self
    }

    pub fn build(
        self,
        validator: ValidatorGenesisConfig,
        genesis: sui_config::genesis::Genesis,
    ) -> NodeConfig {
        let key_path = get_key_path(&validator.key_pair);
        let config_directory = self.config_directory;
        let db_path = config_directory
            .join(AUTHORITIES_DB_NAME)
            .join(key_path.clone());

        let network_address = validator.network_address;
        let consensus_address = validator.consensus_address;
        let consensus_db_path = config_directory.join(CONSENSUS_DB_NAME).join(key_path);
        let internal_worker_address = validator.consensus_internal_worker_address;
        let localhost = local_ip_utils::localhost_for_testing();
        let consensus_config = ConsensusConfig {
            address: consensus_address,
            db_path: consensus_db_path,
            internal_worker_address,
            max_pending_transactions: None,
            max_submit_position: None,
            submit_delay_step_override_millis: None,
            narwhal_config: narwhal_config::Parameters {
                network_admin_server: NetworkAdminServerParameters {
                    primary_network_admin_server_port: local_ip_utils::get_available_port(
                        &localhost,
                    ),
                    worker_network_admin_server_base_port: local_ip_utils::get_available_port(
                        &localhost,
                    ),
                },
                prometheus_metrics: PrometheusMetricsParameters {
                    socket_addr: validator.narwhal_metrics_address,
                },
                ..Default::default()
            },
        };

        let p2p_config = P2pConfig {
            listen_address: validator.p2p_listen_address.unwrap_or_else(|| {
                validator
                    .p2p_address
                    .udp_multiaddr_to_listen_address()
                    .unwrap()
            }),
            external_address: Some(validator.p2p_address),
            ..Default::default()
        };

        NodeConfig {
            protocol_key_pair: AuthorityKeyPairWithPath::new(validator.key_pair),
            network_key_pair: KeyPairWithPath::new(SuiKeyPair::Ed25519(validator.network_key_pair)),
            account_key_pair: KeyPairWithPath::new(validator.account_key_pair),
            worker_key_pair: KeyPairWithPath::new(SuiKeyPair::Ed25519(validator.worker_key_pair)),
            db_path,
            network_address,
            metrics_address: validator.metrics_address,
            admin_interface_port: local_ip_utils::get_available_port(&localhost),
            json_rpc_address: local_ip_utils::new_tcp_address_for_testing(&localhost)
                .to_socket_addr()
                .unwrap(),
            consensus_config: Some(consensus_config),
            enable_event_processing: false,
            enable_index_processing: default_enable_index_processing(),
            genesis: sui_config::node::Genesis::new(genesis),
            grpc_load_shed: None,
            grpc_concurrency_limit: Some(DEFAULT_GRPC_CONCURRENCY_LIMIT),
            p2p_config,
            authority_store_pruning_config: AuthorityStorePruningConfig::validator_config(),
            end_of_epoch_broadcast_channel_capacity:
                default_end_of_epoch_broadcast_channel_capacity(),
            checkpoint_executor_config: Default::default(),
            metrics: None,
            supported_protocol_versions: self.supported_protocol_versions,
            db_checkpoint_config: Default::default(),
            indirect_objects_threshold: usize::MAX,
            expensive_safety_check_config: ExpensiveSafetyCheckConfig::new_enable_all(),
            name_service_package_address: None,
            name_service_registry_id: None,
            name_service_reverse_registry_id: None,
            transaction_deny_config: Default::default(),
            certificate_deny_config: Default::default(),
            state_debug_dump_config: Default::default(),
        }
    }

    pub fn build_new_validator<R: rand::RngCore + rand::CryptoRng>(
        self,
        rng: &mut R,
        network_config: &NetworkConfig,
    ) -> NodeConfig {
        let validator_config = ValidatorGenesisConfigBuilder::new().build(rng);
        self.build(validator_config, network_config.genesis.clone())
    }
}

#[derive(Clone, Debug, Default)]
pub struct FullnodeConfigBuilder {
    config_directory: Option<PathBuf>,
    // port for json rpc api
    rpc_port: Option<u16>,
    rpc_addr: Option<SocketAddr>,
    supported_protocol_versions: Option<SupportedProtocolVersions>,
    db_checkpoint_config: Option<DBCheckpointConfig>,
    expensive_safety_check_config: Option<ExpensiveSafetyCheckConfig>,
}

impl FullnodeConfigBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config_directory(mut self, config_directory: PathBuf) -> Self {
        self.config_directory = Some(config_directory);
        self
    }

    pub fn with_rpc_port(mut self, port: u16) -> Self {
        assert!(self.rpc_addr.is_none() && self.rpc_port.is_none());
        self.rpc_port = Some(port);
        self
    }

    pub fn with_rpc_addr(mut self, addr: SocketAddr) -> Self {
        assert!(self.rpc_addr.is_none() && self.rpc_port.is_none());
        self.rpc_addr = Some(addr);
        self
    }

    pub fn with_supported_protocol_versions(mut self, versions: SupportedProtocolVersions) -> Self {
        self.supported_protocol_versions = Some(versions);
        self
    }

    pub fn with_db_checkpoint_config(mut self, db_checkpoint_config: DBCheckpointConfig) -> Self {
        self.db_checkpoint_config = Some(db_checkpoint_config);
        self
    }

    pub fn with_expensive_safety_check_config(
        mut self,
        expensive_safety_check_config: ExpensiveSafetyCheckConfig,
    ) -> Self {
        self.expensive_safety_check_config = Some(expensive_safety_check_config);
        self
    }

    pub fn build<R: rand::RngCore + rand::CryptoRng>(
        self,
        rng: &mut R,
        network_config: &NetworkConfig,
    ) -> NodeConfig {
        // Take advantage of ValidatorGenesisConfigBuilder to build the keypairs and addresses,
        // even though this is a fullnode.
        let validator_config = ValidatorGenesisConfigBuilder::new().build(rng);
        let ip = validator_config
            .network_address
            .to_socket_addr()
            .unwrap()
            .ip()
            .to_string();

        let key_path = get_key_path(&validator_config.key_pair);
        let config_directory = self
            .config_directory
            .unwrap_or_else(|| tempfile::tempdir().unwrap().into_path());
        let db_path = config_directory.join(FULL_NODE_DB_PATH).join(key_path);

        let p2p_config = {
            let seed_peers = network_config
                .validator_configs
                .iter()
                .map(|config| SeedPeer {
                    peer_id: Some(anemo::PeerId(
                        config.network_key_pair().public().0.to_bytes(),
                    )),
                    address: config.p2p_config.external_address.clone().unwrap(),
                })
                .collect();

            P2pConfig {
                listen_address: validator_config.p2p_listen_address.unwrap_or_else(|| {
                    validator_config
                        .p2p_address
                        .udp_multiaddr_to_listen_address()
                        .unwrap()
                }),
                external_address: Some(validator_config.p2p_address),
                seed_peers,
                ..Default::default()
            }
        };

        let localhost = local_ip_utils::localhost_for_testing();
        let json_rpc_address = self.rpc_addr.unwrap_or_else(|| {
            let rpc_port = self
                .rpc_port
                .unwrap_or_else(|| local_ip_utils::get_available_port(&ip));
            format!("{}:{}", ip, rpc_port).parse().unwrap()
        });

        NodeConfig {
            protocol_key_pair: AuthorityKeyPairWithPath::new(validator_config.key_pair),
            account_key_pair: KeyPairWithPath::new(validator_config.account_key_pair),
            worker_key_pair: KeyPairWithPath::new(SuiKeyPair::Ed25519(
                validator_config.worker_key_pair,
            )),
            network_key_pair: KeyPairWithPath::new(SuiKeyPair::Ed25519(
                validator_config.network_key_pair,
            )),

            db_path,
            network_address: validator_config.network_address,
            metrics_address: local_ip_utils::new_local_tcp_socket_for_testing(),
            admin_interface_port: local_ip_utils::get_available_port(&localhost),
            json_rpc_address,
            consensus_config: None,
            enable_event_processing: true, // This is unused.
            enable_index_processing: default_enable_index_processing(),
            genesis: sui_config::node::Genesis::new(network_config.genesis.clone()),
            grpc_load_shed: None,
            grpc_concurrency_limit: None,
            p2p_config,
            authority_store_pruning_config: AuthorityStorePruningConfig::fullnode_config(),
            end_of_epoch_broadcast_channel_capacity:
                default_end_of_epoch_broadcast_channel_capacity(),
            checkpoint_executor_config: Default::default(),
            metrics: None,
            supported_protocol_versions: self.supported_protocol_versions,
            db_checkpoint_config: self.db_checkpoint_config.unwrap_or_default(),
            indirect_objects_threshold: usize::MAX,
            expensive_safety_check_config: self
                .expensive_safety_check_config
                .unwrap_or_else(ExpensiveSafetyCheckConfig::new_enable_all),
            name_service_package_address: None,
            name_service_registry_id: None,
            name_service_reverse_registry_id: None,
            transaction_deny_config: Default::default(),
            certificate_deny_config: Default::default(),
            state_debug_dump_config: Default::default(),
        }
    }
}

/// Given a validator keypair, return a path that can be used to identify the validator.
fn get_key_path(key_pair: &AuthorityKeyPair) -> String {
    let public_key: AuthorityPublicKeyBytes = key_pair.public().into();
    let mut key_path = Hex::encode(public_key);
    // 12 is rather arbitrary here but it's a nice balance between being short and being unique.
    key_path.truncate(12);
    key_path
}
