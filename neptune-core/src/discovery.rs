//! Peer discovery mechanisms for Neptune Core.
//!
//! This module is adapted from Polkadot SDK's discovery.rs to work with Neptune's
//! networking architecture. It provides peer discovery through:
//!
//! - Bootstrap nodes: Hard-coded node identities and addresses
//! - mDNS: Discovers nodes on the local network
//! - Kademlia DHT: Random walk queries for peer propagation
//!
//! The `DiscoveryBehaviour` integrates with Neptune's existing peer management
//! system through channels and events.

use std::{
    cmp,
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroUsize,
    task::{Context, Poll},
    time::Duration,
};

use futures::prelude::*;
use futures_timer::Delay;
use libp2p::{
    core::{transport::PortUse, Endpoint, Multiaddr},
    kad::{
        self,
        store::{MemoryStore, MemoryStoreConfig},
        Behaviour as Kademlia, BucketInserts, Config as KademliaConfig, Event as KademliaEvent,
        GetClosestPeersError, GetClosestPeersOk,
        GetRecordOk, PeerRecord, QueryId, QueryResult, Record, RecordKey,
    },
    mdns::{self, tokio::Behaviour as TokioMdns},
    multiaddr::Protocol,
    swarm::{
        behaviour::{
            toggle::{Toggle, ToggleConnectionHandler},
            ExternalAddrConfirmed, FromSwarm,
        },
        ConnectionDenied, ConnectionId, NetworkBehaviour, StreamProtocol, THandler,
        THandlerInEvent, THandlerOutEvent, ToSwarm,
    },
    PeerId,
};
use linked_hash_set::LinkedHashSet;
use tracing::{debug, info, trace, warn};

use crate::config_models::network::Network;

/// Logging target for the file.
const LOG_TARGET: &str = "neptune-discovery";

/// Maximum number of known external addresses that we will cache.
const MAX_KNOWN_EXTERNAL_ADDRESSES: usize = 32;

/// Default value for Kademlia replication factor.
pub const DEFAULT_KADEMLIA_REPLICATION_FACTOR: usize = 20;

/// The minimum number of peers we expect an answer before we terminate the request.
const GET_RECORD_REDUNDANCY_FACTOR: u32 = 4;

/// Query timeout for Kademlia requests.
const KAD_QUERY_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum number of provider keys that can be stored.
const KADEMLIA_MAX_PROVIDER_KEYS: usize = 1024;

/// TTL for provider records.
const KADEMLIA_PROVIDER_RECORD_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours

/// Republish interval for provider records.
const KADEMLIA_PROVIDER_REPUBLISH_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60); // 12 hours

/// `DiscoveryBehaviour` configuration for Neptune Core.
pub struct DiscoveryConfig {
    local_peer_id: PeerId,
    permanent_addresses: Vec<(PeerId, Multiaddr)>,
    dht_random_walk: bool,
    allow_private_ip: bool,
    allow_non_globals_in_dht: bool,
    discovery_only_if_under_num: u64,
    enable_mdns: bool,
    kademlia_disjoint_query_paths: bool,
    kademlia_protocol: Option<StreamProtocol>,
    kademlia_replication_factor: NonZeroUsize,
    network: Network,
}

impl DiscoveryConfig {
    /// Create a default configuration with the given peer ID and network.
    pub fn new(local_peer_id: PeerId, network: Network) -> Self {
        Self {
            local_peer_id,
            permanent_addresses: Vec::new(),
            dht_random_walk: true,
            allow_private_ip: true,
            allow_non_globals_in_dht: false,
            discovery_only_if_under_num: std::u64::MAX,
            enable_mdns: network.performs_peer_discovery(),
            kademlia_disjoint_query_paths: false,
            kademlia_protocol: None,
            kademlia_replication_factor: NonZeroUsize::new(DEFAULT_KADEMLIA_REPLICATION_FACTOR)
                .expect("value is a constant; constant is non-zero; qed."),
            network,
        }
    }

