//! Datagram runtime for a server-admitted local voice gateway.
//!
//! A grant is delivered over the authenticated community connection, NEVER by
//! LAN discovery. The gateway removes only this hop's envelope: the ordinary
//! client/server voice packet and its inner E2EE remain encrypted. The fixed
//! upstream and exact session binding prevent an open UDP proxy. No keys or
//! addresses are written to disk. Broker and app admission are separate from
//! this data plane; possessing an announcement is not an admission.
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAGIC: &[u8; 4] = b"RGW1";
const HEADER: usize = 36;
const DATA: u8 = 0;
const PROBE: u8 = 1;
const PONG: u8 = 2;
const MAX_PACKET: usize = 1200; // UDP payload budget: no new MTU-sized envelopes
pub(crate) const MAX_INNER: usize = MAX_PACKET - HEADER - 17;
pub(crate) const MAX_ROUTES: usize = 32;
const MAX_ISSUED: usize = 4096;
pub(crate) const MAX_LEASE: Duration = Duration::from_secs(2);
const HEALTH: Duration = Duration::from_millis(300);

/// Fresh random id and key per admission. Not Debug: the key is secret.
/// Only the server's authenticated admission worker may provide this value.
pub(crate) struct Grant {
    pub id: [u8; 16],
    pub session: u64,
    pub key: Zeroizing<[u8; 32]>,
    pub valid_for: Duration,
}

#[derive(Default)]
struct Replay {
    highest: Option<u64>,
    bits: u128,
}
impl Replay {
    fn accept(&mut self, seq: u64) -> bool {
        match self.highest {
            None => {
                self.highest = Some(seq);
                self.bits = 1;
                true
            }
            Some(high) if seq > high => {
                let shift = seq - high;
                self.bits = if shift >= 128 {
                    1
                } else {
                    (self.bits << shift) | 1
                };
                self.highest = Some(seq);
                true
            }
            Some(high) => {
                let shift = high - seq;
                if shift >= 128 || self.bits & (1 << shift) != 0 {
                    return false;
                }
                self.bits |= 1 << shift;
                true
            }
        }
    }
}
struct Lease {
    grant: Grant,
    derived_key: Zeroizing<[u8; 32]>,
    expires: Instant,
    received: Replay,
    next: u64,
    client: Option<SocketAddr>,
}
fn expiry(grant: &Grant, requested: Instant, now: Instant) -> Option<Instant> {
    if requested > now
        || grant.valid_for.is_zero()
        || grant.valid_for > MAX_LEASE
        || grant.id == [0; 16]
        || *grant.key == [0; 32]
    {
        return None;
    }
    requested
        .checked_add(grant.valid_for)
        .filter(|end| *end > now)
}
fn nonce(direction: u32, sequence: u64) -> [u8; 12] {
    let mut out = [0; 12];
    out[..4].copy_from_slice(&(0x4757_0000 | direction).to_le_bytes());
    out[4..].copy_from_slice(&sequence.to_le_bytes());
    out
}
fn header(packet: &[u8]) -> Option<([u8; 16], u64, u64)> {
    if packet.len() < HEADER + 17 || packet.len() > MAX_PACKET || packet.get(..4)? != MAGIC {
        return None;
    }
    Some((
        packet[4..20].try_into().ok()?,
        u64::from_le_bytes(packet[20..28].try_into().ok()?),
        u64::from_le_bytes(packet[28..36].try_into().ok()?),
    ))
}
fn valid_inner(bytes: &[u8], session: u64) -> bool {
    (33..=MAX_INNER).contains(&bytes.len())
        && bytes.get(..8) == Some(session.to_le_bytes().as_slice())
    // A voice session id can itself start with RGW1. The exact session and
    // fixed real upstream prevent recursive routing, not a magic-byte guess.
}
impl Lease {
    fn new(grant: Grant, requested: Instant, now: Instant) -> Option<Self> {
        let expires = expiry(&grant, requested, now)?;
        // Bind the encryption key to this admission too: an issuer accidentally
        // repeating key material across ids must not repeat a stream nonce.
        use hmac::Mac;
        let mut mac =
            <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(grant.key.as_slice()).ok()?;
        mac.update(b"relay-local-voice-gateway-v1");
        mac.update(&grant.id);
        mac.update(&grant.session.to_le_bytes());
        let derived_key = Zeroizing::new(mac.finalize().into_bytes().into());
        Some(Self {
            grant,
            derived_key,
            expires,
            received: Replay::default(),
            next: 0,
            client: None,
        })
    }
    fn seal(&mut self, direction: u32, kind: u8, bytes: &[u8], now: Instant) -> Option<Vec<u8>> {
        if now >= self.expires || bytes.len() > MAX_INNER {
            return None;
        }
        let sequence = self.next;
        self.next = self.next.checked_add(1)?; // reserve even if encryption fails
        let mut out = MAGIC.to_vec();
        out.extend(self.grant.id);
        out.extend(self.grant.session.to_le_bytes());
        out.extend(sequence.to_le_bytes());
        let mut body = Vec::with_capacity(1 + bytes.len());
        body.push(kind);
        body.extend(bytes);
        let cipher = ChaCha20Poly1305::new_from_slice(self.derived_key.as_slice()).ok()?;
        out.extend(
            cipher
                .encrypt(
                    Nonce::from_slice(&nonce(direction, sequence)),
                    Payload {
                        msg: &body,
                        aad: &out,
                    },
                )
                .ok()?,
        );
        Some(out)
    }
    fn open(&mut self, direction: u32, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        let (id, session, seq) = header(packet)?;
        if now >= self.expires || id != self.grant.id || session != self.grant.session {
            return None;
        }
        let cipher = ChaCha20Poly1305::new_from_slice(self.derived_key.as_slice()).ok()?;
        let clear = cipher
            .decrypt(
                Nonce::from_slice(&nonce(direction, seq)),
                Payload {
                    msg: &packet[HEADER..],
                    aad: &packet[..HEADER],
                },
            )
            .ok()?;
        self.received.accept(seq).then_some(clear)
    }
}

