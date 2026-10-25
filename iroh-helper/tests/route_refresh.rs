//! Applying an already verified admission to a route which is open, for later
//! handshakes only. A real host, a stand-in community and a friend's client on one PC
//! over iroh's loopback mode: this proves the bookkeeping and the rules, not real
//! networks, relays or NAT.
mod common;

use common::*;
use relay_iroh::{
    client::{Client, RouteSpec},
    control,
    host::{HostConfig, HostService},
    token, Network,
};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    net::{TcpListener, UdpSocket},
    time::{sleep, timeout},
};

const SECRET: &str = "control-secret-for-the-route-refresh-helper-0123456789";

struct Host {
    community: Community,
    service: HostService,
    key: [u8; 32],
}

async fn start(key: [u8; 32], token: &str, community: Community) -> Host {
    let service = HostService::start(HostConfig {
        secret_key: iroh::SecretKey::from_bytes(&key),
        token: token.to_owned(),
        peer_port: community.tcp,
        ingress_token: INGRESS.into(),
        udp_port: community.udp,
        media_udp_port: 0,
        network: Network::Loopback,
    })
    .await
    .unwrap();
    Host {
        community,
        service,
        key,
    }
}

async fn host(token: &str) -> Host {
    start(
        token::random_bytes::<32>().unwrap(),
        token,
        community().await,
    )
    .await
}

impl Host {
    fn spec(&self, token: &str) -> RouteSpec {
        RouteSpec {
            id: self.service.endpoint_id(),
            relay: None,
            token: token.to_owned(),
            direct: self.service.addr().ip_addrs().copied().collect(),
        }
    }
}

/// One health request through a route's local gateway.
async fn alive(at: SocketAddr) -> bool {
    let (status, _) = http(at, &get("/api/health", "")).await;
    status == "HTTP/1.1 200 OK"
}

/// One voice packet through a route's local voice port, echoed back by the community.
async fn voice(app: &UdpSocket, port: u16, number: u8) -> bool {
    let packet = vec![number; 64];
    for _ in 0..20 {
        app.send_to(&packet, ("127.0.0.1", port)).await.unwrap();
        let mut buffer = [0u8; 2048];
        if let Ok(Ok((n, _))) =
            timeout(Duration::from_millis(300), app.recv_from(&mut buffer)).await
        {
            return buffer[..n] == packet[..];
        }
    }
    false
}

#[tokio::test]
async fn a_verified_admission_is_applied_while_every_live_part_of_the_route_stays() {
    let a = token::new_token().unwrap();
    let b = token::new_token().unwrap();
    let one = host(&a).await;
    let other_token = token::new_token().unwrap();
    let other = host(&other_token).await;
    let client = Client::new(Network::Loopback).await.unwrap();
    let route = client.route(one.spec(&a)).await.unwrap();
    let unrelated = client.route(other.spec(&other_token)).await.unwrap();
    let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(alive(route.http).await && voice(&app, route.voice_udp_port, 1).await);
    assert!(alive(unrelated.http).await);
    let (id, other_id) = (one.service.endpoint_id(), other.service.endpoint_id());
    let (live, other_live) = (
        client.connection_id(&id).await.unwrap(),
        client.connection_id(&other_id).await.unwrap(),
    );

    // The host moves on to a new admission and the app, having verified it, tells us.
    one.service.set_token(&b).unwrap();
    let refreshed = client.refresh_route(one.spec(&b), &a).await.unwrap();
    assert_eq!(refreshed, route, "the same local addresses, not new ones");
    // Nothing was dialled, closed or replaced: the same connection carries on.
    assert_eq!(client.connection_id(&id).await, Some(live));
    assert!(alive(route.http).await);
    assert!(
        voice(&app, route.voice_udp_port, 2).await,
        "the same voice socket still reaches the host"
    );
    // Asking again (the first reply was lost) is the same answer and changes nothing,
    // whatever the caller still believes the old admission was.
    assert_eq!(client.refresh_route(one.spec(&b), &a).await.unwrap(), route);
    assert_eq!(
        client
            .refresh_route(one.spec(&b), &token::new_token().unwrap())
            .await
            .unwrap(),
        route
    );
    assert_eq!(client.connection_id(&id).await, Some(live));
    // The map entry is the same one: the new admission finds the existing route.
    assert_eq!(client.route(one.spec(&b)).await.unwrap(), route);
    assert_eq!(client.connection_id(&id).await, Some(live));
    // Another route on the same client was never part of it.
    assert_eq!(
        client.route(other.spec(&other_token)).await.unwrap(),
        unrelated
    );
    assert_eq!(client.connection_id(&other_id).await, Some(other_live));
    assert!(alive(unrelated.http).await);
    client.close().await;
}

