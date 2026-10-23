use super::*;
use crate::tests::{new_invite, request, signup};
use openidconnect::{
    core::{CoreEdDsaPrivateSigningKey, CoreIdTokenClaims},
    Audience, EmptyAdditionalClaims, EndUserUsername, JsonWebKeySet, PrivateSigningKey,
    StandardClaims, SubjectIdentifier,
};
use std::io::{BufRead, Write};
const PASSWORD: &str = "a-long-test-password";

#[path = "people_tests.rs"]
mod people_tests;

/// An explicitly selected fixture only; normal builds never admit HTTP issuers.
/// The external owner drives the real client worker and provider, then closes
/// this fixture through stdin. No private token is written to a log or file.
#[tokio::test]
#[ignore = "requires an explicitly owned live loopback identity service and client journey"]
async fn owned_live_account_link_journey_server() {
    let issuer = std::env::var("RELAY_OWN_LINK_ISSUER").expect("own issuer required");
    let url = Url::parse(&issuer).unwrap();
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.path(), "/realms/relay-accounts");
    assert!(plain(&url) && url.port().is_some());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let community = format!("http://{}", listener.local_addr().unwrap());
    let mut provider = Provider::new(
        "https://accounts.example.test/realms/relay-accounts",
        "relay-desktop",
        &community,
    )
    .unwrap();
    provider.issuer = url;
    provider.fixture_http = true;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Own full account-link journey",
        0,
        ServerConfig {
            account_provider: Some(provider),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let alice = signup(&app, "Alice", "").await;
    let bob = signup(&app, "Bob", &new_invite(&app, &alice).await).await;
    println!(
        "RELAY_OWN_LINK_SERVER={}",
        json!({"issuer":issuer,"server_url":community,"alice":alice,"bob":bob})
    );
    std::io::stdout().flush().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let _ = stop.send(());
    });
    // Fixture lifetime for manual native GUI work, not an OIDC/security deadline.
    let lifetime = if std::env::var_os("RELAY_OWN_LINK_NATIVE_UI").is_some() {
        900
    } else {
        180
    };
    tokio::time::timeout(Duration::from_secs(lifetime), async {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    })
    .await
    .expect("own fixture shutdown deadline");
    let db = lock(&state.0.db);
    assert!(linked(&db, &alice.user.id).unwrap().is_none());
    assert!(linked(&db, &bob.user.id).unwrap().is_none());
    let users: usize = db
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        users, 2,
        "link/unlink never creates replacement local users"
    );
}

fn signing(seed: u8) -> CoreEdDsaPrivateSigningKey {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
    ];
    der.extend_from_slice(&[seed; 32]);
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
        STANDARD.encode(der)
    );
    CoreEdDsaPrivateSigningKey::from_ed25519_pem(&pem, None).unwrap()
}