/// One local hop. `healthy` proves this hop only; before switching the app
/// must also confirm its ordinary authenticated server ping THROUGH this hop.
pub(crate) struct Client {
    lease: Lease,
    next_probe: Instant,
    pending: Vec<(u64, Instant)>,
    replies: u8,
    last_reply: Option<Instant>,
}
impl Client {
    pub fn new(grant: Grant, requested: Instant, now: Instant) -> Option<Self> {
        Some(Self {
            lease: Lease::new(grant, requested, now)?,
            next_probe: now,
            pending: Vec::new(),
            replies: 0,
            last_reply: None,
        })
    }
    pub fn renew(&mut self, valid_for: Duration, requested: Instant, now: Instant) -> bool {
        if valid_for.is_zero() || valid_for > MAX_LEASE || requested > now {
            return false;
        }
        let Some(end) = requested.checked_add(valid_for).filter(|end| *end > now) else {
            return false;
        };
        if now >= self.lease.expires {
            return false;
        } // new admission after a gap
        self.lease.expires = end;
        true
    }
    pub fn probe(&mut self, now: Instant) -> Option<Vec<u8>> {
        if now < self.next_probe || now >= self.lease.expires {
            return None;
        }
        self.next_probe = now + Duration::from_millis(50);
        let id = self.lease.next;
        self.pending
            .retain(|(_, at)| now.saturating_duration_since(*at) < HEALTH);
        if self.pending.len() >= 4 {
            self.pending.remove(0);
        }
        self.pending.push((id, now));
        self.lease.seal(0, PROBE, &id.to_le_bytes(), now)
    }
    pub fn healthy(&self, now: Instant) -> bool {
        now < self.lease.expires
            && self.replies >= 3
            && self
                .last_reply
                .is_some_and(|at| now.saturating_duration_since(at) < HEALTH)
    }
    pub fn datagram(&mut self, bytes: &[u8], now: Instant) -> Option<Vec<u8>> {
        if !self.healthy(now) || !valid_inner(bytes, self.lease.grant.session) {
            return None;
        }
        self.lease.seal(0, DATA, bytes, now)
    }
    /// Outer authentication/replay checks only. The app MUST still open the
    /// returned packet with its ordinary server/session cipher and E2EE keys.
    pub fn receive(&mut self, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        let clear = self.lease.open(1, packet, now)?;
        match clear.first().copied()? {
            PONG if clear.len() == 9 => {
                let id = u64::from_le_bytes(clear[1..].try_into().ok()?);
                if let Some(index) = self.pending.iter().position(|(pending, _)| *pending == id) {
                    let (_, sent) = self.pending.remove(index);
                    if now.saturating_duration_since(sent) < HEALTH {
                        self.replies = self.replies.saturating_add(1);
                        self.last_reply = Some(now);
                    }
                }
                None
            }
            DATA if valid_inner(&clear[1..], self.lease.grant.session) => Some(clear[1..].to_vec()),
            _ => None,
        }
    }
}

