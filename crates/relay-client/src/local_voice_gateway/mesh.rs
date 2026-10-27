//! Fanout between linked local voice gateways: the "two households" model.
//!
//! A gateway that the server approved for exactly one community, channel and
//! media session receives each person's stream once, copies it to the people in
//! its own home, and sends ONE copy to each tree neighbour in the route plan
//! (`relay_protocol::voice_mesh`) that leads to a listener. The frame itself is
//! the sender's original end-to-end protected voice frame: a gateway reads only
//! the mesh header and never decodes, decrypts or re-encodes the audio.
//!
//! Every hop is a server-granted lease whose key is bound to the full binding.
//! Each packet is checked for its sender's identity, the reverse path to its
//! origin, visited forwarders, hop limit, age and a per-stream replay window
//! before any copy is made. Revoking a person or a link takes effect at once.
use super::{
    expiry, header, local, Client, Grant, Lease, Replay, MAX_INNER, MAX_ISSUED, MAX_PACKET,
    MAX_ROUTES, PONG, PROBE,
};
use relay_protocol::voice_mesh::{
    Binding, MeshHeader, NodeId, RoutePlan, HEADER_LEN, MAX_AGE_MS, MAX_FORWARDERS,
    MAX_LATENESS_MS, MAX_MEMBERS,
};
use std::{
    collections::{HashMap, HashSet},
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

/// Envelope kind for a mesh packet. Distinct from the single-hop kinds, so the
/// plain upstream router never forwards one.
pub(crate) const MESH: u8 = 3;
/// Largest original protected frame a mesh packet can carry.
pub(crate) const MAX_FRAME: usize = MAX_INNER - HEADER_LEN;
const UP: u32 = 0;
const DOWN: u32 = 1;

/// Each end of a link seals with its own direction, so the two ends of one
/// link key never share a nonce.
fn link_directions(me: NodeId, peer: NodeId) -> (u32, u32) {
    if me < peer {
        (2, 3)
    } else {
        (3, 2)
    }
}

/// Mixes the full binding into a server grant: a key issued for another
/// community, channel or call can never open this mesh.
pub(crate) fn bind(grant: Grant, binding: &Binding) -> Option<Grant> {
    use hmac::Mac;
    if !binding.valid() || grant.session != binding.media_session {
        return None;
    }
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(grant.key.as_slice()).ok()?;
    mac.update(b"relay-voice-mesh-v1");
    for part in [
        binding.community_id.as_bytes(),
        binding.channel_id.as_bytes(),
    ] {
        mac.update(&(part.len() as u32).to_le_bytes());
        mac.update(part);
    }
    mac.update(&binding.media_session.to_le_bytes());
    Some(Grant {
        id: grant.id,
        session: grant.session,
        key: Zeroizing::new(mac.finalize().into_bytes().into()),
        valid_for: grant.valid_for,
    })
}

fn same_key(a: &Grant, b: &Grant) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(a.key.as_slice().ct_eq(b.key.as_slice()))
}

/// The server's approval of this gateway for one binding, as one plan node.
pub(crate) struct Approval {
    pub node: NodeId,
    pub binding: Binding,
    pub valid_until: Instant,
}

/// How a person reaches this gateway. Home members must come from the LAN; a
/// lone person the plan attached here over the internet is admitted as such.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    Lan,
    Internet,
}

struct Member {
    stream: u32,
    reach: Reach,
    lease: Lease,
}
struct Link {
    lease: Lease,
    address: SocketAddr,
    out: u32,
    inn: u32,
}
#[derive(Default)]
struct Stream {
    seen: Replay,
    baseline: Option<u32>,
}

/// Why packets were not forwarded. Counted, never logged with contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub forwarded: u64,
    pub delivered: u64,
    pub duplicate: u64,
    pub looped: u64,
    pub expired: u64,
    pub revoked: u64,
    pub muted: u64,
    pub refused: u64,
}

enum Reject {
    Refused,
    Looped,
    Revoked,
    Muted,
    Expired,
    Duplicate,
}

enum Inbound {
    Pong(SocketAddr, Vec<u8>),
    Frame(Option<NodeId>, MeshHeader, Vec<u8>),
}

