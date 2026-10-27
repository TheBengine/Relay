//! The client's control plane for the mesh, from the server's answers to real
//! UDP: a gateway applies its assignment, two people in its home get their
//! places, a frame crosses unchanged, and every withdrawal falls back to the host.
use super::*;
use std::net::UdpSocket;

const INSTANCE: &str = "abababababababababababababababab";

fn binding() -> plan::Binding {
    plan::Binding {
        community_id: "community".into(),
        channel_id: "vc1".into(),
        media_session: 9001,
    }
}

fn key(n: u8) -> String {
    URL_SAFE_NO_PAD.encode([n; 32])
}

fn id(n: u8) -> String {
    wire::encode_id(&[n; 16])
}

/// One home: the gateway's own seat (stream 1) and one person on its LAN (stream 2).
fn home_plan(epoch: u32) -> plan::RoutePlan {
    let member = |id: &str, stream| plan::Member {
        id: id.into(),
        stream,
        home: Some(plan::NodeId(1)),
        candidates: vec![plan::Candidate {
            node: plan::NodeId(1),
            first_hop: plan::HopQuality::LAN,
        }],
        muted: false,
        deafened: false,
    };
    let mut built = plan::RoutePlan::build(&plan::Topology {
        binding: binding(),
        epoch,
        gateways: vec![plan::Gateway {
            id: "gus".into(),
            node: plan::NodeId(1),
            approval: Some(binding()),
            uplink: None,
            upload_kbps: Some(0),
        }],
        trunks: Vec::new(),
        members: vec![member("11", 1), member("22", 2)],
        host_relay: false,
        speakers: 0,
    })
    .unwrap();
    built.epoch = epoch;
    built
}

fn assignment(epoch: u32, approved: bool) -> plan::MeshAssignment {
    let plan = home_plan(epoch);
    plan::MeshAssignment {
        approval: approved.then(|| plan::MeshApproval {
            gateway_id: "gus".into(),
            instance: INSTANCE.into(),
            binding: binding(),
            node: plan::NodeId(1),
            epoch,
            ttl_ms: wire::LEASE_TTL_MS,
        }),
        plan: approved.then_some(plan),
        links: Vec::new(),
        members: [(1, 31), (2, 32)]
            .into_iter()
            .map(|(stream, n)| plan::MemberGrant {
                id: id(n),
                stream,
                key: key(n),
                ttl_ms: wire::LEASE_TTL_MS,
            })
            .collect(),
        peers: Vec::new(),
    }
}

fn place(gateway: SocketAddr, stream: u32, n: u8) -> plan::MemberAssignment {
    plan::MemberAssignment {
        binding: binding(),
        epoch: 1,
        stream,
        node: plan::NodeId(1),
        gateway_id: "gus".into(),
        ip: gateway.ip().to_string(),
        port: gateway.port(),
        lease_id: id(n),
        key: key(n),
        ttl_ms: wire::LEASE_TTL_MS,
    }
}

struct Person {
    socket: UdpSocket,
    member: Member,
}

impl Person {
    fn new(gateway: SocketAddr, stream: u32, n: u8) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Short: a hop is healthy only with a reply in the last 300 ms.
        socket.set_read_timeout(Some(Duration::from_millis(30))).unwrap();
        let mut member = Member::default();
        member.assign(Some(place(gateway, stream, n)), Instant::now());
        assert!(member.client.is_some());
        Self { socket, member }
    }
    fn send(&self, packets: Vec<(SocketAddr, Vec<u8>)>) -> usize {
        for (to, packet) in &packets {
            self.socket.send_to(packet, to).unwrap();
        }
        packets.len()
    }
    /// Reads until quiet; returns frames heard.
    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut buffer = [0u8; 1500];
        while let Ok((n, from)) = self.socket.recv_from(&mut buffer) {
            if let Some(Some(frame)) = self.member.receive(from, &buffer[..n], false, Instant::now()) {
                frames.push(frame);
            }
        }
        frames
    }
}

fn protected(mark: u8) -> Vec<u8> {
    let mut frame = vec![mark; 120];
    frame[0] = relay_protocol::AUDIO_PACKET_V3;
    frame
}

#[test]
fn a_home_carries_its_own_speech_and_every_withdrawal_falls_back_to_the_host() {
    let mut gateway = Gateway::start("127.0.0.1".parse().unwrap()).unwrap();
    assert!(gateway.endpoint.is_none(), "no helper in tests: no links to other homes");
    gateway.apply(assignment(1, true), INSTANCE, Instant::now());
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(gateway.worker.status().applied_epoch, Some(1));
    let address = gateway.worker.address;
    let mut gus = Person::new(address, 1, 31);
    let mut cleo = Person::new(address, 2, 32);
    // Not proven yet: nothing goes to the mesh, the host carries everything.
    assert!(gus.member.frame(&protected(1), Instant::now()).is_empty());
    for _ in 0..4 {
        for person in [&mut gus, &mut cleo] {
            let probes = person.member.probes(Instant::now());
            person.send(probes);
            person.drain();
        }
        std::thread::sleep(Duration::from_millis(55));
    }
    assert!(gus.member.healthy(Instant::now()) && cleo.member.healthy(Instant::now()));
    // Gus speaks: Cleo hears the original protected frame, byte for byte.
    let packets = gus.member.frame(&protected(7), Instant::now());
    assert_eq!(gus.send(packets), 1);
    assert_eq!(cleo.drain(), vec![protected(7)]);
    // The server repeats the same lease: kept, with its sequence and replay state.
    gus.member.assign(Some(place(address, 1, 31)), Instant::now());
    let packets = gus.member.frame(&protected(8), Instant::now());
    gus.send(packets);
    assert_eq!(cleo.drain(), vec![protected(8)]);
    // The plan drops Gus: his client goes, and frames go only to the host again.
    gus.member.assign(None, Instant::now());
    assert!(gus.member.frame(&protected(9), Instant::now()).is_empty());
    // A dropped lease id is never used again with fresh counters.
    gus.member.assign(Some(place(address, 1, 31)), Instant::now());
    assert!(gus.member.client.is_none());
    // The gateway loses its approval: nothing is forwarded any more.
    gateway.apply(assignment(2, false), INSTANCE, Instant::now());
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(gateway.worker.status().applied_epoch, None);
    let mut dana = Person::new(address, 2, 33);
    let probes = dana.member.probes(Instant::now());
    dana.send(probes);
    dana.drain();
    assert!(!dana.member.healthy(Instant::now()));
}

#[test]
fn an_assignment_for_another_instance_or_an_invalid_plan_is_not_applied() {
    let mut gateway = Gateway::start("127.0.0.1".parse().unwrap()).unwrap();
    gateway.apply(assignment(1, true), "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd", Instant::now());
    let mut broken = assignment(1, true);
    if let Some(plan) = broken.plan.as_mut() {
        plan.binding.channel_id = "vc2".into();
    }
    gateway.apply(broken, INSTANCE, Instant::now());
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(gateway.worker.status().applied_epoch, None);
    assert!(gateway.approved.is_none());
    // Keys that are not 32 bytes never become leases.
    let mut member = Member::default();
    let mut bad = place(gateway.worker.address, 2, 40);
    bad.key = "short".into();
    member.assign(Some(bad), Instant::now());
    assert!(member.client.is_none());
}
