use super::*;

#[test]
#[cfg(windows)]
fn account_transport_retains_only_typed_policy_denial_through_discovery_and_token_errors() {
    use crate::connection_errors::{hint_or, WINDOWS_CONNECTION_BLOCKED};
    let sanitized = account_transport_error(&io::Error::from_raw_os_error(10013));
    assert_eq!(sanitized.raw_os_error(), Some(10013));
    let discovery = openidconnect::DiscoveryError::Request(sanitized);
    assert_eq!(
        hint_or(&discovery, "discovery fallback"),
        WINDOWS_CONNECTION_BLOCKED
    );
    let token: openidconnect::RequestTokenError<
        io::Error,
        openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
    > = openidconnect::RequestTokenError::Request(account_transport_error(&discovery));
    assert_eq!(
        hint_or(&token, "token fallback"),
        WINDOWS_CONNECTION_BLOCKED
    );
}

#[test]
fn account_transport_redacts_unrelated_errors_and_does_not_infer_a_policy_denial() {
    for error in [
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "https://private.invalid/?token=secret",
        ),
        io::Error::other("socket 10013 in untrusted response text"),
        io::Error::from_raw_os_error(10061),
    ] {
        let sanitized = account_transport_error(&error);
        assert_eq!(sanitized.to_string(), "Relay account request failed");
        assert_eq!(sanitized.raw_os_error(), None);
        let discovery = openidconnect::DiscoveryError::Request(sanitized);
        assert_eq!(
            crate::connection_errors::hint_or(&discovery, "fallback"),
            "fallback"
        );
    }
}

#[test]
fn people_service_destination_requires_https_and_no_embedded_credentials() {
    assert_eq!(
        people_destination("https://accounts.example.test/people/").unwrap(),
        "https://accounts.example.test/people"
    );
    for url in [
        "http://accounts.example.test/people",
        "https://user:secret@accounts.example.test",
        "https://accounts.example.test/people?token=secret",
        "https://accounts.example.test/people#fragment",
        "file:///C:/people",
    ] {
        assert!(people_destination(url).is_err());
    }
}
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use openidconnect::{
    core::{CoreEdDsaPrivateSigningKey, CoreIdToken, CoreIdTokenClaims},
    AccessToken, Audience, EmptyAdditionalClaims, EndUserUsername, JsonWebKeySet,
    PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

fn provider(url: &str) -> Provider {
    Provider {
        issuer: Url::parse(url).unwrap(),
        client_id: "relay-desktop".into(),
        fixture_http: url.starts_with("http://127.0.0.1:"),
    }
}

// Deterministic, test-only Ed25519 keys. No operating-system/user keys are read.
fn key(seed: u8) -> CoreEdDsaPrivateSigningKey {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&[seed; 32]);
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
        STANDARD.encode(der)
    );
    CoreEdDsaPrivateSigningKey::from_ed25519_pem(&pem, None).unwrap()
}

fn token_json(claims: CoreIdTokenClaims, key: &CoreEdDsaPrivateSigningKey) -> Value {
    let access = AccessToken::new("own-fixture-access-token".into());
    let id = CoreIdToken::new(
        claims,
        key,
        CoreJwsSigningAlgorithm::EdDsa,
        Some(&access),
        None,
    )
    .unwrap();
    json!({"access_token":access.secret(),"token_type":"Bearer","expires_in":300,"id_token":id,"refresh_token":"own-fixture-refresh-token"})
}

fn claims(
    provider: &Provider,
    nonce: &str,
    subject: &str,
    handle: &str,
    seconds: i64,
) -> CoreIdTokenClaims {
    let now = chrono::Utc::now();
    CoreIdTokenClaims::new(
        IssuerUrl::new(provider.issuer.to_string()).unwrap(),
        vec![Audience::new(provider.client_id.clone())],
        now + chrono::Duration::seconds(seconds),
        now,
        StandardClaims::new(SubjectIdentifier::new(subject.into())),
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(nonce.into())))
    .set_preferred_username(Some(EndUserUsername::new(handle.into())))
}

#[test]
fn provider_configuration_is_explicit_https_and_same_origin_only() {
    let good = Provider::new(
        "https://accounts.example.test/realms/relay-accounts/",
        "relay-desktop",
    )
    .unwrap();
    assert_eq!(
        good.issuer.as_str(),
        "https://accounts.example.test/realms/relay-accounts"
    );
    assert!(good
        .allows(&Url::parse("https://accounts.example.test/realms/relay-accounts/keys").unwrap()));
    for url in [
        "http://accounts.example.test/realms/relay-accounts",
        "file:///C:/anything",
        "https://user:secret@accounts.example.test/realms/relay-accounts",
        "https://accounts.example.test/realms/relay-accounts?other=1",
        "https://accounts.example.test/realms/relay-accounts#fragment",
    ] {
        assert!(Provider::new(url, "relay-desktop").is_err());
    }
    for url in [
        "http://127.0.0.1/keys",
        "https://elsewhere.example.test/keys",
        "https://accounts.example.test:444/keys",
        "https://user@accounts.example.test/keys",
        "https://accounts.example.test/keys?q=1",
    ] {
        assert!(!good.allows(&Url::parse(url).unwrap()));
    }
    for handle in ["alice_shared", "a12", "very_long_name_123456789"] {
        assert!(handle_valid(handle));
    }
    for handle in [
        "ab",
        "Alice",
        "1alice",
        "alice.bob",
        "æble",
        "alice\n",
        "toolong_name_1234567890123456",
    ] {
        assert!(!handle_valid(handle));
    }
}