pub(crate) struct MeshGateway {
    node: NodeId,
    binding: Binding,
    tag: u64,
    valid_until: Instant,
    plan: RoutePlan,
    members: HashMap<[u8; 16], Member>,
    links: HashMap<NodeId, Link>,
    streams: HashMap<u32, Stream>,
    revoked: HashSet<u32>,
    used: HashSet<[u8; 16]>,
    clock: Instant,
    pub stats: Stats,
}

impl MeshGateway {
    pub fn new(approval: Approval, plan: RoutePlan, now: Instant) -> Option<Self> {
        if now >= approval.valid_until
            || plan.validate().is_err()
            || plan.binding != approval.binding
            || !plan.nodes.contains(&approval.node)
        {
            return None;
        }
        Some(Self {
            node: approval.node,
            tag: approval.binding.tag(),
            binding: approval.binding,
            valid_until: approval.valid_until,
            plan,
            members: HashMap::new(),
            links: HashMap::new(),
            streams: HashMap::new(),
            revoked: HashSet::new(),
            used: HashSet::new(),
            clock: now,
            stats: Stats::default(),
        })
    }

    pub fn node(&self) -> NodeId {
        self.node
    }

    /// A newer plan for the same binding. People who left it lose their lease;
    /// links that are no longer tree edges are closed.
    pub fn apply_plan(&mut self, plan: RoutePlan) -> bool {
        if plan.validate().is_err()
            || plan.binding != self.binding
            || !plan.nodes.contains(&self.node)
            || (plan.epoch.wrapping_sub(self.plan.epoch) as i32) <= 0
        {
            return false;
        }
        let neighbours = plan.neighbors(self.node);
        self.links.retain(|node, _| neighbours.contains(node));
        self.members
            .retain(|_, member| plan.attachment(member.stream).is_some());
        self.plan = plan;
        true
    }

    /// Withdrawn approval: nothing is accepted or sent from now on.
    pub fn withdraw(&mut self, now: Instant) {
        self.valid_until = now;
        self.members.clear();
        self.links.clear();
    }

    /// Admits one person's lease. The stream must be in the plan (it may be
    /// attached elsewhere during a controlled switch; then nothing is forwarded
    /// from it here until the plan moves it). A renewal keeps nonce and replay state.
    pub fn admit(
        &mut self,
        stream: u32,
        reach: Reach,
        grant: Grant,
        requested: Instant,
        now: Instant,
    ) -> bool {
        if now >= self.valid_until
            || self.revoked.contains(&stream)
            || self.plan.attachment(stream).is_none()
        {
            return false;
        }
        let Some(grant) = bind(grant, &self.binding) else {
            return false;
        };
        let Some(end) = expiry(&grant, requested, now) else {
            return false;
        };
        if let Some(existing) = self.members.get_mut(&grant.id) {
            if existing.stream != stream
                || existing.reach != reach
                || now >= existing.lease.expires
                || !same_key(&existing.lease.grant, &grant)
            {
                return false;
            }
            existing.lease.expires = end;
            return true;
        }
        if self.used.len() >= MAX_ISSUED
            || self.used.contains(&grant.id)
            || self.members.len() >= MAX_ROUTES
        {
            return false;
        }
        self.used.insert(grant.id);
        // One live lease per person: a new admission replaces the old one.
        self.members.retain(|_, member| member.stream != stream);
        let Some(lease) = Lease::new(grant, requested, now) else {
            return false;
        };
        self.members.insert(
            lease.grant.id,
            Member {
                stream,
                reach,
                lease,
            },
        );
        true
    }

