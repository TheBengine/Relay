//! W-01 and R7: a password or a saved login never travels in clear text without
//! the person's acknowledgement for that address, and a saved login never goes to
//! a server that cannot prove the host key the login was made with.
use super::*;
use relay_protocol::handover::{RouteChallenge, RoutePin, RouteState};

const PASSWORD: &str = "synthetic-lan-password";
const TOKEN: &str = "synthetic-saved-token";

fn summary(server_id: &str) -> Reply {
    Reply::json(
        200,
        json!({
            "ok": true,
            "server_name": "Fixture LAN",
            "server_id": server_id,
            "protocol_version": API_PROTOCOL_VERSION,
            "rtc_enabled": false,
        }),
    )
}

/// The mock listens on loopback; the worker reaches it by a LAN-looking name,
/// so the address is plain HTTP to "another machine".
fn lan_address(worker: &mut Worker, server: &MockHttp) -> String {
    let socket: std::net::SocketAddr = server.url.trim_start_matches("http://").parse().unwrap();
    worker.http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .resolve("relay-lan.test", socket)
        .build()
        .unwrap();
    format!("http://relay-lan.test:{}", socket.port())
}

fn password_login(url: &str) -> Command {
    Command::Auth {
        authority: None,
        url: url.into(),
        expected_server_id: None,
        request: AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: "lan-member".into(),
            password: PASSWORD.into(),
            invite: String::new(),
        },
        register: false,
        request_id: 7,
    }
}

fn remembered(server_id: &str, host: Option<RoutePin>) -> crate::credential_vault::RememberedSession {
    crate::credential_vault::RememberedSession {
        authority: None,
        host,
        server_id: Some(server_id.into()),
        session: Session {
            token: TOKEN.into(),
            user: User {
                id: "fixture-user".into(),
                ..Default::default()
            },
        },
    }
}

fn credential_seen(server: &MockHttp) -> bool {
    server.requests.lock().unwrap().iter().any(|request| {
        request.authorization.is_some()
            || String::from_utf8_lossy(&request.body).contains(PASSWORD)
    })
}

#[test]
fn signing_in_to_a_plain_http_lan_address_sends_no_password_until_acknowledged() {
    let (_, _, pin, _) = crate::host_route::tests::fixture();
    let server_id = pin.server_id.clone();
    let server = MockHttp::with_request(move |request| match request.path.as_str() {
        "/api/health" => summary(&server_id),
        _ => Reply::json(401, json!({"error": "fixture refuses"})),
    });
    let mut client = TestWorker::new();
    let url = lan_address(&mut client.worker, &server);
    let refused = client.worker.handle(password_login(&url));
    assert_eq!(refused.unwrap_err(), crate::cleartext::CONSENT_NEEDED);
    assert!(!credential_seen(&server), "a password left before the acknowledgement");

    // Loopback is this PC talking to itself and needs no acknowledgement.
    let local = client.worker.handle(password_login(&server.url));
    assert_ne!(local.unwrap_err(), crate::cleartext::CONSENT_NEEDED);
    server.requests.lock().unwrap().clear();

    // An acknowledgement covers exactly this address.
    client
        .worker
        .handle(Command::AllowUnencrypted {
            url: url.replace("relay-lan.test", "relay-other.test"),
        })
        .unwrap();
    assert_eq!(
        client.worker.handle(password_login(&url)).unwrap_err(),
        crate::cleartext::CONSENT_NEEDED
    );
    assert!(!credential_seen(&server));
    client
        .worker
        .handle(Command::AllowUnencrypted { url: url.clone() })
        .unwrap();
    let _ = client.worker.handle(password_login(&url));
    assert!(
        credential_seen(&server),
        "after the acknowledgement the sign-in proceeds"
    );
}

#[test]
fn resuming_a_saved_login_on_a_plain_http_lan_address_sends_no_token_until_acknowledged() {
    let (_, _, pin, _) = crate::host_route::tests::fixture();
    let server_id = pin.server_id.clone();
    let server = MockHttp::with_request(move |request| match request.path.as_str() {
        "/api/health" => summary(&server_id),
        _ => Reply::json(401, json!({"error": "fixture refuses"})),
    });
    let mut client = TestWorker::new();
    let url = lan_address(&mut client.worker, &server);
    let saved = remembered(&pin.server_id, None);
    let refused = client
        .worker
        .resume_same_host(url.clone(), saved.clone(), Some(pin.server_id.clone()));
    assert_eq!(refused.unwrap_err(), crate::cleartext::CONSENT_NEEDED);
    assert!(!credential_seen(&server), "a saved token left before the acknowledgement");
    client
        .worker
        .handle(Command::AllowUnencrypted { url: url.clone() })
        .unwrap();
    let _ = client
        .worker
        .resume_same_host(url, saved, Some(pin.server_id.clone()));
    assert!(credential_seen(&server));
}

/// A server that answers the route check: `key` signs the state it claims.
fn route_reply(
    request: &RecordedRequest,
    key: &ed25519_dalek::SigningKey,
    pin: &RoutePin,
) -> Reply {
    let challenge: RouteChallenge = serde_json::from_slice(&request.body).unwrap();
    let proof = crate::host_route::tests::sign(
        key,
        RouteState {
            pin: pin.clone(),
            challenge: challenge.challenge,
            writes_allowed: true,
            moved: None,
            history: Vec::new(),
        },
    );
    Reply::json(200, serde_json::to_value(proof).unwrap())
}

#[test]
fn a_saved_login_is_not_offered_to_a_different_server_at_the_same_address() {
    let (real, impostor, pin, _) = crate::host_route::tests::fixture();
    for case in ["other-key", "no-route-answer", "same-key"] {
        let server_id = pin.server_id.clone();
        let claimed = pin.clone();
        let real = real.clone();
        let impostor = impostor.clone();
        // The other server copies the public community id into its health answer.
        let server = MockHttp::with_request(move |request| match request.path.as_str() {
            "/api/health" => summary(&server_id),
            "/api/hosting/handover/route" => match case {
                "other-key" => route_reply(request, &impostor, &claimed),
                "same-key" => route_reply(request, &real, &claimed),
                _ => Reply::json(404, json!({"error": "not found"})),
            },
            _ => Reply::json(401, json!({"error": "fixture refuses"})),
        });
        let mut client = TestWorker::new();
        let result = client.worker.resume_same_host(
            server.url.clone(),
            remembered(&pin.server_id, Some(pin.clone())),
            Some(pin.server_id.clone()),
        );
        let requests = server.requests.lock().unwrap();
        match case {
            "same-key" => {
                // The real host proved its key, then (and only then) the token went.
                let route = requests
                    .iter()
                    .position(|r| r.path == "/api/hosting/handover/route")
                    .unwrap();
                let token = requests.iter().position(|r| r.authorization.is_some()).unwrap();
                assert!(route < token, "{case}");
            }
            _ => {
                assert_eq!(
                    result.unwrap_err(),
                    crate::host_route::NOT_THE_SAME_SERVER,
                    "{case}"
                );
                assert!(
                    requests.iter().all(|r| r.authorization.is_none()),
                    "{case}: the saved token reached another server"
                );
            }
        }
        assert!(client.worker.connection.is_none(), "{case}");
    }
}