/// Only the fixed upstream may send server packets. All other inputs need a
/// current grant, exact session, authenticated envelope and a local source.
pub(crate) struct Router {
    upstream: SocketAddr,
    leases: HashMap<u64, Lease>,
    used: HashSet<[u8; 16]>,
}
fn local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        _ => false,
    }
}
impl Router {
    pub fn new(upstream: SocketAddr) -> Option<Self> {
        if upstream.port() == 0 || upstream.ip().is_unspecified() || upstream.ip().is_multicast() {
            return None;
        }
        Some(Self {
            upstream,
            leases: HashMap::new(),
            used: HashSet::new(),
        })
    }
    pub fn grant(&mut self, grant: Grant, requested: Instant, now: Instant) -> bool {
        use subtle::ConstantTimeEq;
        let Some(end) = expiry(&grant, requested, now) else {
            return false;
        };
        if let Some(existing) = self.leases.get_mut(&grant.session) {
            if existing.grant.id == grant.id {
                if now >= existing.expires
                    || !bool::from(existing.grant.key.as_slice().ct_eq(grant.key.as_slice()))
                {
                    return false;
                }
                existing.expires = end;
                return true; // retain both nonce and replay counters
            }
        }
        self.leases.retain(|_, lease| now < lease.expires);
        if self.used.len() >= MAX_ISSUED
            || self.used.contains(&grant.id)
            || (!self.leases.contains_key(&grant.session) && self.leases.len() >= MAX_ROUTES)
        {
            return false;
        }
        self.used.insert(grant.id);
        self.leases.insert(
            grant.session,
            Lease::new(grant, requested, now).expect("validated expiry"),
        );
        true
    }
    pub fn revoke(&mut self, session: u64) {
        self.leases.remove(&session);
    }
    pub fn receive(
        &mut self,
        from: SocketAddr,
        packet: &[u8],
        now: Instant,
    ) -> Option<(SocketAddr, Vec<u8>)> {
        if from == self.upstream {
            let session = u64::from_le_bytes(packet.get(..8)?.try_into().ok()?);
            if !valid_inner(packet, session) {
                return None;
            }
            let lease = self.leases.get_mut(&session)?;
            let client = lease.client?;
            return Some((client, lease.seal(1, DATA, packet, now)?));
        }
        if !local(from.ip()) || from.port() == 0 {
            return None;
        }
        let (_, session, seq) = header(packet)?;
        let lease = self.leases.get_mut(&session)?;
        // Reordered traffic from an old NAT binding cannot move the destination back.
        if lease.client != Some(from) && lease.received.highest.is_some_and(|high| seq <= high) {
            return None;
        }
        let clear = lease.open(0, packet, now)?;
        match clear.first().copied()? {
            PROBE if clear.len() == 9 => {
                lease.client = Some(from);
                Some((from, lease.seal(1, PONG, &clear[1..], now)?))
            }
            DATA if valid_inner(&clear[1..], session) => {
                lease.client = Some(from);
                Some((self.upstream, clear[1..].to_vec()))
            }
            _ => None,
        }
    }
}