    /// Opens the server-granted link to one tree neighbour at its fixed address.
    pub fn link(
        &mut self,
        peer: NodeId,
        address: SocketAddr,
        grant: Grant,
        requested: Instant,
        now: Instant,
    ) -> bool {
        if now >= self.valid_until
            || peer == self.node
            || !self.plan.neighbors(self.node).contains(&peer)
            || address.port() == 0
            || address.ip().is_unspecified()
            || address.ip().is_multicast()
        {
            return false;
        }
        let Some(grant) = bind(grant, &self.binding) else {
            return false;
        };
        let Some(end) = expiry(&grant, requested, now) else {
            return false;
        };
        if let Some(existing) = self.links.get_mut(&peer) {
            if existing.lease.grant.id == grant.id {
                if existing.address != address
                    || now >= existing.lease.expires
                    || !same_key(&existing.lease.grant, &grant)
                {
                    return false;
                }
                existing.lease.expires = end;
                return true;
            }
        }
        if self.used.len() >= MAX_ISSUED || self.used.contains(&grant.id) {
            return false;
        }
        self.used.insert(grant.id);
        let Some(lease) = Lease::new(grant, requested, now) else {
            return false;
        };
        let (out, inn) = link_directions(self.node, peer);
        self.links.insert(
            peer,
            Link {
                lease,
                address,
                out,
                inn,
            },
        );
        true
    }

    /// Access revocation for one person: their lease closes now and their
    /// stream is refused here even if a stale plan or a peer still carries it.
    pub fn revoke(&mut self, stream: u32, now: Instant) {
        self.members.retain(|_, member| member.stream != stream);
        if self.revoked.len() >= MAX_ISSUED {
            self.withdraw(now); // fail closed rather than forget a revocation
            return;
        }
        self.revoked.insert(stream);
    }

    pub fn unlink(&mut self, peer: NodeId) {
        self.links.remove(&peer);
    }

    fn local_ms(&self, now: Instant) -> u32 {
        now.saturating_duration_since(self.clock).as_millis() as u32
    }

    fn inbound(
        &mut self,
        from: SocketAddr,
        packet: &[u8],
        now: Instant,
    ) -> Result<Inbound, Reject> {
        if now >= self.valid_until {
            return Err(Reject::Refused);
        }
        let (id, session, seq) = header(packet).ok_or(Reject::Refused)?;
        if session != self.binding.media_session {
            return Err(Reject::Refused);
        }
        let (from_node, clear, member_stream) = if let Some(member) = self.members.get_mut(&id) {
            if from.port() == 0 || (member.reach == Reach::Lan && !local(from.ip())) {
                return Err(Reject::Refused);
            }
            // Reordered traffic from an old NAT binding cannot move the destination back.
            if member.lease.client != Some(from)
                && member
                    .lease
                    .received
                    .highest
                    .is_some_and(|high| seq <= high)
            {
                return Err(Reject::Refused);
            }
            let clear = member.lease.open(UP, packet, now).ok_or(Reject::Refused)?;
            member.lease.client = Some(from);
            match clear.first().copied() {
                Some(PROBE) if clear.len() == 9 => {
                    let pong = member
                        .lease
                        .seal(DOWN, PONG, &clear[1..], now)
                        .ok_or(Reject::Refused)?;
                    return Ok(Inbound::Pong(from, pong));
                }
                Some(MESH) => (None, clear, Some(member.stream)),
                _ => return Err(Reject::Refused),
            }
        } else {
            let peer = self
                .links
                .iter()
                .find(|(_, link)| link.lease.grant.id == id)
                .map(|(node, _)| *node)
                .ok_or(Reject::Refused)?;
            let link = self.links.get_mut(&peer).ok_or(Reject::Refused)?;
            if from != link.address {
                return Err(Reject::Refused);
            }
            let clear = link
                .lease
                .open(link.inn, packet, now)
                .ok_or(Reject::Refused)?;
            if clear.first() != Some(&MESH) {
                return Err(Reject::Refused);
            }
            (Some(peer), clear, None)
        };
        let (head, frame) = MeshHeader::decode(&clear[1..]).ok_or(Reject::Refused)?;
        if head.binding != self.tag {
            return Err(Reject::Refused);
        }
        match (from_node, member_stream) {
            // A person may only send their own stream, fresh from the source.
            (None, Some(stream)) => {
                if head.stream != stream
                    || !head.via.is_empty()
                    || head.hops_left != MAX_FORWARDERS
                    || head.age_ms != 0
                {
                    return Err(Reject::Refused);
                }
            }
            (Some(peer), _) => {
                if head.via.contains(&self.node) || head.hops_left == 0 {
                    return Err(Reject::Looped);
                }
                if head.via.last() != Some(&peer) {
                    return Err(Reject::Refused);
                }
            }
            (None, None) => return Err(Reject::Refused),
        }
        if self.revoked.contains(&head.stream) {
            return Err(Reject::Revoked);
        }
        let sender = self.plan.attachment(head.stream).ok_or(Reject::Revoked)?;
        // Reverse path: a stream enters only from the side of its origin.
        if self.plan.toward(self.node, sender.node) != from_node {
            return Err(Reject::Looped);
        }
        if sender.muted {
            return Err(Reject::Muted);
        }
        if head.age_ms > MAX_AGE_MS {
            return Err(Reject::Expired);
        }
        let local_ms = self.local_ms(now);
        if self.streams.len() >= MAX_MEMBERS * 2 && !self.streams.contains_key(&head.stream) {
            self.streams
                .retain(|stream, _| self.plan.attachment(*stream).is_some());
        }
        let state = self.streams.entry(head.stream).or_default();
        // Clock-free age: lateness against the best delay seen for this stream.
        let offset = local_ms.wrapping_sub(head.origin_ms);
        let late = match state.baseline {
            None => 0,
            Some(base) => offset.wrapping_sub(base) as i32,
        };
        if late > MAX_LATENESS_MS as i32 {
            return Err(Reject::Expired);
        }
        if !state.seen.accept(head.seq) {
            return Err(Reject::Duplicate);
        }
        if late < 0 || state.baseline.is_none() {
            state.baseline = Some(offset);
        }
        Ok(Inbound::Frame(from_node, head, frame.to_vec()))
    }