    /// Set the number of active connections at which we pause discovery.
    pub fn discovery_limit(&mut self, limit: u64) -> &mut Self {
        self.discovery_only_if_under_num = limit;
        self
    }

    /// Set custom nodes which never expire, e.g. bootstrap or reserved nodes.
    pub fn with_permanent_addresses<I>(&mut self, permanent_addresses: I) -> &mut Self
    where
        I: IntoIterator<Item = (PeerId, Multiaddr)>,
    {
        self.permanent_addresses.extend(permanent_addresses);
        self
    }

    /// Whether the discovery behaviour should periodically perform a random
    /// walk on the DHT to discover peers.
    pub fn with_dht_random_walk(&mut self, value: bool) -> &mut Self {
        self.dht_random_walk = value;
        self
    }

    /// Should private IPv4/IPv6 addresses be reported?
    pub fn allow_private_ip(&mut self, value: bool) -> &mut Self {
        self.allow_private_ip = value;
        self
    }

    /// Should non-global addresses be inserted to the DHT?
    pub fn allow_non_globals_in_dht(&mut self, value: bool) -> &mut Self {
        self.allow_non_globals_in_dht = value;
        self
    }

    /// Should MDNS discovery be supported?
    pub fn with_mdns(&mut self, value: bool) -> &mut Self {
        self.enable_mdns = value;
        self
    }

    /// Add discovery via Kademlia for the given network.
    pub fn with_kademlia(&mut self) -> &mut Self {
        self.kademlia_protocol = Some(kademlia_protocol_name(&self.network));
        self
    }

    /// Require iterative Kademlia DHT queries to use disjoint paths.
    pub fn use_kademlia_disjoint_query_paths(&mut self, value: bool) -> &mut Self {
        self.kademlia_disjoint_query_paths = value;
        self
    }

    /// Sets Kademlia replication factor.
    pub fn with_kademlia_replication_factor(&mut self, value: NonZeroUsize) -> &mut Self {
        self.kademlia_replication_factor = value;
        self
    }

    /// Create a `DiscoveryBehaviour` from this config.
    pub fn finish(self) -> DiscoveryBehaviour {
        let Self {
            local_peer_id,
            permanent_addresses,
            dht_random_walk,
            allow_private_ip,
            allow_non_globals_in_dht,
            discovery_only_if_under_num,
            enable_mdns,
            kademlia_disjoint_query_paths,
            kademlia_protocol,
            kademlia_replication_factor,
            network,
        } = self;

        let kademlia = if let Some(ref kademlia_protocol) = kademlia_protocol {
            let mut config = KademliaConfig::new(kademlia_protocol.clone());

            config.set_replication_factor(kademlia_replication_factor);
            config.set_record_filtering(libp2p::kad::StoreInserts::FilterBoth);
            config.set_query_timeout(KAD_QUERY_TIMEOUT);
            config.set_kbucket_inserts(BucketInserts::Manual);
            config.disjoint_query_paths(kademlia_disjoint_query_paths);
            config.set_provider_record_ttl(Some(KADEMLIA_PROVIDER_RECORD_TTL));
            config.set_provider_publication_interval(Some(KADEMLIA_PROVIDER_REPUBLISH_INTERVAL));

            let store = MemoryStore::with_config(
                local_peer_id,
                MemoryStoreConfig {
                    max_provided_keys: KADEMLIA_MAX_PROVIDER_KEYS,
                    ..Default::default()
                },
            );

            let mut kad = Kademlia::with_config(local_peer_id, store, config);
            kad.set_mode(Some(kad::Mode::Server));

            for (peer_id, addr) in &permanent_addresses {
                kad.add_address(peer_id, addr.clone());
            }

            Some(kad)
        } else {
            None
        };

        DiscoveryBehaviour {
            permanent_addresses,
            ephemeral_addresses: HashMap::new(),
            kademlia: Toggle::from(kademlia),
            next_kad_random_query: if dht_random_walk {
                Some(Delay::new(Duration::new(0, 0)))
            } else {
                None
            },
            duration_to_next_kad: Duration::from_secs(1),
            pending_events: VecDeque::new(),
            local_peer_id,
            num_connections: 0,
            allow_private_ip,
            discovery_only_if_under_num,
            mdns: if enable_mdns {
                match TokioMdns::new(mdns::Config::default(), local_peer_id) {
                    Ok(mdns) => Toggle::from(Some(mdns)),
                    Err(err) => {
                        warn!(target: LOG_TARGET, "Failed to initialize mDNS: {:?}", err);
                        Toggle::from(None)
                    }
                }
            } else {
                Toggle::from(None)
            },
            allow_non_globals_in_dht,
            known_external_addresses: LinkedHashSet::new(),
            records_to_publish: Default::default(),
            kademlia_protocol,
            provider_keys_requested: HashMap::new(),
            network,
        }
    }
}

