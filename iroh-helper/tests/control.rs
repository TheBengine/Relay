//! The control API a real Relay uses, driven the same way: a helper which hosts
//! a community and a helper for a friend, each with its own secret and folder.
mod common;

use common::*;
use relay_iroh::{control, Network};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const SECRET: &str = "control-secret-of-the-first-helper-0123456789";
const OTHER_SECRET: &str = "control-secret-of-the-second-helper-987654321";

struct Helper {
    control: Arc<control::Control>,
    at: SocketAddr,
    secret: &'static str,
    dir: PathBuf,
}

async fn helper(secret: &'static str, name: &str) -> Helper {
    let dir =
        std::env::temp_dir().join(format!("relay-iroh-control-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let control = control::Control::new(secret, dir.clone(), Network::Loopback).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(control::serve(listener, control.clone()));
    Helper {
        control,
        at,
        secret,
        dir,
    }
}

impl Helper {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.raw(
            method,
            path,
            body.map(|b| b.to_string()),
            &[("Authorization", &format!("Bearer {}", self.secret))],
        )
        .await
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<String>,
        headers: &[(&str, &str)],
    ) -> (u16, Value) {
        let body = body.unwrap_or_default();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(&body);
        let mut socket = TcpStream::connect(self.at).await.unwrap();
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        timeout(Duration::from_secs(20), socket.read_to_end(&mut answer))
            .await
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&answer).to_string();
        let status = text
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let json = serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap_or(""))
            .unwrap_or(Value::Null);
        (status, json)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn preparing_host_identity_is_idle_and_cannot_replace_a_running_host() {
    let host=helper(SECRET,"prepared-identity").await;
    assert_eq!(host.raw("GET","/host-identity",None,&[]).await.0,403);
    let (code,identity)=host.call("GET","/host-identity",None).await;
    assert_eq!(code,200);
    let (_,status)=host.call("GET","/status",None).await;
    assert_eq!(status["phase"],"idle");
    assert!(status.get("endpoint_id").is_none());
    assert_eq!(host.call("GET","/host-identity",None).await.1,identity);
    let upstream=TcpListener::bind("127.0.0.1:0").await.unwrap();
    let binding=json!({"server_id":"11111111-1111-4111-8111-111111111111","lease_id":"22222222-2222-4222-8222-222222222222"});
    let input=json!({"binding":binding,"host":{"peer_port":upstream.local_addr().unwrap().port(),
        "ingress_token":"private-ingress-capability-0123456789","udp_port":4581}});
    let (code,started)=host.call("POST","/host-owned",Some(input.clone())).await;
    assert_eq!(code,200);
    assert_eq!(started["route"]["endpoint_id"],identity["endpoint_id"]);
    assert_eq!(started["route"]["token"],identity["token"]);
    assert_eq!(host.call("GET","/host-identity",None).await.0,409);
    assert_eq!(host.call("POST","/host-owned",Some(input)).await.1,started);
    host.control.shutdown().await;
}

#[tokio::test]
async fn scoped_destination_transport_keeps_legacy_keys_and_survives_rotation_without_regeneration() {
    let host=helper(SECRET,"scoped-destination").await;
    let (_,legacy)=host.call("GET","/host-identity",None).await;
    let legacy_key=std::fs::read(host.dir.join("host.key")).unwrap();
    let legacy_token=std::fs::read(host.dir.join("admission.token")).unwrap();
    let scope="11111111-1111-8111-8111-111111111111";
    let prepare=json!({"scope":scope});
    assert_eq!(host.raw("POST","/prepare-host-identity",Some(prepare.to_string()),&[]).await.0,403);
    let (code,prepared)=host.call("POST","/prepare-host-identity",Some(prepare.clone())).await;
    assert_eq!(code,200);
    assert_ne!(prepared["endpoint_id"],legacy["endpoint_id"]);
    assert_ne!(prepared["token"],legacy["token"]);
    assert_eq!(host.call("GET","/status",None).await.1["phase"],"idle");
    let upstream=TcpListener::bind("127.0.0.1:0").await.unwrap();
    let binding=json!({"server_id":"22222222-2222-4222-8222-222222222222","lease_id":"33333333-3333-4333-8333-333333333333"});
    let mut input=json!({"binding":binding,"host":{"peer_port":upstream.local_addr().unwrap().port(),
        "ingress_token":"private-scoped-ingress-capability-0123456789","udp_port":4581,
        "identity_scope":scope,"expected_endpoint":prepared["endpoint_id"],"expected_admission":prepared["token"]}});
    let (code,started)=host.call("POST","/host-owned",Some(input.clone())).await;
    assert_eq!(code,200);
    assert_eq!(started["route"]["endpoint_id"],prepared["endpoint_id"]);
    // Preparing another transfer while hosting must not touch this endpoint.
    let (_,other)=host.call("POST","/prepare-host-identity",Some(json!({"scope":"44444444-4444-8444-8444-444444444444"}))).await;
    assert_ne!(other["endpoint_id"],prepared["endpoint_id"]);
    assert_eq!(host.call("GET","/status",None).await.1["endpoint_id"],prepared["endpoint_id"]);
    let (code,rotated)=host.call("POST","/rotate-token-owned",Some(json!({"binding":binding}))).await;
    assert_eq!(code,200);
    assert_ne!(rotated["token"],prepared["token"]);
    assert_eq!(host.call("POST","/host-owned",Some(input.clone())).await.0,409,"old setup admission cannot stop a running host");
    assert_eq!(host.call("GET","/status",None).await.1["phase"],"hosting");
    assert_eq!(host.call("POST","/unhost-owned",Some(json!({"binding":binding}))).await.0,200);
    host.control.shutdown().await;
    let reopened=control::Control::new(SECRET,host.dir.clone(),Network::Loopback).unwrap();
    let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at=listener.local_addr().unwrap();
    tokio::spawn(control::serve(listener,reopened.clone()));
    // Reuse this fixture's HTTP client after a real control restart.
    let host=Helper{control:reopened,at,secret:SECRET,dir:host.dir.clone()};
    assert_eq!(host.call("POST","/host-owned",Some(input.clone())).await.0,502,"a rotated standby cannot be silently advertised as the old invitation");
    input["host"].as_object_mut().unwrap().remove("expected_admission");
    let (code,resumed)=host.call("POST","/host-owned",Some(input.clone())).await;
    assert_eq!(code,200);
    assert_eq!(resumed["route"]["endpoint_id"],prepared["endpoint_id"]);
    assert_eq!(resumed["route"]["token"],rotated["token"]);
    assert_eq!(std::fs::read(host.dir.join("host.key")).unwrap(),legacy_key);
    assert_eq!(std::fs::read(host.dir.join("admission.token")).unwrap(),legacy_token);
    host.call("POST","/unhost-owned",Some(json!({"binding":binding}))).await;
    let folder=host.dir.join("prepared-hosts").join(scope);
    std::fs::remove_file(folder.join("host.key")).unwrap();
    assert_eq!(host.call("POST","/prepare-host-identity",Some(prepare.clone())).await.0,409);
    assert_eq!(host.call("POST","/host-owned",Some(input)).await.0,502);
    assert!(!folder.join("host.key").exists());
    std::fs::remove_dir_all(&folder).unwrap();
    assert_eq!(host.call("POST","/prepare-host-identity",Some(prepare)).await.0,409);
    assert!(!folder.exists(),"even the loss of the whole namespace is not a new identity");
    assert_eq!(host.call("POST","/prepare-host-identity",Some(json!({"scope":"../host"}))).await.0,409);
    assert_eq!(host.call("POST","/prepare-host-identity",Some(json!({"scope":scope,"expected_endpoint":other["endpoint_id"]}))).await.0,409);
    host.control.shutdown().await;
}

#[tokio::test]
async fn only_the_apps_secret_opens_the_control_api_and_a_browser_never_does() {
    let h = helper(SECRET, "auth").await;
    assert_eq!(h.call("GET", "/status", None).await.0, 200);
    for (name, headers) in [
        ("no secret", vec![]),
        (
            "wrong secret",
            vec![(
                "Authorization",
                "Bearer not-the-secret-of-this-helper-000000000",
            )],
        ),
        ("wrong scheme", vec![("Authorization", SECRET)]),
        (
            "browser origin",
            vec![
                ("Authorization", &format!("Bearer {SECRET}")[..]),
                ("Origin", "https://evil.example"),
            ],
        ),
        (
            "fetch metadata",
            vec![
                ("Authorization", &format!("Bearer {SECRET}")[..]),
                ("Sec-Fetch-Site", "cross-site"),
            ],
        ),
    ] {
        let (status, _) = h.raw("GET", "/status", None, &headers).await;
        assert_eq!(status, 403, "{name}");
    }
    assert_eq!(h.call("GET", "/nothing", None).await.0, 404);
    assert_eq!(h.call("DELETE", "/status", None).await.0, 404);
    // Bad bodies never start anything.
    assert_eq!(
        h.call(
            "POST",
            "/host",
            Some(json!({"peer_port": 1, "ingress_token": "x", "udp_port": 2, "extra": 1}))
        )
        .await
        .0,
        400
    );
    assert_eq!(
        h.call("POST", "/host", Some(json!({"peer_port": 1})))
            .await
            .0,
        400
    );
    let big = "a".repeat(5000);
    assert_eq!(
        h.call(
            "POST",
            "/route",
            Some(json!({"endpoint_id": big, "token": "x"}))
        )
        .await
        .0,
        400
    );
    for bad in [
        json!({"endpoint_id": "zz", "token": "abcdefghijklmnopqrstuvwxyz0123456789"}),
        json!({"endpoint_id": "a".repeat(64), "token": "short"}),
        json!({"endpoint_id": "a".repeat(64), "token": "abcdefghijklmnopqrstuvwxyz0123456789", "relay": "http://insecure.example"}),
    ] {
        assert_eq!(h.call("POST", "/route", Some(bad)).await.0, 400);
    }
    assert_eq!(h.call("GET", "/status", None).await.1["phase"], "idle");
    assert!(Helper::drop_check(&h));
}

impl Helper {
    fn drop_check(_: &Helper) -> bool {
        true
    }
}

#[tokio::test]
async fn a_host_and_a_friend_helper_carry_chat_end_to_end_and_the_identity_survives_a_restart() {
    let community = community().await;
    let host = helper(SECRET, "host").await;
    let friend = helper(OTHER_SECRET, "friend").await;
    let ingress = "an-ingress-capability-for-the-peer-listener-0123456789";

    let (status, started) = host
        .call("POST", "/host", Some(json!({"peer_port": community.tcp, "ingress_token": ingress, "udp_port": community.udp})))
        .await;
    assert_eq!(status, 200, "{started}");
    let id = started["endpoint_id"].as_str().unwrap().to_owned();
    let token = started["token"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 64);
    assert_eq!(token.len(), 43);
    assert_eq!(
        host.call("GET", "/status", None).await.1["phase"],
        "hosting"
    );
    // A helper hosts one community.
    assert_eq!(host.call("POST", "/host", Some(json!({"peer_port": community.tcp, "ingress_token": ingress, "udp_port": community.udp}))).await.0, 409);

    let route = json!({"endpoint_id": id, "token": token, "direct": started["direct"]});
    let (status, probed) = friend.call("POST", "/probe", Some(route.clone())).await;
    assert_eq!(status, 200, "{probed}");
    assert!(probed["rtt_ms"].as_f64().unwrap() >= 0.0);
    let (status, opened) = friend.call("POST", "/route", Some(route.clone())).await;
    assert_eq!(status, 200, "{opened}");
    let http_url = opened["http_url"].as_str().unwrap();
    assert!(http_url.starts_with("http://127.0.0.1:"));
    let at: SocketAddr = http_url.trim_start_matches("http://").parse().unwrap();
    let (status, _) = common::http(at, &common::get("/api/health", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(opened["voice_udp_port"].as_u64().unwrap() > 0);
    let (_, path) = friend
        .call("POST", "/path", Some(json!({"endpoint_id": id})))
        .await;
    assert_eq!(path["kind"], "direct", "{path}");

    // Replacing the token locks out a friend who still holds the old one.
    let (status, rotated) = host.call("POST", "/rotate-token", None).await;
    assert_eq!(status, 200);
    let fresh = rotated["token"].as_str().unwrap().to_owned();
    assert_ne!(fresh, token);
    let stranger = helper("control-secret-of-a-third-helper-1122334455", "stranger").await;
    let old_route = json!({"endpoint_id": id, "token": token, "direct": started["direct"]});
    assert_eq!(
        stranger.call("POST", "/route", Some(old_route)).await.0,
        502
    );
    let new_route = json!({"endpoint_id": id, "token": fresh, "direct": started["direct"]});
    let (code,stranger_route)=stranger.call("POST","/route",Some(new_route)).await;
    assert_eq!(code,200);

    // The friend's route is closed on request.
    assert_eq!(
        friend
            .call("POST", "/close-route", Some(json!({"endpoint_id": id})))
            .await
            .0,
        200
    );
    assert_eq!(
        friend
            .call("POST", "/path", Some(json!({"endpoint_id": id})))
            .await
            .1["kind"],
        "unknown"
    );

    // Stopping and starting again keeps the host's identity and its current token.
    assert_eq!(host.call("POST", "/unhost", None).await.0, 200);
    assert_eq!(host.call("POST", "/unhost", None).await.0, 409);
    let (status, again) = host
        .call("POST", "/host", Some(json!({"peer_port": community.tcp, "ingress_token": ingress, "udp_port": community.udp})))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        again["endpoint_id"],
        id.as_str(),
        "an invitation keeps working after a restart"
    );
    assert_eq!(again["token"], fresh.as_str());
    let (code,reconnected)=stranger.call("POST","/route",Some(json!({"endpoint_id":id,"token":fresh,"direct":again["direct"]}))).await;
    assert_eq!(code,200,"{reconnected}");
    assert_eq!(reconnected["http_url"],stranger_route["http_url"],"refreshing addresses keeps the existing gateway");
    let at:SocketAddr=reconnected["http_url"].as_str().unwrap().trim_start_matches("http://").parse().unwrap();
    assert_eq!(common::http(at,&common::get("/api/health","")).await.0,"HTTP/1.1 200 OK");

    assert_eq!(host.call("POST", "/shutdown", None).await.0, 200);
    timeout(Duration::from_secs(2), host.control.stop.notified())
        .await
        .expect("shutdown is signalled");
    host.control.shutdown().await;
    friend.control.shutdown().await;
    stranger.control.shutdown().await;
}

#[tokio::test]
async fn a_real_helper_refuses_direct_addresses_which_only_the_test_mode_accepts() {
    let dir = std::env::temp_dir().join(format!("relay-iroh-control-real-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let control = control::Control::new(SECRET, dir.clone(), Network::Internet).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(control::serve(listener, control.clone()));
    let h = Helper {
        control,
        at,
        secret: SECRET,
        dir,
    };
    let attempt = json!({"endpoint_id": "a".repeat(64), "token": "abcdefghijklmnopqrstuvwxyz0123456789", "direct": ["127.0.0.1:9"]});
    assert_eq!(h.call("POST", "/probe", Some(attempt)).await.0, 400);
}

#[tokio::test]
async fn owned_host_retries_keep_admitted_traffic_and_stale_changes_cannot_touch_a_new_lease() {
    use futures_util::{SinkExt, StreamExt};
    let community = community().await;
    let host = helper(SECRET, "owned-host").await;
    let friend = helper(OTHER_SECRET, "owned-friend").await;
    let binding = json!({"server_id":"4be44aef-8c98-4db6-a9d5-81c2eb311330","lease_id":"90eb6d22-1fda-4ff0-b0c5-0aa5dd970a30"});
    let input = json!({"binding":binding,"host":{"peer_port":community.tcp,"udp_port":community.udp,"ingress_token":INGRESS}});
    let (code, first) = host.call("POST", "/host-owned", Some(input.clone())).await;
    assert_eq!(code, 200, "{first}");
    let route = &first["route"];
    let invite =
        json!({"endpoint_id":route["endpoint_id"],"token":route["token"],"direct":route["direct"]});
    let (code, opened) = friend.call("POST", "/route", Some(invite)).await;
    assert_eq!(code, 200, "{opened}");
    let at: SocketAddr = opened["http_url"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    let (mut ongoing, _) = tokio_tungstenite::connect_async(format!(
        "{}/api/ws",
        opened["http_url"].as_str().unwrap().replacen("http://", "ws://", 1)
    )).await.unwrap();
    async fn stream_continues(
        socket:&mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        sequence:u8,
    ) {
        let sent=tokio_tungstenite::tungstenite::Message::Text(format!("same-admitted-stream-{sequence}").into());
        socket.send(sent.clone()).await.unwrap();
        assert_eq!(timeout(Duration::from_secs(2),socket.next()).await.unwrap().unwrap().unwrap(),sent);
    }
    let voice = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    voice
        .connect((
            std::net::Ipv4Addr::LOCALHOST,
            opened["voice_udp_port"].as_u64().unwrap() as u16,
        ))
        .await
        .unwrap();
    async fn still_connected(at: SocketAddr, voice: &tokio::net::UdpSocket, sequence: u8) {
        assert_eq!(
            common::http(at, &common::get("/api/health", "")).await.0,
            "HTTP/1.1 200 OK"
        );
        let frame = [sequence; 120];
        voice.send(&frame).await.unwrap();
        let mut got = [0u8; 1500];
        let count = timeout(Duration::from_secs(2), voice.recv(&mut got))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&got[..count], &frame);
    }
    still_connected(at, &voice, 1).await;
    stream_continues(&mut ongoing,1).await;
    // This also models a lost startup reply. Repeating must reuse the endpoint
    // and leave the already admitted QUIC connection and UDP allocation alive.
    assert_eq!(
        host.call("POST", "/host-owned", Some(input.clone())).await,
        (200, first.clone())
    );
    still_connected(at, &voice, 2).await;
    stream_continues(&mut ongoing,2).await;
    let mut changed = input.clone();
    changed["host"]["ingress_token"] = json!("a-new-backend-ingress-capability-0123456789");
    assert_eq!(host.call("POST", "/host-owned", Some(changed)).await.0, 409);
    let other = json!({"server_id":"77cf35de-9e8d-4231-90de-75f1970e66f8","lease_id":"bd72c2a0-1813-4084-b363-076b65e4d923"});
    for endpoint in ["/unhost-owned", "/rotate-token-owned"] {
        assert_eq!(
            host.call("POST", endpoint, Some(json!({"binding":other})))
                .await
                .0,
            409
        );
    }
    assert_eq!(host.call("POST", "/unhost", None).await.0, 409);
    assert_eq!(host.call("POST", "/rotate-token", None).await.0, 409);
    still_connected(at, &voice, 3).await;
    stream_continues(&mut ongoing,3).await;
    let (code, rotated) = host
        .call(
            "POST",
            "/rotate-token-owned",
            Some(json!({"binding":binding})),
        )
        .await;
    assert_eq!(code, 200);
    assert_ne!(rotated["token"], first["route"]["token"]);
    let again = host.call("POST", "/host-owned", Some(input.clone())).await;
    assert_eq!(again.0, 200);
    assert_eq!(again.1["route"]["token"], rotated["token"]);
    still_connected(at, &voice, 4).await;
    stream_continues(&mut ongoing,4).await;
    assert_eq!(
        host.call("POST", "/unhost-owned", Some(json!({"binding":binding})))
            .await
            .0,
        200
    );
    assert_eq!(
        host.call("POST", "/unhost-owned", Some(json!({"binding":binding})))
            .await
            .0,
        200
    );
    let next = json!({"binding":other,"host":input["host"]});
    assert_eq!(
        host.call("POST", "/host-owned", Some(next.clone())).await.0,
        200
    );
    // A delayed old stop/rotation/start after replacement is fenced by lease.
    assert_eq!(
        host.call("POST", "/unhost-owned", Some(json!({"binding":binding})))
            .await
            .0,
        409
    );
    assert_eq!(
        host.call(
            "POST",
            "/rotate-token-owned",
            Some(json!({"binding":binding}))
        )
        .await
        .0,
        409
    );
    assert_eq!(host.call("POST", "/host-owned", Some(input)).await.0, 409);
    assert_eq!(host.call("POST", "/host-owned", Some(next)).await.0, 200);
    host.control.shutdown().await;
    friend.control.shutdown().await;
}