    /// One received datagram in, every copy to send out.
    pub fn receive(
        &mut self,
        from: SocketAddr,
        packet: &[u8],
        now: Instant,
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        let (from_node, mut head, frame) = match self.inbound(from, packet, now) {
            Ok(Inbound::Pong(to, pong)) => return vec![(to, pong)],
            Ok(Inbound::Frame(from_node, head, frame)) => (from_node, head, frame),
            Err(reason) => {
                let counter = match reason {
                    Reject::Refused => &mut self.stats.refused,
                    Reject::Looped => &mut self.stats.looped,
                    Reject::Revoked => &mut self.stats.revoked,
                    Reject::Muted => &mut self.stats.muted,
                    Reject::Expired => &mut self.stats.expired,
                    Reject::Duplicate => &mut self.stats.duplicate,
                };
                *counter += 1;
                return Vec::new();
            }
        };
        head.hops_left -= 1;
        head.via.push(self.node);
        let mut out = Vec::new();
        if head.hops_left > 0 {
            for peer in self.plan.forward_to(self.node, from_node, head.stream) {
                let age = self.plan.edge(self.node, peer).map_or(0, |e| e.one_way_ms);
                let Some(link) = self.links.get_mut(&peer) else {
                    continue;
                };
                let mut next = head.clone();
                next.age_ms = next.age_ms.saturating_add(age);
                let Some(body) = next.encode(&frame) else {
                    continue;
                };
                if let Some(packet) = link.lease.seal(link.out, MESH, &body, now) {
                    out.push((link.address, packet));
                    self.stats.forwarded += 1;
                }
            }
        }
        let receivers = self.plan.local_receivers(self.node, head.stream);
        if receivers.is_empty() {
            return out;
        }
        let Some(body) = head.encode(&frame) else {
            return out;
        };
        for member in self.members.values_mut() {
            if !receivers.contains(&member.stream) {
                continue;
            }
            // Only the person's live attachment receives: no double delivery
            // to someone whose lease here is a pre-admission for a switch.
            if self.plan.attachment(member.stream).map(|a| a.node) != Some(self.node) {
                continue;
            }
            let Some(to) = member.lease.client else {
                continue;
            };
            if let Some(packet) = member.lease.seal(DOWN, MESH, &body, now) {
                out.push((to, packet));
                self.stats.delivered += 1;
            }
        }
        out
    }
}

struct Hop {
    address: SocketAddr,
    node: NodeId,
    client: Client,
}

/// One person's end. Mute, deafen, the sequence and duplicate detection belong
/// to the person, not to a path, so a path switch keeps all of them.
pub(crate) struct MeshClient {
    stream: u32,
    tag: u64,
    binding: Binding,
    active: Hop,
    next: Option<Hop>,
    seq: u64,
    clock: Instant,
    muted: bool,
    deafened: bool,
    seen: HashMap<u32, Replay>,
}