/// Events generated by the `DiscoveryBehaviour`.
#[derive(Debug)]
pub enum DiscoveryOut {
    /// We have discovered a new peer.
    Discovered(PeerId),

    /// We have discovered a peer but it has an unroutable address.
    UnroutablePeer(PeerId),

    /// A random Kademlia query has started.
    RandomKademliaStarted,

    /// We have successfully found the closest peers to a key.
    ClosestPeersFound {
        key: Vec<u8>,
        peers: Vec<PeerId>,
        duration: Duration,
    },

    /// We failed to find the closest peers to a key.
    ClosestPeersNotFound {
        key: Vec<u8>,
        duration: Duration,
    },

    /// We have successfully retrieved a value from the DHT.
    ValueFound {
        key: RecordKey,
        value: Vec<u8>,
        duration: Duration,
    },

    /// We failed to retrieve a value from the DHT.
    ValueNotFound {
        key: RecordKey,
        duration: Duration,
    },

    /// We have successfully stored a value in the DHT.
    ValuePut(RecordKey, Duration),

    /// We failed to store a value in the DHT.
    ValuePutFailed(RecordKey, Duration),

    /// We have successfully started providing a key.
    StartedProviding(RecordKey, Duration),

    /// We failed to start providing a key.
    StartProvidingFailed(RecordKey, Duration),

    /// We have found providers for a key.
    ProvidersFound {
        key: RecordKey,
        providers: HashSet<PeerId>,
        duration: Duration,
    },

    /// We failed to find providers for a key.
    ProvidersNotFound(RecordKey, Duration),
}

/// Implementation of `NetworkBehaviour` that discovers nodes on the Neptune network.
pub struct DiscoveryBehaviour {
    /// User-defined list of nodes and their addresses.
    permanent_addresses: Vec<(PeerId, Multiaddr)>,
    /// Same as `permanent_addresses`, except that addresses that fail are removed.
    ephemeral_addresses: HashMap<PeerId, Vec<Multiaddr>>,
    /// Kademlia requests and answers.
    kademlia: Toggle<Kademlia<MemoryStore>>,
    /// Discovers nodes on the local network.
    mdns: Toggle<TokioMdns>,
    /// Stream that fires when we need to perform the next random Kademlia query.
    next_kad_random_query: Option<Delay>,
    /// After `next_kad_random_query` triggers, the next one triggers after this duration.
    duration_to_next_kad: Duration,
    /// Events to return in priority when polled.
    pending_events: VecDeque<DiscoveryOut>,
    /// Identity of our local node.
    local_peer_id: PeerId,
    /// Number of nodes we're currently connected to.
    num_connections: u64,
    /// If false, won't return private IPv4/IPv6 addresses.
    allow_private_ip: bool,
    /// Number of active connections over which we interrupt the discovery process.
    discovery_only_if_under_num: u64,
    /// Should non-global addresses be added to the DHT?
    allow_non_globals_in_dht: bool,
    /// A cache of discovered external addresses.
    known_external_addresses: LinkedHashSet<Multiaddr>,
    /// Records to publish per QueryId.
    records_to_publish: HashMap<QueryId, Record>,
    /// The Neptune network-based kademlia protocol name.
    kademlia_protocol: Option<StreamProtocol>,
    /// Provider keys requested with `GET_PROVIDERS` queries.
    provider_keys_requested: HashMap<QueryId, RecordKey>,
    /// The Neptune network this discovery is operating on.
    network: Network,
}

