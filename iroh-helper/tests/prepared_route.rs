//! Owned in-process loopback host and synthetic bytes; no native source/target
//! server pair, application profile, physical device, relay or external network.
mod common;
use common::*;
use relay_iroh::{
    client::{Client, RouteSpec},
    control,
    host::{HostConfig, HostService},
    prepared_route::{Error, Store},
    token, Network,
};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    time::{sleep, timeout},
};
const SECRET: &str = "prepared-control-fixture-secret-for-local-acceptance-0123456789";
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
#[tokio::test]
async fn preflight_with_a_new_admission_keeps_the_active_connection_and_voice_socket() {
    let old = token::new_token().unwrap();
    let new = token::new_token().unwrap();
    let host = Host::new(&old).await;
    let client = Client::new(Network::Loopback).await.unwrap();
    let active = client.route(host.spec(&old)).await.unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(alive(active.http).await && voice(&socket, active.voice_udp_port, 1).await);
    let connection = client
        .connection_id(&host.service.endpoint_id())
        .await
        .unwrap();
    host.service.set_token(&new).unwrap();
    let prepared = Store::new();
    let isolated = prepared
        .open(&Network::Loopback, host.spec(&new))
        .await
        .unwrap();
    assert_ne!(isolated.route.http, active.http);
    assert_ne!(isolated.route.voice_udp_port, active.voice_udp_port);
    assert!(alive(isolated.route.http).await);
    assert_eq!(client.route(host.spec(&old)).await.unwrap(), active);
    assert_eq!(
        client.connection_id(&host.service.endpoint_id()).await,
        Some(connection)
    );
    assert!(voice(&socket, active.voice_udp_port, 2).await);
    // A refused independent admission closes only its own client.
    assert!(matches!(
        prepared
            .open(&Network::Loopback, host.spec(&token::new_token().unwrap()))
            .await,
        Err(Error::Failed)
    ));
    assert!(alive(active.http).await && voice(&socket, active.voice_udp_port, 3).await);
    prepared.close(&isolated.id).await.unwrap();
    // A closed Windows port may leave connect pending before reporting refusal.
    // Either refusal or no connection within the deadline is acceptable; acceptance is not.
    assert!(timeout(
        Duration::from_millis(300),
        TcpStream::connect(isolated.route.http)
    )
    .await.map_or(true, |result| result.is_err()));
    assert!(alive(active.http).await);
    assert_eq!(
        client.connection_id(&host.service.endpoint_id()).await,
        Some(connection)
    );
    client.close().await;
    prepared.shutdown().await;
}
#[tokio::test]
async fn leases_are_bounded_exact_expiring_and_closed_with_the_store() {
    let admission = token::new_token().unwrap();
    let host = Host::new(&admission).await;
    let store = Store::with_lifetime(Duration::from_millis(600));
    let a = store
        .open(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    let b = store
        .open(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    assert!(matches!(
        store.open(&Network::Loopback, host.spec(&admission)).await,
        Err(Error::Busy)
    ));
    assert!(matches!(
        store.close(&token::new_token().unwrap()).await,
        Err(Error::Missing)
    ));
    assert!(store.status(&a.id).is_ok() && store.status(&b.id).is_ok());
    store.close(&a.id).await.unwrap();
    assert!(matches!(store.status(&a.id), Err(Error::Missing)));
    assert!(alive(b.route.http).await);
    sleep(Duration::from_millis(700)).await;
    assert!(matches!(store.status(&b.id), Err(Error::Missing)));
    assert!(
        timeout(Duration::from_millis(300), TcpStream::connect(b.route.http))
            .await.map_or(true, |result| result.is_err())
    );
    let fresh = store
        .open(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    store.shutdown().await;
    assert!(matches!(store.status(&fresh.id), Err(Error::Missing)));
    assert!(timeout(
        Duration::from_millis(300),
        TcpStream::connect(fresh.route.http)
    )
    .await.map_or(true, |result| result.is_err()));
    assert!(matches!(
        store.open(&Network::Loopback, host.spec(&admission)).await,
        Err(Error::Failed)
    ));
}
fn post(path: &str, value: &serde_json::Value, extra: &str) -> String {
    let body = value.to_string();
    format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
#[tokio::test]
async fn the_private_control_api_scopes_lease_lookup_and_release_without_touching_normal_routes() {
    let dir = Scratch::new("prepared-control");
    let control = control::Control::new(SECRET, dir.path.clone(), Network::Loopback).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = tokio::spawn(control::serve(listener, control.clone()));
    let admission = token::new_token().unwrap();
    let host = Host::new(&admission).await;
    let auth = format!("Authorization: Bearer {SECRET}\r\n");
    let spec = host.spec(&admission);
    let route = serde_json::json!({"endpoint_id":spec.id.to_string(),"token":admission,"direct":spec.direct.iter().map(ToString::to_string).collect::<Vec<_>>()});
    assert!(http(address, &post("/prepared-route", &route, ""))
        .await
        .0
        .contains("403"));
    assert!(http(
        address,
        &post(
            "/prepared-route",
            &route,
            &format!("{auth}Origin: https://untrusted.invalid\r\n")
        )
    )
    .await
    .0
    .contains("403"));
    assert!(http(address, &get("/prepared-route", &auth))
        .await
        .0
        .contains("404"));
    let (status, normal) = http(address, &post("/route", &route, &auth)).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let normal: serde_json::Value = serde_json::from_str(&normal).unwrap();
    let (status, opened) = http(address, &post("/prepared-route", &route, &auth)).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let opened: serde_json::Value = serde_json::from_str(&opened).unwrap();
    assert_ne!(opened["http_url"], normal["http_url"]);
    let lease = serde_json::json!({"lease":opened["lease"]});
    let (status, current) = http(address, &post("/prepared-route-status", &lease, &auth)).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let current: serde_json::Value = serde_json::from_str(&current).unwrap();
    assert_eq!(current["http_url"], opened["http_url"]);
    assert!(http(
        address,
        &post(
            "/close-prepared-route",
            &serde_json::json!({"lease":token::new_token().unwrap()}),
            &auth
        )
    )
    .await
    .0
    .contains("409"));
    assert_eq!(
        http(address, &post("/close-prepared-route", &lease, &auth))
            .await
            .0,
        "HTTP/1.1 200 OK"
    );
    assert!(
        http(address, &post("/prepared-route-status", &lease, &auth))
            .await
            .0
            .contains("409")
    );
    let normal: SocketAddr = normal["http_url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(normal).await);
    control.shutdown().await;
    service.abort();
}
#[tokio::test]
async fn cancelling_an_opening_route_releases_its_own_slot_and_sockets() {
    let admission = token::new_token().unwrap();
    let host = Host::new(&admission).await;
    let store = Store::new();
    let silence = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let stalled = RouteSpec {
        id: iroh::SecretKey::from_bytes(&token::random_bytes::<32>().unwrap()).public(),
        relay: None,
        token: admission.clone(),
        direct: vec![silence.local_addr().unwrap()],
    };
    assert!(timeout(
        Duration::from_millis(30),
        store.open(&Network::Loopback, stalled)
    )
    .await
    .is_err());
    sleep(Duration::from_millis(1150)).await;
    let a = store
        .open(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    let b = store
        .open(&Network::Loopback, host.spec(&admission))
        .await
        .unwrap();
    assert!(alive(a.route.http).await && alive(b.route.http).await);
    store.shutdown().await;
}