/// Run only against token material from our explicitly started loopback provider.
/// No user profile, browser session or public identity service is consulted.
#[test]
#[ignore = "requires an owned, live Keycloak fixture and fresh code-flow tokens"]
fn owned_keycloak_tokens_and_refresh_pass_the_normal_identity_verifier() {
    let path = std::env::var("RELAY_OWN_OIDC_TOKEN_FIXTURE")
        .expect("explicit own token fixture path is required");
    let path = std::path::Path::new(&path);
    assert!(path.is_absolute());
    assert!(std::fs::metadata(path).unwrap().len() < 64 * 1024);
    let fixture: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let provider = provider(fixture["issuer"].as_str().unwrap());
    assert!(provider.fixture_http);
    assert_eq!(provider.issuer.path(), "/realms/relay-accounts");
    assert!(provider.issuer.query().is_none() && provider.issuer.fragment().is_none());
    let http = Transport {
        client: reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
        provider: provider.clone(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let metadata = CoreProviderMetadata::discover(
        &IssuerUrl::new(provider.issuer.to_string()).unwrap(),
        &http,
    )
    .expect("own provider discovery and JWKS");
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(provider.client_id.clone()),
        None,
    );
    let nonce = Nonce::new(fixture["nonce"].as_str().unwrap().to_owned());
    let tokens: CoreTokenResponse = serde_json::from_value(fixture["tokens"].clone()).unwrap();
    let refreshed: CoreTokenResponse =
        serde_json::from_value(fixture["refreshed"].clone()).unwrap();
    let account = checked_account(
        &tokens,
        &client.id_token_verifier(),
        &nonce,
        &provider,
        false,
    )
    .expect("actual Keycloak code-flow identity");
    let next = checked_account(
        &refreshed,
        &client.id_token_verifier(),
        &nonce,
        &provider,
        true,
    )
    .expect("actual Keycloak refreshed identity");
    assert_eq!(account.handle, fixture["expected_handle"].as_str().unwrap());
    assert_eq!(next.handle, account.handle);
    assert_eq!(next.subject, account.subject);
    assert_eq!(next.issuer, account.issuer);
}

#[test]
fn callback_requires_exact_host_path_unique_parameters_and_current_state() {
    let provider = provider("https://accounts.example.test/realms/relay-accounts");
    let state = CsrfToken::new("private-state-for-this-attempt".into());
    let host = "127.0.0.1:4583";
    let callback = |target: &str, headers: &str| {
        format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n{headers}\r\n").into_bytes()
    };
    let good = format!("{CALLBACK_PATH}?code=our-code&state={}", state.secret());
    assert!(matches!(
        parse_callback(&callback(&good, ""), host, &state, &provider),
        Ok(Callback::Code(_))
    ));
    for target in [
        format!("{good}&code=second"),
        format!("{good}&state=second"),
        format!("{good}&error=denied"),
        format!("{good}&iss=https%3A%2F%2Fwrong.test"),
        format!("{CALLBACK_PATH}?code=bad&state=someone-else"),
        format!("//other.test{good}"),
        format!("/wrong?code=our-code&state={}", state.secret()),
        format!("{good}#ignored"),
    ] {
        assert!(
            parse_callback(&callback(&target, ""), host, &state, &provider).is_err(),
            "{target}"
        );
    }
    assert!(parse_callback(
        &callback(&good, "Host: other.test\r\n"),
        host,
        &state,
        &provider
    )
    .is_err());
    assert!(parse_callback(
        &callback(&good, "Transfer-Encoding: chunked\r\n"),
        host,
        &state,
        &provider
    )
    .is_err());
    assert!(parse_callback(
        &callback(&good, "Content-Length: 4\r\n"),
        host,
        &state,
        &provider
    )
    .is_err());
    let denied = format!(
        "{CALLBACK_PATH}?error=access_denied&state={}",
        state.secret()
    );
    assert!(matches!(
        parse_callback(&callback(&denied, ""), host, &state, &provider),
        Ok(Callback::Denied)
    ));
}

#[test]
fn only_signed_current_receiver_and_nonce_bound_identities_become_accounts() {
    let provider = provider("https://accounts.example.test/realms/relay-accounts");
    let signing = key(7);
    let other = key(8);
    let verifier = CoreIdTokenVerifier::new_public_client(
        ClientId::new(provider.client_id.clone()),
        IssuerUrl::new(provider.issuer.to_string()).unwrap(),
        JsonWebKeySet::new(vec![signing.as_verification_key()]),
    );
    let nonce = Nonce::new("nonce-for-only-this-request".into());
    let original = claims(
        &provider,
        nonce.secret(),
        "stable-subject-a",
        "alice_shared",
        300,
    );
    let good: CoreTokenResponse =
        serde_json::from_value(token_json(original.clone(), &signing)).unwrap();
    let account = checked_account(&good, &verifier, &nonce, &provider, false).unwrap();
    assert_eq!(
        (account.subject.as_str(), account.handle.as_str()),
        ("stable-subject-a", "alice_shared")
    );
    let wrong_claims = [
        original
            .clone()
            .set_issuer(IssuerUrl::new("https://other.example.test/realm".into()).unwrap()),
        original
            .clone()
            .set_audiences(vec![Audience::new("another-app".into())]),
        original
            .clone()
            .set_nonce(Some(Nonce::new("stale-attempt".into()))),
        original.clone().set_nonce(None),
        original
            .clone()
            .set_expiration(chrono::Utc::now() - chrono::Duration::seconds(1)),
        original
            .clone()
            .set_issue_time(chrono::Utc::now() + chrono::Duration::seconds(120)),
        original
            .clone()
            .set_preferred_username(Some(EndUserUsername::new("Display Name".into()))),
    ];
    for claims in wrong_claims {
        let token: CoreTokenResponse =
            serde_json::from_value(token_json(claims, &signing)).unwrap();
        assert!(checked_account(&token, &verifier, &nonce, &provider, false).is_err());
    }
    let wrong_signature: CoreTokenResponse =
        serde_json::from_value(token_json(original.clone(), &other)).unwrap();
    assert!(checked_account(&wrong_signature, &verifier, &nonce, &provider, false).is_err());
    let mut swapped = token_json(original.clone(), &signing);
    swapped["access_token"] = json!("swapped-access-token");
    let swapped: CoreTokenResponse = serde_json::from_value(swapped).unwrap();
    assert!(checked_account(&swapped, &verifier, &nonce, &provider, false).is_err());
    let refreshed: CoreTokenResponse =
        serde_json::from_value(token_json(original.set_nonce(None), &signing)).unwrap();
    assert!(
        checked_account(&refreshed, &verifier, &nonce, &provider, true).is_ok(),
        "OIDC refresh may omit nonce; a supplied nonce must still match"
    );
}

#[derive(Default)]
struct Authorization {
    nonce: String,
    challenge: String,
    exchanges: usize,
    refreshes: usize,
    rotate: bool,
    expected_refresh: Option<String>,
    fail_refreshes: usize,
    reject_refresh: bool,
    offline_supported: bool,
}

struct Fixture {
    provider: Provider,
    state: Arc<Mutex<Authorization>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/realms/relay-accounts",
            listener.local_addr().unwrap()
        );
        let provider = provider(&url);
        let state = Arc::new(Mutex::new(Authorization::default()));
        let shared = state.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let quit = stopped.clone();
        let service = provider.clone();
        let worker = thread::spawn(move || {
            let signing = key(17);
            let public = signing.as_verification_key();
            while !quit.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut data = Vec::new();
                let mut block = [0; 2048];
                let (header_end, length) = loop {
                    let n = stream.read(&mut block).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&block[..n]);
                    if let Some(at) = data.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..at]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|s| s.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (at + 4, length);
                    }
                };
                while data.len() < header_end + length {
                    let n = stream.read(&mut block).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&block[..n]);
                }
                let head = String::from_utf8_lossy(&data[..header_end]);
                let path = head.split_whitespace().nth(1).unwrap();
                let mut status = "200 OK";
                let result = if path.ends_with("/.well-known/openid-configuration") {
                    {
                        let mut metadata = json!({"issuer":url,"authorization_endpoint":format!("{url}/authorize"),"token_endpoint":format!("{url}/token"),"jwks_uri":format!("{url}/keys"),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["EdDSA"],"code_challenge_methods_supported":["S256"]});
                        if shared.lock().unwrap().offline_supported {
                            metadata["scopes_supported"] =
                                json!(["openid", "profile", "offline_access"]);
                        }
                        metadata
                    }
                } else if path.ends_with("/keys") {
                    json!({"keys":[public]})
                } else if path.ends_with("/token") {
                    let params: HashMap<_, _> = url::form_urlencoded::parse(&data[header_end..])
                        .into_owned()
                        .collect();
                    let mut state = shared.lock().unwrap();
                    assert_eq!(
                        params.get("client_id").map(String::as_str),
                        Some("relay-desktop")
                    );
                    if params["grant_type"] == "authorization_code" {
                        assert_eq!(params["code"], "own-fixture-code");
                        assert_eq!(
                            URL_SAFE_NO_PAD
                                .encode(Sha256::digest(params["code_verifier"].as_bytes())),
                            state.challenge
                        );
                        state.exchanges += 1;
                    } else {
                        assert_eq!(params["grant_type"], "refresh_token");
                        assert_eq!(
                            params["refresh_token"],
                            state
                                .expected_refresh
                                .as_deref()
                                .unwrap_or("own-fixture-refresh-token")
                        );
                        state.refreshes += 1;
                    }
                    if params["grant_type"] == "refresh_token" && state.reject_refresh {
                        status = "400 Bad Request";
                        json!({"error":"invalid_grant"})
                    } else if params["grant_type"] == "refresh_token" && state.fail_refreshes > 0 {
                        state.fail_refreshes -= 1;
                        status = "503 Service Unavailable";
                        json!({"error":"temporarily_unavailable"})
                    } else {
                        let mut tokens = token_json(
                            claims(
                                &service,
                                &state.nonce,
                                "stable-subject-a",
                                "alice_shared",
                                if state.refreshes == 0 { 31 } else { 300 },
                            ),
                            &signing,
                        );
                        if params["grant_type"] == "refresh_token" && state.rotate {
                            let rotated = format!("owned-rotated-refresh-{}", state.refreshes);
                            tokens["refresh_token"] = json!(rotated);
                            state.expected_refresh = Some(rotated);
                        }
                        tokens
                    }
                } else {
                    panic!("unexpected fixture request")
                };
                let body = serde_json::to_vec(&result).unwrap();
                write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            provider,
            state,
            stopped,
            worker: Some(worker),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