impl DiscoveryBehaviour {
    /// Returns the list of nodes that we know exist in the network.
    pub fn known_peers(&mut self) -> HashSet<PeerId> {
        let mut peers = HashSet::new();
        if let Some(k) = self.kademlia.as_mut() {
            for b in k.kbuckets() {
                for e in b.iter() {
                    if !peers.contains(e.node.key.preimage()) {
                        peers.insert(*e.node.key.preimage());
                    }
                }
            }
        }
        peers
    }

    /// Adds a hard-coded address for the given peer, that never expires.
    pub fn add_known_address(&mut self, peer_id: PeerId, addr: Multiaddr) {
        let addrs_list = self.ephemeral_addresses.entry(peer_id).or_default();
        if addrs_list.contains(&addr) {
            return;
        }

        if let Some(kademlia) = self.kademlia.as_mut() {
            kademlia.add_address(&peer_id, addr.clone());
        }

        addrs_list.push(addr.clone());
        self.pending_events.push_back(DiscoveryOut::Discovered(peer_id));
    }

    /// Add an address for a peer discovered through the identify protocol.
    pub fn add_self_reported_address(
        &mut self,
        peer_id: &PeerId,
        supported_protocols: &[StreamProtocol],
        addr: Multiaddr,
    ) {
        if let Some(kademlia) = self.kademlia.as_mut() {
            if let Some(ref kademlia_protocol) = self.kademlia_protocol {
                if supported_protocols.iter().any(|p| p == kademlia_protocol) {
                    kademlia.add_address(peer_id, addr.clone());
                }
            }
        }

        let addrs_list = self.ephemeral_addresses.entry(*peer_id).or_default();
        if !addrs_list.contains(&addr) {
            addrs_list.push(addr);
        }
    }

    /// Start fetching a record from the DHT.
    pub fn get_value(&mut self, key: RecordKey) {
        if let Some(kademlia) = self.kademlia.as_mut() {
            kademlia.get_record(key);
        }
    }

    /// Start storing a record in the DHT.
    pub fn put_value(&mut self, key: RecordKey, value: Vec<u8>) {
        if let Some(kademlia) = self.kademlia.as_mut() {
            let record = Record {
                key,
                value,
                publisher: None,
                expires: None,
            };
            kademlia.put_record(record, kad::Quorum::One).ok();
        }
    }

    /// Bootstrap the Kademlia DHT.
    pub fn bootstrap(&mut self) {
        if let Some(kademlia) = self.kademlia.as_mut() {
            if let Err(e) = kademlia.bootstrap() {
                warn!(target: LOG_TARGET, "Failed to bootstrap Kademlia: {:?}", e);
            }
        }
    }

    /// Get addresses for a peer.
    pub fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        let mut addresses = Vec::new();

        // Add permanent addresses
        for (p, addr) in &self.permanent_addresses {
            if p == peer_id {
                addresses.push(addr.clone());
            }
        }

        // Add ephemeral addresses
        if let Some(addrs) = self.ephemeral_addresses.get(peer_id) {
            addresses.extend(addrs.iter().cloned());
        }

        // Add addresses from Kademlia
        if let Some(kademlia) = self.kademlia.as_mut() {
            // Note: In newer libp2p versions, addresses are managed differently
            // This would need to be adapted based on the specific libp2p version
            // For now, we'll skip this to focus on the core structure
            // addresses.extend(kademlia.addresses_of_peer(peer_id));
        }

        // Filter private IPs if not allowed
        if !self.allow_private_ip {
            addresses.retain(|addr| {
                addr.iter().all(|protocol| match protocol {
                    Protocol::Ip4(ip) => !ip.is_private(),
                    Protocol::Ip6(ip) => !ip.is_loopback() && !ip.is_unspecified(),
                    _ => true,
                })
            });
        }

