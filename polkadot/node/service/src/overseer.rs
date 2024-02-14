// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::{AuthorityDiscoveryApi, Block, Error, Hash, IsParachainNode, Registry};
use polkadot_node_subsystem_types::{DefaultSubsystemClient, RuntimeApiSubsystemClient};
use polkadot_overseer::{DummySubsystem, InitializedOverseerBuilder, SubsystemError};
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_core::traits::SpawnNamed;

use polkadot_availability_distribution::IncomingRequestReceivers;
use polkadot_node_core_approval_voting::Config as ApprovalVotingConfig;
use polkadot_node_core_av_store::Config as AvailabilityConfig;
use polkadot_node_core_candidate_validation::Config as CandidateValidationConfig;
use polkadot_node_core_chain_selection::Config as ChainSelectionConfig;
use polkadot_node_core_dispute_coordinator::Config as DisputeCoordinatorConfig;
use polkadot_node_network_protocol::{
	peer_set::{PeerSet, PeerSetProtocolNames},
	request_response::{
		v1 as request_v1, v2 as request_v2, IncomingRequestReceiver, ReqProtocolNames,
	},
};
#[cfg(any(feature = "malus", test))]
pub use polkadot_overseer::{dummy::dummy_overseer_builder, HeadSupportsParachains};
use polkadot_overseer::{
	metrics::Metrics as OverseerMetrics, MetricsTrait, Overseer, OverseerConnector, OverseerHandle,
	SpawnGlue,
};

use parking_lot::Mutex;
use polkadot_primitives::runtime_api::ParachainHost;
use sc_authority_discovery::Service as AuthorityDiscoveryService;
use sc_client_api::AuxStore;
use sc_keystore::LocalKeystore;
use sc_network::{NetworkStateInfo, NotificationService};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_consensus_babe::BabeApi;
use std::{collections::HashMap, sync::Arc};

pub use polkadot_approval_distribution::ApprovalDistribution as ApprovalDistributionSubsystem;
pub use polkadot_availability_bitfield_distribution::BitfieldDistribution as BitfieldDistributionSubsystem;
pub use polkadot_availability_distribution::AvailabilityDistributionSubsystem;
pub use polkadot_availability_recovery::AvailabilityRecoverySubsystem;
pub use polkadot_collator_protocol::{CollatorProtocolSubsystem, ProtocolSide};
pub use polkadot_dispute_distribution::DisputeDistributionSubsystem;
pub use polkadot_gossip_support::GossipSupport as GossipSupportSubsystem;
pub use polkadot_network_bridge::{
	Metrics as NetworkBridgeMetrics, NetworkBridgeRx as NetworkBridgeRxSubsystem,
	NetworkBridgeTx as NetworkBridgeTxSubsystem,
};
pub use polkadot_node_collation_generation::CollationGenerationSubsystem;
pub use polkadot_node_core_approval_voting::ApprovalVotingSubsystem;
pub use polkadot_node_core_av_store::AvailabilityStoreSubsystem;
pub use polkadot_node_core_backing::CandidateBackingSubsystem;
pub use polkadot_node_core_bitfield_signing::BitfieldSigningSubsystem;
pub use polkadot_node_core_candidate_validation::CandidateValidationSubsystem;
pub use polkadot_node_core_chain_api::ChainApiSubsystem;
pub use polkadot_node_core_chain_selection::ChainSelectionSubsystem;
pub use polkadot_node_core_dispute_coordinator::DisputeCoordinatorSubsystem;
pub use polkadot_node_core_prospective_parachains::ProspectiveParachainsSubsystem;
pub use polkadot_node_core_provisioner::ProvisionerSubsystem;
pub use polkadot_node_core_pvf_checker::PvfCheckerSubsystem;
pub use polkadot_node_core_runtime_api::RuntimeApiSubsystem;
use polkadot_node_subsystem::OverseerSubsystemContext;
use polkadot_node_subsystem_util::{
	rand::{self, SeedableRng},
	Subsystem,
};
pub use polkadot_statement_distribution::StatementDistributionSubsystem;