/// One candidate's two-stage proof: first this LAN hop, then an authenticated
/// challenge echoed by the real voice server. An unanswered hop or expired
/// lease can never be presented as a usable end-to-end voice path.
/// The owner keeps admission-id history for the call: never create a fresh
/// Client for an id already used, even after dropping a failed candidate.
pub(crate) struct Path {
    pub address: SocketAddr,
    client: Client,
    pending: Vec<([u8; 16], Instant)>,
    next_probe: Instant,
    last_server_reply: Option<Instant>,
    rtt_us: Option<u64>,
}
impl Path {
    pub fn new(
        address: SocketAddr,
        grant: Grant,
        requested: Instant,
        now: Instant,
    ) -> Option<Self> {
        if !local(address.ip()) || address.port() < 1024 {
            return None;
        }
        Some(Self {
            address,
            client: Client::new(grant, requested, now)?,
            pending: Vec::new(),
            next_probe: now,
            last_server_reply: None,
            rtt_us: None,
        })
    }
    pub fn id(&self) -> [u8; 16] {
        self.client.lease.grant.id
    }
    pub fn remaining(&self, now: Instant) -> Duration {
        self.client.lease.expires.saturating_duration_since(now)
    }
    pub fn renew(&mut self, grant: &Grant, requested: Instant, now: Instant) -> bool {
        use subtle::ConstantTimeEq;
        let current = &self.client.lease.grant;
        if current.id != grant.id
            || current.session != grant.session
            || !bool::from(current.key.as_slice().ct_eq(grant.key.as_slice()))
        {
            return false;
        }
        self.client.renew(grant.valid_for, requested, now)
    }
    pub fn hop_probe(&mut self, now: Instant) -> Option<Vec<u8>> {
        self.client.probe(now)
    }
    pub fn receive(&mut self, source: SocketAddr, bytes: &[u8], now: Instant) -> Option<Vec<u8>> {
        if source != self.address {
            return None;
        }
        self.client.receive(bytes, now)
    }
    /// Plaintext for the ordinary voice session cipher. The media worker must
    /// reserve its normal nonce, encrypt this, then wrap it for the gateway.
    pub fn server_challenge(&mut self, now: Instant) -> Option<Vec<u8>> {
        if !self.client.healthy(now) || now < self.next_probe {
            return None;
        }
        self.next_probe = now + Duration::from_millis(200);
        self.pending
            .retain(|(_, at)| now.saturating_duration_since(*at) < Duration::from_millis(500));
        if self.pending.len() >= 4 {
            self.pending.remove(0);
        }
        let mut challenge = [0; 16];
        getrandom::getrandom(&mut challenge).ok()?;
        self.pending.push((challenge, now));
        let mut payload = vec![relay_protocol::UDP_PATH_PROBE];
        payload.extend(challenge);
        Some(payload)
    }
    /// Call only AFTER opening the real voice server's cipher/replay window,
    /// and only for a reply received inside this gateway's authenticated envelope.
    pub fn confirm_server(&mut self, plaintext: &[u8], now: Instant) -> bool {
        if plaintext.len() != 17 || plaintext[0] != relay_protocol::UDP_PATH_PROBE {
            return false;
        }
        let Ok(id) = <[u8; 16]>::try_from(&plaintext[1..]) else {
            return false;
        };
        let Some(index) = self.pending.iter().position(|(pending, _)| *pending == id) else {
            return false;
        };
        let (_, sent) = self.pending.remove(index);
        let age = now.saturating_duration_since(sent);
        if age >= Duration::from_millis(500) || !self.client.healthy(now) {
            return false;
        }
        self.rtt_us = Some(age.as_micros().clamp(1, u64::MAX as u128) as u64);
        self.last_server_reply = Some(now);
        true
    }
    /// Stick to a proven selected path while it is healthy. Select a new path
    /// only after the old path actually failed, or this one measured faster.
    pub fn usable(
        &self,
        now: Instant,
        selected: bool,
        previous_rtt_us: u64,
        previous_failed: bool,
    ) -> bool {
        self.client.healthy(now)
            && self
                .last_server_reply
                .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_millis(750))
            && (selected
                || previous_failed
                || (previous_rtt_us > 0
                    && self
                        .rtt_us
                        .is_some_and(|rtt| rtt.saturating_add(2000) < previous_rtt_us)))
    }
    pub fn wrap(&mut self, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.client.datagram(packet, now)
    }
    pub fn server_rtt_us(&self) -> Option<u64> {
        self.rtt_us
    }
    pub fn server_measured_at(&self) -> Option<Instant> {
        self.last_server_reply
    }
}