        addresses
    }

    /// Handle connection established event.
    pub fn on_connection_established(&mut self, _peer_id: PeerId) {
        self.num_connections += 1;
        
        // Add peer to Kademlia if we have the protocol
        if let Some(_kademlia) = self.kademlia.as_mut() {
            // TODO: Check if peer supports our Kademlia protocol before adding
            // This would require protocol negotiation information
            // For now, we'll skip this implementation detail
        }
    }

    /// Handle connection closed event.
    pub fn on_connection_closed(&mut self, _peer_id: PeerId) {
        if self.num_connections > 0 {
            self.num_connections -= 1;
        }
    }

    /// Handle external address confirmed event.
    pub fn on_external_addr_confirmed(&mut self, addr: Multiaddr) {
        if self.known_external_addresses.insert(addr.clone()) {
            info!(target: LOG_TARGET, "Discovered external address: {}", addr);
        }
    }
}

impl NetworkBehaviour for DiscoveryBehaviour {
    type ConnectionHandler = ToggleConnectionHandler<
        <Toggle<Kademlia<MemoryStore>> as NetworkBehaviour>::ConnectionHandler,
    >;
    type ToSwarm = DiscoveryOut;

    fn handle_established_inbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        peer: PeerId,
        _local_addr: &Multiaddr,
        _remote_addr: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        todo!("Implement handle_established_inbound_connection")
    }

    fn handle_established_outbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        peer: PeerId,
        _addr: &Multiaddr,
        _role_override: Endpoint,
        _port_use: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        todo!("Implement handle_established_outbound_connection")
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        match event {
            FromSwarm::ConnectionEstablished(connection_established) => {
                self.on_connection_established(connection_established.peer_id);
            }
            FromSwarm::ConnectionClosed(connection_closed) => {
                self.on_connection_closed(connection_closed.peer_id);
            }
            FromSwarm::ExternalAddrConfirmed(ExternalAddrConfirmed { addr }) => {
                self.on_external_addr_confirmed(addr.clone());
            }
            _ => {
                // Forward other events to sub-behaviors
                self.kademlia.on_swarm_event(event.clone());
                self.mdns.on_swarm_event(event);
            }
        }
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        connection_id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        todo!("Implement on_connection_handler_event")
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        // Return any pending events first
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(ToSwarm::GenerateEvent(event));
        }

        // Handle random Kademlia queries
        if let Some(next_kad_random_query) = self.next_kad_random_query.as_mut() {
            while next_kad_random_query.poll_unpin(cx).is_ready() {
                if self.num_connections < self.discovery_only_if_under_num {
                    if let Some(kademlia) = self.kademlia.as_mut() {
                        let random_peer_id = PeerId::random();
                        debug!(target: LOG_TARGET, "Starting random Kademlia query for {:?}", random_peer_id);
                        
                        let query_id = kademlia.get_closest_peers(random_peer_id);
                        self.pending_events.push_back(DiscoveryOut::RandomKademliaStarted);
                    }
                }

                // Schedule next random query
                *next_kad_random_query = Delay::new(self.duration_to_next_kad);
                self.duration_to_next_kad = cmp::min(self.duration_to_next_kad * 2, Duration::from_secs(60));
            }
        }

        // Poll Kademlia
        while let Poll::Ready(event) = self.kademlia.poll(cx) {
            match event {
                ToSwarm::GenerateEvent(event) => match event {
                    KademliaEvent::OutboundQueryProgressed {
                        result: QueryResult::GetClosestPeers(result),
                        stats,
                        ..
                    } => {
                        let ev = match result {
                            Ok(GetClosestPeersOk { key, peers }) => {
                                trace!(target: LOG_TARGET, "Kademlia query completed for key: {:?}", key);
                                DiscoveryOut::ClosestPeersFound {
                                    key: key.to_vec(),
                                    peers: peers.into_iter().map(|p| p.peer_id).collect(),
                                    duration: stats.duration().unwrap_or_default(),
                                }
                            }
                            Err(GetClosestPeersError::Timeout { key, .. }) => {
                                debug!(target: LOG_TARGET, "Kademlia query timed out for key: {:?}", key);
                                DiscoveryOut::ClosestPeersNotFound {
                                    key: key.to_vec(),
                                    duration: stats.duration().unwrap_or_default(),
                                }
                            }
                        };
                        return Poll::Ready(ToSwarm::GenerateEvent(ev));
                    }
                    KademliaEvent::OutboundQueryProgressed {
                        result: QueryResult::GetRecord(result),
                        stats,
                        ..
                    } => {
                        let ev = match result {
                            Ok(GetRecordOk::FoundRecord(PeerRecord { record, .. })) => {
                                trace!(target: LOG_TARGET, "Retrieved record for key: {:?}", record.key);
                                DiscoveryOut::ValueFound {
                                    key: record.key,
                                    value: record.value,
                                    duration: stats.duration().unwrap_or_default(),
                                }
                            }
                            Ok(GetRecordOk::FinishedWithNoAdditionalRecord { .. }) |
                            Err(_) => {
                                debug!(target: LOG_TARGET, "Failed to retrieve record");
                                // TODO: Extract key from error
                                todo!("Extract key from GetRecord error")
                            }
                        };
                        return Poll::Ready(ToSwarm::GenerateEvent(ev));
                    }
                    // TODO: Handle other Kademlia events
                    _ => {
                        debug!(target: LOG_TARGET, "Unhandled Kademlia event: {:?}", event);
                    }
                },
                ToSwarm::Dial { opts } => return Poll::Ready(ToSwarm::Dial { opts }),
                event => {
                    return Poll::Ready(event.map_out(|_| {
                        unreachable!("`GenerateEvent` is handled in a branch above; qed")
                    }));
                }
            }
        }

        // Poll mDNS
        while let Poll::Ready(ev) = self.mdns.poll(cx) {
            match ev {
                ToSwarm::GenerateEvent(event) => match event {
                    mdns::Event::Discovered(list) => {
                        if self.num_connections >= self.discovery_only_if_under_num {
                            continue;
                        }

                        self.pending_events.extend(
                            list.into_iter().map(|(peer_id, _)| DiscoveryOut::Discovered(peer_id)),
                        );
                        if let Some(ev) = self.pending_events.pop_front() {
                            return Poll::Ready(ToSwarm::GenerateEvent(ev));
                        }
                    }
                    mdns::Event::Expired(_) => {}
                },
                ToSwarm::Dial { .. } => {
                    unreachable!("mDNS never dials!");
                }
                ToSwarm::NotifyHandler { event, .. } => match event {},
                event => {
                    return Poll::Ready(
                        event
                            .map_in(|_| {
                                unreachable!("`NotifyHandler` is handled in a branch above; qed")
                            })
                            .map_out(|_| {
                                unreachable!("`GenerateEvent` is handled in a branch above; qed")
                            }),
                    );
                }
            }
        }

        Poll::Pending
    }
}

/// Generate Kademlia protocol name based on Neptune network.
fn kademlia_protocol_name(network: &Network) -> StreamProtocol {
    let name = format!("/neptune/{}/kad", network.id());
    StreamProtocol::try_from_owned(name).expect("protocol name is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    #[test]
    fn test_kademlia_protocol_name() {
        let main_protocol = kademlia_protocol_name(&Network::Main);
        assert_eq!(main_protocol.as_ref(), "/neptune/0/kad");

        let testnet_protocol = kademlia_protocol_name(&Network::Testnet(42));
        assert_eq!(testnet_protocol.as_ref(), "/neptune/45/kad");
    }

    #[test]
    fn test_discovery_config_creation() {
        let keypair = Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id();
        let config = DiscoveryConfig::new(peer_id, Network::Main);
        
        assert_eq!(config.local_peer_id, peer_id);
        assert_eq!(config.network, Network::Main);
        assert!(config.enable_mdns); // Main network should enable mDNS
    }

    #[test]
    fn test_regtest_no_mdns() {
        let keypair = Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id();
        let config = DiscoveryConfig::new(peer_id, Network::RegTest);
        
        assert!(!config.enable_mdns); // RegTest should not enable mDNS
    }
}