/// Arguments passed for overseer construction.
pub struct OverseerGenArgs<'a, Spawner, RuntimeClient>
where
	Spawner: 'static + SpawnNamed + Clone + Unpin,
{
	/// Runtime client generic, providing the `ProvieRuntimeApi` trait besides others.
	pub runtime_client: Arc<RuntimeClient>,
	/// Underlying network service implementation.
	pub network_service: Arc<sc_network::NetworkService<Block, Hash>>,
	/// Underlying syncing service implementation.
	pub sync_service: Arc<sc_network_sync::SyncingService<Block>>, /* TODOR , Box<dyn
	                                                                * sp_consensus::SyncOracle +
	                                                                * Send> */
	/// Underlying authority discovery service.
	pub authority_discovery_service: AuthorityDiscoveryService,
	/// Collations request receiver for network protocol v1.
	pub collation_req_v1_receiver: IncomingRequestReceiver<request_v1::CollationFetchingRequest>,
	/// Collations request receiver for network protocol v2.
	pub collation_req_v2_receiver: IncomingRequestReceiver<request_v2::CollationFetchingRequest>,
	/// Receiver for available data requests.
	pub available_data_req_receiver:
		IncomingRequestReceiver<request_v1::AvailableDataFetchingRequest>,
	/// Prometheus registry, commonly used for production systems, less so for test.
	pub registry: Option<&'a Registry>,
	/// Task spawner to be used throughout the overseer and the APIs it provides.
	pub spawner: Spawner,
	/// Determines the behavior of the collator.
	pub is_parachain_node: IsParachainNode,
	/// Overseer channel capacity override.
	pub overseer_message_channel_capacity_override: Option<usize>,
	/// Request-response protocol names source.
	pub req_protocol_names: ReqProtocolNames,
	/// `PeerSet` protocol names to protocols mapping.
	pub peerset_protocol_names: PeerSetProtocolNames,
	/// The offchain transaction pool factory.
	pub offchain_transaction_pool_factory: OffchainTransactionPoolFactory<Block>,
	/// Notification services for validation/collation protocols.
	pub notification_services: HashMap<PeerSet, Box<dyn NotificationService>>,
}

pub struct ExtendedOverseerGenArgs {
	/// The keystore to use for i.e. validator keys.
	pub keystore: Arc<LocalKeystore>,
	/// The underlying key value store for the parachains.
	pub parachains_db: Arc<dyn polkadot_node_subsystem_util::database::Database>,
	/// Configuration for the candidate validation subsystem.
	pub candidate_validation_config: Option<CandidateValidationConfig>,
	/// Configuration for the availability store subsystem.
	pub availability_config: AvailabilityConfig,
	/// POV request receiver.
	pub pov_req_receiver: IncomingRequestReceiver<request_v1::PoVFetchingRequest>,
	/// Erasure chunks request receiver.
	pub chunk_req_receiver: IncomingRequestReceiver<request_v1::ChunkFetchingRequest>,
	/// Receiver for incoming large statement requests.
	pub statement_req_receiver: IncomingRequestReceiver<request_v1::StatementFetchingRequest>,
	/// Receiver for incoming candidate requests.
	pub candidate_req_v2_receiver: IncomingRequestReceiver<request_v2::AttestedCandidateRequest>,
	/// Configuration for the approval voting subsystem.
	pub approval_voting_config: ApprovalVotingConfig,
	/// Receiver for incoming disputes.
	pub dispute_req_receiver: IncomingRequestReceiver<request_v1::DisputeRequest>,
	/// Configuration for the dispute coordinator subsystem.
	pub dispute_coordinator_config: DisputeCoordinatorConfig,
	/// Configuration for the chain selection subsystem.
	pub chain_selection_config: ChainSelectionConfig,
}

