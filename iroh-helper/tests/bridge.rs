//! A host, a friend and a stand-in community on one PC, over iroh's loopback
//! mode. This proves the bridge, the admission and the rules; it does not prove
//! anything about real networks, relays or NAT.
mod common;

use common::*;
use futures_util::{SinkExt, StreamExt};
use relay_iroh::{
    client::{Client, RouteSpec},
    host::{HostConfig, HostService},
    token, Network,
};
use std::time::Duration;
use tokio::{
    net::UdpSocket,
    time::{sleep, timeout},
};
use tokio_tungstenite::tungstenite::Message;

struct World {
    community: Community,
    host: HostService,
    token: String,
    client: Client,
}

async fn world() -> World {
    world_with_media(0).await
}

async fn world_with_media(media_udp_port: u16) -> World {
    let community = community().await;
    let token = token::new_token().unwrap();
    let key = iroh::SecretKey::from_bytes(&token::random_bytes::<32>().unwrap());
    let host = HostService::start(HostConfig {
        secret_key: key,
        token: token.clone(),
        peer_port: community.tcp,
        ingress_token: INGRESS.into(),
        udp_port: community.udp,
        media_udp_port,
        network: Network::Loopback,
    })
    .await
    .unwrap();
    let client = Client::new(Network::Loopback).await.unwrap();
    World {
        community,
        host,
        token,
        client,
    }
}

#[tokio::test]
async fn admitted_media_stream_reaches_only_the_fixed_video_port_and_is_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let video = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = video.local_addr().unwrap().port();
    let w = world_with_media(port).await;
    let route = w.client.route(w.spec(&w.token)).await.unwrap();
    let (status, body) = http(route.http, &get("/_relay/media", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["media_udp_port"],
        port
    );
    let (denied, _) = http(route.http, &get("/_relay/media?target=127.0.0.1:1", "")).await;
    assert!(denied.contains("403"));
    let (denied, _) = http(
        route.http,
        &get("/_relay/media", "Origin: https://example.invalid\r\n"),
    )
    .await;
    assert!(denied.contains("403"));
    let mut stream = tokio::net::TcpStream::connect(route.http).await.unwrap();
    stream.write_all(b"POST /_relay/media HTTP/1.1\r\nHost: relay\r\nConnection: upgrade\r\nUpgrade: relay-media-v1\r\nContent-Length: 0\r\n\r\n").await.unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 4096);
        header.push(
            timeout(Duration::from_secs(3), stream.read_u8())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(String::from_utf8(header)
        .unwrap()
        .starts_with("HTTP/1.1 101"));
    let payload = vec![0x81; 1600];
    stream.write_u16(payload.len() as u16).await.unwrap();
    stream.write_all(&payload).await.unwrap();
    let mut packet = [0; 1601];
    let (size, peer) = timeout(Duration::from_secs(3), video.recv_from(&mut packet))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&packet[..size], payload.as_slice());
    video.send_to(&packet[..size], peer).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), stream.read_u16())
            .await
            .unwrap()
            .unwrap(),
        1600
    );
    stream.read_exact(&mut packet[..1600]).await.unwrap();
    assert_eq!(&packet[..1600], payload.as_slice());
    stream.write_u16(1601).await.unwrap();
    let closed = timeout(Duration::from_secs(3), stream.read_u8())
        .await
        .unwrap();
    assert!(closed.is_err(), "oversized framing closes the stream");
    assert!(
        w.community.seen.lock().unwrap().is_empty(),
        "media never goes to a generic HTTP proxy"
    );
    w.client.close().await;
    w.host.stop().await;
}

impl World {
    fn spec(&self, token: &str) -> RouteSpec {
        RouteSpec {
            id: self.host.endpoint_id(),
            relay: None,
            token: token.to_owned(),
            direct: self.host.addr().ip_addrs().copied().collect(),
        }
    }
}

#[tokio::test]
async fn a_friend_reaches_the_public_api_and_the_community_sees_a_private_peer() {
    let w = world().await;
    let route = w.client.route(w.spec(&w.token)).await.unwrap();
    // The friend tries to pass itself off as the host's own PC and as the ingress.
    let (status, body) = http(
        route.http,
        &get(
            "/api/health",
            "Authorization: Bearer friend-session\r\nX-Relay-Network-Peer: 1.2.3.4\r\nX-Relay-Network-Ingress: forged\r\nX-Forwarded-For: 127.0.0.1\r\n",
        ),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        seen["ingress"], INGRESS,
        "the host's capability replaces anything the friend sent"
    );
    let peer = seen["peer"].as_str().unwrap();
    assert!(peer.starts_with("100.") && peer != "1.2.3.4", "{peer}");
    assert_eq!(
        seen["forwarded"], "",
        "identity headers never reach the community"
    );
    assert_eq!(
        seen["authorization"], "Bearer friend-session",
        "the friend's own session passes through"
    );
    assert!(seen["host"].as_str().unwrap().starts_with("127.0.0.1:"));
    w.client.close().await;
    w.host.stop().await;
}