#[tokio::test]
#[ignore = "requires an explicitly owned loopback provider and client account-first journey"]
async fn owned_live_account_first_journey_server() {
    let issuer =
        Url::parse(&std::env::var("RELAY_OWN_LINK_ISSUER").expect("own issuer required")).unwrap();
    assert_eq!(issuer.scheme(), "http");
    assert_eq!(issuer.host_str(), Some("127.0.0.1"));
    assert_eq!(issuer.path(), "/realms/relay-accounts");
    assert!(issuer.port().is_some() && plain(&issuer));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let community = format!("http://{}", listener.local_addr().unwrap());
    let mut provider = Provider::new(
        "https://accounts.example.test/realms/relay-accounts",
        "relay-desktop",
        &community,
    )
    .unwrap();
    provider.issuer = issuer.clone();
    provider.fixture_http = true;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Owned account-first journey",
        0,
        ServerConfig {
            account_provider: Some(provider),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let invite = new_invite(&app, &owner).await;
    println!(
        "RELAY_OWN_LINK_SERVER={}",
        json!({"issuer":issuer,"server_url":community,"server_id":server_id(&lock(&state.0.db)).unwrap(),"invite":invite})
    );
    std::io::stdout().flush().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let _ = stop.send(());
    });
    tokio::time::timeout(Duration::from_secs(180), async {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    })
    .await
    .expect("owned fixture shutdown deadline")
    .unwrap();
    let db = lock(&state.0.db);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM linked_identities", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
}
struct Fixture {
    provider: Provider,
    task: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let issuer = format!("{origin}/realms/relay-accounts");
        let public = signing(23).as_verification_key();
        let keys = serde_json::to_value(JsonWebKeySet::new(vec![public])).unwrap();
        let metadata = json!({"issuer":issuer,"authorization_endpoint":format!("{origin}/auth"),"token_endpoint":format!("{origin}/token"),"jwks_uri":format!("{origin}/keys"),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["EdDSA"]});
        let app = Router::new()
            .route(
                "/realms/relay-accounts/.well-known/openid-configuration",
                get(move || {
                    let m = metadata.clone();
                    async move { Json(m) }
                }),
            )
            .route(
                "/keys",
                get(move || {
                    let k = keys.clone();
                    async move { Json(k) }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut provider = Provider::new(
            "https://accounts.example.test/realms/relay-accounts",
            "relay-desktop",
            "https://community.example.test",
        )
        .unwrap();
        provider.issuer = Url::parse(&issuer).unwrap();
        provider.fixture_http = true;
        Self { provider, task }
    }
    fn claims(&self, nonce: &str) -> CoreIdTokenClaims {
        let now = chrono::Utc::now();
        CoreIdTokenClaims::new(
            IssuerUrl::new(self.provider.issuer.to_string()).unwrap(),
            vec![Audience::new("relay-desktop".into())],
            now + chrono::Duration::seconds(300),
            now,
            StandardClaims::new(SubjectIdentifier::new("own-stable-alice-subject".into())),
            EmptyAdditionalClaims {},
        )
        .set_nonce(Some(Nonce::new(nonce.into())))
        .set_auth_time(Some(now))
        .set_preferred_username(Some(EndUserUsername::new("alice_shared".into())))
    }
    fn token(&self, nonce: &str) -> String {
        token(self.claims(nonce), 23)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn token(claims: CoreIdTokenClaims, seed: u8) -> String {
    CoreIdToken::new(
        claims,
        &signing(seed),
        CoreJwsSigningAlgorithm::EdDsa,
        None,
        None,
    )
    .unwrap()
    .to_string()
}
async fn begin(app: &Router, session: &Session) -> Value {
    let (status, value) = request(
        app,
        "POST",
        "/api/account-link",
        Some(&session.token),
        json!({"action":"begin","password":PASSWORD}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}
async fn finish(
    app: &Router,
    session: &Session,
    challenge: &Value,
    jwt: String,
) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        "/api/account-link",
        Some(&session.token),
        json!({"action":"link","challenge_id":challenge["challenge_id"],"id_token":jwt}),
    )
    .await
}

#[test]
fn account_provider_is_fixed_https_with_a_canonical_safe_community_destination() {
    let issuer = "https://accounts.example.test/realms/relay-accounts";
    for destination in [
        "https://community.example.test",
        "http://127.0.0.1:4580",
        &format!("http://{}.iroh:4580", "a".repeat(64)),
    ] {
        assert!(Provider::new(issuer, "relay-desktop", destination).is_ok());
    }
    for destination in [
        "http://192.168.1.10:4580",
        "file:///C:/private",
        "https://user:pw@community.example.test",
        "https://community.example.test?x=1",
    ] {
        assert!(Provider::new(issuer, "relay-desktop", destination).is_err());
    }
    assert!(Provider::new(
        "http://issuer.example.test/realm",
        "relay-desktop",
        "https://community.example.test"
    )
    .is_err());
}

async fn login_challenge(app: &Router, register: bool, invite: &str) -> Value {
    let (status, value) = request(
        app,
        "POST",
        "/api/account-login/challenge",
        None,
        json!({"destination":"https://community.example.test","register":register,"invite":invite}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}
async fn login_proof(app: &Router, challenge: &Value, jwt: String) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        "/api/account-login",
        None,
        json!({"challenge_id":challenge["challenge_id"],"id_token":jwt}),
    )
    .await
}

#[tokio::test]
async fn relay_account_login_joins_with_invitation_and_preserves_identity_without_a_server_password(
) {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Account-first test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "alice_shared", "").await;
    let challenge = login_challenge(&app, true, "").await;
    assert_eq!(
        login_proof(
            &app,
            &challenge,
            fixture.token(challenge["nonce"].as_str().unwrap())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let invite = new_invite(&app, &owner).await;
    let challenge = login_challenge(&app, true, &invite).await;
    let jwt = fixture.token(challenge["nonce"].as_str().unwrap());
    let (status, session) = login_proof(&app, &challenge, jwt.clone()).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let joined: Session = serde_json::from_value(session).unwrap();
    assert_ne!(joined.user.id, owner.user.id);
    assert_eq!(joined.user.profile.display_name, "alice_shared");
    assert_eq!(joined.user.role, "member");
    assert_ne!(
        joined.user.username, owner.user.username,
        "A colliding name must not claim the old account"
    );
    assert!(login_proof(&app, &challenge, jwt).await.0.is_client_error());
    let (status, boot) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&joined.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(boot["account_login_only"], true);
    assert_eq!(boot["protection"]["level"], "protected");
    assert!(boot["linked_identities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["user_id"] == joined.user.id && l["handle"] == "alice_shared"));
    let again = login_challenge(&app, false, "").await;
    let (status, session) = login_proof(
        &app,
        &again,
        fixture.token(again["nonce"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["user"]["id"], joined.user.id);
    let db = lock(&state.0.db);
    let count: u32 = db
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
    assert_eq!(
        db.query_row(
            "SELECT password_hash FROM users WHERE id=?1",
            [&joined.user.id],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        login::ACCOUNT_ONLY
    );
}

#[tokio::test]
async fn relay_account_login_binds_signed_proofs_and_never_bypasses_a_community_authenticator() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Account security test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Alice", "").await;
    let challenge = login_challenge(&app, true, "").await;
    assert!(login_proof(&app, &challenge, fixture.token("wrong-nonce"))
        .await
        .0
        .is_client_error());
    // Spending an invalid proof cannot leave a reusable challenge.
    assert!(login_proof(
        &app,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap())
    )
    .await
    .0
    .is_client_error());
    let link = begin(&app, &owner).await;
    assert_eq!(
        finish(
            &app,
            &owner,
            &link,
            fixture.token(link["nonce"].as_str().unwrap())
        )
        .await
        .0,
        StatusCode::OK
    );
    let challenge = login_challenge(&app, false, "").await;
    let (status, session) = login_proof(
        &app,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["user"]["id"], owner.user.id);
    {
        let db = lock(&state.0.db);
        db.execute("INSERT INTO authenticator_credentials(user_id,sealed,salt,revision) VALUES(?1,?2,?3,'own-test-revision')",params![owner.user.id,vec![1u8;32],vec![1u8;16]]).unwrap();
    }
    let challenge = login_challenge(&app, false, "").await;
    let (status, error) = login_proof(
        &app,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap()),
    )
    .await;
    assert!(status.is_client_error(), "{error}");
    assert!(error.to_string().contains("authenticator"));
}

#[tokio::test]
async fn relay_account_login_creates_only_local_first_owner_and_keeps_encryption_choice() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Own first owner test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let (status, challenge) = request(
        &app,
        "POST",
        "/api/account-login/challenge",
        None,
        json!({"destination":"http://127.0.0.1:4580","register":true,"initial_protection":"open"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    let (status, session) = login_proof(
        &app,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["user"]["role"], "admin");
    let token = session["token"].as_str().unwrap();
    let (_, boot) = request(&app, "GET", "/api/bootstrap", Some(token), Value::Null).await;
    assert_eq!(boot["protection"]["level"], "open");
    assert_eq!(boot["protection"]["revision"], 1);
    let (status, _) = request(
        &app,
        "POST",
        "/api/account-login/challenge",
        None,
        json!({"destination":"http://192.168.111.6:4580","register":false}),
    )
    .await;
    assert!(
        status.is_client_error(),
        "A global proof must not be sent over plain LAN HTTP"
    );
}

use tower::ServiceExt as _;

fn local_request(
    path: &str,
    peer: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> axum::http::Request<Body> {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:4580")
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    request
}

/// A server must never hand out a sign-in challenge for someone else's
/// address: a malicious server could collect it, show it to a user as its own,
/// and use the user's signed proof to sign in here as them.
#[tokio::test]
async fn relay_account_login_only_issues_challenges_for_this_servers_own_addresses() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    const CONTROL: &str = "fixture-local-control-secret-0123456789";
    let state = AppState::open_with_config(
        temp.path(),
        "Own address test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            local_control_token: Some(CONTROL.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let challenge = |destination: &str| {
        let app = app.clone();
        let destination = destination.to_owned();
        async move {
            request(
                &app,
                "POST",
                "/api/account-login/challenge",
                None,
                json!({"destination":destination,"register":false}),
            )
            .await
        }
    };
    // The relay attack: a challenge bound to another server's address.
    let (status, value) = challenge("https://evil.example.test").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
    let iroh = format!("http://{}.iroh:4580", "c".repeat(64));
    let (status, _) = challenge(&iroh).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unknown iroh address is refused"
    );
    // The server's own configured address and this PC's loopback still work.
    let (status, value) = challenge("https://community.example.test").await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let (status, value) = challenge("http://127.0.0.1:4580").await;
    assert_eq!(status, StatusCode::OK, "{value}");

    // Only the host app on this PC, with the local secret, can add its iroh address.
    let server = state.server_id().unwrap();
    let instance = state.instance_id().to_owned();
    let good = [
        ("x-relay-local-control", CONTROL),
        ("x-relay-instance-id", instance.as_str()),
        ("x-relay-server-id", server.as_str()),
    ];
    let body = json!({"url": iroh});
    let send = |request: axum::http::Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(request).await.unwrap().status() }
    };
    let path = "/api/local/account-destination";
    assert_eq!(
        send(local_request(
            path,
            "192.168.1.20:5000",
            &good,
            body.clone()
        ))
        .await,
        StatusCode::FORBIDDEN,
        "not from another computer"
    );
    let wrong = [
        ("x-relay-local-control", "not-the-secret"),
        ("x-relay-instance-id", instance.as_str()),
        ("x-relay-server-id", server.as_str()),
    ];
    assert_eq!(
        send(local_request(path, "127.0.0.1:5000", &wrong, body.clone())).await,
        StatusCode::FORBIDDEN,
        "not without the local secret"
    );
    let mut forwarded = good.to_vec();
    forwarded.push(("x-forwarded-for", "1.2.3.4"));
    assert_eq!(
        send(local_request(
            path,
            "127.0.0.1:5000",
            &forwarded,
            body.clone()
        ))
        .await,
        StatusCode::FORBIDDEN,
        "not through a proxy"
    );
    assert_eq!(
        send(local_request(
            path,
            "127.0.0.1:5000",
            &good,
            json!({"url":"http://192.168.1.20:4580"})
        ))
        .await,
        StatusCode::BAD_REQUEST,
        "plain LAN HTTP never carries a proof"
    );
    let (status, _) = challenge(&iroh).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "refused attempts added nothing"
    );
    assert_eq!(
        send(local_request(path, "127.0.0.1:5000", &good, body.clone())).await,
        StatusCode::NO_CONTENT
    );
    let (status, value) = challenge(&iroh).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let digest = format!("{:x}", Sha256::digest(iroh.as_bytes()));
    assert!(
        value["nonce"].as_str().unwrap().contains(&digest),
        "the proof is bound to the iroh address"
    );
    // Still never someone else's address.
    let (status, _) = challenge("https://evil.example.test").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Friends coming in through the network listener can never add addresses.
    let ingress = network_ingress_router(state.clone(), "i".repeat(40));
    let mut through = good.to_vec();
    let token = "i".repeat(40);
    through.push(("x-relay-network-ingress", token.as_str()));
    through.push(("x-relay-network-peer", "100.64.0.9"));
    let response = ingress
        .oneshot(local_request(path, "127.0.0.1:5000", &through, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn relay_account_login_never_initializes_a_first_owner_from_network_ingress() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Own ingress test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let network = app.clone().layer(axum::Extension(NetworkIngress {
        peer_ip: "192.168.111.5".parse().unwrap(),
    }));
    let (status, _) = request(
        &network,
        "POST",
        "/api/account-login/challenge",
        None,
        json!({"destination":"https://community.example.test","register":true}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let challenge = login_challenge(&app, true, "").await;
    let (status, _) = login_proof(
        &network,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn relay_account_login_requires_a_fresh_invitation_after_removal_and_never_bypasses_a_ban() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Own membership test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let owner = signup(&app, "Owner", "").await;
    let invite = new_invite(&app, &owner).await;
    let challenge = login_challenge(&app, true, &invite).await;
    let (status, session) = login_proof(
        &app,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let user = session["user"]["id"].as_str().unwrap();
    lock(&state.0.db)
        .execute(
            "UPDATE membership SET status='removed' WHERE user_id=?1",
            [user],
        )
        .unwrap();
    let challenge = login_challenge(&app, false, "").await;
    assert_eq!(
        login_proof(
            &app,
            &challenge,
            fixture.token(challenge["nonce"].as_str().unwrap())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let invite = new_invite(&app, &owner).await;
    let challenge = login_challenge(&app, true, &invite).await;
    let (status, rejoined) = login_proof(
        &app,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rejoined}");
    assert_eq!(rejoined["user"]["id"], user);
    lock(&state.0.db)
        .execute(
            "UPDATE membership SET status='banned' WHERE user_id=?1",
            [user],
        )
        .unwrap();
    let invite = new_invite(&app, &owner).await;
    let challenge = login_challenge(&app, true, &invite).await;
    assert_eq!(
        login_proof(
            &app,
            &challenge,
            fixture.token(challenge["nonce"].as_str().unwrap())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn explicit_identity_link_is_unique_session_bound_single_use_and_preserves_local_account() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Own identity test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let alice = signup(&app, "Alice", "").await;
    let bob = signup(&app, "Bob", &new_invite(&app, &alice).await).await;
    let challenge = begin(&app, &alice).await;
    assert!(challenge["nonce"]
        .as_str()
        .unwrap()
        .contains(&alice.user.id));
    assert!(finish(
        &app,
        &bob,
        &challenge,
        fixture.token(challenge["nonce"].as_str().unwrap())
    )
    .await
    .0
    .is_client_error());
    let jwt = fixture.token(challenge["nonce"].as_str().unwrap());
    let (status, value) = finish(&app, &alice, &challenge, jwt.clone()).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["linked"]["handle"], "alice_shared");
    assert!(
        !value.to_string().contains("id_token")
            && !value.to_string().contains("own-stable-alice-subject")
    );
    assert!(finish(&app, &alice, &challenge, jwt)
        .await
        .0
        .is_client_error());
    let other = begin(&app, &bob).await;
    assert_eq!(
        finish(
            &app,
            &bob,
            &other,
            fixture.token(other["nonce"].as_str().unwrap())
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, boot) = request(
        &app,
        "GET",
        "/api/bootstrap",
        Some(&alice.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(boot["user"]["id"], alice.user.id);
    assert_eq!(boot["user"]["username"], "Alice");
    assert_eq!(boot["user"]["role"], "admin");
    assert_eq!(boot["linked_identities"][0]["handle"], "alice_shared");
    let (status, _) = request(
        &app,
        "POST",
        "/api/account-link",
        Some(&alice.token),
        json!({"action":"unlink","password":"wrong"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(linked(&lock(&state.0.db), &alice.user.id)
        .unwrap()
        .is_some());
    let (status, _) = request(
        &app,
        "POST",
        "/api/account-link",
        Some(&alice.token),
        json!({"action":"unlink","password":PASSWORD}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(linked(&lock(&state.0.db), &alice.user.id)
        .unwrap()
        .is_none());
    assert!(state
        .auth(&{
            let mut h = HeaderMap::new();
            h.insert(
                header::AUTHORIZATION,
                format!("Bearer {}", alice.token).parse().unwrap(),
            );
            h
        })
        .is_ok());
    assert_eq!(
        lock(&state.0.db)
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn linking_rejects_wrong_signed_claims_and_spends_each_proof_attempt() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Own bad identity test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let alice = signup(&app, "Alice", "").await;
    for case in 0..8 {
        let challenge = begin(&app, &alice).await;
        let nonce = challenge["nonce"].as_str().unwrap();
        let mut claims = fixture.claims(nonce);
        claims = match case {
            0 => claims
                .set_issuer(IssuerUrl::new("https://other.example.test/realm".into()).unwrap()),
            1 => claims.set_audiences(vec![Audience::new("another-app".into())]),
            2 => claims.set_nonce(Some(Nonce::new("another-challenge".into()))),
            3 => claims.set_expiration(chrono::Utc::now() - chrono::Duration::seconds(1)),
            4 => claims.set_auth_time(None),
            5 => claims.set_auth_time(Some(chrono::Utc::now() - chrono::Duration::seconds(600))),
            6 => claims.set_preferred_username(Some(EndUserUsername::new("Alice Bad".into()))),
            _ => claims,
        };
        let signed = token(claims, if case == 7 { 24 } else { 23 });
        let result = finish(&app, &alice, &challenge, signed).await;
        assert_eq!(
            result.0,
            StatusCode::BAD_REQUEST,
            "case{case}: {}",
            result.1
        );
        assert!(
            finish(&app, &alice, &challenge, fixture.token(nonce))
                .await
                .0
                .is_client_error(),
            "failed proofs cannot be replayed"
        );
        assert!(linked(&lock(&state.0.db), &alice.user.id)
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn enabled_local_authenticator_is_required_and_recovery_is_spent_before_browser_approval() {
    let fixture = Fixture::new().await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::open_with_config(
        temp.path(),
        "Own MFA link test",
        0,
        ServerConfig {
            account_provider: Some(fixture.provider.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let app = router(state.clone());
    let alice = signup(&app, "Alice", "").await;
    let (status, setup) = request(
        &app,
        "POST",
        "/api/authenticator",
        Some(&alice.token),
        json!({"action":"setup","password":PASSWORD}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let code = totp_rs::Totp::from_url(setup["otpauth_uri"].as_str().unwrap())
        .unwrap()
        .generate(now())
        .to_string();
    let (status, enabled) = request(
        &app,
        "POST",
        "/api/authenticator",
        Some(&alice.token),
        json!({"action":"enable","password":PASSWORD,"code":code,"setup_id":setup["setup_id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let missing = request(
        &app,
        "POST",
        "/api/account-link",
        Some(&alice.token),
        json!({"action":"begin","password":PASSWORD}),
    )
    .await;
    assert_eq!(missing.0, StatusCode::UNAUTHORIZED);
    assert_eq!(missing.1["error"], AUTHENTICATOR_REQUIRED);
    let recovery = enabled["recovery_codes"][0].as_str().unwrap();
    let payload = json!({"action":"begin","password":PASSWORD,"code":recovery});
    let (status, challenge) = request(
        &app,
        "POST",
        "/api/account-link",
        Some(&alice.token),
        payload.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/account-link",
            Some(&alice.token),
            payload
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        finish(
            &app,
            &alice,
            &challenge,
            fixture.token(challenge["nonce"].as_str().unwrap())
        )
        .await
        .0,
        StatusCode::OK
    );
}