/// Obtain a prepared validator `Overseer`, that is initialized with all default values.
pub fn validator_overseer_builder<Spawner, RuntimeClient>(
	OverseerGenArgs {
		runtime_client,
		network_service,
		sync_service,
		authority_discovery_service,
		collation_req_v1_receiver: _,
		collation_req_v2_receiver: _,
		available_data_req_receiver,
		registry,
		spawner,
		is_parachain_node,
		overseer_message_channel_capacity_override,
		req_protocol_names,
		peerset_protocol_names,
		offchain_transaction_pool_factory,
		notification_services,
	}: OverseerGenArgs<Spawner, RuntimeClient>,
	ExtendedOverseerGenArgs {
		keystore,
		parachains_db,
		candidate_validation_config,
		availability_config,
		pov_req_receiver,
		chunk_req_receiver,
		statement_req_receiver,
		candidate_req_v2_receiver,
		approval_voting_config,
		dispute_req_receiver,
		dispute_coordinator_config,
		chain_selection_config,
	}: ExtendedOverseerGenArgs,
) -> Result<
	InitializedOverseerBuilder<
		SpawnGlue<Spawner>,
		Arc<DefaultSubsystemClient<RuntimeClient>>,
		CandidateValidationSubsystem,
		PvfCheckerSubsystem,
		CandidateBackingSubsystem,
		StatementDistributionSubsystem<rand::rngs::StdRng>,
		AvailabilityDistributionSubsystem,
		AvailabilityRecoverySubsystem,
		BitfieldSigningSubsystem,
		BitfieldDistributionSubsystem,
		ProvisionerSubsystem,
		RuntimeApiSubsystem<DefaultSubsystemClient<RuntimeClient>>,
		AvailabilityStoreSubsystem,
		NetworkBridgeRxSubsystem<
			Arc<sc_network::NetworkService<Block, Hash>>,
			AuthorityDiscoveryService,
		>,
		NetworkBridgeTxSubsystem<
			Arc<sc_network::NetworkService<Block, Hash>>,
			AuthorityDiscoveryService,
		>,
		ChainApiSubsystem<RuntimeClient>,
		CollationGenerationSubsystem,
		CollatorProtocolSubsystem,
		ApprovalDistributionSubsystem,
		ApprovalVotingSubsystem,
		GossipSupportSubsystem<AuthorityDiscoveryService>,
		DisputeCoordinatorSubsystem,
		DisputeDistributionSubsystem<AuthorityDiscoveryService>,
		ChainSelectionSubsystem,
		ProspectiveParachainsSubsystem,
	>,
	Error,
