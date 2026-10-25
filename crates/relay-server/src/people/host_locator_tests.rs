//! The host locator directory: signed, opaque records found by a capability ID.
//! Hosts are modelled with real keys and a real signed handover chain, so these
//! checks exercise the same verification a reader's client performs.
use super::*;
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use relay_protocol::{
    handover::{self as model, ActivationGrant, HostIdentity, RoutePin},
    host_locator::{self as locator, Record},
};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const LOOKUP: &str = locator::LOOKUP_PATH;
const PUBLISH: &str = locator::PUBLISH_PATH;

/// An independent copy of the key-derived host identity, so the server's own is checked.
fn identity(key: &SigningKey) -> HostIdentity {
    let mut hash = Sha256::new();
    hash.update(b"relay-host-identity-v1\0");
    hash.update(key.verifying_key().as_bytes());
    let digest = hash.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    HostIdentity {
        host_id: uuid::Uuid::from_bytes(bytes).to_string(),
        public_key: B64.encode(key.verifying_key().as_bytes()),
    }
}

struct World {
    keys: Vec<SigningKey>,
    server: String,
}

impl World {
    fn new() -> Self {
        Self {
            keys: (0..10)
                .map(|i| SigningKey::from_bytes(&[i as u8 + 21; 32]))
                .collect(),
            server: uuid::Uuid::new_v4().to_string(),
        }
    }
    fn pin(&self, key: usize, term: u64) -> RoutePin {
        RoutePin {
            server_id: self.server.clone(),
            host: identity(&self.keys[key]),
            term,
        }
    }
    /// A complete signed chain along `path` (key indexes); the key at position `i`
    /// holds term `i + 1`. Later edges commit to the ones before them, the first
    /// is a legacy edge without that field.
    fn history(&self, path: &[usize]) -> Vec<ActivationGrant> {
        let mut out: Vec<ActivationGrant> = vec![];
        for i in 0..path.len() - 1 {
            let body = model::ReceiptBody {
                server_id: self.server.clone(),
                source_host_id: uuid::Uuid::new_v4().to_string(),
                term: i as u64 + 1,
                preparation_id: uuid::Uuid::new_v4().to_string(),
                export_id: uuid::Uuid::new_v4().to_string(),
                target_identity: identity(&self.keys[path[i + 1]]),
                target_url: "https://target.example/".into(),
                database_sha256: "a".repeat(64),
                manifest_sha256: "b".repeat(64),
                database_bytes: 45,
                challenge: B64.encode([4; 32]),
            };
            let receipt = model::Receipt {
                signature: B64.encode(
                    self.keys[path[i + 1]]
                        .sign(&model::receipt_message(&body))
                        .to_bytes(),
                ),
                body,
            };
            let mut grant = ActivationGrant {
                receipt,
                source_identity: identity(&self.keys[path[i]]),
                next_term: i as u64 + 2,
                history_sha256: (i > 0)
                    .then(|| format!("{:x}", Sha256::digest(model::history_message(&out)))),
                signature: String::new(),
            };
            grant.signature = B64.encode(
                self.keys[path[i]]
                    .sign(&model::grant_message(&grant))
                    .to_bytes(),
            );
            out.push(grant);
        }
        out
    }
    /// A signed record whose current host is `path[history.len()]`, anchored at `path[anchor_at]`.
    fn record(
        &self,
        path: &[usize],
        history: &[ActivationGrant],
        anchor_at: usize,
        id: &str,
        issued_ms: u64,
        seed: u8,
    ) -> Record {
        let at = history.len();
        let mut record = Record {
            id: id.into(),
            anchor: self.pin(path[anchor_at], anchor_at as u64 + 1),
            current: self.pin(path[at], at as u64 + 1),
            history: history.to_vec(),
            issued_ms,
            expires_ms: issued_ms + 3_600_000,
            nonce: B64.encode([3u8; 12]),
            ciphertext: B64.encode(vec![seed; 40]),
            signature: String::new(),
        };
        self.sign(&mut record, path[at]);
        record
    }
    fn sign(&self, record: &mut Record, key: usize) {
        record.signature = B64.encode(
            self.keys[key]
                .sign(&locator::record_message(record))
                .to_bytes(),
        );
    }
}

fn id_of(n: u32) -> String {
    format!("{:064x}", n as u128 + 1)
}