impl MeshClient {
    pub fn new(
        binding: &Binding,
        stream: u32,
        address: SocketAddr,
        node: NodeId,
        grant: Grant,
        requested: Instant,
        now: Instant,
    ) -> Option<Self> {
        let grant = bind(grant, binding)?;
        Some(Self {
            stream,
            tag: binding.tag(),
            binding: binding.clone(),
            active: Hop {
                address,
                node,
                client: Client::new(grant, requested, now)?,
            },
            next: None,
            seq: 0,
            clock: now,
            muted: false,
            deafened: false,
            seen: HashMap::new(),
        })
    }
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }
    pub fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
    }
    pub fn muted(&self) -> bool {
        self.muted
    }
    pub fn deafened(&self) -> bool {
        self.deafened
    }
    pub fn node(&self) -> NodeId {
        self.active.node
    }
    /// Whether `from` is one of this person's gateway hops.
    pub fn serves(&self, from: SocketAddr) -> bool {
        from == self.active.address || self.next.as_ref().is_some_and(|hop| hop.address == from)
    }
    pub fn switching(&self) -> bool {
        self.next.is_some()
    }
    pub fn healthy(&self, now: Instant) -> bool {
        self.active.client.healthy(now)
    }

    /// Health probes for the active hop and a hop being switched to.
    pub fn probes(&mut self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut out = Vec::new();
        for hop in std::iter::once(&mut self.active).chain(self.next.as_mut()) {
            if let Some(probe) = hop.client.probe(now) {
                out.push((hop.address, probe));
            }
        }
        out
    }

    /// One original protected frame, sent unchanged. Muted: nothing leaves.
    /// While switching, it goes over both hops; listeners drop the copy.
    pub fn frame(&mut self, protected: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        if self.muted || protected.is_empty() || protected.len() > MAX_FRAME {
            return Vec::new();
        }
        let origin = now.saturating_duration_since(self.clock).as_millis() as u32;
        let Some(body) =
            MeshHeader::first(self.tag, self.stream, self.seq, origin).encode(protected)
        else {
            return Vec::new();
        };
        self.seq += 1;
        let mut out = Vec::new();
        for hop in std::iter::once(&mut self.active).chain(self.next.as_mut()) {
            if hop.client.healthy(now) {
                if let Some(packet) = hop.client.lease.seal(UP, MESH, &body, now) {
                    out.push((hop.address, packet));
                }
            }
        }
        out
    }

    /// A packet from one of this person's gateways. Returns the sender's stream
    /// and the original protected frame for the E2EE layer, exactly once.
    pub fn receive(
        &mut self,
        from: SocketAddr,
        packet: &[u8],
        now: Instant,
    ) -> Option<(u32, Vec<u8>)> {
        let hop = if from == self.active.address {
            &mut self.active
        } else {
            self.next.as_mut().filter(|hop| hop.address == from)?
        };
        let node = hop.node;
        let (kind, body) = hop.client.open_kind(packet, now)?;
        if kind != MESH {
            return None;
        }
        let (head, frame) = MeshHeader::decode(&body)?;
        if head.binding != self.tag || head.stream == self.stream || head.via.last() != Some(&node)
        {
            return None;
        }
        if self.deafened {
            return None;
        }
        if self.seen.len() >= MAX_MEMBERS * 2 && !self.seen.contains_key(&head.stream) {
            return None;
        }
        self.seen
            .entry(head.stream)
            .or_default()
            .accept(head.seq)
            .then(|| (head.stream, frame.to_vec()))
    }

    pub fn stream(&self) -> u32 {
        self.stream
    }
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    /// The lease ids of the active hop and of a hop being switched to.
    pub fn lease_ids(&self) -> ([u8; 16], Option<[u8; 16]>) {
        (
            self.active.client.lease.grant.id,
            self.next.as_ref().map(|hop| hop.client.lease.grant.id),
        )
    }

    /// The server repeated a live lease: keep its id, key, nonce and replay
    /// state and extend it. A different key under the same id is refused.
    pub fn renew(&mut self, grant: Grant, requested: Instant, now: Instant) -> bool {
        let Some(grant) = bind(grant, &self.binding) else {
            return false;
        };
        for hop in std::iter::once(&mut self.active).chain(self.next.as_mut()) {
            if hop.client.lease.grant.id == grant.id {
                return same_key(&hop.client.lease.grant, &grant)
                    && hop.client.renew(grant.valid_for, requested, now);
            }
        }
        false
    }

    /// Make before break: lease the new hop while the old one keeps working.
    pub fn begin_switch(
        &mut self,
        address: SocketAddr,
        node: NodeId,
        grant: Grant,
        requested: Instant,
        now: Instant,
    ) -> bool {
        if node == self.active.node {
            return false;
        }
        let Some(grant) = bind(grant, &self.binding) else {
            return false;
        };
        let Some(client) = Client::new(grant, requested, now) else {
            return false;
        };
        self.next = Some(Hop {
            address,
            node,
            client,
        });
        true
    }

    /// Completes the switch only once the new hop is proven.
    pub fn commit_switch(&mut self, now: Instant) -> bool {
        if !self
            .next
            .as_ref()
            .is_some_and(|hop| hop.client.healthy(now))
        {
            return false;
        }
        self.active = self.next.take().expect("checked above");
        true
    }

    pub fn abort_switch(&mut self) {
        self.next = None;
    }
}

