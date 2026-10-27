//! Links between the voice gateways of two homes ("two households").
//!
//! Each gateway binds one endpoint with a fresh key and its own ALPN. For every
//! peer the server granted a link to, it keeps one loopback UDP socket: what
//! the local mesh worker sends to that socket goes to the peer as a QUIC
//! datagram, and the peer's datagrams come out of that socket to the worker.
//! iroh connects directly where the NATs allow it and through a relay when
//! nothing else works, so neither home needs a port reachable from outside.
//!
//! Only allow-listed peers may connect, one connection each. The datagrams stay
//! sealed with the mesh's own link key, which this helper never sees; it only
//! carries them.
use crate::Network;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use iroh::{endpoint::Connection, Endpoint, EndpointAddr, EndpointId, RelayUrl};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::UdpSocket, task::JoinHandle, time::timeout};

pub const ALPN: &[u8] = b"relay-voice-mesh/1";
/// Peers one gateway links to at once (the broker's gateways per channel, with room).
pub const MAX_PEERS: usize = 8;
const MAX_DATAGRAM: usize = 1500;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(2);

struct Peer {
    socket: Arc<UdpSocket>,
    connection: Arc<tokio::sync::Mutex<Option<Connection>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

pub struct MeshLinks {
    endpoint: Endpoint,
    /// The local mesh worker's loopback port.
    worker: Arc<Mutex<Option<SocketAddr>>>,
    peers: Arc<Mutex<HashMap<EndpointId, Arc<Peer>>>>,
    accept: JoinHandle<()>,
}

impl MeshLinks {
    /// A fresh endpoint for this gateway run.
    pub async fn start(network: &Network) -> Result<Self> {
        let endpoint = crate::bind_alpns(network, None, vec![ALPN.to_vec()]).await?;
        let peers: Arc<Mutex<HashMap<EndpointId, Arc<Peer>>>> = Arc::default();
        let worker: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
        let accept = tokio::spawn(accept_loop(endpoint.clone(), peers.clone(), worker.clone()));
        Ok(Self {
            endpoint,
            worker,
            peers,
            accept,
        })
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub fn relay_url(&self) -> Option<String> {
        self.endpoint
            .addr()
            .relay_urls()
            .next()
            .map(|u| u.to_string())
    }

    /// Where datagrams from peers are delivered: the worker's loopback port.
    pub fn set_worker(&self, port: u16) -> Result<()> {
        if port == 0 {
            bail!("the mesh worker port is required");
        }
        *self.worker.lock().unwrap() = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        Ok(())
    }

    /// Allows one peer and returns the loopback port the worker uses for it.
    /// The side with the smaller endpoint id dials; the other only accepts, so
    /// two gateways never race two connections.
    pub async fn peer(
        &self,
        id: EndpointId,
        relay: Option<RelayUrl>,
        direct: Vec<SocketAddr>,
    ) -> Result<u16> {
        if id == self.endpoint.id() {
            bail!("a gateway does not link to itself");
        }
        if let Some(peer) = self.peers.lock().unwrap().get(&id) {
            return Ok(peer.socket.local_addr()?.port());
        }
        if self.peers.lock().unwrap().len() >= MAX_PEERS {
            bail!("too many mesh links");
        }
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let port = socket.local_addr()?.port();
        let peer = Arc::new(Peer {
            socket,
            connection: Arc::default(),
            tasks: Mutex::new(Vec::new()),
        });
        let out = tokio::spawn(outgoing(peer.clone(), self.worker.clone()));
        peer.tasks.lock().unwrap().push(out);
        if self.endpoint.id() < id {
            let mut addr = EndpointAddr::new(id);
            if let Some(relay) = relay {
                addr = addr.with_relay_url(relay);
            }
            for direct in direct {
                addr = addr.with_ip_addr(direct);
            }
            let dial = tokio::spawn(dial_loop(
                self.endpoint.clone(),
                addr,
                peer.clone(),
                self.worker.clone(),
            ));
            peer.tasks.lock().unwrap().push(dial);
        }
        let mut peers = self.peers.lock().unwrap();
        if let Some(existing) = peers.get(&id) {
            return Ok(existing.socket.local_addr()?.port());
        }
        peers.insert(id, peer);
        Ok(port)
    }

    /// Ends one link at once; its loopback socket is released.
    pub async fn remove(&self, id: &EndpointId) {
        let peer = self.peers.lock().unwrap().remove(id);
        if let Some(peer) = peer {
            if let Some(connection) = peer.connection.lock().await.take() {
                connection.close(0u32.into(), b"link revoked");
            }
        }
    }

    /// Keeps exactly these peers and ends every other link.
    pub async fn retain(&self, keep: &[EndpointId]) {
        let gone: Vec<EndpointId> = self
            .peers
            .lock()
            .unwrap()
            .keys()
            .filter(|id| !keep.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.remove(&id).await;
        }
    }

    /// Whether a live connection to this peer exists now.
    pub async fn connected(&self, id: &EndpointId) -> bool {
        self.path(id).await.is_some()
    }

    /// The live connection's path (direct or relay) and round trip, if any.
    pub async fn path(&self, id: &EndpointId) -> Option<crate::path::PathInfo> {
        let peer = self.peers.lock().unwrap().get(id).cloned()?;
        let slot = peer.connection.lock().await;
        slot.as_ref()
            .filter(|c| c.close_reason().is_none())
            .map(crate::path::of)
    }

    pub async fn close(self) {
        self.accept.abort();
        let peers: Vec<Arc<Peer>> = self.peers.lock().unwrap().drain().map(|(_, p)| p).collect();
        for peer in peers {
            if let Some(connection) = peer.connection.lock().await.take() {
                connection.close(0u32.into(), b"gateway stopped");
            }
        }
        self.endpoint.close().await;
    }
}

async fn accept_loop(
    endpoint: Endpoint,
    peers: Arc<Mutex<HashMap<EndpointId, Arc<Peer>>>>,
    worker: Arc<Mutex<Option<SocketAddr>>>,
) {
    let me = endpoint.id();
    while let Some(incoming) = endpoint.accept().await {
        let peers = peers.clone();
        let worker = worker.clone();
        tokio::spawn(async move {
            let Ok(accepting) = incoming.accept() else {
                return;
            };
            let Ok(connection) = timeout(CONNECT_TIMEOUT, accepting).await else {
                return;
            };
            let Ok(connection) = connection else { return };
            // Only a peer this gateway was told to link with, and only one
            // that should be dialing us (its id is the smaller one).
            let id = connection.remote_id();
            let peer = peers.lock().unwrap().get(&id).cloned();
            let Some(peer) = peer.filter(|_| id < me) else {
                connection.close(1u32.into(), b"not a granted mesh link");
                return;
            };
            let previous = peer.connection.lock().await.replace(connection.clone());
            if let Some(previous) = previous {
                previous.close(0u32.into(), b"replaced");
            }
            incoming_datagrams(connection, peer, worker).await;
        });
    }
}

async fn dial_loop(
    endpoint: Endpoint,
    addr: EndpointAddr,
    peer: Arc<Peer>,
    worker: Arc<Mutex<Option<SocketAddr>>>,
) {
    loop {
        let live = peer
            .connection
            .lock()
            .await
            .as_ref()
            .is_some_and(|c| c.close_reason().is_none());
        if !live {
            if let Ok(Ok(connection)) =
                timeout(CONNECT_TIMEOUT, endpoint.connect(addr.clone(), ALPN)).await
            {
                *peer.connection.lock().await = Some(connection.clone());
                incoming_datagrams(connection, peer.clone(), worker.clone()).await;
                continue;
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// The peer's datagrams, delivered to the worker from this link's own socket.
async fn incoming_datagrams(
    connection: Connection,
    peer: Arc<Peer>,
    worker: Arc<Mutex<Option<SocketAddr>>>,
) {
    while let Ok(datagram) = connection.read_datagram().await {
        let to = *worker.lock().unwrap();
        if let (Some(to), true) = (to, datagram.len() <= MAX_DATAGRAM) {
            let _ = peer.socket.send_to(&datagram, to).await;
        }
    }
}

/// The worker's datagrams for this peer. Anything not from the worker's own
/// loopback port is ignored; a packet while the link is down is lost, as UDP is.
async fn outgoing(peer: Arc<Peer>, worker: Arc<Mutex<Option<SocketAddr>>>) {
    let mut drops = crate::DatagramDrops::default();
    let mut buffer = vec![0u8; MAX_DATAGRAM + 1];
    while let Ok((length, from)) = peer.socket.recv_from(&mut buffer).await {
        if Some(from) != *worker.lock().unwrap() || length > MAX_DATAGRAM {
            continue;
        }
        let connection = peer.connection.lock().await.clone();
        if let Some(connection) = connection.filter(|c| c.close_reason().is_none()) {
            drops.record(
                connection
                    .send_datagram(Bytes::copy_from_slice(&buffer[..length]))
                    .is_err(),
            );
        }
    }
}

/// Parses what the control API receives.
pub fn parse_peer(
    endpoint_id: &str,
    relay: Option<&str>,
) -> Result<(EndpointId, Option<RelayUrl>)> {
    let id: EndpointId = endpoint_id
        .parse()
        .context("the peer endpoint is invalid")?;
    let relay = relay
        .map(|r| {
            if !r.starts_with("https://") || r.len() > 200 {
                bail!("the peer relay is invalid");
            }
            r.parse::<RelayUrl>().context("the peer relay is invalid")
        })
        .transpose()?;
    Ok((id, relay))
}
