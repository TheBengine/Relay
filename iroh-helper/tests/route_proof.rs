//! The route-proof exchange against a real host over iroh's loopback mode: a stand-in
//! community which returns a proof, and the helper's control API in front of the
//! exchange. Like the other tests here this proves the bridge and the rules, not
//! real networks, relays or NAT.
mod common;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use bytes::Bytes;
use common::*;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use relay_iroh::{
    client::{Client, RouteSpec},
    control,
    host::{HostConfig, HostService},
    route_proof::{ProofError, Prover, MAX_PROOF_BYTES, PATH},
    token, Network,
};
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, time::sleep};

const PROOF: u8 = 0;
const NOT_FOUND: u8 = 1;
const ARRAY: u8 = 2;
const GARBAGE: u8 = 3;
const HUGE: u8 = 4;
const SLOW: u8 = 5;
const REDIRECT: u8 = 6;
const SECRET: &str = "control-secret-for-the-route-proof-helper-0123456789";

#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A stand-in community which records every request and answers by mode.
struct Mini {
    port: u16,
    mode: Arc<AtomicU8>,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

async fn mini() -> Mini {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mode = Arc::new(AtomicU8::new(PROOF));
    let seen: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let (accept_mode, accept_seen) = (mode.clone(), seen.clone());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (mode, seen) = (accept_mode.clone(), accept_seen.clone());
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let (mode, seen) = (mode.clone(), seen.clone());
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = body
                            .collect()
                            .await
                            .map(|b| b.to_bytes())
                            .unwrap_or_default();
                        let body = String::from_utf8_lossy(&body).into_owned();
                        seen.lock().unwrap().push(Recorded {
                            method: parts.method.to_string(),
                            path: parts.uri.path().to_owned(),
                            headers: parts
                                .headers
                                .iter()
                                .map(|(k, v)| {
                                    (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned())
                                })
                                .collect(),
                            body: body.clone(),
                        });
                        let challenge = serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|v| v["challenge"].as_str().map(str::to_owned))
                            .unwrap_or_default();
                        let (status, text): (u16, String) = match mode.load(Ordering::SeqCst) {
                            NOT_FOUND => (404, "{}".to_owned()),
                            ARRAY => (200, "[1,2]".to_owned()),
                            GARBAGE => (200, "this is not json".to_owned()),
                            HUGE => (
                                200,
                                format!("{{\"pad\":\"{}\"}}", "a".repeat(MAX_PROOF_BYTES + 4096)),
                            ),
                            SLOW => {
                                sleep(Duration::from_secs(30)).await;
                                (200, "{}".to_owned())
                            }
                            REDIRECT => (302, String::new()),
                            _ => (
                                200,
                                format!("{{\"proof\":\"fixture\",\"challenge\":\"{challenge}\"}}"),
                            ),
                        };
                        let mut response = Response::new(Full::new(Bytes::from(text)));
                        *response.status_mut() = status.try_into().unwrap();
                        if status == 302 {
                            response.headers_mut().insert(
                                "location",
                                "http://127.0.0.1:1/elsewhere".parse().unwrap(),
                            );
                        }
                        Ok::<_, Infallible>(response)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    Mini { port, mode, seen }
}

struct World {
    mini: Mini,
    host: HostService,
    token: String,
    prover: Prover,
}

async fn world_with(prover: Prover) -> World {
    let mini = mini().await;
    let udp = community().await.udp;
    let token = token::new_token().unwrap();
    let key = iroh::SecretKey::from_bytes(&token::random_bytes::<32>().unwrap());
    let host = HostService::start(HostConfig {
        secret_key: key,
        token: token.clone(),
        peer_port: mini.port,
        ingress_token: INGRESS.into(),
        udp_port: udp,
        media_udp_port: 0,
        network: Network::Loopback,
    })
    .await
    .unwrap();
    World {
        mini,
        host,
        token,
        prover,
    }
}

async fn world() -> World {
    world_with(Prover::new()).await
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
    fn mode(&self, mode: u8) {
        self.mini.mode.store(mode, Ordering::SeqCst);
    }
    fn seen(&self) -> Vec<Recorded> {
        self.mini.seen.lock().unwrap().clone()
    }
    async fn prove(&self, token: &str, challenge: &str) -> Result<Bytes, ProofError> {
        self.prover
            .prove(&Network::Loopback, self.spec(token), challenge)
            .await
    }
}

fn challenge(seed: u8) -> String {
    URL_SAFE_NO_PAD.encode([seed; 32])
}