#[tokio::test]
async fn after_a_real_disconnect_the_route_reconnects_with_the_new_admission_on_the_same_ports() {
    let a = token::new_token().unwrap();
    let b = token::new_token().unwrap();
    let first = host(&a).await;
    let key = first.key;
    let client = Client::new(Network::Loopback).await.unwrap();
    let route = client.route(first.spec(&a)).await.unwrap();
    let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(alive(route.http).await && voice(&app, route.voice_udp_port, 1).await);
    let id = first.service.endpoint_id();
    let before = client.connection_id(&id).await.unwrap();
    // The same host key comes back, now admitting only the new token, at a new address.
    first.service.stop().await;
    let second = start(key, &b, community().await).await;
    assert_eq!(second.service.endpoint_id(), id);
    // Verified by the app, applied by us: no dial happens here.
    let refreshed = client.refresh_route(second.spec(&b), &a).await.unwrap();
    assert_eq!(refreshed, route);
    // The old connection died with the old host; the next request makes the one
    // handshake, with the refreshed token and address, behind the same local port.
    sleep(Duration::from_millis(300)).await;
    assert!(
        alive(route.http).await,
        "reconnected through the same gateway"
    );
    let after = client.connection_id(&id).await.unwrap();
    assert_ne!(after, before, "a genuine new connection");
    assert!(voice(&app, route.voice_udp_port, 2).await);
    assert_eq!(client.route(second.spec(&b)).await.unwrap(), route);
    assert!(second
        .community
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.contains("/api/health")));
    client.close().await;
}

#[tokio::test]
async fn a_missing_route_a_wrong_expectation_or_a_malformed_token_changes_nothing() {
    let a = token::new_token().unwrap();
    let b = token::new_token().unwrap();
    let one = host(&a).await;
    let stranger = host(&token::new_token().unwrap()).await;
    let client = Client::new(Network::Loopback).await.unwrap();
    // No route yet: refused, and no route springs into being.
    assert!(client.refresh_route(one.spec(&b), &a).await.is_err());
    assert_eq!(client.connection_id(&one.service.endpoint_id()).await, None);
    let route = client.route(one.spec(&a)).await.unwrap();
    let id = one.service.endpoint_id();
    let live = client.connection_id(&id).await.unwrap();
    // The wrong expected admission, a route which is not this one, and unusable tokens.
    let wrong = token::new_token().unwrap();
    assert!(client.refresh_route(one.spec(&b), &wrong).await.is_err());
    assert!(client.refresh_route(stranger.spec(&b), &a).await.is_err());
    assert!(client.refresh_route(one.spec("short"), &a).await.is_err());
    assert!(client.refresh_route(one.spec(&b), "short").await.is_err());
    let message = format!(
        "{:?}",
        client
            .refresh_route(one.spec(&b), &wrong)
            .await
            .err()
            .unwrap()
    );
    assert!(!message.contains(&a) && !message.contains(&b) && !message.contains(&wrong));
    // Still the original admission and connection: the right expectation still works,
    // and only now does the route hold the new token.
    assert_eq!(client.connection_id(&id).await, Some(live));
    assert!(alive(route.http).await);
    one.service.set_token(&b).unwrap();
    assert_eq!(client.refresh_route(one.spec(&b), &a).await.unwrap(), route);
    assert!(client.refresh_route(one.spec(&wrong), &a).await.is_err());
    assert_eq!(client.connection_id(&id).await, Some(live));
    client.close().await;
}

// ---- the private control route ----

struct Helper {
    at: SocketAddr,
    _scratch: Scratch,
}

async fn helper(name: &str) -> Helper {
    let scratch=Scratch::new(name);
    let dir=scratch.path.clone();
    let control = control::Control::new(SECRET, dir.clone(), Network::Loopback).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(control::serve(listener, control));
    Helper { at, _scratch:scratch }
}