>
where
	RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
	RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
	Spawner: 'static + SpawnNamed + Clone + Unpin,
{
	use polkadot_node_subsystem_util::metrics::Metrics;

	let metrics = <OverseerMetrics as MetricsTrait>::register(registry)?;
	let notification_sinks = Arc::new(Mutex::new(HashMap::new()));

	let spawner = SpawnGlue(spawner);

	let network_bridge_metrics: NetworkBridgeMetrics = Metrics::register(registry)?;

	let runtime_api_client = Arc::new(DefaultSubsystemClient::new(
		runtime_client.clone(),
		offchain_transaction_pool_factory,
	));

	let builder = Overseer::builder()
		.network_bridge_tx(NetworkBridgeTxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			network_bridge_metrics.clone(),
			req_protocol_names,
			peerset_protocol_names.clone(),
			notification_sinks.clone(),
		))
		.network_bridge_rx(NetworkBridgeRxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			Box::new(sync_service.clone()),
			network_bridge_metrics,
			peerset_protocol_names,
			notification_services,
			notification_sinks,
		))
		.availability_distribution(AvailabilityDistributionSubsystem::new(
			keystore.clone(),
			IncomingRequestReceivers { pov_req_receiver, chunk_req_receiver },
			Metrics::register(registry)?,
		))
		.availability_recovery(AvailabilityRecoverySubsystem::with_chunks_if_pov_large(
			available_data_req_receiver,
			Metrics::register(registry)?,
		))
		.availability_store(AvailabilityStoreSubsystem::new(
			parachains_db.clone(),
			availability_config,
			Box::new(sync_service.clone()),
			Metrics::register(registry)?,
		))
		.bitfield_distribution(BitfieldDistributionSubsystem::new(Metrics::register(registry)?))
		.bitfield_signing(BitfieldSigningSubsystem::new(
			keystore.clone(),
			Metrics::register(registry)?,
		))
		.candidate_backing(CandidateBackingSubsystem::new(
			keystore.clone(),
			Metrics::register(registry)?,
		))
		.candidate_validation(CandidateValidationSubsystem::with_config(
			candidate_validation_config,
			Metrics::register(registry)?, // candidate-validation metrics
			Metrics::register(registry)?, // validation host metrics
		))
		.pvf_checker(PvfCheckerSubsystem::new(keystore.clone(), Metrics::register(registry)?))
		.chain_api(ChainApiSubsystem::new(runtime_client.clone(), Metrics::register(registry)?))
		.collation_generation(CollationGenerationSubsystem::new(Metrics::register(registry)?))
		.collator_protocol({
			let side = match is_parachain_node {
				IsParachainNode::Collator(_) | IsParachainNode::FullNode =>
					return Err(Error::Overseer(SubsystemError::Context(
						"build validator overseer for parachain node".to_owned(),
					))),
				IsParachainNode::No => ProtocolSide::Validator {
					keystore: keystore.clone(),
					eviction_policy: Default::default(),
					metrics: Metrics::register(registry)?,
				},
			};
			CollatorProtocolSubsystem::new(side)
		})
		.provisioner(ProvisionerSubsystem::new(Metrics::register(registry)?))
		.runtime_api(RuntimeApiSubsystem::new(
			runtime_api_client.clone(),
			Metrics::register(registry)?,
			spawner.clone(),
		))
		.statement_distribution(StatementDistributionSubsystem::new(
			keystore.clone(),
			statement_req_receiver,
			candidate_req_v2_receiver,
			Metrics::register(registry)?,
			rand::rngs::StdRng::from_entropy(),
		))
		.approval_distribution(ApprovalDistributionSubsystem::new(Metrics::register(registry)?))
		.approval_voting(ApprovalVotingSubsystem::with_config(
			approval_voting_config,
			parachains_db.clone(),
			keystore.clone(),
			Box::new(sync_service.clone()),
			Metrics::register(registry)?,
		))
		.gossip_support(GossipSupportSubsystem::new(
			keystore.clone(),
			authority_discovery_service.clone(),
			Metrics::register(registry)?,
		))
		.dispute_coordinator(DisputeCoordinatorSubsystem::new(
			parachains_db.clone(),
			dispute_coordinator_config,
			keystore.clone(),
			Metrics::register(registry)?,
		))
		.dispute_distribution(DisputeDistributionSubsystem::new(
			keystore.clone(),
			dispute_req_receiver,
			authority_discovery_service.clone(),
			Metrics::register(registry)?,
		))
		.chain_selection(ChainSelectionSubsystem::new(chain_selection_config, parachains_db))
		.prospective_parachains(ProspectiveParachainsSubsystem::new(Metrics::register(registry)?))
		.activation_external_listeners(Default::default())
		.span_per_active_leaf(Default::default())
		.active_leaves(Default::default())
		.supports_parachains(runtime_api_client)
		.metrics(metrics)
		.spawner(spawner);

	let builder = if let Some(capacity) = overseer_message_channel_capacity_override {
		builder.message_channel_capacity(capacity)
	} else {
		builder
	};
	Ok(builder)
}

pub type InitializedCollatorOverseerBuilder<Spawner, SubsystemClient, ChainApi> =
	InitializedOverseerBuilder<
		SpawnGlue<Spawner>,
		Arc<SubsystemClient>,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		AvailabilityRecoverySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		RuntimeApiSubsystem<SubsystemClient>,
		DummySubsystem,
		NetworkBridgeRxSubsystem<
			Arc<sc_network::NetworkService<Block, Hash>>,
			AuthorityDiscoveryService,
		>,
		NetworkBridgeTxSubsystem<
			Arc<sc_network::NetworkService<Block, Hash>>,
			AuthorityDiscoveryService,
		>,
		ChainApi,
		CollationGenerationSubsystem,
		CollatorProtocolSubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		DummySubsystem,
		ProspectiveParachainsSubsystem,
	>;