#[tokio::test]
async fn private_paths_browsers_and_proxy_requests_never_reach_the_community() {
    let w = world().await;
    let route = w.client.route(w.spec(&w.token)).await.unwrap();
    for request in [
        get("/host-secret", ""),
        get("/api/local-management/stop", ""),
        get("/localapi/v0/status", ""),
        get("/api/health", "Origin: https://evil.example\r\n"),
        get("/index.html", ""),
        "GET http://127.0.0.1:1/api/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            .to_owned(),
    ] {
        let (status, _) = http(route.http, &request).await;
        assert!(
            status.contains("403") || status.contains("400"),
            "{request:?} -> {status}"
        );
    }
    assert!(
        w.community.seen.lock().unwrap().is_empty(),
        "nothing refused may reach the community: {:?}",
        w.community.seen.lock().unwrap()
    );
    let (status, _) = http(route.http, &get("/api/health", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(
        w.community.seen.lock().unwrap().as_slice(),
        ["GET /api/health"]
    );
    w.client.close().await;
    w.host.stop().await;
}

#[tokio::test]
async fn a_websocket_is_carried_through_and_closes_cleanly() {
    let w = world().await;
    let route = w.client.route(w.spec(&w.token)).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{}/api/ws", route.http))
        .await
        .expect("the upgrade passes through the bridge");
    for text in ["first", "second", "a longer third message"] {
        socket.send(Message::Text(text.into())).await.unwrap();
        let answer = timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(answer.to_text().unwrap(), text);
    }
    socket.close(None).await.unwrap();
    assert!(w
        .community
        .seen
        .lock()
        .unwrap()
        .contains(&"GET /api/ws".to_owned()));
    w.client.close().await;
    w.host.stop().await;
}

#[tokio::test]
async fn voice_datagrams_travel_both_ways_through_the_loopback_port() {
    let w = world().await;
    let route = w.client.route(w.spec(&w.token)).await.unwrap();
    let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for number in 0..5u8 {
        let packet = vec![number; 100 + number as usize];
        // The first packets may be sent while the path is still being set up.
        let mut echoed = None;
        for _ in 0..20 {
            app.send_to(&packet, ("127.0.0.1", route.voice_udp_port))
                .await
                .unwrap();
            let mut buffer = [0u8; 2048];
            if let Ok(Ok((n, _))) =
                timeout(Duration::from_millis(300), app.recv_from(&mut buffer)).await
            {
                echoed = Some(buffer[..n].to_vec());
                break;
            }
        }
        assert_eq!(
            echoed.as_deref(),
            Some(&packet[..]),
            "packet {number} did not come back unchanged"
        );
    }
    // A packet larger than Relay's own limit is dropped, not split or forwarded.
    app.send_to(&vec![9u8; 1600], ("127.0.0.1", route.voice_udp_port))
        .await
        .unwrap();
    let mut buffer = [0u8; 4096];
    assert!(
        timeout(Duration::from_millis(400), app.recv_from(&mut buffer))
            .await
            .is_err()
    );
    w.client.close().await;
    w.host.stop().await;
}

#[tokio::test]
async fn a_wrong_token_is_refused_and_repeated_failures_lock_that_key_out() {
    let w = world().await;
    let wrong = token::new_token().unwrap();
    for attempt in 0..5 {
        let error = w
            .client
            .route(w.spec(&wrong))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("did not accept") || error.contains("answer"),
            "attempt {attempt}: {error}"
        );
    }
    // The same friend key is now refused even with the right token.
    assert!(
        w.client.route(w.spec(&w.token)).await.is_err(),
        "five wrong tokens lock the key out"
    );
    // Another friend is not affected.
    let other = Client::new(Network::Loopback).await.unwrap();
    let route = other
        .route(w.spec(&w.token))
        .await
        .expect("a different key with the right token is admitted");
    let (status, _) = http(route.http, &get("/api/health", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(
        w.community.seen.lock().unwrap().len() == 1,
        "the refused friend never reached the community"
    );
    other.close().await;
    w.client.close().await;
    w.host.stop().await;
}

#[tokio::test]
async fn rotating_the_token_locks_out_new_connections_with_the_old_one() {
    let w = world().await;
    let fresh = token::new_token().unwrap();
    w.host.set_token(&fresh).unwrap();
    let old_friend = Client::new(Network::Loopback).await.unwrap();
    assert!(old_friend.route(w.spec(&w.token)).await.is_err());
    let new_friend = Client::new(Network::Loopback).await.unwrap();
    assert!(new_friend.route(w.spec(&fresh)).await.is_ok());
    assert!(w.host.set_token("short").is_err());
    old_friend.close().await;
    new_friend.close().await;
    w.client.close().await;
    w.host.stop().await;
}

#[tokio::test]
async fn the_path_is_reported_as_direct_with_a_delay_and_never_with_an_address() {
    let w = world().await;
    assert_eq!(
        w.client.path(&w.host.endpoint_id()).await.kind,
        "unknown",
        "nothing is open yet"
    );
    let ms = w.client.probe(w.spec(&w.token)).await.unwrap();
    assert!((0.0..5_000.0).contains(&ms));
    let route = w.client.route(w.spec(&w.token)).await.unwrap();
    let (status, _) = http(route.http, &get("/api/health", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    sleep(Duration::from_millis(300)).await;
    let path = w.client.path(&w.host.endpoint_id()).await;
    assert_eq!(path.kind, "direct", "{path:?}");
    let json = serde_json::to_string(&path).unwrap();
    assert!(!json.contains("127.0.0.1"), "{json}");
    w.client.close_route(&w.host.endpoint_id()).await;
    assert_eq!(w.client.path(&w.host.endpoint_id()).await.kind, "unknown");
    w.client.close().await;
    w.host.stop().await;
}