#[test]
fn real_loopback_code_pkce_signature_and_refresh_flow_never_needs_a_community_password() {
    let fixture = Fixture::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let provider = fixture.provider.clone();
    let (tx, rx) = mpsc::sync_channel(4);
    let worker = thread::spawn(move || run(provider, stop, &tx, 0));
    let Event::Browser(url) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected browser authorization")
    };
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(params["scope"].split_whitespace().any(|s| s == "openid"));
    assert!(!params["scope"].contains("email"));
    {
        let mut state = fixture.state.lock().unwrap();
        state.nonce = params["nonce"].clone();
        state.challenge = params["code_challenge"].clone();
    }
    let mut callback = Url::parse(&params["redirect_uri"]).unwrap();
    assert_eq!(callback.host_str(), Some("127.0.0.1"));
    assert_eq!(callback.path(), CALLBACK_PATH);
    callback
        .query_pairs_mut()
        .append_pair("code", "own-fixture-code")
        .append_pair("state", &params["state"]);
    let response = reqwest::blocking::get(callback).unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let html = response.text().unwrap();
    assert!(!html.contains("own-fixture-code") && !html.contains(&params["state"]));
    let Event::Account(first) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected signed account")
    };
    assert_eq!(first.handle, "alice_shared");
    let Event::Account(refreshed) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("expected renewed account")
    };
    assert_eq!(refreshed.subject, first.subject);
    assert!(refreshed.expires_at > first.expires_at);
    cancel.store(true, Ordering::Release);
    assert!(worker.join().unwrap().is_ok());
    let state = fixture.state.lock().unwrap();
    assert_eq!((state.exchanges, state.refreshes), (1, 1));
}

