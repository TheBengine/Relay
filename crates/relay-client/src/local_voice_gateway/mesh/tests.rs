use super::*;
use crate::local_voice_gateway::MAX_LEASE;
use relay_protocol::voice_mesh::{
    Candidate, Gateway, HopQuality, Member as Person, PlanError, Topology, Trunk, HOST,
};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

const A: NodeId = NodeId(1);
const B: NodeId = NodeId(2);
/// A V3 protected voice frame: 60-byte fixed header, about 44 bytes of account
/// and device id, 80 bytes of Opus (32 kbit/s, 20 ms) and a 16-byte tag.
const FRAME: usize = 200;

fn binding() -> Binding {
    Binding {
        community_id: "community".into(),
        channel_id: "voice-1".into(),
        media_session: 77,
    }
}
fn q(rtt_ms: u32) -> HopQuality {
    HopQuality {
        rtt_ms,
        loss_permille: 0,
        jitter_ms: 0,
    }
}
fn grant(n: u16) -> Grant {
    let mut id = [0xA5; 16];
    id[..2].copy_from_slice(&n.to_le_bytes());
    let mut key = [0x3C; 32];
    key[..2].copy_from_slice(&n.to_le_bytes());
    Grant {
        id,
        session: binding().media_session,
        key: Zeroizing::new(key),
        valid_for: MAX_LEASE,
    }
}
fn person(id: &str, stream: u32, home: Option<NodeId>, candidates: &[(NodeId, u32)]) -> Person {
    Person {
        id: id.into(),
        stream,
        home,
        candidates: candidates
            .iter()
            .map(|(node, rtt)| Candidate {
                node: *node,
                first_hop: q(*rtt),
            })
            .collect(),
        muted: false,
        deafened: false,
    }
}
/// 3 + 3 + 1 + 1: homes A and B with their gateways, lone C and D.
fn households() -> Topology {
    let gateway = |node, id: &str| Gateway {
        id: id.into(),
        node,
        approval: Some(binding()),
        uplink: Some(q(40)),
        upload_kbps: None,
    };
    Topology {
        binding: binding(),
        epoch: 1,
        gateways: vec![gateway(A, "gw-a"), gateway(B, "gw-b")],
        trunks: vec![Trunk {
            a: A,
            b: B,
            quality: q(30),
        }],
        members: vec![
            person("a1", 1, Some(A), &[(A, 1)]),
            person("a2", 2, Some(A), &[(A, 1)]),
            person("a3", 3, Some(A), &[(A, 1)]),
            person("b1", 4, Some(B), &[(B, 1)]),
            person("b2", 5, Some(B), &[(B, 1)]),
            person("b3", 6, Some(B), &[(B, 1)]),
            person("c", 7, None, &[(HOST, 20), (A, 30)]),
            person("d", 8, None, &[(HOST, 25), (B, 60)]),
        ],
        host_relay: true,
        speakers: 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Site {
    HomeA,
    HomeB,
    PlaceC,
    PlaceD,
    Host,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct Load {
    up: u64,
    up_bytes: u64,
    down: u64,
    down_bytes: u64,
}
/// IPv4 + UDP headers on every datagram.
const IP_UDP: u64 = 28;

struct World {
    gateways: HashMap<SocketAddr, MeshGateway>,
    clients: HashMap<SocketAddr, MeshClient>,
    site: HashMap<SocketAddr, Site>,
    node_address: BTreeMap<NodeId, SocketAddr>,
    stream_address: BTreeMap<u32, SocketAddr>,
    queue: VecDeque<(SocketAddr, SocketAddr, Vec<u8>, bool)>,
    load: BTreeMap<Site, Load>,
    heard: HashMap<u32, Vec<(u32, Vec<u8>)>>,
    start: Instant,
    next_grant: u16,
}

fn addr(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

impl World {
    fn new(plan: &RoutePlan) -> World {
        let start = Instant::now();
        let mut world = World {
            gateways: HashMap::new(),
            clients: HashMap::new(),
            site: HashMap::new(),
            node_address: BTreeMap::new(),
            stream_address: BTreeMap::new(),
            queue: VecDeque::new(),
            load: BTreeMap::new(),
            heard: HashMap::new(),
            start,
            next_grant: 1,
        };
        for (node, address, site) in [
            (HOST, "192.0.2.10:4581", Site::Host),
            (A, "192.168.1.2:47000", Site::HomeA),
            (B, "192.168.2.2:47000", Site::HomeB),
        ] {
            world.site.insert(addr(address), site);
            if plan.nodes.contains(&node) {
                world.node_address.insert(node, addr(address));
                let approval = Approval {
                    node,
                    binding: binding(),
                    valid_until: start + Duration::from_secs(60),
                };
                world.gateways.insert(
                    addr(address),
                    MeshGateway::new(approval, plan.clone(), start).unwrap(),
                );
            }
        }
        for edge in &plan.edges {
            let n = world.grant_id();
            let (a, b) = (world.node_address[&edge.a], world.node_address[&edge.b]);
            assert!(world
                .gateways
                .get_mut(&a)
                .unwrap()
                .link(edge.b, b, grant(n), start, start));
            assert!(world
                .gateways
                .get_mut(&b)
                .unwrap()
                .link(edge.a, a, grant(n), start, start));
        }
        let people = [
            (1, "192.168.1.11:50000", Site::HomeA),
            (2, "192.168.1.12:50000", Site::HomeA),
            (3, "192.168.1.13:50000", Site::HomeA),
            (4, "192.168.2.11:50000", Site::HomeB),
            (5, "192.168.2.12:50000", Site::HomeB),
            (6, "192.168.2.13:50000", Site::HomeB),
            (7, "203.0.113.7:50000", Site::PlaceC),
            (8, "198.51.100.20:50000", Site::PlaceD),
        ];
        for (stream, address, site) in people {
            world.site.insert(addr(address), site);
            world.stream_address.insert(stream, addr(address));
            let Some(attachment) = plan.attachment(stream) else {
                continue;
            };
            world.attach(stream, attachment.node);
        }
        world.warm_up(start);
        world
    }
    fn grant_id(&mut self) -> u16 {
        self.next_grant += 1;
        self.next_grant
    }
    fn attach(&mut self, stream: u32, node: NodeId) {
        let n = self.grant_id();
        let gateway = self.node_address[&node];
        let me = self.stream_address[&stream];
        let reach = if self.site[&gateway] == self.site[&me] {
            Reach::Lan
        } else {
            Reach::Internet
        };
        assert!(self.gateways.get_mut(&gateway).unwrap().admit(
            stream,
            reach,
            grant(n),
            self.start,
            self.start
        ));
        self.clients.insert(
            me,
            MeshClient::new(
                &binding(),
                stream,
                gateway,
                node,
                grant(n),
                self.start,
                self.start,
            )
            .unwrap(),
        );
    }
    fn warm_up(&mut self, from: Instant) {
        for tick in 0..4 {
            let now = from + Duration::from_millis(tick * 51);
            self.probe(now);
        }
    }
    fn probe(&mut self, now: Instant) {
        let mut sent = Vec::new();
        for (me, client) in &mut self.clients {
            for (to, packet) in client.probes(now) {
                sent.push((*me, to, packet, false));
            }
        }
        self.queue.extend(sent);
        self.pump(now);
    }
    fn speak(&mut self, stream: u32, seq: u64, now: Instant) {
        let me = self.stream_address[&stream];
        let packets = self
            .clients
            .get_mut(&me)
            .unwrap()
            .frame(&frame(stream, seq), now);
        for (to, packet) in packets {
            self.queue.push_back((me, to, packet, true));
        }
    }
    fn pump(&mut self, now: Instant) {
        while let Some((from, to, bytes, media)) = self.queue.pop_front() {
            if media && self.site[&from] != self.site[&to] {
                let size = bytes.len() as u64 + IP_UDP;
                let up = self.load.entry(self.site[&from]).or_default();
                up.up += 1;
                up.up_bytes += size;
                let down = self.load.entry(self.site[&to]).or_default();
                down.down += 1;
                down.down_bytes += size;
            }
            if let Some(gateway) = self.gateways.get_mut(&to) {
                for (next, packet) in gateway.receive(from, &bytes, now) {
                    self.queue.push_back((to, next, packet, media));
                }
            } else if let Some(client) = self.clients.get_mut(&to) {
                if let Some(heard) = client.receive(from, &bytes, now) {
                    self.heard.entry(client.stream).or_default().push(heard);
                }
            }
        }
    }
    fn gateway(&mut self, node: NodeId) -> &mut MeshGateway {
        let address = self.node_address[&node];
        self.gateways.get_mut(&address).unwrap()
    }
    fn client(&mut self, stream: u32) -> &mut MeshClient {
        let address = self.stream_address[&stream];
        self.clients.get_mut(&address).unwrap()
    }
    fn heard(&self, listener: u32, speaker: u32) -> usize {
        self.heard
            .get(&listener)
            .map_or(0, |all| all.iter().filter(|(s, _)| *s == speaker).count())
    }
}

/// Stand-in for one original E2EE-protected voice frame: unique bytes per frame.
fn frame(stream: u32, seq: u64) -> Vec<u8> {
    let mut bytes = vec![0x5A; FRAME];
    bytes[..4].copy_from_slice(&stream.to_le_bytes());
    bytes[4..12].copy_from_slice(&seq.to_le_bytes());
    bytes
}

fn site_of(stream: u32) -> Site {
    match stream {
        1..=3 => Site::HomeA,
        4..=6 => Site::HomeB,
        7 => Site::PlaceC,
        _ => Site::PlaceD,
    }
}

/// One shared voice host: every speaker sends one copy up, and the host sends
/// one copy to every other listener. Sizes follow the ordinary voice UDP
/// format: 16-byte header, the same protected frame, 16-byte tag.
fn shared_host(speakers: &[u32], listeners: &[u32], frames: u64) -> BTreeMap<Site, Load> {
    let size = IP_UDP + 16 + FRAME as u64 + 16;
    let mut load: BTreeMap<Site, Load> = BTreeMap::new();
    for _ in 0..frames {
        for speaker in speakers {
            let up = load.entry(site_of(*speaker)).or_default();
            up.up += 1;
            up.up_bytes += size;
            for listener in listeners.iter().filter(|l| *l != speaker) {
                let down = load.entry(site_of(*listener)).or_default();
                down.down += 1;
                down.down_bytes += size;
            }
        }
    }
    load
}

fn run(speakers: &[u32], frames: u64) -> (World, BTreeMap<Site, Load>) {
    let plan = RoutePlan::build(&households()).unwrap();
    let mut world = World::new(&plan);
    let begin = world.start + Duration::from_millis(220);
    for seq in 0..frames {
        let now = begin + Duration::from_millis(seq * 20);
        world.probe(now);
        for speaker in speakers {
            world.speak(*speaker, seq, now);
        }
        world.pump(now);
    }
    let baseline = shared_host(speakers, &[1, 2, 3, 4, 5, 6, 7, 8], frames);
    (world, baseline)
}

fn report(
    title: &str,
    mesh: &BTreeMap<Site, Load>,
    baseline: &BTreeMap<Site, Load>,
    frames: u64,
) -> String {
    let mut out = format!("{title} ({frames} frames of 20 ms, frame {FRAME} B, +28 B IP/UDP)\n");
    out += "site       | shared host: up / down copies (kB) | gateways: up / down copies (kB)\n";
    let mut totals = [0u64; 4];
    for site in [Site::HomeA, Site::HomeB, Site::PlaceC, Site::PlaceD] {
        let b = baseline.get(&site).copied().unwrap_or_default();
        let m = mesh.get(&site).copied().unwrap_or_default();
        out += &format!(
            "{:<10} | {:>5} / {:>5} ({:>6.1} / {:>6.1}) | {:>5} / {:>5} ({:>6.1} / {:>6.1})\n",
            format!("{site:?}"),
            b.up,
            b.down,
            b.up_bytes as f64 / 1000.0,
            b.down_bytes as f64 / 1000.0,
            m.up,
            m.down,
            m.up_bytes as f64 / 1000.0,
            m.down_bytes as f64 / 1000.0,
        );
        totals[0] += b.up + b.down;
        totals[1] += b.up_bytes + b.down_bytes;
        totals[2] += m.up + m.down;
        totals[3] += m.up_bytes + m.down_bytes;
    }
    out += &format!(
        "homes' links in total: shared host {} copies / {:.1} kB, gateways {} copies / {:.1} kB\n",
        totals[0],
        totals[1] as f64 / 1000.0,
        totals[2],
        totals[3] as f64 / 1000.0
    );
    out
}

#[test]
fn simulation_three_speakers_in_a_cross_to_b_as_three_copies_not_nine() {
    const FRAMES: u64 = 50;
    let (world, baseline) = run(&[1, 2, 3], FRAMES);
    let mesh = &world.load;
    // Per 20 ms: B's link carries 3 streams instead of 3 x 3 = 9 copies.
    assert_eq!(baseline[&Site::HomeB].down, 9 * FRAMES);
    assert_eq!(mesh[&Site::HomeB].down, 3 * FRAMES);
    // Speech inside home A never touches A's own internet link for A's people.
    for listener in 1..=8u32 {
        for speaker in [1, 2, 3u32] {
            let expected = if listener == speaker {
                0
            } else {
                FRAMES as usize
            };
            assert_eq!(
                world.heard(listener, speaker),
                expected,
                "{listener} hears {speaker} exactly once per frame"
            );
        }
    }
    // The original protected frame arrives byte for byte: no decode or re-encode.
    for (speaker, bytes) in &world.heard[&4] {
        let seq = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
        assert_eq!(bytes, &frame(*speaker, seq));
    }
    println!(
        "{}",
        report("Three speakers in home A", mesh, &baseline, FRAMES)
    );
}

#[test]
fn simulation_everyone_speaking_against_one_shared_voice_host() {
    const FRAMES: u64 = 50;
    let everyone = [1, 2, 3, 4, 5, 6, 7, 8];
    let (world, baseline) = run(&everyone, FRAMES);
    for listener in everyone {
        for speaker in everyone {
            let expected = if listener == speaker {
                0
            } else {
                FRAMES as usize
            };
            assert_eq!(world.heard(listener, speaker), expected);
        }
    }
    let mesh = &world.load;
    // Each home link receives one copy per remote stream instead of one per listener.
    assert_eq!(baseline[&Site::HomeB].down, 3 * 7 * FRAMES);
    assert_eq!(
        mesh[&Site::HomeB].down,
        5 * FRAMES,
        "a1 a2 a3, c and d once each"
    );
    assert_eq!(baseline[&Site::HomeA].down, 3 * 7 * FRAMES);
    let total = |load: &BTreeMap<Site, Load>| {
        load.iter()
            .filter(|(site, _)| **site != Site::Host)
            .map(|(_, l)| l.up + l.down)
            .sum::<u64>()
    };
    assert!(total(mesh) < total(&baseline));
    for gateway in world.gateways.values() {
        assert_eq!(
            gateway.stats.duplicate + gateway.stats.looped + gateway.stats.refused,
            0
        );
    }
    println!("{}", report("Everyone speaking", mesh, &baseline, FRAMES));
}

/// Seals a packet on a link the way a peer would, for forged-input tests.
fn forge(n: u16, direction: u32, head: &MeshHeader, now: Instant) -> Vec<u8> {
    let mut lease = Lease::new(bind(grant(n), &binding()).unwrap(), now, now).unwrap();
    lease.next = 1000 + u64::from(head.via.len() as u8) * 100 + head.seq;
    lease
        .seal(
            direction,
            MESH,
            &head.encode(&frame(head.stream, head.seq)).unwrap(),
            now,
        )
        .unwrap()
}

/// A world with the A-B link on a known grant number, for forging.
fn linked() -> (World, u16, Instant) {
    let plan = RoutePlan::build(&households()).unwrap();
    let mut world = World::new(&plan);
    let start = world.start;
    let at = start + Duration::from_millis(220);
    let n = world.grant_id();
    let (a, b) = (world.node_address[&A], world.node_address[&B]);
    world.gateway(A).unlink(B);
    world.gateway(B).unlink(A);
    assert!(world.gateway(A).link(B, b, grant(n), start, start));
    assert!(world.gateway(B).link(A, a, grant(n), start, start));
    world.probe(at);
    (world, n, at)
}

#[test]
fn loops_wrong_paths_and_exhausted_hop_limits_are_refused() {
    let (mut world, n, at) = linked();
    let tag = binding().tag();
    let b = world.node_address[&B];
    // B sends A's own stream back to A: the reverse path says it comes from A's side.
    let mut back = MeshHeader::first(tag, 1, 5, 0);
    back.hops_left -= 2;
    back.via = vec![A, B];
    let packet = forge(n, 3, &back, at);
    assert!(world.gateway(A).receive(b, &packet, at).is_empty());
    assert_eq!(world.gateway(A).stats.looped, 1, "A is already in via");
    let mut wrong_side = MeshHeader::first(tag, 1, 6, 0);
    wrong_side.hops_left -= 1;
    wrong_side.via = vec![B];
    assert!(world
        .gateway(A)
        .receive(b, &forge(n, 3, &wrong_side, at), at)
        .is_empty());
    assert_eq!(
        world.gateway(A).stats.looped,
        2,
        "a1's stream never enters A from B"
    );
    // via must end with the link's own peer.
    let mut spoofed = MeshHeader::first(tag, 4, 1, 0);
    spoofed.hops_left -= 1;
    spoofed.via = vec![HOST];
    assert!(world
        .gateway(A)
        .receive(b, &forge(n, 3, &spoofed, at), at)
        .is_empty());
    // A frame from b1 with one hop left is still heard in A but not sent on to the host.
    let mut last = MeshHeader::first(tag, 4, 2, 0);
    last.hops_left = 1;
    last.via = vec![HOST, NodeId(9), B];
    let out = world.gateway(A).receive(b, &forge(n, 3, &last, at), at);
    assert!(!out.is_empty());
    assert!(out.iter().all(|(to, _)| *to != world.node_address[&HOST]));
    // A packet on the right key from the wrong address is not the peer.
    let mut fine = MeshHeader::first(tag, 4, 3, 0);
    fine.hops_left -= 1;
    fine.via = vec![B];
    assert!(world
        .gateway(A)
        .receive(addr("192.168.2.99:47000"), &forge(n, 3, &fine, at), at)
        .is_empty());
    // A plan with a cycle is never applied.
    let mut cyclic = RoutePlan::build(&households()).unwrap();
    cyclic.epoch += 1;
    cyclic.edges.push(relay_protocol::voice_mesh::Edge {
        a: HOST,
        b: B,
        one_way_ms: 1,
    });
    assert_eq!(cyclic.validate(), Err(PlanError::NotATree));
    assert!(!world.gateway(A).apply_plan(cyclic));
}

#[test]
fn duplicates_are_forwarded_once_and_replays_never() {
    let (mut world, _, at) = linked();
    let a1 = world.stream_address[&1];
    let a = world.node_address[&A];
    let first = world.client(1).frame(&frame(1, 0), at);
    assert_eq!(first.len(), 1);
    let (_, packet) = &first[0];
    let copies = world.gateway(A).receive(a1, packet, at);
    // a2, a3, c locally and B and the host once each.
    assert_eq!(copies.len(), 5);
    assert!(
        world.gateway(A).receive(a1, packet, at).is_empty(),
        "exact replay"
    );
    // The same stream sequence in a new envelope is a duplicate, not a new frame.
    world.client(1).seq -= 1;
    let again = world.client(1).frame(&frame(1, 0), at);
    assert!(world.gateway(A).receive(a1, &again[0].1, at).is_empty());
    assert_eq!(world.gateway(A).stats.duplicate, 1);
    // A listener also drops a duplicate that reaches it twice.
    let to_a2: Vec<_> = copies
        .iter()
        .filter(|(to, _)| *to == world.stream_address[&2])
        .collect();
    assert_eq!(to_a2.len(), 1);
    let (_, delivered) = to_a2[0];
    let a2 = world.client(2);
    assert!(a2.receive(a, delivered, at).is_some());
    assert!(a2.receive(a, delivered, at).is_none());
}

#[test]
fn a_person_can_only_send_their_own_fresh_stream() {
    let (mut world, _, at) = linked();
    let a1 = world.stream_address[&1];
    let client = world.client(1);
    // Claiming b1's stream through a1's own lease.
    client.stream = 4;
    let forged = client.frame(&frame(4, 0), at);
    client.stream = 1;
    assert!(world.gateway(A).receive(a1, &forged[0].1, at).is_empty());
    // A home lease from outside the LAN is refused.
    let packet = world.client(1).frame(&frame(1, 0), at);
    assert!(world
        .gateway(A)
        .receive(addr("8.8.8.8:50000"), &packet[0].1, at)
        .is_empty());
    // A raw protected frame without an envelope is never forwarded.
    assert!(world.gateway(A).receive(a1, &frame(1, 0), at).is_empty());
    assert!(world.gateway(A).stats.refused >= 3);
}

#[test]
fn old_and_late_frames_are_dropped() {
    let (mut world, n, at) = linked();
    let b = world.node_address[&B];
    let tag = binding().tag();
    let mut aged = MeshHeader::first(tag, 4, 1, 0);
    aged.hops_left -= 1;
    aged.via = vec![B];
    aged.age_ms = MAX_AGE_MS + 1;
    assert!(world
        .gateway(A)
        .receive(b, &forge(n, 3, &aged, at), at)
        .is_empty());
    assert_eq!(world.gateway(A).stats.expired, 1);
    // Clock-free lateness: the sender's clock says 0 and 20 ms, the second
    // frame arrives 400 ms after the first.
    let mut on_time = MeshHeader::first(tag, 4, 2, 1000);
    on_time.hops_left -= 1;
    on_time.via = vec![B];
    assert!(!world
        .gateway(A)
        .receive(b, &forge(n, 3, &on_time, at), at)
        .is_empty());
    let mut late = on_time.clone();
    late.seq = 3;
    late.origin_ms = 1020;
    let later = at + Duration::from_millis(400);
    assert!(world
        .gateway(A)
        .receive(b, &forge(n, 3, &late, later), later)
        .is_empty());
    assert_eq!(world.gateway(A).stats.expired, 2);
}

#[test]
fn revoked_people_links_and_approvals_stop_at_once() {
    let (mut world, n, at) = linked();
    let a2 = world.stream_address[&2];
    let b = world.node_address[&B];
    // Revoke a2 at A: their frames stop and they hear nothing more.
    world.gateway(A).revoke(2, at);
    let packet = world.client(2).frame(&frame(2, 0), at);
    assert!(world.gateway(A).receive(a2, &packet[0].1, at).is_empty());
    let from_a1 = world.client(1).frame(&frame(1, 0), at);
    let a1 = world.stream_address[&1];
    let out = world.gateway(A).receive(a1, &from_a1[0].1, at);
    assert!(out.iter().all(|(to, _)| *to != a2));
    assert!(
        !world.gateway(A).admit(2, Reach::Lan, grant(900), at, at),
        "no re-admission"
    );
    // B learns of the revocation: a2's stream from A is refused even before a new plan.
    world.gateway(B).revoke(2, at);
    let mut stale = MeshHeader::first(binding().tag(), 2, 9, 0);
    stale.hops_left -= 1;
    stale.via = vec![A];
    let a = world.node_address[&A];
    assert!(world
        .gateway(B)
        .receive(a, &forge(n, 2, &stale, at), at)
        .is_empty());
    assert_eq!(world.gateway(B).stats.revoked, 1);
    let plan = RoutePlan::build(&households()).unwrap();
    // The plan without a2 closes their lease everywhere.
    let mut next = plan.without(2);
    next.epoch = plan.epoch + 1;
    assert!(world.gateway(A).apply_plan(next.clone()));
    assert!(world.gateway(A).members.values().all(|m| m.stream != 2));
    assert!(
        !world.gateway(A).apply_plan(next),
        "an old or equal epoch is refused"
    );
    // Closing the link: A sends nothing toward B any more.
    world.gateway(A).unlink(B);
    let from_a1 = world.client(1).frame(&frame(1, 1), at);
    let a1 = world.stream_address[&1];
    let out = world.gateway(A).receive(a1, &from_a1[0].1, at);
    assert!(out.iter().all(|(to, _)| *to != b));
    assert!(!out.is_empty(), "home speech goes on");
    // Withdrawn approval: nothing at all.
    world.gateway(A).withdraw(at);
    let from_a1 = world.client(1).frame(&frame(1, 2), at);
    let a1 = world.stream_address[&1];
    assert!(world.gateway(A).receive(a1, &from_a1[0].1, at).is_empty());
}

#[test]
fn gateways_and_leases_are_bound_to_community_channel_and_media_session() {
    let plan = RoutePlan::build(&households()).unwrap();
    let now = Instant::now();
    let approval = |binding: Binding| Approval {
        node: A,
        binding,
        valid_until: now + Duration::from_secs(5),
    };
    let other_channel = Binding {
        channel_id: "voice-2".into(),
        ..binding()
    };
    assert!(MeshGateway::new(approval(other_channel.clone()), plan.clone(), now).is_none());
    assert!(MeshGateway::new(
        Approval {
            valid_until: now,
            ..approval(binding())
        },
        plan.clone(),
        now
    )
    .is_none());
    assert!(MeshGateway::new(
        Approval {
            node: NodeId(9),
            ..approval(binding())
        },
        plan.clone(),
        now
    )
    .is_none());
    let mut gateway = MeshGateway::new(approval(binding()), plan.clone(), now).unwrap();
    assert!(gateway.admit(1, Reach::Lan, grant(10), now, now));
    // The same server grant used under another channel's binding cannot open A.
    let mut stranger = MeshClient::new(
        &other_channel,
        1,
        addr("192.168.1.2:47000"),
        A,
        grant(10),
        now,
        now,
    )
    .unwrap();
    let probe = stranger.probes(now).pop().unwrap().1;
    assert!(gateway
        .receive(addr("192.168.1.11:50000"), &probe, now)
        .is_empty());
    // A grant for another media session is refused outright.
    let mut wrong = grant(11);
    wrong.session = 78;
    assert!(!gateway.admit(2, Reach::Lan, wrong, now, now));
    // Only a tree neighbour can be linked, and not a person outside the plan.
    assert!(!gateway.link(NodeId(5), addr("192.0.2.99:4000"), grant(12), now, now));
    assert!(!gateway.admit(99, Reach::Lan, grant(13), now, now));
}

#[test]
fn a_controlled_path_switch_keeps_mute_deafen_and_sequence() {
    let plan = RoutePlan::build(&households()).unwrap();
    assert_eq!(plan.attachment(7).unwrap().node, A);
    let mut world = World::new(&plan);
    let start = world.start;
    let mut now = start + Duration::from_millis(220);
    world.client(7).set_muted(true);
    world.client(7).set_deafened(true);
    // Pre-admit C at the host while A still carries C.
    let n = world.grant_id();
    let host = world.node_address[&HOST];
    assert!(world
        .gateway(HOST)
        .admit(7, Reach::Internet, grant(n), start, start));
    assert!(world
        .client(7)
        .begin_switch(host, HOST, grant(n), start, start));
    assert!(
        !world.client(7).commit_switch(now),
        "not before the new hop is proven"
    );
    for _ in 0..4 {
        world.probe(now);
        now += Duration::from_millis(51);
    }
    // The plan moves C; every forwarder applies it.
    let moved = plan
        .with_state(7, true, true)
        .unwrap()
        .switch(7, HOST, q(20))
        .unwrap();
    for node in moved.nodes.clone() {
        assert!(world.gateway(node).apply_plan(moved.clone()));
    }
    assert!(world.client(7).commit_switch(now));
    assert_eq!(world.client(7).node(), HOST);
    assert!(world.client(7).muted() && world.client(7).deafened());
    world.speak(7, 0, now);
    world.speak(1, 0, now);
    world.pump(now);
    assert_eq!(world.heard(1, 7), 0, "still muted after the switch");
    assert_eq!(world.heard(7, 1), 0, "still deafened after the switch");
    // Unmuted again, C is heard through the host, once, with a continuous sequence.
    world.client(7).set_muted(false);
    world.client(7).set_deafened(false);
    let unmuted = moved.with_state(7, false, false).unwrap();
    for node in unmuted.nodes.clone() {
        assert!(world.gateway(node).apply_plan(unmuted.clone()));
    }
    let seq_before = world.client(7).seq;
    world.speak(7, 1, now);
    world.speak(1, 1, now);
    world.pump(now);
    assert_eq!(world.client(7).seq, seq_before + 1);
    for listener in 1..=8u32 {
        if listener != 7 {
            assert_eq!(world.heard(listener, 7), 1, "{listener} hears C once");
        }
    }
    assert_eq!(world.heard(7, 1), 1);
}

#[test]
fn during_a_switch_both_paths_carry_frames_and_listeners_hear_each_once() {
    let plan = RoutePlan::build(&households()).unwrap();
    let mut world = World::new(&plan);
    let start = world.start;
    let mut now = start + Duration::from_millis(220);
    let n = world.grant_id();
    let host = world.node_address[&HOST];
    assert!(world
        .gateway(HOST)
        .admit(7, Reach::Internet, grant(n), start, start));
    assert!(world
        .client(7)
        .begin_switch(host, HOST, grant(n), start, start));
    for _ in 0..4 {
        world.probe(now);
        now += Duration::from_millis(51);
    }
    // Half the forwarders have the new plan, half the old one.
    let moved = plan.switch(7, HOST, q(20)).unwrap();
    assert!(world.gateway(HOST).apply_plan(moved.clone()));
    for seq in 0..5 {
        world.speak(7, seq, now);
        world.pump(now);
    }
    assert!(world.gateway(A).apply_plan(moved.clone()));
    assert!(world.gateway(B).apply_plan(moved));
    assert!(world.client(7).commit_switch(now));
    for seq in 5..10 {
        world.speak(7, seq, now);
        world.pump(now);
    }
    for listener in 1..=8u32 {
        if listener != 7 {
            assert!(
                world.heard(listener, 7) <= 10,
                "never more than once per frame"
            );
            let mut seqs: Vec<u64> = world.heard[&listener]
                .iter()
                .filter(|(s, _)| *s == 7)
                .map(|(_, f)| u64::from_le_bytes(f[4..12].try_into().unwrap()))
                .collect();
            let count = seqs.len();
            seqs.dedup();
            assert_eq!(seqs.len(), count, "no frame twice");
            assert!(
                seqs.iter().any(|s| *s >= 5),
                "{listener} hears C after the switch"
            );
        }
    }
}

#[test]
fn udp_workers_fan_out_between_two_homes_over_real_sockets() {
    use std::{net::UdpSocket, thread};
    const FRAMES: u64 = 10;
    let mut topology = households();
    topology.members.retain(|m| m.home.is_some());
    let plan = RoutePlan::build(&topology).unwrap();
    assert_eq!(plan.nodes, vec![A, B], "no host needed between two homes");
    let now = Instant::now();
    let make = |node| {
        let worker = MeshWorker::start(addr("127.0.0.1:0")).unwrap();
        let approval = Approval {
            node,
            binding: binding(),
            valid_until: now + Duration::from_secs(60),
        };
        assert!(worker.assign(approval, plan.clone()));
        worker
    };
    let home_a = make(A);
    let home_b = make(B);
    assert!(home_a.link(B, home_b.link_address, grant(500), now));
    assert!(home_b.link(A, home_a.link_address, grant(500), now));
    let mut people = Vec::new();
    for stream in 1..=6u32 {
        let (node, worker) = if stream <= 3 {
            (A, &home_a)
        } else {
            (B, &home_b)
        };
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let n = 600 + stream as u16;
        assert!(worker.admit(stream, Reach::Lan, grant(n), now));
        let client =
            MeshClient::new(&binding(), stream, worker.address, node, grant(n), now, now).unwrap();
        people.push((stream, socket, client));
    }
    thread::sleep(Duration::from_millis(30));
    assert_eq!(home_a.status().node, Some(A));
    assert_eq!(home_a.status().applied_epoch, Some(plan.epoch));
    let mut buffer = [0; MAX_PACKET + 1];
    for _ in 0..4 {
        for (_, socket, client) in &mut people {
            for (to, probe) in client.probes(Instant::now()) {
                socket.send_to(&probe, to).unwrap();
                let (n, from) = socket.recv_from(&mut buffer).unwrap();
                assert!(client.receive(from, &buffer[..n], Instant::now()).is_none());
            }
        }
        thread::sleep(Duration::from_millis(55));
    }
    assert!(people.iter().all(|(_, _, c)| c.healthy(Instant::now())));
    for seq in 0..FRAMES {
        for (stream, socket, client) in people.iter_mut().take(3) {
            for (to, packet) in client.frame(&frame(*stream, seq), Instant::now()) {
                socket.send_to(&packet, to).unwrap();
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    let mut heard: BTreeMap<(u32, u32), u64> = BTreeMap::new();
    for (stream, socket, client) in &mut people {
        while let Ok((n, from)) = socket.recv_from(&mut buffer) {
            if let Some((speaker, bytes)) = client.receive(from, &buffer[..n], Instant::now()) {
                let seq = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
                assert_eq!(bytes, frame(speaker, seq), "original frame, byte for byte");
                *heard.entry((*stream, speaker)).or_default() += 1;
            }
        }
    }
    for listener in 1..=6u32 {
        for speaker in 1..=3u32 {
            let expected = if listener == speaker { 0 } else { FRAMES };
            assert_eq!(
                heard.get(&(listener, speaker)).copied().unwrap_or(0),
                expected
            );
        }
    }
    // 3 streams x 10 frames crossed into B once each: 30 copies, not 3 x 3 x 10 = 90.
    assert_eq!(home_b.link_copies(), 3 * FRAMES);
    assert_eq!(home_a.link_copies(), 0);
    assert!(home_a.revoke(1));
    thread::sleep(Duration::from_millis(30));
    let (_, socket, client) = &mut people[0];
    for (to, packet) in client.frame(&frame(1, FRAMES), Instant::now()) {
        socket.send_to(&packet, to).unwrap();
    }
    let (_, socket, _) = &people[1];
    assert!(
        socket.recv_from(&mut buffer).is_err(),
        "revoked at the gateway: nothing forwarded"
    );
    drop(home_a);
    drop(home_b);
}