#[tokio::test]
async fn one_fixed_post_goes_through_the_normal_admission_and_the_proof_comes_back_unchanged() {
    let w = world().await;
    let c = challenge(1);
    let proof = w.prove(&w.token, &c).await.unwrap();
    assert_eq!(
        proof,
        Bytes::from(format!("{{\"proof\":\"fixture\",\"challenge\":\"{c}\"}}")),
        "the host's bytes, not re-encoded and not judged"
    );
    let seen = w.seen();
    assert_eq!(seen.len(), 1, "exactly one request reached the community");
    let request = &seen[0];
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("POST", PATH)
    );
    assert_eq!(request.body, format!("{{\"challenge\":\"{c}\"}}"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    // The host adds its own private-ingress headers; the exchange adds no credential.
    assert_eq!(request.header("x-relay-network-ingress"), Some(INGRESS));
    assert_eq!(
        request.header("host"),
        Some(format!("127.0.0.1:{}", w.mini.port).as_str())
    );
    for absent in [
        "authorization",
        "cookie",
        "origin",
        "upgrade",
        "x-forwarded-for",
        "forwarded",
        "x-relay-local-control",
    ] {
        assert!(request.header(absent).is_none(), "{absent}");
    }
}

#[tokio::test]
async fn refusals_odd_answers_and_a_dead_host_all_fail_the_same_way_and_stay_bounded() {
    let w = world_with(Prover::with_deadline(Duration::from_secs(2))).await;
    let c = challenge(2);
    for (name, mode) in [
        ("not 200", NOT_FOUND),
        ("not an object", ARRAY),
        ("not json", GARBAGE),
        ("over the size limit", HUGE),
        ("a redirect", REDIRECT),
    ] {
        w.mode(mode);
        assert_eq!(
            w.prove(&w.token, &c).await,
            Err(ProofError::Failed),
            "{name}"
        );
    }
    // The redirect was not followed: every attempt made exactly one request.
    assert_eq!(w.seen().len(), 5);
    assert!(w.seen().iter().all(|r| r.path == PATH));
    // A wrong token is refused at admission, and nothing reaches the community.
    w.mode(PROOF);
    let before = w.seen().len();
    assert_eq!(
        w.prove("a-wrong-token-a-wrong-token-a-wrong-token-0", &c)
            .await,
        Err(ProofError::Failed)
    );
    assert_eq!(w.seen().len(), before);
    // A host which never answers is cut off by the deadline.
    w.mode(SLOW);
    let started = Instant::now();
    assert_eq!(w.prove(&w.token, &c).await, Err(ProofError::Failed));
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    // An address nobody answers at.
    w.mode(PROOF);
    let mut nobody = w.spec(&w.token);
    nobody.id = iroh::SecretKey::from_bytes(&[8u8; 32]).public();
    nobody.direct = vec!["127.0.0.1:9".parse().unwrap()];
    let started = Instant::now();
    assert_eq!(
        w.prover.prove(&Network::Loopback, nobody, &c).await,
        Err(ProofError::Failed)
    );
    assert!(started.elapsed() < Duration::from_secs(6));
}

#[tokio::test]
async fn at_most_two_exchanges_run_at_once_and_a_third_is_refused_not_queued() {
    let w = Arc::new(world_with(Prover::with_deadline(Duration::from_secs(2))).await);
    w.mode(SLOW);
    let c = challenge(3);
    let running: Vec<_> = (0..2)
        .map(|_| {
            let (w, c) = (w.clone(), c.clone());
            tokio::spawn(async move { w.prove(&w.token, &c).await })
        })
        .collect();
    sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    assert_eq!(w.prove(&w.token, &c).await, Err(ProofError::Busy));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "refused at once"
    );
    for task in running {
        assert_eq!(task.await.unwrap(), Err(ProofError::Failed));
    }
    // The slots came back.
    w.mode(PROOF);
    assert!(w.prove(&w.token, &c).await.is_ok());
}

