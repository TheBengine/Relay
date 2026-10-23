//! Bounded direct routing. Relay authorizes short leases; audio keeps its inner E2EE.
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use relay_protocol::{
    direct_voice::{Peer, Roster, LEASE_MS, MARKER},
    AUDIO_PACKET_V3, UDP_MAX_PACKET,
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    time::{Duration, Instant},
};

const HEADER: usize = 28;
const PROBE: u8 = 0;
const PONG: u8 = 1;
const AUDIO: u8 = 2;
const HEALTH: Duration = Duration::from_millis(150);

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
                let distance = seq - high;
                self.bits = if distance >= 128 {
                    1
                } else {
                    self.bits << distance | 1
                };
                self.highest = Some(seq);
                true
            }
            Some(high) => {
                let distance = high - seq;
                if distance >= 128 || self.bits & (1 << distance) != 0 {
                    return false;
                }
                self.bits |= 1 << distance;
                true
            }
        }
    }
}
struct Route {
    peer: Peer,
    expires: Instant,
    replay: Replay,
    next_probe: Instant,
    pending_probes: VecDeque<(u64, Instant)>,
    last_pong: Option<Instant>,
    rtts: VecDeque<u64>,
}
pub enum Received {
    Audio(Vec<u8>),
    Reply(SocketAddr, Vec<u8>),
    Control,
}
pub struct DirectVoice {
    local: u64,
    next_sequence: u64,
    next_request: u64,
    requests: VecDeque<(u64, Instant)>,
    route: Option<Route>,
}
impl DirectVoice {
    pub fn new(local: u64) -> Self {
        Self {
            local,
            next_sequence: 0,
            next_request: 0,
            requests: VecDeque::new(),
            route: None,
        }
    }
    pub fn request(&mut self, enabled: bool, rtt_us: u64, now: Instant) -> Vec<u8> {
        if !enabled {
            self.disable(now);
        }
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        self.requests.push_back((id, now));
        while self.requests.len() > 4 {
            self.requests.pop_front();
        }
        relay_protocol::direct_voice::request(id, enabled, rtt_us)
    }
    pub fn disable(&mut self, now: Instant) {
        // Keep the global nonce counter across opt-out and fresh leases.
        // Replay history stays with the same pair key; inner E2EE replay checks
        // also apply when changing pairs and returning to a previous pair.
        if let Some(route) = &mut self.route {
            route.expires = now;
            route.last_pong = None;
            route.rtts.clear();
            route.pending_probes.clear();
        }
    }
    pub fn install(&mut self, roster: Roster, now: Instant) -> bool {
        if roster.request == u64::MAX && roster.peer.is_none() {
            self.requests.clear();
            self.disable(now);
            return true;
        }
        let Some(index) = self
            .requests
            .iter()
            .position(|(id, _)| *id == roster.request)
        else {
            return false;
        };
        let (_, requested) = self.requests.remove(index).unwrap();
        let expires = requested + Duration::from_millis(LEASE_MS);
        if now >= expires {
            return false;
        }
        // An older reply must never renew permissions after a newer response.
        self.requests.retain(|(id, _)| *id > roster.request);
        let Some(peer) = roster.peer else {
            self.disable(now);
            return true;
        };
        if peer.session == self.local
            || peer.user.is_empty()
            || peer.user.len() > 128
            || peer.address.port() == 0
            || peer.address.ip().is_unspecified()
            || peer.address.ip().is_multicast()
        {
            return false;
        }
        if let Some(route) = &mut self.route {
            if route.peer.session == peer.session && route.peer.key == peer.key {
                if route.peer.address != peer.address || now >= route.expires {
                    route.last_pong = None;
                    route.rtts.clear();
                    route.pending_probes.clear();
                    route.next_probe = now;
                }
                route.peer = peer;
                route.expires = expires;
                return true;
            }
        }
        self.route = Some(Route {
            peer,
            expires,
            replay: Replay::default(),
            next_probe: now,
            pending_probes: VecDeque::new(),
            last_pong: None,
            rtts: VecDeque::new(),
        });
        true
    }
    fn seal(&mut self, kind: u8, body: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
        let route = self.route.as_ref()?;
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.checked_add(1)?;
        let mut header = MARKER.to_vec();
        header.extend(self.local.to_le_bytes());
        header.extend(route.peer.session.to_le_bytes());
        header.extend(sequence.to_le_bytes());
        let mut plaintext = vec![kind];
        plaintext.extend(body);
        let encrypted = ChaCha20Poly1305::new_from_slice(&route.peer.key)
            .ok()?
            .encrypt(
                Nonce::from_slice(&nonce(self.local < route.peer.session, sequence)),
                Payload {
                    msg: &plaintext,
                    aad: &header,
                },
            )
            .ok()?;
        header.extend(encrypted);
        (header.len() <= UDP_MAX_PACKET).then_some((route.peer.address, header))
    }
    pub fn probe(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        let route = self.route.as_mut()?;
        if now >= route.expires || now < route.next_probe {
            return None;
        }
        route.next_probe = now + Duration::from_millis(50);
        let id = self.next_sequence;
        route
            .pending_probes
            .retain(|(_, sent)| now.saturating_duration_since(*sent) < HEALTH);
        route.pending_probes.push_back((id, now));
        while route.pending_probes.len() > 4 {
            route.pending_probes.pop_front();
        }
        self.seal(PROBE, &id.to_le_bytes())
    }
    pub fn receive(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Option<Received> {
        if bytes.len() < HEADER + 17 || bytes.len() > UDP_MAX_PACKET || bytes.get(..4)? != MARKER {
            return None;
        }
        let sender = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
        let recipient = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
        let sequence = u64::from_le_bytes(bytes[20..28].try_into().ok()?);
        let route = self.route.as_mut()?;
        if now >= route.expires
            || from != route.peer.address
            || sender != route.peer.session
            || recipient != self.local
        {
            return None;
        }
        let plaintext = ChaCha20Poly1305::new_from_slice(&route.peer.key)
            .ok()?
            .decrypt(
                Nonce::from_slice(&nonce(sender < recipient, sequence)),
                Payload {
                    msg: &bytes[HEADER..],
                    aad: &bytes[..HEADER],
                },
            )
            .ok()?;
        if !route.replay.accept(sequence) {
            return None;
        }
        match plaintext.first().copied()? {
            PROBE if plaintext.len() == 9 => {
                let (address, reply) = self.seal(PONG, &plaintext[1..])?;
                Some(Received::Reply(address, reply))
            }
            PONG if plaintext.len() == 9 => {
                let id = u64::from_le_bytes(plaintext[1..9].try_into().ok()?);
                if let Some(index) = route
                    .pending_probes
                    .iter()
                    .position(|(pending, _)| *pending == id)
                {
                    let (_, sent) = route.pending_probes.remove(index).unwrap();
                    if now.saturating_duration_since(sent) < HEALTH {
                        route.last_pong = Some(now);
                        route
                            .rtts
                            .push_back(now.duration_since(sent).as_micros().max(1) as u64);
                        if route.rtts.len() > 3 {
                            route.rtts.pop_front();
                        }
                    }
                }
                Some(Received::Control)
            }
            AUDIO if route.peer.receive_allowed && valid_sender(&plaintext[1..], &route.peer) => {
                Some(Received::Audio(plaintext[1..].to_vec()))
            }
            _ => None,
        }
    }
    pub fn active(&self, local_relay_rtt_us: u64, now: Instant) -> bool {
        self.route.as_ref().is_some_and(|route| {
            now < route.expires
                && route.peer.send_allowed
                && route
                    .last_pong
                    .is_some_and(|pong| now.saturating_duration_since(pong) < HEALTH)
                && route.rtts.len() == 3
                && local_relay_rtt_us > 0
                && route.peer.relay_rtt_us > 0
                && self.rtt_us().is_some_and(|rtt| {
                    rtt.saturating_add(2000)
                        < local_relay_rtt_us.saturating_add(route.peer.relay_rtt_us)
                })
        })
    }
    pub fn rtt_us(&self) -> Option<u64> {
        let mut values: Vec<_> = self.route.as_ref()?.rtts.iter().copied().collect();
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        Some(values[values.len() / 2])
    }
    pub fn rtt_measured_at(&self) -> Option<Instant> {
        self.route.as_ref()?.last_pong
    }
    pub fn audio(
        &mut self,
        body: &[u8],
        local_relay_rtt_us: u64,
        now: Instant,
    ) -> Option<(SocketAddr, Vec<u8>)> {
        if body.first() != Some(&AUDIO_PACKET_V3) || !self.active(local_relay_rtt_us, now) {
            return None;
        }
        self.seal(AUDIO, body)
    }
}
fn nonce(low_to_high: bool, seq: u64) -> [u8; 12] {
    let mut result = [0; 12];
    result[..4].copy_from_slice(&(if low_to_high { 2_u32 } else { 3_u32 }).to_le_bytes());
    result[4..].copy_from_slice(&seq.to_le_bytes());
    result
}

fn valid_sender(frame: &[u8], peer: &Peer) -> bool {
    if frame.len() < 79 || frame[0] != AUDIO_PACKET_V3 || frame[1..9] != peer.session.to_le_bytes()
    {
        return false;
    }
    let user_len = frame[34] as usize;
    frame.get(60..60 + user_len) == Some(peer.user.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn peer(session: u64) -> Peer {
        Peer {
            session,
            user: format!("u{session}"),
            address: format!("127.0.0.1:{}", 12000 + session).parse().unwrap(),
            key: [42; 32],
            relay_rtt_us: 10_000,
            send_allowed: true,
            receive_allowed: true,
        }
    }
    fn install(route: &mut DirectVoice, other: u64, now: Instant) {
        let request = route.request(true, 10_000, now);
        let id = relay_protocol::direct_voice::parse_request(&request)
            .unwrap()
            .0;
        assert!(route.install(
            Roster {
                request: id,
                peer: Some(peer(other))
            },
            now
        ));
    }
    fn audio() -> Vec<u8> {
        let mut result = vec![0; 100];
        result[0] = AUDIO_PACKET_V3;
        result[1..9].copy_from_slice(&1_u64.to_le_bytes());
        result[34] = 2;
        result[60..62].copy_from_slice(b"u1");
        result
    }
    fn prove_route(a: &mut DirectVoice, b: &mut DirectVoice, origin: Instant, rtt_ms: u64) {
        for tick in [0, 50, 100] {
            let now = origin + Duration::from_millis(tick);
            let (_, probe) = a.probe(now).unwrap();
            let Some(Received::Reply(_, pong)) = b.receive(
                peer(1).address,
                &probe,
                now + Duration::from_millis(rtt_ms / 2),
            ) else {
                panic!("expected authenticated pong");
            };
            assert!(matches!(
                a.receive(peer(2).address, &pong, now + Duration::from_millis(rtt_ms)),
                Some(Received::Control)
            ));
        }
    }
    #[test]
    fn selects_only_proven_faster_route_and_falls_back_on_missed_probe_or_expired_lease() {
        let now = Instant::now();
        let mut a = DirectVoice::new(1);
        let mut b = DirectVoice::new(2);
        install(&mut a, 2, now);
        install(&mut b, 1, now);
        assert!(!a.active(10_000, now));
        prove_route(&mut a, &mut b, now, 2);
        assert!(a.active(10_000, now + Duration::from_millis(102)));
        assert_eq!(a.rtt_us(), Some(2000));
        assert!(!a.active(0, now + Duration::from_millis(102)));
        assert!(!a.active(10_000, now + Duration::from_millis(252)));
        assert!(a
            .audio(&audio(), 10_000, now + Duration::from_millis(500))
            .is_none());
        let mut slow = DirectVoice::new(1);
        let mut other = DirectVoice::new(2);
        install(&mut slow, 2, now);
        install(&mut other, 1, now);
        prove_route(&mut slow, &mut other, now, 30);
        assert!(!slow.active(10_000, now + Duration::from_millis(130)));
    }
    #[test]
    fn changed_endpoint_discards_outstanding_route_proofs() {
        let now = Instant::now();
        let mut a = DirectVoice::new(1);
        let mut b = DirectVoice::new(2);
        install(&mut a, 2, now);
        install(&mut b, 1, now);
        let (_, probe) = a.probe(now).unwrap();
        let Some(Received::Reply(_, pong)) = b.receive(peer(1).address, &probe, now) else {
            panic!("pong");
        };
        let request = relay_protocol::direct_voice::parse_request(&a.request(true, 10_000, now))
            .unwrap()
            .0;
        let mut changed = peer(2);
        changed.address.set_port(13002);
        assert!(a.install(
            Roster {
                request,
                peer: Some(changed.clone())
            },
            now
        ));
        assert!(a.route.as_ref().unwrap().pending_probes.is_empty());
        a.receive(changed.address, &pong, now + Duration::from_millis(2))
            .unwrap();
        assert_eq!(a.rtt_us(), None);
        assert!(!a.active(10_000, now + Duration::from_millis(2)));
    }

    #[test]
    fn overlapping_sixty_ms_probes_can_select_a_faster_direct_route() {
        let now = Instant::now();
        let mut a = DirectVoice::new(1);
        let mut b = DirectVoice::new(2);
        install(&mut a, 2, now);
        install(&mut b, 1, now);
        a.route.as_mut().unwrap().peer.relay_rtt_us = 60_000;
        let mut replies = Vec::new();
        for millis in [0, 50] {
            let (_, packet) = a.probe(now + Duration::from_millis(millis)).unwrap();
            let Some(Received::Reply(_, reply)) = b.receive(
                peer(1).address,
                &packet,
                now + Duration::from_millis(millis + 30),
            ) else {
                panic!("pong");
            };
            replies.push(reply);
        }
        a.receive(
            peer(2).address,
            &replies[0],
            now + Duration::from_millis(60),
        )
        .unwrap();
        let (_, third) = a.probe(now + Duration::from_millis(100)).unwrap();
        let Some(Received::Reply(_, reply)) =
            b.receive(peer(1).address, &third, now + Duration::from_millis(130))
        else {
            panic!("third pong");
        };
        a.receive(
            peer(2).address,
            &replies[1],
            now + Duration::from_millis(110),
        )
        .unwrap();
        a.receive(peer(2).address, &reply, now + Duration::from_millis(160))
            .unwrap();
        assert_eq!(a.rtt_us(), Some(60_000));
        assert!(a.active(60_000, now + Duration::from_millis(160)));
        assert!(a.route.as_ref().unwrap().pending_probes.is_empty());
    }

    #[test]
    fn authentication_replay_endpoint_and_sender_checks_precede_audio_delivery() {
        let now = Instant::now();
        let mut a = DirectVoice::new(1);
        let mut b = DirectVoice::new(2);
        install(&mut a, 2, now);
        install(&mut b, 1, now);
        prove_route(&mut a, &mut b, now, 2);
        let at = now + Duration::from_millis(105);
        let (_, packet) = a.audio(&audio(), 10_000, at).unwrap();
        let mut tampered = packet.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(b.receive(peer(1).address, &tampered, at).is_none());
        assert!(b.receive(peer(2).address, &packet, at).is_none());
        let Some(Received::Audio(clear)) = b.receive(peer(1).address, &packet, at) else {
            panic!("authenticated audio");
        };
        assert_eq!(clear, audio());
        assert!(b.receive(peer(1).address, &packet, at).is_none());
        let mut impersonation = audio();
        impersonation[61] = b'2';
        let (_, wrong_user) = a.audio(&impersonation, 10_000, at).unwrap();
        assert!(b.receive(peer(1).address, &wrong_user, at).is_none());
        b.route.as_mut().unwrap().peer.receive_allowed = false;
        let (_, gated) = a.audio(&audio(), 10_000, at).unwrap();
        assert!(b.receive(peer(1).address, &gated, at).is_none());
    }
    #[test]
    fn lease_uses_request_time_and_revocation_invalidates_inflight_old_rosters() {
        let now = Instant::now();
        let mut a = DirectVoice::new(1);
        let first = relay_protocol::direct_voice::parse_request(&a.request(true, 10_000, now))
            .unwrap()
            .0;
        assert!(!a.install(
            Roster {
                request: first,
                peer: Some(peer(2))
            },
            now + Duration::from_millis(501)
        ));
        let second = relay_protocol::direct_voice::parse_request(&a.request(true, 10_000, now))
            .unwrap()
            .0;
        assert!(a.install(
            Roster {
                request: u64::MAX,
                peer: None
            },
            now
        ));
        assert!(!a.install(
            Roster {
                request: second,
                peer: Some(peer(2))
            },
            now
        ));
        install(&mut a, 2, now);
        let sequence = a.next_sequence;
        a.probe(now).unwrap();
        a.disable(now);
        install(&mut a, 2, now);
        assert!(
            a.next_sequence > sequence,
            "route replacement cannot reuse an AEAD nonce"
        );
        assert!(!a.active(10_000, now));
    }
    #[test]
    fn direct_probes_and_ciphertext_travel_between_two_real_udp_bindings() {
        use std::net::UdpSocket;
        let sockets = [
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
        ];
        for socket in &sockets {
            socket
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
        }
        let now = Instant::now();
        let mut a = DirectVoice::new(1);
        let mut b = DirectVoice::new(2);
        install(&mut a, 2, now);
        install(&mut b, 1, now);
        a.route.as_mut().unwrap().peer.address = sockets[1].local_addr().unwrap();
        b.route.as_mut().unwrap().peer.address = sockets[0].local_addr().unwrap();
        let mut bytes = [0; UDP_MAX_PACKET];
        for _ in 0..3 {
            let (address, probe) = a.probe(Instant::now()).unwrap();
            sockets[0].send_to(&probe, address).unwrap();
            let (n, source) = sockets[1].recv_from(&mut bytes).unwrap();
            let Some(Received::Reply(address, reply)) =
                b.receive(source, &bytes[..n], Instant::now())
            else {
                panic!("pong");
            };
            sockets[1].send_to(&reply, address).unwrap();
            let (n, source) = sockets[0].recv_from(&mut bytes).unwrap();
            a.receive(source, &bytes[..n], Instant::now()).unwrap();
            std::thread::sleep(Duration::from_millis(51));
        }
        let (address, packet) = a.audio(&audio(), 10_000, Instant::now()).unwrap();
        sockets[0].send_to(&packet, address).unwrap();
        let (n, source) = sockets[1].recv_from(&mut bytes).unwrap();
        assert!(matches!(
            b.receive(source, &bytes[..n], Instant::now()),
            Some(Received::Audio(_))
        ));
    }
}