enum Command {
    Assign(Box<(Approval, RoutePlan)>),
    Admit(u32, Reach, Grant, Instant),
    Link(NodeId, SocketAddr, Grant, Instant),
    Revoke(u32),
    Unlink(NodeId),
    Withdraw,
}

/// What the worker has applied, for the gateway's next report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkerStatus {
    pub node: Option<NodeId>,
    pub applied_epoch: Option<u32>,
}

/// Runs one [`MeshGateway`] on its own sockets: one on the LAN for the home's
/// people and one on loopback for the links to other homes (the iroh helper
/// carries those). It owns only its sockets and memory; dropping it stops and
/// joins the worker. Commands come from the authenticated control plane only,
/// never from a socket.
pub(crate) struct MeshWorker {
    pub address: SocketAddr,
    pub link_address: SocketAddr,
    commands: mpsc::SyncSender<Command>,
    stop: Arc<AtomicBool>,
    wake: Arc<crate::audio_wake::Wake>,
    link_copies: Arc<AtomicU64>,
    status: Arc<Mutex<WorkerStatus>>,
    thread: Option<JoinHandle<()>>,
}

fn apply(gateway: &mut Option<MeshGateway>, command: Command, now: Instant) {
    match command {
        Command::Assign(assigned) => {
            let (approval, plan) = *assigned;
            match gateway.as_mut() {
                Some(current)
                    if current.binding == approval.binding && current.node == approval.node =>
                {
                    current.valid_until = approval.valid_until;
                    current.apply_plan(plan);
                }
                // A new binding or node is a new gateway: no state carries over.
                _ => *gateway = MeshGateway::new(approval, plan, now),
            }
        }
        Command::Withdraw => {
            if let Some(current) = gateway.as_mut() {
                current.withdraw(now);
            }
            *gateway = None;
        }
        command => {
            let Some(current) = gateway.as_mut() else {
                return;
            };
            match command {
                Command::Admit(stream, reach, grant, requested) => {
                    current.admit(stream, reach, grant, requested, now);
                }
                Command::Link(peer, address, grant, requested) => {
                    current.link(peer, address, grant, requested, now);
                }
                Command::Revoke(stream) => current.revoke(stream, now),
                Command::Unlink(peer) => current.unlink(peer),
                Command::Assign(_) | Command::Withdraw => {}
            }
        }
    }
}