#[test]
fn linking_uses_the_destination_nonce_max_age_and_same_signed_account_without_a_refresh_session() {
    for same in [true, false] {
        let fixture = Fixture::new();
        let provider = fixture.provider.clone();
        let expected = Account {
            issuer: provider.issuer.to_string(),
            subject: if same {
                "stable-subject-a"
            } else {
                "other-subject"
            }
            .into(),
            handle: "alice_shared".into(),
            expires_at: u64::MAX,
        };
        let requested_nonce = format!(
            "relay-link-v1:own-community:own-local-user:{}:{}",
            "a".repeat(64),
            "b".repeat(43)
        );
        let (tx, rx) = mpsc::sync_channel(4);
        let nonce = requested_nonce.clone();
        let worker = thread::spawn(move || {
            run_flow(
                provider,
                Arc::new(AtomicBool::new(false)),
                &tx,
                0,
                Some(LinkRequest {
                    nonce,
                    expected,
                    recent_auth: true,
                }),
            )
        });
        let Event::Browser(url) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected explicit browser approval");
        };
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["nonce"], requested_nonce);
        assert_eq!(params["max_age"], "300");
        {
            let mut state = fixture.state.lock().unwrap();
            state.nonce = params["nonce"].clone();
            state.challenge = params["code_challenge"].clone();
        }
        let mut callback = Url::parse(&params["redirect_uri"]).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "own-fixture-code")
            .append_pair("state", &params["state"]);
        assert_eq!(reqwest::blocking::get(callback).unwrap().status(), 200);
        let result = worker.join().unwrap();
        if same {
            assert!(result.is_ok());
            let Event::Proof(proof) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
                panic!("expected one destination-bound proof");
            };
            assert!(proof.id_token.len() > 100);
        } else {
            assert!(result.is_err());
            assert!(rx.try_recv().is_err());
        }
        let state = fixture.state.lock().unwrap();
        assert_eq!((state.exchanges, state.refreshes), (1, 0));
    }
}

