//! Integration layer between Neptune's discovery system and existing peer management.
//!
//! This module provides the bridge between the libp2p-based discovery system
//! and Neptune's existing channel-based peer management architecture.

use std::collections::HashMap;
use std::net::SocketAddr;
use futures::StreamExt;

use libp2p::{Multiaddr, PeerId, Swarm};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::{
    config_models::network::Network,
    discovery::{DiscoveryBehaviour, DiscoveryConfig, DiscoveryOut},
};

/// Events sent from the discovery system to the main loop.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A new peer has been discovered.
    PeerDiscovered {
        peer_id: PeerId,
        addresses: Vec<Multiaddr>,
    },
    /// A peer has become unreachable.
    PeerUnreachable { peer_id: PeerId },
    /// DHT value retrieved.
    ValueRetrieved { key: String, value: Vec<u8> },
    /// DHT value storage completed.
    ValueStored { key: String },
}

/// Commands sent from the main loop to the discovery system.
#[derive(Debug)]
pub enum DiscoveryCommand {
    /// Add a known peer address.
    AddKnownPeer { peer_id: PeerId, addr: Multiaddr },
    /// Start bootstrapping the DHT.
    Bootstrap,
    /// Store a value in the DHT.
    StoreValue { key: String, value: Vec<u8> },
    /// Retrieve a value from the DHT.
    GetValue { key: String },
    /// Update connection count.
    UpdateConnectionCount { count: u64 },
}

/// Discovery manager that integrates with Neptune's existing architecture.
pub struct DiscoveryManager {
    /// The libp2p swarm with discovery behavior.
    swarm: Swarm<DiscoveryBehaviour>,
    /// Channel for receiving commands from the main loop.
    command_rx: mpsc::UnboundedReceiver<DiscoveryCommand>,
    /// Channel for sending events to the main loop.
    event_tx: broadcast::Sender<DiscoveryEvent>,
    /// Known peers and their connection status.
    known_peers: HashMap<PeerId, PeerConnectionStatus>,
    /// The Neptune network we're operating on.
    network: Network,
}

#[derive(Debug, Clone)]
struct PeerConnectionStatus {
    addresses: Vec<Multiaddr>,
    last_seen: std::time::Instant,
    connection_attempts: u32,
}

