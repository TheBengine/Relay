//! Two homes' gateways linked over iroh on loopback: datagrams cross both ways
//! through each link's own loopback port, only granted peers connect, and a
//! revoked link stops at once. No relay, directory or external network.
use relay_iroh::{mesh_link::MeshLinks, Network};
use std::time::Duration;
use tokio::{net::UdpSocket, time::timeout};

struct Home {
    links: MeshLinks,
    worker: UdpSocket,
}

async fn home() -> Home {
    let links = MeshLinks::start(&Network::Loopback).await.unwrap();
    let worker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    links
        .set_worker(worker.local_addr().unwrap().port())
        .unwrap();
    Home { links, worker }
}

async fn link(a: &Home, b: &Home) -> (u16, u16) {
    let to_b = a
        .links
        .peer(
            b.links.endpoint_id(),
            None,
            b.links.addr().ip_addrs().copied().collect(),
        )
        .await
        .unwrap();
    let to_a = b
        .links
        .peer(
            a.links.endpoint_id(),
            None,
            a.links.addr().ip_addrs().copied().collect(),
        )
        .await
        .unwrap();
    (to_b, to_a)
}

/// Sends until one datagram arrives (the dialing side may still be connecting).
async fn crosses(from: &Home, port: u16, to: &Home, mark: u8) -> Option<u16> {
    let mut buffer = [0u8; 2048];
    for _ in 0..80 {
        from.worker
            .send_to(&[mark; 200], ("127.0.0.1", port))
            .await
            .unwrap();
        if let Ok(Ok((n, source))) =
            timeout(Duration::from_millis(100), to.worker.recv_from(&mut buffer)).await
        {
            if n == 200 && buffer[..n].iter().all(|b| *b == mark) {
                return Some(source.port());
            }
        }
    }
    None
}

#[tokio::test]
async fn two_homes_exchange_datagrams_through_their_own_link_ports() {
    let (a, b) = (home().await, home().await);
    let (to_b, to_a) = link(&a, &b).await;
    // B receives from the port it uses for A, so its worker sees one fixed peer address.
    assert_eq!(crosses(&a, to_b, &b, 1).await, Some(to_a));
    assert_eq!(crosses(&b, to_a, &a, 2).await, Some(to_b));
    assert!(a.links.connected(&b.links.endpoint_id()).await);
    let path = a.links.path(&b.links.endpoint_id()).await.unwrap();
    assert_eq!(path.kind, "direct", "loopback is a direct path");
    assert!(path.rtt_ms >= 0.0);
    // Asking again returns the same port: one link per peer.
    assert_eq!(
        a.links
            .peer(b.links.endpoint_id(), None, Vec::new())
            .await
            .unwrap(),
        to_b
    );
}

#[tokio::test]
async fn only_granted_peers_connect_and_a_revoked_link_stops() {
    let (a, b, c) = (home().await, home().await, home().await);
    let (to_b, to_a) = link(&a, &b).await;
    assert!(crosses(&a, to_b, &b, 3).await.is_some());
    // C links to B on its own, but B never granted C: nothing reaches B's worker.
    let to_b_from_c = c
        .links
        .peer(
            b.links.endpoint_id(),
            None,
            b.links.addr().ip_addrs().copied().collect(),
        )
        .await
        .unwrap();
    if c.links.endpoint_id() < b.links.endpoint_id() {
        assert_eq!(crosses(&c, to_b_from_c, &b, 4).await, None);
    }
    // A gateway never links to itself.
    assert!(a
        .links
        .peer(a.links.endpoint_id(), None, Vec::new())
        .await
        .is_err());
    // B drops A: A's datagrams no longer arrive.
    b.links.retain(&[]).await;
    assert!(!b.links.connected(&a.links.endpoint_id()).await);
    assert_eq!(crosses(&a, to_b, &b, 5).await, None);
    let _ = to_a;
}