#[test]
fn cancellation_drops_old_results_and_expired_accounts_require_sign_in_again() {
    let (tx, rx) = mpsc::sync_channel(4);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut controller = Controller {
        provider: Some(provider("https://accounts.example.test/realm")),
        account: None,
        error: None,
        warning: None,
        last_identity: None,
        account_changed: false,
        resuming: false,
        persistence: Arc::new(Persistence::new(None)),
        job: Some(Job {
            cancel: cancel.clone(),
            events: rx,
        }),
        waiting: true,
        browser_ready: false,
        link_proof: None,
    };
    assert!(tx
        .try_send(Event::Account(Account {
            issuer: "https://accounts.example.test/realm".into(),
            subject: "old".into(),
            handle: "old_user".into(),
            expires_at: u64::MAX
        }))
        .is_ok());
    controller.disconnect();
    assert!(cancel.load(Ordering::Acquire));
    assert!(controller.poll().is_none());
    assert!(controller.account.is_none());
    controller.account = Some(Account {
        issuer: "issuer".into(),
        subject: "expired".into(),
        handle: "old_user".into(),
        expires_at: 0,
    });
    controller.poll();
    assert!(controller.account.is_none());
    assert!(controller.error.is_some());
}

/// Real provider + real Relay HTTP server + the normal client code/PKCE worker.
/// The owner submits only its own HTTP login forms and returns the exact local
/// callback. This does not claim native UI or public HTTPS acceptance.
#[test]
#[ignore = "requires an explicitly owned live identity provider and Relay server"]
fn owned_live_global_account_links_to_existing_community_account() {
    use std::io::Write;
    let config = std::env::var("RELAY_OWN_LINK_JOURNEY").expect("own journey config required");
    assert!(config.len() < 8192);
    let config: Value = serde_json::from_str(&config).unwrap();
    let service = provider(config["issuer"].as_str().unwrap());
    assert!(service.fixture_http && service.issuer.path() == "/realms/relay-accounts");
    let community = Url::parse(config["server_url"].as_str().unwrap()).unwrap();
    assert_eq!(community.scheme(), "http");
    assert_eq!(community.host_str(), Some("127.0.0.1"));
    assert!(community.port().is_some() && plain_url(&community));
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let api = |actor: &str, method: &str, path: &str, body: Value| {
        let request = http
            .request(method.parse().unwrap(), community.join(path).unwrap())
            .bearer_auth(config[actor]["token"].as_str().unwrap());
        let response = if method == "GET" {
            request
        } else {
            request.json(&body)
        }
        .send()
        .unwrap();
        (
            response.status().as_u16(),
            response.json::<Value>().unwrap(),
        )
    };
    let authorize = |link: Option<LinkRequest>| {
        let provider = service.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let stopped = cancel.clone();
        let (tx, rx) = mpsc::sync_channel(4);
        let worker = thread::spawn(move || run_flow(provider, stopped, &tx, CALLBACK_PORT, link));
        let Event::Browser(url) = rx.recv_timeout(Duration::from_secs(15)).unwrap() else {
            panic!("expected explicit browser approval");
        };
        println!("RELAY_OWN_LINK_BROWSER={url}");
        std::io::stdout().flush().unwrap();
        let result = rx
            .recv_timeout(Duration::from_secs(45))
            .expect("own callback result");
        cancel.store(true, Ordering::Release);
        assert!(worker.join().unwrap().is_ok());
        result
    };
    let Event::Account(account) = authorize(None) else {
        panic!("expected app-wide signed account");
    };
    assert_eq!(account.handle, "relay_friend");
    let (status, before) = api("alice", "GET", "/api/bootstrap", Value::Null);
    assert_eq!(status, 200);
    let begin = |actor: &str| {
        let (status, value) = api(
            actor,
            "POST",
            "/api/account-link",
            json!({"action":"begin","password":"a-long-test-password"}),
        );
        assert_eq!(status, 200, "{value}");
        value
    };
    let challenge = begin("alice");
    let Event::Proof(proof) = authorize(Some(LinkRequest {
        nonce: challenge["nonce"].as_str().unwrap().into(),
        expected: account.clone(),
        recent_auth: true,
    })) else {
        panic!("expected destination-bound proof");
    };
    let payload = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(proof.id_token.split('.').nth(1).unwrap())
            .unwrap(),
    );
    let claims: Value = serde_json::from_slice(&payload).unwrap();
    println!(
        "OWN_LINK_PROOF_METADATA={}",
        json!({
            "auth_time_present":claims["auth_time"].is_number(),
            "issuer_matches":claims["iss"]==config["issuer"],
            "nonce_matches":claims["nonce"]==challenge["nonce"],
            "iat_age_seconds":chrono::Utc::now().timestamp()-claims["iat"].as_i64().unwrap(),
            "auth_age_seconds":claims["auth_time"].as_i64().map(|t|chrono::Utc::now().timestamp()-t)
        })
    );
    assert!(
        claims["auth_time"].is_number(),
        "own provider omitted required recent auth_time"
    );
    let body = json!({"action":"link","challenge_id":challenge["challenge_id"],"id_token":proof.id_token.as_str()});
    let (status, linked) = api("alice", "POST", "/api/account-link", body.clone());
    assert_eq!(status, 200, "{linked}");
    assert_eq!(linked["linked"]["handle"], "relay_friend");
    assert_eq!(api("alice", "POST", "/api/account-link", body).0, 400);
    let (status, after) = api("alice", "GET", "/api/bootstrap", Value::Null);
    assert_eq!(status, 200);
    assert_eq!(
        before["user"], after["user"],
        "linking must not replace local identity/role/profile"
    );
    assert_eq!(after["linked_identities"][0]["handle"], "relay_friend");
    let challenge = begin("bob");
    let Event::Proof(proof) = authorize(Some(LinkRequest {
        nonce: challenge["nonce"].as_str().unwrap().into(),
        expected: account,
        recent_auth: true,
    })) else {
        panic!("expected Bob-bound proof for the same global account");
    };
    assert_eq!(api("bob", "POST", "/api/account-link", json!({"action":"link","challenge_id":challenge["challenge_id"],"id_token":proof.id_token.as_str()})).0,409);
    let (status, value) = api(
        "alice",
        "POST",
        "/api/account-link",
        json!({"action":"unlink","password":"a-long-test-password"}),
    );
    assert_eq!(status, 200, "{value}");
    assert!(value["linked"].is_null());
    println!("OWN_LINK_JOURNEY_OK: app sign-in, destination-bound link, bootstrap, replay rejection, duplicate identity rejection, unlink; local user preserved");
}