enum Command {
    Grant(Grant, Instant),
    Revoke(u64),
}
/// Owns only its UDP socket and memory. Dropping it wakes/joins the bounded
/// worker; it never waits for a community server or touches hardware devices.
pub(crate) struct Worker {
    pub address: SocketAddr,
    peer_traffic: Arc<AtomicBool>,
    commands: mpsc::SyncSender<Command>,
    stop: Arc<AtomicBool>,
    wake: Arc<crate::audio_wake::Wake>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(bind: SocketAddr, upstream: SocketAddr) -> std::io::Result<Self> {
        let mut router = Router::new(upstream).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid voice upstream")
        })?;
        if !bind.ip().is_unspecified() && !local(bind.ip()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Gateway needs a local IPv4 bind",
            ));
        }
        let socket = UdpSocket::bind(bind)?;
        socket.set_nonblocking(true)?;
        let address = socket.local_addr()?;
        // Windows cannot send from a private-NIC-bound socket to loopback.
        // Keep the advertised LAN bind narrow and own a connected loopback
        // socket for this gateway's independent helper route instead.
        let uplink = if upstream.ip().is_loopback() && !address.ip().is_loopback() {
            let uplink = UdpSocket::bind("127.0.0.1:0")?;
            uplink.connect(upstream)?;
            uplink.set_nonblocking(true)?;
            Some(uplink)
        } else {
            None
        };
        let (commands, receiver) = mpsc::sync_channel(MAX_ROUTES);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let wake = Arc::new(crate::audio_wake::Wake::new());
        let waking = wake.clone();
        let peer_traffic = Arc::new(AtomicBool::new(false));
        let activity = peer_traffic.clone();
        let thread = thread::Builder::new()
            .name("local-voice-gateway".into())
            .spawn(move || {
                let wait =
                    crate::audio_wake::WorkerWait::new_pair(&waking, &socket, uplink.as_ref());
                let mut buffer = [0; MAX_PACKET + 1];
                while !stopping.load(Ordering::Acquire) {
                    wait.begin_receive();
                    for command in receiver.try_iter().take(MAX_ROUTES) {
                        match command {
                            Command::Grant(grant, requested) => {
                                router.grant(grant, requested, Instant::now());
                            }
                            Command::Revoke(session) => router.revoke(session),
                        }
                    }
                    for _ in 0..64 {
                        match socket.recv_from(&mut buffer) {
                            Ok((n, from)) => {
                                if uplink.is_some() && from == upstream {
                                    continue;
                                }
                                if let Some((to, bytes)) =
                                    router.receive(from, &buffer[..n], Instant::now())
                                {
                                    if from != upstream {
                                        activity.store(true, Ordering::Release);
                                    }
                                    if to == upstream && uplink.is_some() {
                                        let _ = uplink.as_ref().unwrap().send(&bytes);
                                    } else {
                                        let _ = socket.send_to(&bytes, to);
                                    }
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                                continue
                            }
                            Err(_) => break,
                        }
                    }
                    if let Some(uplink) = &uplink {
                        for _ in 0..64 {
                            match uplink.recv(&mut buffer) {
                                Ok(n) => {
                                    if let Some((to, bytes)) =
                                        router.receive(upstream, &buffer[..n], Instant::now())
                                    {
                                        let _ = socket.send_to(&bytes, to);
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
            peer_traffic,
            commands,
            stop,
            wake,
            thread: Some(thread),
        })
    }
    /// Only a successfully authenticated peer packet marks activity. Unused
    /// broker grants and unauthenticated UDP noise cannot pin the wrong NIC.
    pub fn take_peer_activity(&self) -> bool {
        self.peer_traffic.swap(false, Ordering::AcqRel)
    }
    pub fn grant(&self, grant: Grant, requested: Instant) -> bool {
        let sent = self
            .commands
            .try_send(Command::Grant(grant, requested))
            .is_ok();
        self.wake.notify();
        sent
    }
    pub fn revoke(&self, session: u64) -> bool {
        if self.commands.try_send(Command::Revoke(session)).is_err() {
            self.stop.store(true, Ordering::Release); // fail closed even if the command queue is full
            self.wake.notify();
            if let Some(thread) = &self.thread {
                thread.thread().unpark();
            }
            return false;
        }
        self.wake.notify();
        true
    }
}
impl Drop for Worker {
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
mod tests {
    use super::*;
    fn grant(n: u8) -> Grant {
        Grant {
            id: [n; 16],
            session: u64::from(n),
            key: Zeroizing::new([40 + n; 32]),
            valid_for: MAX_LEASE,
        }
    }
    fn inner(session: u64) -> Vec<u8> {
        let mut bytes = vec![0x91; 100];
        bytes[..8].copy_from_slice(&session.to_le_bytes());
        bytes
    }
    fn ready(
        client: &mut Client,
        router: &mut Router,
        source: SocketAddr,
        now: Instant,
    ) -> Instant {
        let mut at = now;
        for n in 0..3 {
            at = now + Duration::from_millis(n * 50);
            let request = client.probe(at).unwrap();
            let (to, pong) = router.receive(source, &request, at).unwrap();
            assert_eq!(to, source);
            assert!(client
                .receive(&pong, at + Duration::from_millis(1))
                .is_none());
        }
        assert!(client.healthy(at + Duration::from_millis(1)));
        at + Duration::from_millis(1)
    }
    #[test]
    fn a_local_pong_never_proves_the_server_path_and_stale_paths_cannot_be_selected() {
        let now = Instant::now();
        let address = "192.168.1.20:47000".parse().unwrap();
        let source = "192.168.1.5:47001".parse().unwrap();
        let mut router = Router::new("198.51.100.8:4581".parse().unwrap()).unwrap();
        assert!(router.grant(grant(1), now, now));
        let mut path = Path::new(address, grant(1), now, now).unwrap();
        let at = ready(&mut path.client, &mut router, source, now);
        assert!(!path.usable(at, false, 100_000, true));
        let probe = path.server_challenge(at).unwrap();
        let mut foreign = probe.clone();
        foreign[1] ^= 1;
        assert!(!path.confirm_server(&foreign, at));
        assert!(path.confirm_server(&probe, at + Duration::from_millis(8)));
        assert!(
            !path.confirm_server(&probe, at + Duration::from_millis(9)),
            "challenge consumed once"
        );
        let at = at + Duration::from_millis(10);
        assert!(path.usable(at, false, 100_000, false));
        assert!(
            !path.usable(at, false, 5_000, false),
            "do not replace a faster path"
        );
        assert!(path.usable(at, true, 5_000, false));
        assert!(!path.usable(at + HEALTH, true, 100_000, true));
        assert!(path.wrap(&inner(1), at + MAX_LEASE).is_none());
    }
    #[test]
    fn gateway_marker_is_not_a_reserved_voice_session_prefix() {
        let session = u64::from_le_bytes(*b"RGW1abcd");
        let mut issued = grant(1);
        issued.session = session;
        let now = Instant::now();
        let mut client = Client::new(issued, now, now).unwrap();
        // Incoming ordinary server traffic must retain the session namespace.
        assert!(valid_inner(&inner(session), session));
        assert!(!valid_inner(&inner(session), 1));
        let mut router = Router::new("198.51.100.8:4581".parse().unwrap()).unwrap();
        let mut issued = grant(1);
        issued.session = session;
        assert!(router.grant(issued, now, now));
        let at = ready(
            &mut client,
            &mut router,
            "192.168.1.5:12000".parse().unwrap(),
            now,
        );
        let packet = client.datagram(&inner(session), at).unwrap();
        assert!(router
            .receive("192.168.1.5:12000".parse().unwrap(), &packet, at)
            .is_some());
    }
    #[test]
    fn admission_authentication_session_binding_replay_and_revocation_are_enforced() {
        let now = Instant::now();
        let source: SocketAddr = "192.168.1.5:12000".parse().unwrap();
        let upstream: SocketAddr = "198.51.100.8:4581".parse().unwrap();
        let mut router = Router::new(upstream).unwrap();
        let mut client = Client::new(grant(1), now, now).unwrap();
        let probe = client.probe(now).unwrap();
        assert!(router.receive(source, &probe, now).is_none());
        assert!(router.grant(grant(1), now, now));
        let mut client = Client::new(grant(1), now, now).unwrap();
        let at = ready(&mut client, &mut router, source, now);
        assert!(
            client.datagram(&inner(2), at).is_none(),
            "another session is not forwarded"
        );
        assert!(client.datagram(&vec![1; MAX_INNER + 1], at).is_none());
        let packet = client.datagram(&inner(1), at).unwrap();
        let mut bad = packet.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(
            router.receive(source, &bad, at).is_none(),
            "bad AEAD cannot advance replay state"
        );
        let (to, bytes) = router.receive(source, &packet, at).unwrap();
        assert_eq!(to, upstream);
        assert_eq!(bytes, inner(1));
        assert!(
            router.receive(source, &packet, at).is_none(),
            "no replay amplification"
        );
        assert!(router
            .receive(
                "8.8.8.8:12000".parse().unwrap(),
                &client.datagram(&inner(1), at).unwrap(),
                at
            )
            .is_none());
        assert!(
            router.receive(source, &inner(1), at).is_none(),
            "raw client datagrams are never proxied"
        );
        router.revoke(1);
        assert!(router
            .receive(source, &client.datagram(&inner(1), at).unwrap(), at)
            .is_none());
        assert!(
            !router.grant(grant(1), at, at),
            "revoked ids cannot be replayed into a new admission"
        );
        let mut fresh = grant(1);
        fresh.id = [9; 16];
        fresh.key = Zeroizing::new([99; 32]);
        assert!(router.grant(fresh, at, at));
        let mut first = Client::new(grant(2), at, at).unwrap();
        let mut another = grant(2);
        another.id = [77; 16];
        let mut second = Client::new(another, at, at).unwrap();
        let a = first.probe(at).unwrap();
        let b = second.probe(at).unwrap();
        assert_ne!(
            &a[HEADER..a.len() - 16],
            &b[HEADER..b.len() - 16],
            "admission ids separate the stream cipher even if an issuer repeats key material"
        );
    }
    #[test]
    fn lease_age_health_failure_and_renewal_preserve_nonce_history() {
        let now = Instant::now();
        let source = "192.168.1.5:12000".parse().unwrap();
        let mut router = Router::new("198.51.100.8:4581".parse().unwrap()).unwrap();
        assert!(
            !router.grant(grant(1), now - MAX_LEASE, now),
            "late replies do not gain fresh validity"
        );
        assert!(router.grant(grant(1), now, now));
        let mut client = Client::new(grant(1), now, now).unwrap();
        let at = ready(&mut client, &mut router, source, now);
        let packet = client.datagram(&inner(1), at).unwrap();
        assert!(router.receive(source, &packet, at).is_some());
        assert!(router.grant(grant(1), at, at));
        assert!(client.renew(MAX_LEASE, at, at));
        assert!(
            router.receive(source, &packet, at).is_none(),
            "renewal retains replay state"
        );
        let next = client.datagram(&inner(1), at).unwrap();
        assert_ne!(&packet[28..36], &next[28..36]);
        assert!(router.receive(source, &next, at).is_some());
        assert!(
            !client.healthy(at + HEALTH),
            "lost hop falls back without waiting for lease expiry"
        );
        assert!(client.datagram(&inner(1), at + HEALTH).is_none());
        assert!(router.receive(source, &next, at + MAX_LEASE).is_none());
        assert!(
            !router.grant(grant(1), at + MAX_LEASE, at + MAX_LEASE),
            "fresh admission is required after expiry"
        );
        let mut too_long = grant(2);
        too_long.valid_for = MAX_LEASE + Duration::from_millis(1);
        assert!(!router.grant(too_long, at, at));
        assert!(Client::new(grant(2), at + Duration::from_secs(1), at).is_none());
    }
    #[test]
    fn reordered_old_binding_and_untrusted_upstream_cannot_redirect_a_route() {
        let now = Instant::now();
        let old = "192.168.1.5:12000".parse().unwrap();
        let new = "192.168.1.5:13000".parse().unwrap();
        let upstream = "198.51.100.8:4581".parse().unwrap();
        let mut router = Router::new(upstream).unwrap();
        assert!(router.grant(grant(1), now, now));
        let mut client = Client::new(grant(1), now, now).unwrap();
        let at = ready(&mut client, &mut router, old, now);
        let late = client.datagram(&inner(1), at).unwrap();
        let newer = client.datagram(&inner(1), at).unwrap();
        assert!(router.receive(new, &newer, at).is_some());
        assert!(router.receive(old, &late, at).is_none());
        assert!(router
            .receive("198.51.100.9:4581".parse().unwrap(), &inner(1), at)
            .is_none());
        let (to, encrypted) = router.receive(upstream, &inner(1), at).unwrap();
        assert_eq!(to, new);
        assert_eq!(client.receive(&encrypted, at).unwrap(), inner(1));
        assert!(client.receive(&encrypted, at).is_none());
        let mut nested = inner(1);
        nested[..4].copy_from_slice(MAGIC);
        assert!(
            client.datagram(&nested, at).is_none(),
            "no nested gateway envelope or routing loop"
        );
    }
    #[test]
    fn route_tables_and_old_admission_ids_are_bounded() {
        let now = Instant::now();
        let mut router = Router::new("198.51.100.8:4581".parse().unwrap()).unwrap();
        for n in 1..=MAX_ROUTES as u8 {
            assert!(router.grant(grant(n), now, now));
        }
        assert!(!router.grant(grant(100), now, now));
        let mut changed = grant(1);
        changed.key = Zeroizing::new([99; 32]);
        assert!(
            !router.grant(changed, now, now),
            "same id must not reset nonce/replay with a different key"
        );
        router.used = (0..MAX_ISSUED as u128).map(u128::to_le_bytes).collect();
        assert!(!router.grant(grant(100), now + MAX_LEASE, now + MAX_LEASE));
    }
    fn voice_packet(session: u64, sequence: u64, direction: u32, clear: &[u8]) -> Vec<u8> {
        let mut packet = session.to_le_bytes().to_vec();
        packet.extend(sequence.to_le_bytes());
        let mut iv = [0; 12];
        iv[..4].copy_from_slice(&direction.to_le_bytes());
        iv[4..].copy_from_slice(&sequence.to_le_bytes());
        let cipher = ChaCha20Poly1305::new_from_slice(&[71; 32]).unwrap();
        packet.extend(
            cipher
                .encrypt(
                    Nonce::from_slice(&iv),
                    Payload {
                        msg: clear,
                        aad: &packet,
                    },
                )
                .unwrap(),
        );
        packet
    }
    #[test]
    fn udp_worker_carries_synthetic_opus_without_opening_the_voice_cipher_and_stops_cleanly() {
        let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let worker = Worker::start(
            "127.0.0.1:0".parse().unwrap(),
            upstream.local_addr().unwrap(),
        )
        .unwrap();
        let now = Instant::now();
        assert!(worker.grant(grant(1), now));
        let mut client = Client::new(grant(1), now, now).unwrap();
        let mut buffer = [0; MAX_PACKET + 1];
        for _ in 0..3 {
            while let Some(probe) = client.probe(Instant::now()) {
                socket.send_to(&probe, worker.address).unwrap();
                let (n, from) = socket.recv_from(&mut buffer).unwrap();
                assert_eq!(from, worker.address);
                client.receive(&buffer[..n], Instant::now());
                break;
            }
            thread::sleep(Duration::from_millis(55));
        }
        assert!(client.healthy(Instant::now()));
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let samples: Vec<f32> = (0..480)
            .map(|n| (n as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 0.2)
            .collect();
        let mut opus = [0; 512];
        let length = encoder.encode_float(&samples, &mut opus).unwrap();
        let mut clear = vec![1];
        clear.extend(1u64.to_le_bytes());
        clear.extend(&opus[..length]);
        let voice = voice_packet(1, 1, 0, &clear);
        let envelope = client.datagram(&voice, Instant::now()).unwrap();
        assert!(envelope.len() <= MAX_PACKET);
        assert!(!envelope
            .windows(length)
            .any(|window| window == &opus[..length]));
        socket.send_to(&envelope, worker.address).unwrap();
        let (n, gateway) = upstream.recv_from(&mut buffer).unwrap();
        assert_eq!(
            &buffer[..n],
            voice.as_slice(),
            "voice ciphertext stays exactly unchanged"
        );
        // Only the actual endpoint opens the original client/server voice cipher.
        let cipher = ChaCha20Poly1305::new_from_slice(&[71; 32]).unwrap();
        let mut iv = [0; 12];
        iv[4..].copy_from_slice(&1u64.to_le_bytes());
        let received = cipher
            .decrypt(
                Nonce::from_slice(&iv),
                Payload {
                    msg: &buffer[16..n],
                    aad: &buffer[..16],
                },
            )
            .unwrap();
        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
        let mut decoded = [0.0; 480];
        assert_eq!(
            decoder
                .decode_float(&received[9..], &mut decoded, false)
                .unwrap(),
            480
        );
        assert!(decoded.iter().map(|s| s * s).sum::<f32>() > 0.0001);
        let reply = voice_packet(1, 1, 1, &clear);
        upstream.send_to(&reply, gateway).unwrap();
        let (n, _) = socket.recv_from(&mut buffer).unwrap();
        assert_eq!(client.receive(&buffer[..n], Instant::now()).unwrap(), reply);
        assert!(worker.revoke(1));
        thread::sleep(Duration::from_millis(10));
        socket
            .send_to(
                &client.datagram(&voice, Instant::now()).unwrap(),
                worker.address,
            )
            .unwrap();
        assert!(
            upstream.recv_from(&mut buffer).is_err(),
            "revocation stops forwarding"
        );
        let started = Instant::now();
        let address = worker.address;
        drop(worker);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(UdpSocket::bind(address).is_ok(), "socket is released");
    }
}