impl Helper {
    async fn post(
        &self,
        path: &str,
        bearer: Option<&str>,
        body: &str,
        extra: &str,
    ) -> (String, String) {
        let authorization = bearer
            .map(|b| format!("Authorization: Bearer {b}\r\n"))
            .unwrap_or_default();
        http(
            self.at,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{authorization}{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
        .await
    }
}

fn route_json(host: &Host, token: &str) -> serde_json::Value {
    serde_json::json!({
        "endpoint_id": host.service.endpoint_id().to_string(),
        "token": token,
        "direct": host.service.addr().ip_addrs().map(|a| a.to_string()).collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn the_private_refresh_route_applies_an_admission_only_for_an_open_route_and_only_for_the_app(
) {
    let a = token::new_token().unwrap();
    let b = token::new_token().unwrap();
    let c = token::new_token().unwrap();
    let one = host(&a).await;
    let helper = helper("refresh").await;
    let refresh = |expected: &str, token: &str| {
        serde_json::json!({"route": route_json(&one, token), "expected_token": expected})
            .to_string()
    };
    // No route has been opened through this helper: a generic conflict, nothing created.
    let (status, reply) = helper
        .post("/route-refresh", Some(SECRET), &refresh(&a, &b), "")
        .await;
    assert!(status.contains("409"), "{status}");
    assert!(!reply.contains(&a) && !reply.contains(&b));
    // Open it the ordinary way; the host then rotates to B.
    let (status, opened) = helper
        .post(
            "/route",
            Some(SECRET),
            &route_json(&one, &a).to_string(),
            "",
        )
        .await;
    assert!(status.contains("200"), "{status} {opened}");
    let opened: serde_json::Value = serde_json::from_str(&opened).unwrap();
    let at: SocketAddr = opened["http_url"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    assert!(alive(at).await);
    one.service.set_token(&b).unwrap();
    // The app refreshes: the same reply as /route gave, and the route still works.
    let (status, reply) = helper
        .post("/route-refresh", Some(SECRET), &refresh(&a, &b), "")
        .await;
    assert!(status.contains("200"), "{status} {reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap(),
        opened
    );
    assert!(alive(at).await);
    // A lost reply, asked again: the same answer. The ordinary route finds the same entry.
    let (status, again) = helper
        .post("/route-refresh", Some(SECRET), &refresh(&a, &b), "")
        .await;
    assert!(status.contains("200") && again == reply, "{status}");
    let (_, ordinary) = helper
        .post(
            "/route",
            Some(SECRET),
            &route_json(&one, &b).to_string(),
            "",
        )
        .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ordinary).unwrap(),
        opened
    );
    // A wrong expectation: conflict, nothing said about either token, route untouched.
    let (status, reply) = helper
        .post("/route-refresh", Some(SECRET), &refresh(&a, &c), "")
        .await;
    assert!(status.contains("409"), "{status}");
    assert!(!reply.contains(&a) && !reply.contains(&b) && !reply.contains(&c));
    assert!(alive(at).await);

    // Only the app with the private secret, and never a browser.
    let good = refresh(&b, &c);
    for (name, bearer, extra) in [
        ("no secret", None, ""),
        (
            "wrong secret",
            Some("not-the-control-secret-not-the-control-secret"),
            "",
        ),
        (
            "browser origin",
            Some(SECRET),
            "Origin: https://evil.example\r\n",
        ),
        (
            "browser fetch",
            Some(SECRET),
            "Sec-Fetch-Site: cross-site\r\n",
        ),
    ] {
        let (status, _) = helper.post("/route-refresh", bearer, &good, extra).await;
        assert!(status.contains("403"), "{name}: {status}");
    }
    // Malformed requests are refused as such.
    let route = route_json(&one, &c);
    let mut short_token = route.clone();
    short_token["token"] = "short".into();
    let mut bad_relay = route.clone();
    bad_relay["relay"] = "http://insecure.example".into();
    let mut bad_id = route.clone();
    bad_id["endpoint_id"] = "zz".into();
    let big = serde_json::json!({"route": route, "expected_token": b, "pad": "a".repeat(5000)})
        .to_string();
    for (name, body) in [
        ("over 4096 bytes", big),
        ("not json", "nope".to_owned()),
        (
            "no expected token",
            serde_json::json!({"route": route}).to_string(),
        ),
        (
            "short expected token",
            serde_json::json!({"route": route, "expected_token": "short"}).to_string(),
        ),
        (
            "unknown field",
            serde_json::json!({"route": route, "expected_token": b, "force": true}).to_string(),
        ),
        (
            "short route token",
            serde_json::json!({"route": short_token, "expected_token": b}).to_string(),
        ),
        (
            "insecure relay",
            serde_json::json!({"route": bad_relay, "expected_token": b}).to_string(),
        ),
        (
            "bad endpoint",
            serde_json::json!({"route": bad_id, "expected_token": b}).to_string(),
        ),
    ] {
        let (status, reply) = helper.post("/route-refresh", Some(SECRET), &body, "").await;
        assert!(status.contains("400"), "{name}: {status}");
        assert!(
            !reply.contains(&b) && !reply.contains(&c),
            "{name}: nothing echoed"
        );
    }
    // Throughout, the route kept working and a GET on the private path is just not there.
    assert!(alive(at).await);
    let (status, _) = http(
        helper.at,
        &format!("GET /route-refresh HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {SECRET}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(status.contains("404"), "{status}");
}