static PERSISTENCE_TEST_LOCK: Mutex<()> = Mutex::new(());
fn wait_account_workers() {
    let deadline = Instant::now() + Duration::from_secs(3);
    while WORKERS.load(Ordering::Acquire) != 0 {
        assert!(
            Instant::now() < deadline,
            "Owned account worker did not stop"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
#[derive(Default)]
struct MemorySessionStore {
    saved: Mutex<Option<SavedSession>>,
    fail_save: AtomicBool,
}
impl SessionStore for MemorySessionStore {
    fn load(&self) -> Result<Option<SavedSession>> {
        Ok(self.saved.lock().unwrap().clone())
    }
    fn save(&self, saved: &SavedSession) -> Result<()> {
        if self.fail_save.load(Ordering::Acquire) {
            return Err(SAVE_WARNING);
        }
        *self.saved.lock().unwrap() = Some(saved.clone());
        Ok(())
    }
    fn forget(&self) -> Result<()> {
        *self.saved.lock().unwrap() = None;
        Ok(())
    }
}
fn saved_fixture(fixture: &Fixture) -> SavedSession {
    fixture.state.lock().unwrap().nonce = "owned-remembered-nonce".into();
    SavedSession {
        issuer: fixture.provider.issuer.to_string(),
        client_id: fixture.provider.client_id.clone(),
        subject: "stable-subject-a".into(),
        nonce: "owned-remembered-nonce".into(),
        refresh_token: "own-fixture-refresh-token".into(),
    }
}
fn remembered_controller(provider: Provider, store: Arc<MemorySessionStore>) -> Controller {
    Controller {
        provider: Some(provider),
        account: None,
        error: None,
        warning: None,
        persistence: Arc::new(Persistence {
            state: Mutex::new(PersistenceState {
                enabled: false,
                current: None,
            }),
            store,
        }),
        last_identity: None,
        account_changed: false,
        resuming: false,
        job: None,
        waiting: false,
        browser_ready: false,
        link_proof: None,
    }
}
fn restored_account(controller: &mut Controller) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while controller.account.is_none() {
        assert!(
            controller.poll().is_none(),
            "Silent restoration must not open a browser"
        );
        assert!(controller.error.is_none(), "Owned restoration failed");
        assert!(Instant::now() < deadline, "Owned restoration timed out");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        controller.account.as_ref().unwrap().subject,
        "stable-subject-a"
    );
    assert!(!controller.resuming());
}

#[test]
fn remembered_account_restores_without_browser_and_keeps_rotated_token_across_restart() {
    let _guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
    wait_account_workers();
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().rotate = true;
    let store = Arc::new(MemorySessionStore::default());
    *store.saved.lock().unwrap() = Some(saved_fixture(&fixture));
    let mut first = remembered_controller(fixture.provider.clone(), store.clone());
    assert!(first.resume(egui::Context::default(), true));
    assert!(first.resuming() && first.waiting());
    restored_account(&mut first);
    assert_eq!(
        store.saved.lock().unwrap().as_ref().unwrap().refresh_token,
        "owned-rotated-refresh-1"
    );
    first.disconnect();
    assert!(
        store.saved.lock().unwrap().is_some(),
        "Cancel must keep remembered sign-in"
    );
    drop(first);
    wait_account_workers();
    let mut second = remembered_controller(fixture.provider.clone(), store.clone());
    assert!(second.resume(egui::Context::default(), true));
    restored_account(&mut second);
    assert_eq!(
        store.saved.lock().unwrap().as_ref().unwrap().refresh_token,
        "owned-rotated-refresh-2"
    );
    assert_eq!(fixture.state.lock().unwrap().exchanges, 0);
    second.forget().unwrap();
    assert!(store.saved.lock().unwrap().is_none());
}

#[test]
fn temporary_refresh_failure_keeps_saved_signin_and_retries_without_browser() {
    let _guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
    wait_account_workers();
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_refreshes = 1;
    let store = Arc::new(MemorySessionStore::default());
    *store.saved.lock().unwrap() = Some(saved_fixture(&fixture));
    let mut controller = remembered_controller(fixture.provider.clone(), store.clone());
    assert!(controller.resume(egui::Context::default(), true));
    let deadline = Instant::now() + Duration::from_secs(3);
    while controller.warning.is_none() {
        assert!(controller.poll().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(controller.resuming() && controller.waiting());
    assert!(store.saved.lock().unwrap().is_some());
    restored_account(&mut controller);
    assert!(controller.warning.is_none());
    assert_eq!(fixture.state.lock().unwrap().refreshes, 2);
}

#[test]
fn revoked_refresh_requires_signin_and_forgets_only_that_central_session() {
    let _guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
    wait_account_workers();
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().reject_refresh = true;
    let store = Arc::new(MemorySessionStore::default());
    *store.saved.lock().unwrap() = Some(saved_fixture(&fixture));
    let mut controller = remembered_controller(fixture.provider.clone(), store.clone());
    assert!(controller.resume(egui::Context::default(), true));
    let deadline = Instant::now() + Duration::from_secs(3);
    while controller.error.is_none() {
        assert!(controller.poll().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!controller.resuming() && !controller.waiting());
    assert!(controller.account.is_none());
    assert!(store.saved.lock().unwrap().is_none());
}

#[test]
fn remember_choice_changes_during_signin_and_cancelled_workers_cannot_restore_it() {
    let fixture = Fixture::new();
    let saved = saved_fixture(&fixture);
    let store = Arc::new(MemorySessionStore::default());
    let mut controller = remembered_controller(fixture.provider.clone(), store.clone());
    let cancel = AtomicBool::new(false);
    controller
        .persistence
        .update(saved.clone(), &cancel)
        .unwrap();
    assert!(store.saved.lock().unwrap().is_none());
    controller.set_remember(true).unwrap();
    assert!(store.saved.lock().unwrap().is_some());
    controller.set_remember(false).unwrap();
    controller
        .persistence
        .update(saved.clone(), &cancel)
        .unwrap();
    assert!(store.saved.lock().unwrap().is_none());
    controller.set_remember(true).unwrap();
    controller.forget().unwrap();
    cancel.store(true, Ordering::Release);
    controller.persistence.update(saved, &cancel).unwrap();
    assert!(store.saved.lock().unwrap().is_none());
    assert!(controller
        .persistence
        .state
        .lock()
        .unwrap()
        .current
        .is_none());
}

#[test]
fn vault_save_failure_keeps_ram_session_and_is_a_warning() {
    let fixture = Fixture::new();
    let store = Arc::new(MemorySessionStore::default());
    store.fail_save.store(true, Ordering::Release);
    let mut controller = remembered_controller(fixture.provider.clone(), store.clone());
    controller.set_remember(true).unwrap();
    assert!(controller
        .persistence
        .update(saved_fixture(&fixture), &AtomicBool::new(false))
        .is_err());
    assert!(controller
        .persistence
        .state
        .lock()
        .unwrap()
        .current
        .is_some());
    assert!(controller.set_remember(true).is_err());
    assert_eq!(controller.warning, Some(SAVE_WARNING));
    assert!(controller.error.is_none());
}

#[test]
#[cfg(windows)]
fn windows_vault_owned_refresh_survives_two_silent_controller_restarts() {
    let _guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
    wait_account_workers();
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().rotate = true;
    // Unique owned loopback issuer/purpose target: never reads a real account.
    struct Cleanup(VaultStore);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.forget();
        }
    }
    let _cleanup = Cleanup(VaultStore(Some(fixture.provider.clone())));
    let mut initial = Controller::owned(fixture.provider.clone());
    initial.set_remember(true).unwrap();
    initial
        .persistence
        .update(saved_fixture(&fixture), &AtomicBool::new(false))
        .unwrap();
    drop(initial);
    let mut first = Controller::owned(fixture.provider.clone());
    assert!(first.resume(egui::Context::default(), true));
    restored_account(&mut first);
    drop(first);
    wait_account_workers();
    let mut second = Controller::owned(fixture.provider.clone());
    assert!(second.resume(egui::Context::default(), true));
    restored_account(&mut second);
    assert_eq!(fixture.state.lock().unwrap().refreshes, 2);
    assert_eq!(fixture.state.lock().unwrap().exchanges, 0);
    second.forget().unwrap();
    assert!(VaultStore(Some(fixture.provider.clone()))
        .load()
        .unwrap()
        .is_none());
}

#[test]
fn replacement_signed_subject_signals_old_community_sessions_must_be_removed() {
    let fixture = Fixture::new();
    let mut controller = remembered_controller(
        fixture.provider.clone(),
        Arc::new(MemorySessionStore::default()),
    );
    controller.last_identity = Some((fixture.provider.issuer.to_string(), "old-subject".into()));
    let (tx, rx) = mpsc::sync_channel(4);
    controller.job = Some(Job {
        cancel: Arc::new(AtomicBool::new(false)),
        events: rx,
    });
    tx.send(Event::Account(Account {
        issuer: fixture.provider.issuer.to_string(),
        subject: "new-subject".into(),
        handle: "same_handle".into(),
        expires_at: u64::MAX,
    }))
    .unwrap();
    assert!(controller.poll().is_none());
    assert!(controller.take_account_change());
    assert!(!controller.take_account_change());
}

#[test]
fn persistent_signin_keeps_normal_scopes_even_when_offline_access_is_advertised() {
    for advertised in [false, true] {
        let fixture = Fixture::new();
        fixture.state.lock().unwrap().offline_supported = advertised;
        let store = Arc::new(MemorySessionStore::default());
        let persistence = Arc::new(Persistence {
            state: Mutex::new(PersistenceState {
                enabled: true,
                current: None,
            }),
            store,
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let provider = fixture.provider.clone();
        let (tx, rx) = mpsc::sync_channel(4);
        let worker = thread::spawn(move || {
            run_flow_persistent(
                provider,
                worker_cancel,
                &tx,
                0,
                None,
                false,
                Some(persistence),
            )
        });
        let Event::Browser(url) = rx.recv_timeout(Duration::from_secs(3)).unwrap() else {
            panic!("Expected owned authorization URL")
        };
        let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
        let scopes: std::collections::BTreeSet<_> =
            parameters["scope"].split_whitespace().collect();
        assert_eq!(
            scopes,
            std::collections::BTreeSet::from(["openid", "profile"])
        );
        cancel.store(true, Ordering::Release);
        let _ = worker.join().unwrap();
    }
}

#[test]
fn signed_identity_delivery_survives_a_temporarily_full_ui_queue() {
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Event::Warning(Some(RETRY_WARNING))).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let worker = thread::spawn(move || {
        deliver_account(
            &tx,
            &worker_cancel,
            Account {
                issuer: "https://accounts.example/realm".into(),
                subject: "owned-subject".into(),
                handle: "owned_user".into(),
                expires_at: u64::MAX,
            },
        )
    });
    thread::sleep(Duration::from_millis(120));
    assert!(!worker.is_finished());
    assert!(matches!(rx.recv().unwrap(), Event::Warning(_)));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Event::Account(_)
    ));
    assert!(worker.join().unwrap().is_ok());
}

#[test]
fn saved_subject_and_original_nonce_remain_checked_during_silent_restoration() {
    let _guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
    for change_subject in [false, true] {
        wait_account_workers();
        let fixture = Fixture::new();
        let store = Arc::new(MemorySessionStore::default());
        let mut saved = saved_fixture(&fixture);
        if change_subject {
            saved.subject = "other-subject".into();
        } else {
            saved.nonce = "other-original-nonce".into();
        }
        *store.saved.lock().unwrap() = Some(saved);
        let mut controller = remembered_controller(fixture.provider.clone(), store.clone());
        assert!(controller.resume(egui::Context::default(), true));
        let deadline = Instant::now() + Duration::from_secs(3);
        while controller.error.is_none() {
            assert!(controller.poll().is_none());
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(controller.account.is_none());
        assert!(store.saved.lock().unwrap().is_none());
    }
}