impl DiscoveryManager {
    /// Create a new discovery manager.
    pub fn new(
        network: Network,
        bootstrap_peers: Vec<(PeerId, Multiaddr)>,
        event_tx: broadcast::Sender<DiscoveryEvent>,
        command_rx: mpsc::UnboundedReceiver<DiscoveryCommand>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Generate a keypair for this node
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let local_peer_id = keypair.public().to_peer_id();

        // Configure discovery behavior
        let mut config = DiscoveryConfig::new(local_peer_id, network);
        config
            .with_permanent_addresses(bootstrap_peers.clone())
            .with_kademlia()
            .with_mdns(network.performs_peer_discovery())
            .discovery_limit(50); // Reasonable limit for Neptune

        let behavior = config.finish();

        // Create the swarm
        let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_behaviour(|_| behavior)?
            .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(60)))
            .build();

        let known_peers = bootstrap_peers
            .into_iter()
            .map(|(peer_id, addr)| {
                (
                    peer_id,
                    PeerConnectionStatus {
                        addresses: vec![addr],
                        last_seen: std::time::Instant::now(),
                        connection_attempts: 0,
                    },
                )
            })
            .collect();

        Ok(Self {
            swarm,
            command_rx,
            event_tx,
            known_peers,
            network,
        })
    }

    /// Run the discovery manager event loop.
    pub async fn run(&mut self) {
        info!("Starting Neptune discovery manager for network: {}", self.network);

        // Start listening on a random port
        let listen_addr = "/ip4/0.0.0.0/tcp/0".parse().unwrap();
        if let Err(e) = self.swarm.listen_on(listen_addr) {
            error!("Failed to start listening: {:?}", e);
            return;
        }

        // Bootstrap the DHT if we have Kademlia enabled
        self.swarm.behaviour_mut().bootstrap();

        loop {
            tokio::select! {
                // Handle swarm events
                event = self.swarm.select_next_some() => {
                    self.handle_swarm_event(event).await;
                }

                // Handle commands from main loop
                command = self.command_rx.recv() => {
                    match command {
                        Some(cmd) => self.handle_command(cmd).await,
                        None => {
                            warn!("Discovery command channel closed");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Handle events from the libp2p swarm.
    async fn handle_swarm_event(&mut self, event: libp2p::swarm::SwarmEvent<DiscoveryOut>) {
        match event {
            libp2p::swarm::SwarmEvent::Behaviour(discovery_event) => {
                self.handle_discovery_event(discovery_event).await;
            }
            libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                info!("Listening on: {}", address);
            }
            libp2p::swarm::SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                debug!("Connection established with: {}", peer_id);
                self.update_peer_status(peer_id, true);
            }
            libp2p::swarm::SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                debug!("Connection closed with: {} (cause: {:?})", peer_id, cause);
                self.update_peer_status(peer_id, false);
            }
            libp2p::swarm::SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(peer_id) = peer_id {
                    warn!("Failed to connect to {}: {:?}", peer_id, error);
                    self.handle_connection_failure(peer_id);
                }
            }
            _ => {
                // Handle other swarm events as needed
                debug!("Unhandled swarm event: {:?}", event);
            }
        }
    }

    /// Handle discovery-specific events.
    async fn handle_discovery_event(&mut self, event: DiscoveryOut) {
        match event {
            DiscoveryOut::Discovered(peer_id) => {
                info!("Discovered peer: {}", peer_id);
                let addresses = self.swarm.behaviour_mut().addresses_of_peer(&peer_id);
                
                // Update our known peers
                self.known_peers.insert(
                    peer_id,
                    PeerConnectionStatus {
                        addresses: addresses.clone(),
                        last_seen: std::time::Instant::now(),
                        connection_attempts: 0,
                    },
                );

                // Notify the main loop
                let _ = self.event_tx.send(DiscoveryEvent::PeerDiscovered {
                    peer_id,
                    addresses,
                });
            }
            DiscoveryOut::UnroutablePeer(peer_id) => {
                warn!("Discovered unroutable peer: {}", peer_id);
                let _ = self.event_tx.send(DiscoveryEvent::PeerUnreachable { peer_id });
            }
            DiscoveryOut::ValueFound { key, value, .. } => {
                debug!("Retrieved DHT value for key: {:?}", key);
                let _ = self.event_tx.send(DiscoveryEvent::ValueRetrieved {
                    key: String::from_utf8_lossy(key.as_ref()).to_string(),
                    value,
                });
            }
            DiscoveryOut::ValuePut(key, _) => {
                debug!("Stored DHT value for key: {:?}", key);
                let _ = self.event_tx.send(DiscoveryEvent::ValueStored {
                    key: String::from_utf8_lossy(key.as_ref()).to_string(),
                });
            }
            DiscoveryOut::RandomKademliaStarted => {
                debug!("Started random Kademlia query");
            }
            _ => {
                debug!("Unhandled discovery event: {:?}", event);
            }
        }
    }

    /// Handle commands from the main loop.
    async fn handle_command(&mut self, command: DiscoveryCommand) {
        match command {
            DiscoveryCommand::AddKnownPeer { peer_id, addr } => {
                debug!("Adding known peer: {} at {}", peer_id, addr);
                self.swarm.behaviour_mut().add_known_address(peer_id, addr.clone());
                
                // Update our tracking
                self.known_peers
                    .entry(peer_id)
                    .or_insert_with(|| PeerConnectionStatus {
                        addresses: Vec::new(),
                        last_seen: std::time::Instant::now(),
                        connection_attempts: 0,
                    })
                    .addresses
                    .push(addr);
            }
            DiscoveryCommand::Bootstrap => {
                debug!("Bootstrapping DHT");
                self.swarm.behaviour_mut().bootstrap();
            }
            DiscoveryCommand::StoreValue { key, value } => {
                debug!("Storing DHT value for key: {}", key);
                let record_key = libp2p::kad::RecordKey::new(&key);
                self.swarm.behaviour_mut().put_value(record_key, value);
            }
            DiscoveryCommand::GetValue { key } => {
                debug!("Retrieving DHT value for key: {}", key);
                let record_key = libp2p::kad::RecordKey::new(&key);
                self.swarm.behaviour_mut().get_value(record_key);
            }
            DiscoveryCommand::UpdateConnectionCount { count } => {
                // This would be used to update the discovery behavior's connection count
                // for discovery limiting purposes
                debug!("Updated connection count: {}", count);
                // TODO: Add method to update connection count in DiscoveryBehaviour
            }
        }
    }

    /// Update peer connection status.
    fn update_peer_status(&mut self, peer_id: PeerId, connected: bool) {
        if let Some(status) = self.known_peers.get_mut(&peer_id) {
            status.last_seen = std::time::Instant::now();
            if connected {
                status.connection_attempts = 0;
            }
        }
    }

    /// Handle connection failures.
    fn handle_connection_failure(&mut self, peer_id: PeerId) {
        if let Some(status) = self.known_peers.get_mut(&peer_id) {
            status.connection_attempts += 1;
            
            // If we've failed too many times, consider the peer unreachable
            if status.connection_attempts > 3 {
                let _ = self.event_tx.send(DiscoveryEvent::PeerUnreachable { peer_id });
            }
        }
    }
}

/// Helper function to convert Neptune's SocketAddr to libp2p Multiaddr.
pub fn socket_addr_to_multiaddr(socket_addr: SocketAddr) -> Multiaddr {
    match socket_addr {
        SocketAddr::V4(addr) => {
            format!("/ip4/{}/tcp/{}", addr.ip(), addr.port())
                .parse()
                .expect("valid multiaddr")
        }
        SocketAddr::V6(addr) => {
            format!("/ip6/{}/tcp/{}", addr.ip(), addr.port())
                .parse()
                .expect("valid multiaddr")
        }
    }
}

/// Helper function to convert libp2p Multiaddr to Neptune's SocketAddr.
pub fn multiaddr_to_socket_addr(multiaddr: &Multiaddr) -> Option<SocketAddr> {
    let mut ip = None;
    let mut port = None;

    for protocol in multiaddr.iter() {
        match protocol {
            libp2p::multiaddr::Protocol::Ip4(addr) => ip = Some(addr.into()),
            libp2p::multiaddr::Protocol::Ip6(addr) => ip = Some(addr.into()),
            libp2p::multiaddr::Protocol::Tcp(p) => port = Some(p),
            _ => {}
        }
    }

    match (ip, port) {
        (Some(ip), Some(port)) => Some(SocketAddr::new(ip, port)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_socket_addr_conversion() {
        let ipv4_addr = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8080);
        let multiaddr = socket_addr_to_multiaddr(ipv4_addr);
        assert_eq!(multiaddr.to_string(), "/ip4/127.0.0.1/tcp/8080");
        
        let converted_back = multiaddr_to_socket_addr(&multiaddr).unwrap();
        assert_eq!(converted_back, ipv4_addr);
    }

    #[test]
    fn test_ipv6_conversion() {
        let ipv6_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 9090);
        let multiaddr = socket_addr_to_multiaddr(ipv6_addr);
        assert_eq!(multiaddr.to_string(), "/ip6/::1/tcp/9090");
        
        let converted_back = multiaddr_to_socket_addr(&multiaddr).unwrap();
        assert_eq!(converted_back, ipv6_addr);
    }
}