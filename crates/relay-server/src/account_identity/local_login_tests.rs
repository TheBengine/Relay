//! The host app signing in to a server it runs on this PC with the Relay
//! account it already holds (server creation, reopening, restart).
use super::*;

const CONTROL: &str = "fixture-local-control-secret-0123456789";
/// The nonce of the desktop app's own sign-in. A refresh grant returns it (or
/// none) in every later ID token; it is never bound to a server.
const DESKTOP_NONCE: &str = "owned-desktop-sign-in-nonce-7d1c2f";

fn claims_for(
    fixture: &Fixture,
    subject: &str,
    handle: &str,
    nonce: Option<&str>,
    issued: chrono::DateTime<chrono::Utc>,
) -> CoreIdTokenClaims {
    CoreIdTokenClaims::new(
        IssuerUrl::new(fixture.provider.issuer.to_string()).unwrap(),
        vec![Audience::new("relay-desktop".into())],
        issued + chrono::Duration::seconds(300),
        issued,
        StandardClaims::new(SubjectIdentifier::new(subject.into())),
        EmptyAdditionalClaims {},
    )
    .set_nonce(nonce.map(|n| Nonce::new(n.into())))
    .set_preferred_username(Some(EndUserUsername::new(handle.into())))
}

/// What the desktop app's saved login yields silently: a fresh signed ID token
/// from a refresh grant, with the desktop sign-in's own nonce.
fn refreshed(fixture: &Fixture) -> String {
    token(
        claims_for(
            fixture,
            "own-stable-alice-subject",
            "alice_shared",
            Some(DESKTOP_NONCE),
            chrono::Utc::now(),
        ),
        23,
    )
}