impl MeshWorker {
    pub fn start(bind: SocketAddr) -> std::io::Result<Self> {
        if !bind.ip().is_unspecified() && !local(bind.ip()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Gateway needs a local IPv4 bind",
            ));
        }
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;
        let address = socket.local_addr()?;
        // Windows cannot send from a private-NIC-bound socket to loopback, so
        // links to the local iroh helper get their own loopback socket.
        let links = UdpSocket::bind("127.0.0.1:0")?;
        links.set_nonblocking(true)?;
        let link_address = links.local_addr()?;
        let (commands, receiver) = mpsc::sync_channel(MAX_ROUTES * 2);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let wake = Arc::new(crate::audio_wake::Wake::new());
        let waking = wake.clone();
        let link_copies = Arc::new(AtomicU64::new(0));
        let counting = link_copies.clone();
        let status = Arc::new(Mutex::new(WorkerStatus::default()));
        let publishing = status.clone();
        let thread = thread::Builder::new()
            .name("voice-mesh-gateway".into())
            .spawn(move || {
                let wait = crate::audio_wake::WorkerWait::new_pair(&waking, &socket, Some(&links));
                let mut buffer = [0; MAX_PACKET + 1];
                let mut gateway: Option<MeshGateway> = None;
                while !stopping.load(Ordering::Acquire) {
                    wait.begin_receive();
                    for command in receiver.try_iter().take(MAX_ROUTES * 2) {
                        apply(&mut gateway, command, Instant::now());
                    }
                    if let Ok(mut status) = publishing.lock() {
                        *status = WorkerStatus {
                            node: gateway.as_ref().map(|g| g.node),
                            applied_epoch: gateway.as_ref().map(|g| g.plan.epoch),
                        };
                    }
                    for source in [&socket, &links] {
                        for _ in 0..64 {
                            match source.recv_from(&mut buffer) {
                                Ok((n, from)) => {
                                    let Some(current) = gateway.as_mut() else {
                                        continue;
                                    };
                                    let from_link =
                                        current.links.values().any(|link| link.address == from);
                                    if from_link {
                                        counting.fetch_add(1, Ordering::Relaxed);
                                    }
                                    for (to, bytes) in
                                        current.receive(from, &buffer[..n], Instant::now())
                                    {
                                        let to_link =
                                            current.links.values().any(|link| link.address == to);
                                        let _ = if to_link {
                                            links.send_to(&bytes, to)
                                        } else {
                                            socket.send_to(&bytes, to)
                                        };
                                    }
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    break
                                }
                                Err(error)
                                    if error.kind() == std::io::ErrorKind::ConnectionReset =>
                                {
                                    continue
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    wait.wait(Duration::from_millis(20));
                }
            })?;
        Ok(Self {
            address,
            link_address,
            commands,
            stop,
            wake,
            link_copies,
            status,
            thread: Some(thread),
        })
    }
    fn send(&self, command: Command) -> bool {
        let sent = self.commands.try_send(command).is_ok();
        self.wake.notify();
        sent
    }
    /// The server's current approval and plan. A new binding or node starts a
    /// fresh gateway; the same one applies a newer plan and its new validity.
    pub fn assign(&self, approval: Approval, plan: RoutePlan) -> bool {
        self.send(Command::Assign(Box::new((approval, plan))))
    }
    pub fn admit(&self, stream: u32, reach: Reach, grant: Grant, requested: Instant) -> bool {
        self.send(Command::Admit(stream, reach, grant, requested))
    }
    pub fn link(
        &self,
        peer: NodeId,
        address: SocketAddr,
        grant: Grant,
        requested: Instant,
    ) -> bool {
        self.send(Command::Link(peer, address, grant, requested))
    }
    pub fn unlink(&self, peer: NodeId) -> bool {
        self.send(Command::Unlink(peer))
    }
    /// Revocations fail closed: if the command cannot be queued, the worker stops.
    pub fn revoke(&self, stream: u32) -> bool {
        self.fail_closed(Command::Revoke(stream))
    }
    pub fn withdraw(&self) -> bool {
        self.fail_closed(Command::Withdraw)
    }
    fn fail_closed(&self, command: Command) -> bool {
        if self.send(command) {
            return true;
        }
        self.stop.store(true, Ordering::Release);
        self.wake.notify();
        false
    }
    pub fn status(&self) -> WorkerStatus {
        self.status.lock().map(|s| *s).unwrap_or_default()
    }
    /// Datagrams received from linked forwarders: copies that crossed a link.
    pub fn link_copies(&self) -> u64 {
        self.link_copies.load(Ordering::Relaxed)
    }
}

impl std::ops::Drop for MeshWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake.notify();
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests;
