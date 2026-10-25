//! Owned in-process loopback host and synthetic bytes; no native source/target
//! server pair, application profile, physical device, relay or external network.
mod common;
use common::*;
use relay_iroh::{
    client::RouteSpec,
    control,
    host::{HostConfig, HostService},
    prepared_route::{Error, Purpose, Store},
    token, Network,
};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    time::{sleep, timeout},
};
const SECRET: &str = "voice-gateway-control-fixture-secret-for-local-acceptance-0123456789";
struct Host {
    service: HostService,
    _community: Community,
}
impl Host {
    async fn new(admission: &str) -> Self {
        let c = community().await;
        let service = HostService::start(HostConfig {
            secret_key: iroh::SecretKey::from_bytes(&token::random_bytes::<32>().unwrap()),
            token: admission.into(),
            peer_port: c.tcp,
            ingress_token: INGRESS.into(),
            udp_port: c.udp,
            media_udp_port: 0,
            network: Network::Loopback,
        })
        .await
        .unwrap();
        Self {
            service,
            _community: c,
        }
    }
    fn spec(&self, admission: &str) -> RouteSpec {
        RouteSpec {
            id: self.service.endpoint_id(),
            relay: None,
            token: admission.into(),
            direct: self.service.addr().ip_addrs().copied().collect(),
        }
    }
}
async fn alive(address: SocketAddr) -> bool {
    http(address, &get("/api/health", "")).await.0 == "HTTP/1.1 200 OK"
}
async fn voice(socket: &UdpSocket, port: u16, mark: u8) -> bool {
    let body = vec![mark; 64];
    for _ in 0..12 {
        socket.send_to(&body, ("127.0.0.1", port)).await.unwrap();
        let mut buffer = [0; 2048];
        if let Ok(Ok((n, _))) =
            timeout(Duration::from_millis(100), socket.recv_from(&mut buffer)).await
        {
            return buffer[..n] == body[..];
        }
    }
    false
}
async fn closed(address: SocketAddr) -> bool {
    // A closed Windows port may leave connect pending before reporting refusal.
    // Either refusal or no connection within the deadline is acceptable; acceptance is not.
    timeout(Duration::from_millis(300), TcpStream::connect(address))
        .await
        .map_or(true, |result| result.is_err())
}
fn post(path: &str, value: &serde_json::Value, extra: &str) -> String {
    let body = value.to_string();
    format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap()
}
fn address(reply: &serde_json::Value) -> SocketAddr {
    reply["http_url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn renewal_keeps_the_same_sockets_and_traffic_and_never_reopens_an_expired_lease() {
    let admission = token::new_token().unwrap();
    let host = Host::new(&admission).await;
    let store = Store::with_lifetime(Duration::from_millis(900));
    let opened = store
        .open_gateway(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    assert_eq!(opened.purpose, Purpose::VoiceGateway);
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(alive(opened.route.http).await && voice(&socket, opened.route.voice_udp_port, 1).await);
    // 4 x 500 ms is well past the 900 ms lifetime: only the renewals keep it alive.
    for round in 0..4u8 {
        sleep(Duration::from_millis(500)).await;
        let renewed = store.renew_gateway(&opened.id).unwrap();
        assert_eq!(renewed.id, opened.id);
        assert_eq!(
            renewed.route, opened.route,
            "the exact same HTTP and UDP sockets"
        );
        assert_eq!(renewed.purpose, Purpose::VoiceGateway);
        assert_eq!(
            renewed.expires_in_secs, 1,
            "reset to at most the fixed lifetime"
        );
        assert!(alive(opened.route.http).await);
        assert!(voice(&socket, opened.route.voice_udp_port, 2 + round).await);
    }
    assert_eq!(
        store.status_gateway(&opened.id).unwrap().route,
        opened.route
    );
    // No renewal: the lease ends by itself and a late renewal cannot bring it back.
    sleep(Duration::from_millis(1100)).await;
    assert!(matches!(
        store.status_gateway(&opened.id),
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.renew_gateway(&opened.id),
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.status_gateway(&opened.id),
        Err(Error::Missing)
    ));
    assert!(closed(opened.route.http).await);
    assert!(matches!(
        store.close_gateway(&opened.id).await,
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.renew_gateway(&token::new_token().unwrap()),
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.renew_gateway("not-a-lease"),
        Err(Error::Invalid)
    ));
    // The old resource finishing cleanup removes nothing of a replacement.
    let replacement = store
        .open_gateway(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    assert_ne!(replacement.id, opened.id);
    sleep(Duration::from_millis(300)).await;
    assert!(store.status_gateway(&replacement.id).is_ok());
    assert!(matches!(
        store.status_gateway(&opened.id),
        Err(Error::Missing)
    ));
    assert!(alive(replacement.route.http).await);
    store.shutdown().await;
    assert!(matches!(
        store.renew_gateway(&replacement.id),
        Err(Error::Missing)
    ));
    assert!(closed(replacement.route.http).await);
}

#[tokio::test]
async fn gateway_and_preflight_leases_are_isolated_but_share_the_two_slots() {
    let admission = token::new_token().unwrap();
    let host = Host::new(&admission).await;
    let store = Store::new();
    let preflight = store
        .open(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    let gateway = store
        .open_gateway(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    assert_eq!(preflight.purpose, Purpose::Preflight);
    assert_eq!(gateway.purpose, Purpose::VoiceGateway);
    assert_ne!(preflight.route.http, gateway.route.http);
    // One shared total: neither purpose can take a third slot.
    assert!(matches!(
        store
            .open_gateway(&Network::Loopback, host.spec(&admission))
            .await,
        Err(Error::Busy)
    ));
    assert!(matches!(
        store.open(&Network::Loopback, host.spec(&admission)).await,
        Err(Error::Busy)
    ));
    // Each purpose's calls see the other's id as nothing, and change nothing.
    assert!(matches!(store.status(&gateway.id), Err(Error::Missing)));
    assert!(matches!(
        store.status_gateway(&preflight.id),
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.renew_gateway(&preflight.id),
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.close(&gateway.id).await,
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.close_gateway(&preflight.id).await,
        Err(Error::Missing)
    ));
    assert!(store.status(&preflight.id).is_ok() && store.status_gateway(&gateway.id).is_ok());
    assert!(alive(preflight.route.http).await && alive(gateway.route.http).await);
    // Closing the gateway frees exactly its slot; a replacement has a new id and sockets.
    store.close_gateway(&gateway.id).await.unwrap();
    assert!(matches!(
        store.status_gateway(&gateway.id),
        Err(Error::Missing)
    ));
    assert!(matches!(
        store.renew_gateway(&gateway.id),
        Err(Error::Missing)
    ));
    assert!(closed(gateway.route.http).await);
    assert!(alive(preflight.route.http).await);
    let next = store
        .open_gateway(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    assert_ne!(next.id, gateway.id);
    sleep(Duration::from_millis(300)).await;
    assert!(store.status_gateway(&next.id).is_ok() && store.status(&preflight.id).is_ok());
    store.shutdown().await;
}

#[tokio::test]
async fn the_private_control_api_is_browser_and_secret_guarded_exact_and_purpose_scoped() {
    let dir = Scratch::new("voice-gateway-control");
    let control = control::Control::new(SECRET, dir.path.clone(), Network::Loopback).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    let service = tokio::spawn(control::serve(listener, control.clone()));
    let admission = token::new_token().unwrap();
    let host = Host::new(&admission).await;
    let auth = format!("Authorization: Bearer {SECRET}\r\n");
    let spec = host.spec(&admission);
    let route = serde_json::json!({"endpoint_id":spec.id.to_string(),"token":admission,"direct":spec.direct.iter().map(ToString::to_string).collect::<Vec<_>>()});
    let other = serde_json::json!({"lease":token::new_token().unwrap()});
    let paths = [
        "/voice-gateway-route",
        "/voice-gateway-route-status",
        "/renew-voice-gateway-route",
        "/close-voice-gateway-route",
    ];
    for path in paths {
        let body = if path == "/voice-gateway-route" {
            &route
        } else {
            &other
        };
        // No secret, a wrong secret and a browser origin never reach the pool.
        assert!(http(at, &post(path, body, "")).await.0.contains("403"));
        assert!(http(
            at,
            &post(
                path,
                body,
                "Authorization: Bearer wrong-secret-wrong-secret-wrong-secret-0000\r\n"
            )
        )
        .await
        .0
        .contains("403"));
        assert!(http(
            at,
            &post(
                path,
                body,
                &format!("{auth}Origin: https://untrusted.invalid\r\n")
            )
        )
        .await
        .0
        .contains("403"));
        assert!(http(at, &get(path, &auth)).await.0.contains("404"));
    }
    // Unknown fields on the opening request are refused: no address, token or TTL input.
    let mut extra = route.clone();
    extra["ttl"] = serde_json::json!(600);
    assert!(http(at, &post("/voice-gateway-route", &extra, &auth))
        .await
        .0
        .contains("400"));
    let (status, opened) = http(at, &post("/voice-gateway-route", &route, &auth)).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let opened = json(&opened);
    assert_eq!(opened["purpose"], "voice_gateway");
    assert!(opened["lease"].is_string() && opened["voice_udp_port"].is_u64());
    assert!(opened["expires_in_secs"].as_u64().unwrap() <= 120);
    let lease = serde_json::json!({"lease":opened["lease"]});
    // Renewal accepts nothing but the lease id.
    for extra in ["ttl", "token", "route", "address", "expires_in_secs"] {
        let mut bad = lease.clone();
        bad[extra] = serde_json::json!("x");
        for path in [
            "/voice-gateway-route-status",
            "/renew-voice-gateway-route",
            "/close-voice-gateway-route",
        ] {
            assert!(http(at, &post(path, &bad, &auth)).await.0.contains("400"));
        }
    }
    assert!(http(
        at,
        &post("/renew-voice-gateway-route", &serde_json::json!({}), &auth)
    )
    .await
    .0
    .contains("400"));
    assert!(http(
        at,
        &post(
            "/renew-voice-gateway-route",
            &serde_json::json!({"lease":"short"}),
            &auth
        )
    )
    .await
    .0
    .contains("400"));
    // The refused calls changed nothing.
    let (status, current) = http(at, &post("/voice-gateway-route-status", &lease, &auth)).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let current = json(&current);
    assert_eq!(current["http_url"], opened["http_url"]);
    assert_eq!(current["purpose"], "voice_gateway");
    // A preflight lease and the gateway lease are mutually invisible over the API.
    let (status, preflight) = http(at, &post("/prepared-route", &route, &auth)).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let preflight = json(&preflight);
    assert!(
        preflight.get("purpose").is_none(),
        "the preflight reply is unchanged"
    );
    let prepared = serde_json::json!({"lease":preflight["lease"]});
    for (path, body) in [
        ("/prepared-route-status", &lease),
        ("/close-prepared-route", &lease),
        ("/voice-gateway-route-status", &prepared),
        ("/renew-voice-gateway-route", &prepared),
        ("/close-voice-gateway-route", &prepared),
    ] {
        assert!(http(at, &post(path, body, &auth)).await.0.contains("409"));
    }
    assert!(alive(address(&opened)).await && alive(address(&preflight)).await);
    // Both slots are taken by the two purposes; a third route of either kind is refused.
    assert!(http(at, &post("/voice-gateway-route", &route, &auth))
        .await
        .0
        .contains("429"));
    assert!(http(at, &post("/prepared-route", &route, &auth))
        .await
        .0
        .contains("429"));
    // Renewal returns the exact same resource.
    let (status, renewed) = http(at, &post("/renew-voice-gateway-route", &lease, &auth)).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let renewed = json(&renewed);
    assert_eq!(renewed["lease"], opened["lease"]);
    assert_eq!(renewed["http_url"], opened["http_url"]);
    assert_eq!(renewed["voice_udp_port"], opened["voice_udp_port"]);
    assert_eq!(renewed["purpose"], "voice_gateway");
    // Close is exact and final: no renewal, status or second close afterwards.
    assert_eq!(
        http(at, &post("/close-voice-gateway-route", &lease, &auth))
            .await
            .0,
        "HTTP/1.1 200 OK"
    );
    for path in [
        "/renew-voice-gateway-route",
        "/voice-gateway-route-status",
        "/close-voice-gateway-route",
    ] {
        assert!(http(at, &post(path, &lease, &auth)).await.0.contains("409"));
    }
    assert!(closed(address(&opened)).await);
    assert!(
        alive(address(&preflight)).await,
        "the preflight lease is untouched"
    );
    control.shutdown().await;
    service.abort();
}