fn host_state(fixture: &Fixture, temp: &tempfile::TempDir) -> AppState {
    AppState::open_with_config(
        temp.path(),
        "Created on this PC",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            local_control_token: Some(CONTROL.into()),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn loopback_challenge(app: &Router, register: bool, protection: Option<&str>) -> Value {
    let (status, value) = request(
        app,
        "POST",
        "/api/account-login/challenge",
        None,
        json!({"destination":"http://127.0.0.1:4580","register":register,"initial_protection":protection}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

const LOCAL_LOGIN: &str = "/api/local/account-login";

async fn send(app: &Router, request: axum::http::Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn control_headers(state: &AppState) -> Vec<(&'static str, String)> {
    vec![
        ("x-relay-local-control", CONTROL.to_owned()),
        ("x-relay-instance-id", state.instance_id().to_owned()),
        ("x-relay-server-id", state.server_id().unwrap()),
    ]
}

fn local_login(
    peer: &str,
    headers: &[(&'static str, String)],
    challenge: &Value,
    id_token: &str,
) -> axum::http::Request<Body> {
    let headers: Vec<_> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    local_request(
        LOCAL_LOGIN,
        peer,
        &headers,
        json!({"challenge_id":challenge["challenge_id"],"id_token":id_token}),
    )
}

fn users(state: &AppState) -> u32 {
    lock(&state.0.db)
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap()
}

/// Reproduction of "creating a new server makes me log in again" (6 Nov 2026).
///
/// Cause: a server accepted a Relay account only through `/api/account-login`,
/// whose ID token must carry the nonce of this server's one-use challenge. Such
/// a token comes only from an interactive authorization request in the
/// account page. The desktop app restores its saved Relay login with a refresh
/// grant, and its in-app account page is incognito, so after a restart (or
/// once the account service's browser session has ended) it has no browser
/// session there. The fresh signed token it does hold is refused here
/// (wrong nonce), so the first account on a server the user just created had
/// to be confirmed by typing the account password again.
///
/// Fix: the host app on this PC, which proves itself with the server's local
/// control secret over loopback, completes the same challenge with that
/// refreshed token at `/api/local/account-login`. Every other rule is the
/// shared one: one-use challenge, signature, issuer, audience, a fresh issue
/// time, first-owner and membership rules.
#[tokio::test]
async fn a_server_created_on_this_pc_accepts_the_saved_relay_account_from_its_host_app() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = host_state(&fixture, &temp);
    let app = router(state.clone());

    // The existing proof route refuses what a saved login can produce silently.
    let challenge = loopback_challenge(&app, true, Some("open")).await;
    let (status, value) = login_proof(&app, &challenge, refreshed(&fixture)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    assert_eq!(users(&state), 0);

    // The host app on this PC creates the first account with it, no sign-in page.
    let challenge = loopback_challenge(&app, true, Some("open")).await;
    let (status, session) = send(
        &app,
        local_login("127.0.0.1:5000", &control_headers(&state), &challenge, &refreshed(&fixture)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["user"]["role"], "admin");
    assert_eq!(session["user"]["username"], "alice_shared");
    let owner = session["user"]["id"].as_str().unwrap().to_owned();
    let (_, boot) = request(
        &app,
        "GET",
        "/api/bootstrap",
        session["token"].as_str(),
        Value::Null,
    )
    .await;
    assert_eq!(boot["protection"]["level"], "open", "the creation choice is kept");

    // Reopening this server later (a restart after its saved session ended,
    // or switching back to it) signs the same account in again, silently.
    let challenge = loopback_challenge(&app, false, None).await;
    let (status, again) = send(
        &app,
        local_login("127.0.0.1:5001", &control_headers(&state), &challenge, &refreshed(&fixture)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["user"]["id"], owner.as_str());
    assert_ne!(again["token"], session["token"]);
    assert_eq!(users(&state), 1);

    // The challenge is single use on this route too.
    let (status, _) = send(
        &app,
        local_login("127.0.0.1:5002", &control_headers(&state), &challenge, &refreshed(&fixture)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Only the host app on this PC may use the local route, and only with a
/// fresh, correctly signed token that is not a proof minted for some server.
#[tokio::test]
async fn the_local_account_route_refuses_other_computers_proxies_wrong_secrets_and_bound_proofs() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = host_state(&fixture, &temp);
    let app = router(state.clone());
    let good = control_headers(&state);
    let challenge = loopback_challenge(&app, true, None).await;
    let token = refreshed(&fixture);

    let (status, _) = send(&app, local_login("192.168.1.20:5000", &good, &challenge, &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not from another computer");
    let mut wrong = good.clone();
    wrong[0].1 = "not-the-local-secret-0123456789-0123456789".into();
    let (status, _) = send(&app, local_login("127.0.0.1:5000", &wrong, &challenge, &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not without the local secret");
    let (status, _) = send(&app, local_login("127.0.0.1:5000", &good[1..], &challenge, &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not without any secret");
    let mut forwarded = good.clone();
    forwarded.push(("x-forwarded-for", "1.2.3.4".into()));
    let (status, _) = send(&app, local_login("127.0.0.1:5000", &forwarded, &challenge, &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not through a proxy");
    let mut replaced = good.clone();
    replaced[1].1 = uuid::Uuid::new_v4().to_string();
    let (status, _) = send(&app, local_login("127.0.0.1:5000", &replaced, &challenge, &token)).await;
    assert_eq!(status, StatusCode::CONFLICT, "not for a replaced server instance");

    // Friends arriving through the network listener never reach it.
    let ingress_token = "i".repeat(40);
    let ingress = network_ingress_router(state.clone(), ingress_token.clone());
    let mut through = good.clone();
    through.push(("x-relay-network-ingress", ingress_token));
    through.push(("x-relay-network-peer", "100.64.0.9".into()));
    let (status, _) = send(&ingress, local_login("127.0.0.1:5000", &through, &challenge, &token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not through the network listener");

    // None of the refused requests spent the challenge or created anyone.
    assert_eq!(users(&state), 0);

    // A proof the account page minted for a server (or for linking) is only
    // ever valid where it was bound, never here.
    for nonce in [
        challenge["nonce"].as_str().unwrap().to_owned(),
        format!("relay-login-v1:{}:{}:{}", uuid::Uuid::new_v4(), "0".repeat(64), "a".repeat(43)),
        "relay-link-v1:other".to_owned(),
    ] {
        let challenge = loopback_challenge(&app, true, None).await;
        let bound = token_for(&fixture, "own-stable-alice-subject", Some(&nonce), 0, 23);
        let (status, _) = send(&app, local_login("127.0.0.1:5000", &good, &challenge, &bound)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{nonce}");
    }
    // Stale or wrongly signed tokens are refused.
    let challenge = loopback_challenge(&app, true, None).await;
    let stale = token_for(&fixture, "own-stable-alice-subject", Some(DESKTOP_NONCE), 600, 23);
    let (status, _) = send(&app, local_login("127.0.0.1:5000", &good, &challenge, &stale)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an old identity is not recent");
    let challenge = loopback_challenge(&app, true, None).await;
    let forged = token_for(&fixture, "own-stable-alice-subject", Some(DESKTOP_NONCE), 0, 99);
    let (status, _) = send(&app, local_login("127.0.0.1:5000", &good, &challenge, &forged)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "not signed by the account service");
    assert_eq!(users(&state), 0);
}

fn token_for(fixture: &Fixture, subject: &str, nonce: Option<&str>, age: i64, seed: u8) -> String {
    token(
        claims_for(
            fixture,
            subject,
            "alice_shared",
            nonce,
            chrono::Utc::now() - chrono::Duration::seconds(age),
        ),
        seed,
    )
}

/// The silent route changes how the account is proven, not who may join: a
/// different Relay account still needs an invitation, and an existing account
/// with its own authenticator is never signed in through it.
#[tokio::test]
async fn the_local_account_route_keeps_owner_and_membership_rules_for_other_accounts() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = host_state(&fixture, &temp);
    let app = router(state.clone());
    let good = control_headers(&state);
    let challenge = loopback_challenge(&app, true, None).await;
    let (status, owner) = send(&app, local_login("127.0.0.1:5000", &good, &challenge, &refreshed(&fixture))).await;
    assert_eq!(status, StatusCode::OK, "{owner}");

    let bob = || {
        token(
            claims_for(&fixture, "own-stable-bob-subject", "bob_shared", Some(DESKTOP_NONCE), chrono::Utc::now()),
            23,
        )
    };
    let challenge = loopback_challenge(&app, false, None).await;
    let (status, value) = send(&app, local_login("127.0.0.1:5000", &good, &challenge, &bob())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    let challenge = loopback_challenge(&app, true, None).await;
    let (status, _) = send(&app, local_login("127.0.0.1:5000", &good, &challenge, &bob())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a second account needs an invitation");
    assert_eq!(users(&state), 1);
}
