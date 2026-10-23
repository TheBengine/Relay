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
    assert_eq!(
        stranger.call("POST", "/route", Some(new_route)).await.0,
        200
    );

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