/// Obtain a prepared collator `Overseer`, that is initialized with all default values.
pub fn generic_collator_overseer_builder<Spawner, SubsystemClient, ChainApi>(
	OverseerGenArgs {
		runtime_client,
		network_service,
		sync_service,
		authority_discovery_service,
		collation_req_v1_receiver,
		collation_req_v2_receiver,
		available_data_req_receiver,
		registry,
		spawner,
		is_parachain_node,
		overseer_message_channel_capacity_override,
		req_protocol_names,
		peerset_protocol_names,
		offchain_transaction_pool_factory: _,
		notification_services,
	}: OverseerGenArgs<Spawner, SubsystemClient>,
	chain_api: ChainApi,
) -> Result<InitializedCollatorOverseerBuilder<Spawner, SubsystemClient, ChainApi>, Error>
where
	Spawner: 'static + SpawnNamed + Clone + Unpin,
	ChainApi: Subsystem<
		OverseerSubsystemContext<polkadot_node_subsystem_types::messages::ChainApiMessage>,
		SubsystemError,
	>,
	SubsystemClient: RuntimeApiSubsystemClient + std::marker::Sync + std::marker::Send + 'static,
{
	use polkadot_node_subsystem_util::metrics::Metrics;

	let notification_sinks = Arc::new(Mutex::new(HashMap::new()));

	let spawner = SpawnGlue(spawner);

	let network_bridge_metrics: NetworkBridgeMetrics = Metrics::register(registry)?;

	let builder = Overseer::builder()
		.network_bridge_tx(NetworkBridgeTxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			network_bridge_metrics.clone(),
			req_protocol_names,
			peerset_protocol_names.clone(),
			notification_sinks.clone(),
		))
		.network_bridge_rx(NetworkBridgeRxSubsystem::new(
			network_service.clone(),
			authority_discovery_service.clone(),
			Box::new(sync_service.clone()),
			network_bridge_metrics,
			peerset_protocol_names,
			notification_services,
			notification_sinks,
		))
		.availability_distribution(DummySubsystem)
		.availability_recovery(AvailabilityRecoverySubsystem::for_collator(
			available_data_req_receiver,
			Metrics::register(registry)?,
		))
		.availability_store(DummySubsystem)
		.bitfield_distribution(DummySubsystem)
		.bitfield_signing(DummySubsystem)
		.candidate_backing(DummySubsystem)
		.candidate_validation(DummySubsystem)
		.pvf_checker(DummySubsystem)
		.chain_api(chain_api)
		.collation_generation(CollationGenerationSubsystem::new(Metrics::register(registry)?))
		.collator_protocol({
			let side = match is_parachain_node {
				IsParachainNode::No =>
					return Err(Error::Overseer(SubsystemError::Context(
						"build parachain node overseer for validator".to_owned(),
					))),
				IsParachainNode::Collator(collator_pair) => ProtocolSide::Collator {
					peer_id: network_service.local_peer_id(),
					collator_pair,
					request_receiver_v1: collation_req_v1_receiver,
					request_receiver_v2: collation_req_v2_receiver,
					metrics: Metrics::register(registry)?,
				},
				IsParachainNode::FullNode => ProtocolSide::None,
			};
			CollatorProtocolSubsystem::new(side)
		})
		.provisioner(DummySubsystem)
		.runtime_api(RuntimeApiSubsystem::new(
			runtime_client.clone(),
			Metrics::register(registry)?,
			spawner.clone(),
		))
		.statement_distribution(DummySubsystem)
		.approval_distribution(DummySubsystem)
		.approval_voting(DummySubsystem)
		.gossip_support(DummySubsystem)
		.dispute_coordinator(DummySubsystem)
		.dispute_distribution(DummySubsystem)
		.chain_selection(DummySubsystem)
		.prospective_parachains(ProspectiveParachainsSubsystem::new(Metrics::register(registry)?))
		.activation_external_listeners(Default::default())
		.span_per_active_leaf(Default::default())
		.active_leaves(Default::default())
		.supports_parachains(runtime_client)
		.metrics(Metrics::register(registry)?)
		.spawner(spawner);

	let builder = if let Some(capacity) = overseer_message_channel_capacity_override {
		builder.message_channel_capacity(capacity)
	} else {
		builder
	};
	Ok(builder)
}