#[tokio::test]
async fn a_proof_with_a_newer_admission_does_not_touch_the_route_already_in_use() {
    let w = world().await;
    let client = Client::new(Network::Loopback).await.unwrap();
    let old = w.token.clone();
    // The person's normal route, admitted with the token they hold now.
    let route = client.route(w.spec(&old)).await.unwrap();
    let (status, _) = http(route.http, &get("/api/health", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let normal_peer = w
        .seen()
        .last()
        .and_then(|r| r.header("x-relay-network-peer").map(str::to_owned))
        .unwrap();
    // The host moves on to a new admission for the very same key.
    let fresh = token::new_token().unwrap();
    w.host.set_token(&fresh).unwrap();
    let c = challenge(4);
    assert!(w.prove(&fresh, &c).await.is_ok());
    // The proof came from its own, one-off identity, not the person's route.
    let proof_peer = w
        .seen()
        .last()
        .and_then(|r| r.header("x-relay-network-peer").map(str::to_owned))
        .unwrap();
    assert_ne!(proof_peer, normal_peer);
    // The normal route is exactly as it was: same local address, still carrying traffic.
    let (status, _) = http(route.http, &get("/api/health", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(client.route(w.spec(&old)).await.unwrap(), route);
    // Even a refused proof (the old token no longer admits) leaves it alone.
    assert_eq!(w.prove(&old, &c).await, Err(ProofError::Failed));
    let (status, _) = http(route.http, &get("/api/health", "")).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(client.route(w.spec(&old)).await.unwrap(), route);
}

// ---- the control API in front of it ----

struct Helper {
    at: SocketAddr,
}

async fn helper() -> (Helper, Scratch) {
    let dir = Scratch::new("route-proof");
    let control = control::Control::new(SECRET, dir.path.clone(), Network::Loopback).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(control::serve(listener, control));
    (Helper { at }, dir)
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

#[tokio::test]
async fn the_control_route_needs_the_private_secret_and_a_valid_small_request() {
    let w = world().await;
    let (helper, _dir) = helper().await;
    let c = challenge(5);
    let route = serde_json::json!({
        "endpoint_id": w.host.endpoint_id().to_string(),
        "token": w.token,
        "direct": w.host.addr().ip_addrs().map(|a| a.to_string()).collect::<Vec<_>>(),
    });
    let good = serde_json::json!({"route": route, "challenge": c}).to_string();
    // The right secret: the host's proof, unchanged.
    let (status, body) = helper.post("/route-proof", Some(SECRET), &good, "").await;
    assert!(status.contains("200"), "{status} {body}");
    assert_eq!(
        body,
        format!("{{\"proof\":\"fixture\",\"challenge\":\"{c}\"}}")
    );
    // Not the secret, or a browser: refused before anything is read or opened.
    let before = w.seen().len();
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
        let (status, _) = helper.post("/route-proof", bearer, &good, extra).await;
        assert!(status.contains("403"), "{name}: {status}");
    }
    assert_eq!(w.seen().len(), before, "nothing reached the community");
    // Bad requests are refused as such, with nothing opened.
    let big =
        serde_json::json!({"route": route, "challenge": c, "pad": "a".repeat(5000)}).to_string();
    let no_challenge = serde_json::json!({"route": route}).to_string();
    let bad_challenge = serde_json::json!({"route": route, "challenge": "short"}).to_string();
    let loose_challenge =
        serde_json::json!({"route": route, "challenge": format!("{c}=")}).to_string();
    let unknown =
        serde_json::json!({"route": route, "challenge": c, "path": "/api/other"}).to_string();
    let mut short_token = route.clone();
    short_token["token"] = "short".into();
    let bad_route = serde_json::json!({"route": short_token, "challenge": c}).to_string();
    let mut http_relay = route.clone();
    http_relay["relay"] = "http://insecure.example".into();
    let insecure = serde_json::json!({"route": http_relay, "challenge": c}).to_string();
    for (name, body) in [
        ("over 4096 bytes", big.as_str()),
        ("no challenge", no_challenge.as_str()),
        ("short challenge", bad_challenge.as_str()),
        ("padded challenge", loose_challenge.as_str()),
        ("unknown field", unknown.as_str()),
        ("short token", bad_route.as_str()),
        ("insecure relay", insecure.as_str()),
        ("not json", "nope"),
    ] {
        let (status, reply) = helper.post("/route-proof", Some(SECRET), body, "").await;
        assert!(status.contains("400"), "{name}: {status}");
        assert!(
            !reply.contains(&w.token) && !reply.contains(&c),
            "{name}: nothing echoed"
        );
    }
    assert_eq!(w.seen().len(), before);
    // A host which says no is a generic gateway error, and says nothing about why.
    w.mode(NOT_FOUND);
    let (status, reply) = helper.post("/route-proof", Some(SECRET), &good, "").await;
    assert!(status.contains("502"), "{status}");
    assert!(!reply.contains(&w.token) && !reply.contains(&c));
    // The route is a POST only; the ordinary /route API is untouched by all of this.
    let (status, _) = http(
        helper.at,
        &format!("GET /route-proof HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {SECRET}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(status.contains("404"), "{status}");
}