fn recently(offset_ms: u64) -> u64 {
    now_ms() - offset_ms
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

async fn publish(app: &Router, record: &Record) -> StatusCode {
    send(
        app,
        "POST",
        PUBLISH,
        serde_json::to_vec(record).unwrap(),
        &[],
    )
    .await
    .0
}

async fn lookup(app: &Router, id: &str) -> (StatusCode, Vec<u8>) {
    send(
        app,
        "POST",
        LOOKUP,
        serde_json::to_vec(&json!({"id": id})).unwrap(),
        &[],
    )
    .await
}

fn friends() -> (tempfile::TempDir, AppState, Router) {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    let app = router(state.clone());
    (data, state, app)
}

fn rows(state: &AppState) -> i64 {
    lock(&state.0.db)
        .query_row("SELECT COUNT(*) FROM host_locator_records", [], |r| {
            r.get(0)
        })
        .unwrap()
}

fn text(record: &Record) -> String {
    serde_json::to_string(record).unwrap()
}

#[tokio::test]
async fn a_published_route_is_found_by_its_capability_and_survives_reopening() {
    let (data, state, app) = friends();
    let w = World::new();
    let path = [0, 1, 2];
    let history = w.history(&path);
    let id = id_of(0xabc);
    // Anchored at the first host, current at the third: a complete signed chain.
    let record = w.record(&path, &history, 0, &id, recently(5_000), 7);
    // No bearer of any kind is sent.
    assert_eq!(publish(&app, &record).await, StatusCode::NO_CONTENT);
    let (status, body) = lookup(&app, &id).await;
    assert_eq!(status, StatusCode::OK);
    let found: Record = serde_json::from_slice(&body).unwrap();
    assert_eq!(text(&found), text(&record));
    // Unknown, malformed and upper-case IDs all answer the same "not found".
    let missing = lookup(&app, &id_of(2)).await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    for odd in [
        id.to_uppercase(),
        id[..63].to_string(),
        "x".repeat(64),
        String::new(),
    ] {
        assert_eq!(lookup(&app, &odd).await, missing);
    }
    assert_eq!(
        send(&app, "POST", LOOKUP, b"not json".to_vec(), &[]).await,
        missing
    );
    // The stored row holds the opaque record only: exactly the published fields,
    // no extra secret column, and nothing about the destination in the clear.
    let stored: String = lock(&state.0.db)
        .query_row(
            "SELECT record FROM host_locator_records WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    let keys: Vec<String> = serde_json::from_str::<serde_json::Map<String, Value>>(&stored)
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let mut expected = vec![
        "id",
        "anchor",
        "current",
        "history",
        "issued_ms",
        "expires_ms",
        "nonce",
        "ciphertext",
        "signature",
    ];
    expected.sort();
    let mut keys = keys;
    keys.sort();
    assert_eq!(keys, expected);
    assert!(!stored.contains("iroh") && !stored.contains("token"));
    // A restart keeps it.
    drop(app);
    drop(state);
    let state = AppState::open_with_config(data.path(), "Relay friends", 0, config()).unwrap();
    let app = router(state);
    let (status, body) = lookup(&app, &id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        text(&serde_json::from_slice::<Record>(&body).unwrap()),
        text(&record)
    );
}

#[tokio::test]
async fn malformed_unsigned_tampered_and_expired_records_are_refused_alike() {
    let (_data, state, app) = friends();
    let w = World::new();
    let path = [0, 1, 2];
    let history = w.history(&path);
    let good = |id: &str| w.record(&path, &history, 0, id, recently(5_000), 7);
    let mut cases: Vec<(&str, Record)> = vec![];
    let mut case = |name, record: Record| cases.push((name, record));
    let resign = |record: &mut Record, key: usize| w.sign(record, key);

    let mut r = good(&id_of(10));
    r.id = r.id.to_uppercase();
    case("non-canonical id", r);
    let mut r = good(&id_of(11));
    r.nonce = B64.encode([3u8; 11]);
    resign(&mut r, 2);
    case("short nonce", r);
    let mut r = good(&id_of(12));
    r.ciphertext = B64.encode(vec![1u8; 15]);
    resign(&mut r, 2);
    case("tiny ciphertext", r);
    let mut r = good(&id_of(13));
    r.ciphertext = B64.encode(vec![1u8; locator::MAX_CIPHERTEXT_BYTES + 1]);
    resign(&mut r, 2);
    case("oversized ciphertext", r);
    let mut r = good(&id_of(14));
    r.ciphertext = B64.encode(vec![9u8; 40]);
    case("ciphertext changed after signing", r);
    let mut r = good(&id_of(15));
    resign(&mut r, 5);
    case("signed by some other key", r);
    let mut r = good(&id_of(16));
    r.signature = String::new();
    case("unsigned", r);
    let mut r = good(&id_of(17));
    r.history[0].receipt.body.target_url = "https://elsewhere.example/".into();
    resign(&mut r, 2);
    case("tampered handover edge", r);
    let mut r = good(&id_of(18));
    r.history[1].history_sha256 = Some("0".repeat(64));
    resign(&mut r, 2);
    case("wrong ancestry hash", r);
    let mut r = good(&id_of(19));
    r.history.remove(0);
    resign(&mut r, 2);
    case("shortened history", r);
    let mut r = good(&id_of(20));
    r.anchor = w.pin(7, 1);
    resign(&mut r, 2);
    case("anchor not on the chain", r);
    let mut r = good(&id_of(21));
    r.anchor.server_id = uuid::Uuid::new_v4().to_string();
    resign(&mut r, 2);
    case("anchor of another server", r);
    let mut r = good(&id_of(22));
    r.current.term = 9;
    resign(&mut r, 2);
    case("current term not the chain end", r);
    let mut r = good(&id_of(23));
    r.issued_ms = now_ms() + 120_000;
    r.expires_ms = r.issued_ms + 1000;
    resign(&mut r, 2);
    case("issued in the future", r);
    let mut r = good(&id_of(24));
    r.issued_ms = 0;
    resign(&mut r, 2);
    case("issued at zero", r);
    let mut r = good(&id_of(25));
    r.expires_ms = now_ms() - 1;
    resign(&mut r, 2);
    case("already expired", r);
    let mut r = good(&id_of(26));
    r.expires_ms = r.issued_ms + locator::MAX_LIFETIME_MS + 1;
    resign(&mut r, 2);
    case("lifetime too long", r);
    let mut r = good(&id_of(27));
    r.expires_ms = r.issued_ms - 1;
    resign(&mut r, 2);
    case("expires before issued", r);
    // Nine edges is one more than a route may carry.
    let long_path: Vec<usize> = (0..10).collect();
    let long = w.history(&long_path);
    case(
        "history longer than eight",
        w.record(&long_path, &long, 0, &id_of(28), recently(5_000), 7),
    );

    let mut answers = std::collections::HashSet::new();
    for (name, record) in &cases {
        let (status, body) = send(
            &app,
            "POST",
            PUBLISH,
            serde_json::to_vec(record).unwrap(),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        answers.insert(body);
        // Each refusal is also a miss for its ID.
        assert_eq!(
            lookup(&app, &record.id).await.0,
            StatusCode::NOT_FOUND,
            "{name}"
        );
        lock(&state.0.locator_rates).clear();
    }
    assert_eq!(answers.len(), 1, "every refusal reads the same");
    assert_eq!(rows(&state), 0);
    // Garbage that is not a record at all reads the same.
    let (status, body) = send(&app, "POST", PUBLISH, b"{\"id\":1}".to_vec(), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(answers.contains(&body));
}

#[tokio::test]
async fn rollback_and_conflicts_are_refused_and_only_a_verified_successor_replaces() {
    let (_data, state, app) = friends();
    let w = World::new();
    let main = [0, 1, 2, 3];
    let full = w.history(&main);
    let id = id_of(1);
    let first = w.record(&main[..1], &full[..0], 0, &id, recently(9_000), 1);
    let unlimited = |state: &AppState| lock(&state.0.locator_rates).clear();
    assert_eq!(publish(&app, &first).await, StatusCode::NO_CONTENT);
    // The same record again changes nothing.
    assert_eq!(publish(&app, &first).await, StatusCode::NO_CONTENT);
    unlimited(&state);
    // The same host and term may move forward in time, never back or sideways.
    let later = w.record(&main[..1], &full[..0], 0, &id, recently(6_000), 2);
    assert_eq!(publish(&app, &later).await, StatusCode::NO_CONTENT);
    let older = w.record(&main[..1], &full[..0], 0, &id, recently(8_000), 3);
    assert_eq!(publish(&app, &older).await, StatusCode::BAD_REQUEST);
    let conflict = w.record(&main[..1], &full[..0], 0, &id, later.issued_ms, 4);
    assert_eq!(publish(&app, &conflict).await, StatusCode::BAD_REQUEST);
    unlimited(&state);
    let (_, body) = lookup(&app, &id).await;
    assert_eq!(
        text(&serde_json::from_slice::<Record>(&body).unwrap()),
        text(&later)
    );
    // A different record cannot take the ID under another anchor.
    let other_anchor = w.record(&main[..2], &full[..1], 1, &id, recently(1_000), 5);
    assert_eq!(publish(&app, &other_anchor).await, StatusCode::BAD_REQUEST);
    // The verified successor, using the complete history, replaces it.
    let successor = w.record(&main[..2], &full[..1], 0, &id, recently(4_000), 7);
    assert_eq!(publish(&app, &successor).await, StatusCode::NO_CONTENT);
    // A different host in the same term (another valid chain from the anchor) is no successor.
    let fork = [0, 8];
    let forked = w.history(&fork);
    let sibling = w.record(&fork, &forked[..1], 0, &id, recently(500), 6);
    assert_eq!(sibling.current.term, successor.current.term);
    assert_eq!(publish(&app, &sibling).await, StatusCode::BAD_REQUEST);
    unlimited(&state);
    // Going back to the earlier host is a rollback now.
    let rollback = w.record(&main[..1], &full[..0], 0, &id, recently(100), 8);
    assert_eq!(publish(&app, &rollback).await, StatusCode::BAD_REQUEST);
    unlimited(&state);
    // A higher term whose ancestry is rewritten (a different first edge) is refused,
    // although its own chain is valid and rooted at the same anchor.
    let rewritten_path = [0, 9, 2, 3];
    let rewritten = w.history(&rewritten_path);
    let rival = w.record(&rewritten_path, &rewritten[..3], 0, &id, recently(2_000), 9);
    assert_eq!(publish(&app, &rival).await, StatusCode::BAD_REQUEST);
    // So is a higher term that drops part of the old history.
    let mut shortened = w.record(&main, &full[..3], 0, &id, recently(2_000), 10);
    shortened.history.remove(0);
    w.sign(&mut shortened, 3);
    assert_eq!(publish(&app, &shortened).await, StatusCode::BAD_REQUEST);
    unlimited(&state);
    // The honest continuation of the same ancestry is accepted.
    let next = w.record(&main, &full[..3], 0, &id, recently(2_000), 11);
    assert_eq!(publish(&app, &next).await, StatusCode::NO_CONTENT);
    let (_, body) = lookup(&app, &id).await;
    assert_eq!(
        text(&serde_json::from_slice::<Record>(&body).unwrap()),
        text(&next)
    );
    assert_eq!(rows(&state), 1);
}

#[tokio::test]
async fn live_ids_expiry_cleanup_and_request_rates_are_bounded() {
    let (_data, state, app) = friends();
    let w = World::new();
    let path = [0, 1];
    let history = w.history(&path);
    // One anchor may hold 256 live IDs; the 257th is refused, another anchor is not.
    for n in 0..256u32 {
        lock(&state.0.locator_rates).clear();
        let record = w.record(&path, &history[..0], 0, &id_of(n), recently(5_000), 1);
        assert_eq!(publish(&app, &record).await, StatusCode::NO_CONTENT, "{n}");
    }
    lock(&state.0.locator_rates).clear();
    let extra = w.record(&path, &history[..0], 0, &id_of(256), recently(5_000), 1);
    assert_eq!(publish(&app, &extra).await, StatusCode::BAD_REQUEST);
    let another = World::new();
    let theirs = another.record(
        &path,
        &another.history(&path)[..0],
        0,
        &id_of(257),
        recently(5_000),
        1,
    );
    assert_eq!(publish(&app, &theirs).await, StatusCode::NO_CONTENT);
    // An existing ID can still be refreshed at the cap.
    lock(&state.0.locator_rates).clear();
    let refreshed = w.record(&path, &history[..0], 0, &id_of(0), recently(1_000), 2);
    assert_eq!(publish(&app, &refreshed).await, StatusCode::NO_CONTENT);

    // The whole directory is capped too.
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction().unwrap();
        tx.execute("DELETE FROM host_locator_records", []).unwrap();
        let future = now_ms() + 3_600_000;
        for n in 0..32768u32 {
            tx.execute(
                "INSERT INTO host_locator_records VALUES(?1,?2,'k',1,1,?3,'{}')",
                params![
                    format!("{:064x}", n as u128 + 1000),
                    format!("anchor-{n}"),
                    future
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    lock(&state.0.locator_rates).clear();
    let full = w.record(&path, &history[..0], 0, &id_of(300), recently(5_000), 1);
    assert_eq!(publish(&app, &full).await, StatusCode::BAD_REQUEST);

    // Expired rows are removed in bounded batches and never served.
    {
        let db = lock(&state.0.db);
        db.execute("DELETE FROM host_locator_records", []).unwrap();
        let past = now_ms() - 1000;
        for n in 0..300u32 {
            db.execute(
                "INSERT INTO host_locator_records VALUES(?1,?2,'k',1,1,?3,'{}')",
                params![format!("{:064x}", n as u128 + 5000), "gone", past],
            )
            .unwrap();
        }
    }
    assert_eq!(
        lookup(&app, &format!("{:064x}", 5000)).await.0,
        StatusCode::NOT_FOUND
    );
    lock(&state.0.locator_rates).clear();
    let fresh = w.record(&path, &history[..0], 0, &id_of(301), recently(5_000), 1);
    assert_eq!(publish(&app, &fresh).await, StatusCode::NO_CONTENT);
    assert_eq!(rows(&state), 300 - 256 + 1);

    // Per-ingress budgets: the 31st publish in a minute is turned away before parsing.
    lock(&state.0.locator_rates).clear();
    let spoof = [
        ("x-forwarded-for", "203.0.113.9"),
        ("forwarded", "for=203.0.113.9"),
    ];
    let mut last = StatusCode::OK;
    for n in 0..31 {
        // A forwarded address does not give a fresh allowance, as no proxy is trusted.
        let header = [spoof[n % 2]];
        last = send(&app, "POST", PUBLISH, b"{}".to_vec(), &header).await.0;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        lookup(&app, &id_of(0)).await.0,
        StatusCode::NOT_FOUND,
        "lookups have their own budget"
    );
    for _ in 0..119 {
        lookup(&app, &id_of(0)).await;
    }
    assert_eq!(
        lookup(&app, &id_of(0)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // The rate map itself is bounded: when it is full a new source is turned away.
    {
        let mut rates = lock(&state.0.locator_rates);
        rates.clear();
        for n in 0..4096 {
            rates.insert(format!("lookup:source-{n}"), (now_ms(), 1));
        }
    }
    assert_eq!(
        lookup(&app, &id_of(0)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert!(lock(&state.0.locator_rates).len() <= 4096);
}

#[tokio::test]
async fn only_the_two_exact_post_routes_are_open_and_community_mode_stays_closed() {
    let (_data, state, app) = friends();
    let w = World::new();
    let path = [0, 1];
    let history = w.history(&path);
    let record = w.record(&path, &history[..0], 0, &id_of(1), recently(5_000), 1);
    assert_eq!(publish(&app, &record).await, StatusCode::NO_CONTENT);
    // No enumeration, search or query-string capability; no other method or path.
    for (method, uri) in [
        ("GET", LOOKUP.to_string()),
        ("GET", format!("{LOOKUP}?id={}", id_of(1))),
        ("GET", PUBLISH.to_string()),
        ("PUT", PUBLISH.to_string()),
        ("DELETE", PUBLISH.to_string()),
        ("GET", "/api/host-locator".to_string()),
        ("GET", "/api/host-locator/list".to_string()),
        ("POST", "/api/host-locator/list".to_string()),
        ("POST", format!("{LOOKUP}/")),
        ("POST", format!("{LOOKUP}/{}", id_of(1))),
    ] {
        lock(&state.0.locator_rates).clear();
        let (status, _) = send(
            &app,
            method,
            &uri,
            serde_json::to_vec(&json!({"id": id_of(1)})).unwrap(),
            &[],
        )
        .await;
        assert!(
            status == StatusCode::FORBIDDEN
                || status == StatusCode::NOT_FOUND
                || status == StatusCode::METHOD_NOT_ALLOWED,
            "{method} {uri} answered {status}"
        );
    }
    // The open routes do not open anything else without a bearer.
    for uri in ["/api/bootstrap", "/api/social", "/api/messages"] {
        assert_ne!(
            send(&app, "GET", uri, vec![], &[]).await.0,
            StatusCode::OK,
            "{uri}"
        );
    }
    // A body over the cap is refused before it is read.
    lock(&state.0.locator_rates).clear();
    let big = vec![b' '; locator::MAX_RECORD_BYTES + 1];
    let (status, _) = send(&app, "POST", PUBLISH, big, &[]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, _) = send(&app, "POST", LOOKUP, vec![b' '; 2048], &[]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // An ordinary community serves no locator and asks for the usual sign-in.
    let data = tempfile::tempdir().unwrap();
    let community = AppState::open(data.path(), "Community", 0).unwrap();
    let app = router(community.clone());
    assert_eq!(publish(&app, &record).await, StatusCode::UNAUTHORIZED);
    assert_eq!(lookup(&app, &id_of(1)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(rows(&community), 0);
}