/// Obtain a prepared collator `Overseer`, that is initialized with all default values.
pub fn collator_overseer_builder<Spawner, RuntimeClient>(
	OverseerGenArgs {
		runtime_client,
		network_service,
		sync_service,
		authority_discovery_service,
		collation_req_v1_receiver,
		collation_req_v2_receiver,
		available_data_req_receiver,
		registry,
		spawner,
		is_parachain_node,
		overseer_message_channel_capacity_override,
		req_protocol_names,
		peerset_protocol_names,
		offchain_transaction_pool_factory,
		notification_services,
	}: OverseerGenArgs<Spawner, RuntimeClient>,
) -> Result<
	InitializedCollatorOverseerBuilder<
		Spawner,
		DefaultSubsystemClient<RuntimeClient>,
		ChainApiSubsystem<RuntimeClient>,
	>,
	Error,
>
where
	RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
	RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
	Spawner: 'static + SpawnNamed + Clone + Unpin,
{
	use polkadot_node_subsystem_util::metrics::Metrics;

	let chain_api = ChainApiSubsystem::new(runtime_client.clone(), Metrics::register(registry)?);
	let runtime_client = Arc::new(DefaultSubsystemClient::new(
		runtime_client,
		offchain_transaction_pool_factory.clone(),
	));
	let args = OverseerGenArgs {
		runtime_client,
		network_service,
		sync_service,
		authority_discovery_service,
		collation_req_v1_receiver,
		collation_req_v2_receiver,
		available_data_req_receiver,
		registry,
		spawner,
		is_parachain_node,
		overseer_message_channel_capacity_override,
		req_protocol_names,
		peerset_protocol_names,
		offchain_transaction_pool_factory,
		notification_services,
	};
	generic_collator_overseer_builder(args, chain_api)
}

/// Trait for the `fn` generating the overseer.
pub trait OverseerGen {
	/// Overwrite the full generation of the overseer, including the subsystems.
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<Spawner, RuntimeClient>,
		ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<
		(Overseer<SpawnGlue<Spawner>, Arc<DefaultSubsystemClient<RuntimeClient>>>, OverseerHandle),
		Error,
	>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin;

	// It would be nice to make `create_subsystems` part of this trait,
	// but the amount of generic arguments that would be required as
	// as consequence make this rather annoying to implement and use.
}

/// The regular set of subsystems.
pub struct ValidatorOverseerGen;

impl OverseerGen for ValidatorOverseerGen {
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<Spawner, RuntimeClient>,
		ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<
		(Overseer<SpawnGlue<Spawner>, Arc<DefaultSubsystemClient<RuntimeClient>>>, OverseerHandle),
		Error,
	>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin,
	{
		let ext_args = ext_args.ok_or(Error::Overseer(SubsystemError::Context(
			"create validator overseer as mandatory extended arguments were not provided"
				.to_owned(),
		)))?;
		validator_overseer_builder(args, ext_args)?
			.build_with_connector(connector)
			.map_err(|e| e.into())
	}
}

/// Reduced set of subsystems, to use in collator and collator's full node.
pub struct CollatorOverseerGen;

impl OverseerGen for CollatorOverseerGen {
	fn generate<Spawner, RuntimeClient>(
		&self,
		connector: OverseerConnector,
		args: OverseerGenArgs<Spawner, RuntimeClient>,
		_ext_args: Option<ExtendedOverseerGenArgs>,
	) -> Result<
		(Overseer<SpawnGlue<Spawner>, Arc<DefaultSubsystemClient<RuntimeClient>>>, OverseerHandle),
		Error,
	>
	where
		RuntimeClient: 'static + ProvideRuntimeApi<Block> + HeaderBackend<Block> + AuxStore,
		RuntimeClient::Api: ParachainHost<Block> + BabeApi<Block> + AuthorityDiscoveryApi<Block>,
		Spawner: 'static + SpawnNamed + Clone + Unpin,
	{
		collator_overseer_builder(args)?
			.build_with_connector(connector)
			.map_err(|e| e.into())
	}
}
