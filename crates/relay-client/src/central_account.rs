//! Optional app-wide OIDC account. Community logins and chat keys are independent.
//! Credentials remain in the identity page; remembered refresh tokens use the OS vault.
use eframe::egui;
use openidconnect::{
    core::{
        CoreAuthenticationFlow, CoreClient, CoreIdTokenVerifier, CoreJwsSigningAlgorithm,
        CoreProviderMetadata, CoreTokenResponse,
    },
    AccessTokenHash, AuthorizationCode, ClientId, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, RedirectUrl, Scope, SyncHttpClient, TokenResponse,
};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use url::Url;
use zeroize::Zeroizing;

const CALLBACK_PATH: &str = "/relay-account/callback";
const CALLBACK_PORT: u16 = 4583;
// The in-app form can be waiting before the user begins typing, and enrollment
// or a second factor needs time. Cancellation still stops the listener promptly.
const SIGNIN_TIMEOUT: Duration = Duration::from_secs(600);
const RESPONSE_LIMIT: u64 = 256 * 1024;
static WORKERS: AtomicUsize = AtomicUsize::new(0);

type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, PartialEq, Eq)]
pub struct Provider {
    issuer: Url,
    client_id: String,
    #[cfg(test)]
    fixture_http: bool,
}

impl Provider {
    pub fn configured() -> Result<Option<Self>> {
        let issuer = std::env::var("RELAY_ACCOUNT_ISSUER")
            .ok()
            .or_else(|| option_env!("RELAY_ACCOUNT_ISSUER").map(str::to_owned));
        let Some(issuer) = issuer.filter(|value| !value.trim().is_empty()) else {
            return Ok(None);
        };
        let client = std::env::var("RELAY_ACCOUNT_CLIENT_ID")
            .ok()
            .or_else(|| option_env!("RELAY_ACCOUNT_CLIENT_ID").map(str::to_owned))
            .unwrap_or_else(|| "relay-desktop".into());
        Self::new(&issuer, &client).map(Some)
    }

    pub fn new(issuer: &str, client_id: &str) -> Result<Self> {
        let issuer = Url::parse(issuer.trim().trim_end_matches('/'))
            .map_err(|_| "The Relay account service address is invalid.")?;
        if (issuer.scheme() != "https" && !qa_loopback(&issuer))
            || !plain_url(&issuer)
            || issuer.host_str().is_none()
            || issuer.path() == "/"
            || client_id.is_empty()
            || client_id.len() > 128
            || client_id
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(
                "Relay accounts require a configured HTTPS identity service and public client ID.",
            );
        }
        Ok(Self {
            issuer,
            client_id: client_id.into(),
            #[cfg(test)]
            fixture_http: false,
        })
    }

    /// One independently hosted Friends service beside the account provider.
    /// The address is explicit; never adopt a service supplied by a community.
    pub fn people_url(&self) -> Result<String> {
        #[cfg(test)]
        if self.fixture_http {
            // Explicit owned-loopback fixtures never contact a configured
            // production Friends service, including inherited environment.
            assert_eq!(self.issuer.scheme(), "http");
            assert_eq!(self.issuer.host_str(), Some("127.0.0.1"));
            assert!(self.issuer.port().is_some());
            return Ok(format!("{}/people", self.issuer.origin().ascii_serialization()));
        }
        let configured = std::env::var("RELAY_PEOPLE_URL")
            .ok()
            .or_else(|| option_env!("RELAY_PEOPLE_URL").map(str::to_owned));
        let url = configured
            .unwrap_or_else(|| format!("{}/people", self.issuer.origin().ascii_serialization()));
        people_destination(&url)
    }

    pub fn service(&self) -> &str {
        self.issuer.host_str().unwrap_or("Relay account service")
    }
    pub fn management_url(&self) -> String {
        format!("{}/account/", self.issuer.as_str().trim_end_matches('/'))
    }
    pub fn configure_host(&self, command: &mut std::process::Command) {
        command
            .env("RELAY_ACCOUNT_ISSUER", self.issuer.as_str())
            .env("RELAY_ACCOUNT_CLIENT_ID", &self.client_id);
    }

    pub fn matches(&self, service: &relay_protocol::AccountProvider) -> bool {
        self.issuer.as_str() == service.issuer && self.client_id == service.client_id
    }

    #[cfg(all(test, windows))]
    pub(crate) fn owned_loopback_fixture(issuer: &str) -> Self {
        let issuer = Url::parse(issuer).expect("own fixture issuer");
        assert_eq!(issuer.scheme(), "http");
        assert_eq!(issuer.host_str(), Some("127.0.0.1"));
        assert_eq!(issuer.path(), "/realms/relay-accounts");
        assert!(issuer.port().is_some() && plain_url(&issuer));
        Self {
            issuer,
            client_id: "relay-desktop".into(),
            fixture_http: true,
        }
    }

    fn allows(&self, url: &Url) -> bool {
        let secure = url.scheme() == "https" || qa_loopback(url);
        #[cfg(test)]
        let secure = secure
            || (self.fixture_http && url.scheme() == "http" && url.host_str() == Some("127.0.0.1"));
        secure && plain_url(url) && url.origin() == self.issuer.origin()
    }
}

fn people_destination(url: &str) -> Result<String> {
    let parsed = Url::parse(url).map_err(|_| "The Friends service address is invalid.")?;
    if (parsed.scheme() != "https" && !qa_loopback(&parsed))
        || !plain_url(&parsed)
        || parsed.host_str().is_none()
    {
        return Err("Friends require a configured HTTPS service.");
    }
    Ok(parsed.as_str().trim_end_matches('/').into())
}

/// QA builds only (`qa-loopback-identity`): an owned loopback identity service
/// on 127.0.0.1 with an explicit port may use plain HTTP. Never in a release.
pub(crate) fn qa_loopback(url: &Url) -> bool {
    cfg!(feature = "qa-loopback-identity")
        && url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some()
}

fn plain_url(url: &Url) -> bool {
    url.as_str().len() <= 4096
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.query().is_none()
}

#[derive(Clone)]
pub struct Account {
    pub issuer: String,
    pub subject: String,
    pub handle: String,
    pub expires_at: u64,
}

fn handle_valid(handle: &str) -> bool {
    (3..=24).contains(&handle.len())
        && handle.as_bytes()[0].is_ascii_lowercase()
        && handle
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

struct Transport {
    client: reqwest::blocking::Client,
    provider: Provider,
    cancelled: Arc<AtomicBool>,
}

impl SyncHttpClient for Transport {
    type Error = io::Error;
    fn call(&self, request: HttpRequest) -> io::Result<HttpResponse> {
        let fail = || io::Error::other("Relay account request failed");
        let url = Url::parse(&request.uri().to_string()).map_err(|_| fail())?;
        if self.cancelled.load(Ordering::Acquire)
            || !self.provider.allows(&url)
            || request.body().len() > 16 * 1024
        {
            return Err(fail());
        }
        let (parts, body) = request.into_parts();
        let response = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .body(body)
            .send()
            .map_err(|error| account_transport_error(&error))?;
        if response.status().is_redirection()
            || response
                .content_length()
                .is_some_and(|n| n > RESPONSE_LIMIT)
        {
            return Err(fail());
        }
        let mut result = openidconnect::http::Response::builder().status(response.status());
        for (key, value) in response.headers() {
            result = result.header(key, value);
        }
        let mut bytes = Vec::new();
        response
            .take(RESPONSE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| fail())?;
        if bytes.len() as u64 > RESPONSE_LIMIT || self.cancelled.load(Ordering::Acquire) {
            return Err(fail());
        }
        result.body(bytes).map_err(|_| fail())
    }
}

fn account_transport_error(error: &(dyn std::error::Error + 'static)) -> io::Error {
    if crate::connection_errors::windows_connection_blocked(error) {
        // Preserve only the actionable typed cause through OIDC's error chain.
        // Keeping reqwest's original error would retain its URL and parameters.
        io::Error::from_raw_os_error(10013)
    } else {
        io::Error::other("Relay account request failed")
    }
}

enum Callback {
    Code(Zeroizing<String>),
    Denied,
}

fn parse_callback(
    bytes: &[u8],
    host: &str,
    state: &CsrfToken,
    provider: &Provider,
) -> Result<Callback> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Invalid sign-in callback.")?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or("Incomplete sign-in callback.")?;
    if !body.is_empty() {
        return Err("Invalid sign-in callback body.");
    }
    let mut lines = head.split("\r\n");
    let mut first = lines
        .next()
        .ok_or("Invalid sign-in callback.")?
        .split_whitespace();
    let (Some("GET"), Some(target), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err("Invalid sign-in callback request.");
    };
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || !target.starts_with(&format!("{CALLBACK_PATH}?"))
    {
        return Err("Invalid sign-in callback path.");
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (key, value) = line
            .split_once(':')
            .ok_or("Invalid sign-in callback header.")?;
        if headers
            .insert(key.to_ascii_lowercase(), value.trim())
            .is_some()
        {
            return Err("Repeated sign-in callback header.");
        }
    }
    if headers.get("host") != Some(&host)
        || headers.contains_key("transfer-encoding")
        || headers
            .get("content-length")
            .is_some_and(|value| *value != "0")
    {
        return Err("Invalid sign-in callback origin.");
    }
    let url = Url::parse(&format!("http://{host}{target}"))
        .map_err(|_| "Invalid sign-in callback URL.")?;
    if url.fragment().is_some() || url.path() != CALLBACK_PATH {
        return Err("Invalid sign-in callback path.");
    }
    let mut params = HashMap::new();
    for (key, value) in url.query_pairs() {
        if params
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err("Repeated sign-in callback parameter.");
        }
    }
    let returned_state = params
        .get("state")
        .ok_or("Missing sign-in callback state.")?;
    if !bool::from(returned_state.as_bytes().ct_eq(state.secret().as_bytes())) {
        return Err("Sign-in callback belongs to another attempt.");
    }
    if params
        .get("iss")
        .is_some_and(|issuer| issuer != provider.issuer.as_str())
    {
        return Err("Sign-in callback belongs to another service.");
    }
    match (params.remove("code"), params.remove("error")) {
        (Some(code), None) if !code.is_empty() && code.len() <= 4096 => {
            Ok(Callback::Code(Zeroizing::new(code)))
        }
        (None, Some(_)) => Ok(Callback::Denied),
        _ => Err("Invalid sign-in callback result."),
    }
}

fn reply(stream: &mut TcpStream, accepted: bool) {
    let body = if accepted {
        "<!doctype html><title>Relay account</title><h1>Return to Relay</h1><p>You can close this tab. Relay will check the sign-in result.</p>"
    } else {
        "<!doctype html><title>Relay account</title><h1>Sign-in request not accepted</h1><p>Return to Relay and continue the sign-in you started there.</p>"
    };
    let status = if accepted {
        "200 OK"
    } else {
        "400 Bad Request"
    };
    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}", body.len());
}

fn wait_for_callback(
    listener: &TcpListener,
    state: &CsrfToken,
    provider: &Provider,
    cancelled: &AtomicBool,
) -> Result<Zeroizing<String>> {
    listener
        .set_nonblocking(true)
        .map_err(|_| "Cannot listen for Relay account sign-in.")?;
    let host = listener
        .local_addr()
        .map_err(|_| "Cannot identify Relay account callback.")?
        .to_string();
    let deadline = Instant::now() + SIGNIN_TIMEOUT;
    let mut requests = 0;
    while Instant::now() < deadline && !cancelled.load(Ordering::Acquire) {
        let mut stream = match listener.accept() {
            Ok((stream, address)) if address.ip().is_loopback() => stream,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(_) => return Err("The sign-in callback stopped. Try again."),
        };
        requests += 1;
        if requests > 64 {
            return Err("Too many invalid local sign-in callbacks. Start again.");
        }
        stream
            .set_nonblocking(false)
            .and_then(|()| stream.set_read_timeout(Some(Duration::from_millis(200))))
            .and_then(|()| stream.set_write_timeout(Some(Duration::from_millis(200))))
            .map_err(|_| "Cannot read the Relay account sign-in callback.")?;
        let read_deadline = Instant::now() + Duration::from_secs(2);
        let mut bytes = Zeroizing::new(Vec::new());
        let mut chunk = [0u8; 1024];
        while bytes.len() < 8192
            && Instant::now() < read_deadline
            && !cancelled.load(Ordering::Acquire)
        {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            }
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let callback = parse_callback(&bytes, &host, state, provider);
        reply(&mut stream, callback.is_ok());
        match callback {
            Ok(Callback::Code(code)) => return Ok(code),
            Ok(Callback::Denied) => return Err("Browser sign-in was cancelled."),
            Err(_) => {}
        }
    }
    Err(if cancelled.load(Ordering::Acquire) {
        "Sign-in cancelled."
    } else {
        "Browser sign-in timed out. Try again."
    })
}

fn checked_account(
    tokens: &CoreTokenResponse,
    verifier: &CoreIdTokenVerifier<'_>,
    nonce: &Nonce,
    provider: &Provider,
    refreshing: bool,
) -> Result<Account> {
    let invalid = "The account service returned an invalid identity. Sign in again.";
    let verifier = verifier
        .clone()
        .set_allowed_algs([
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            CoreJwsSigningAlgorithm::EdDsa,
        ])
        .set_issue_time_verifier_fn(|issued| {
            if issued > chrono::Utc::now() + chrono::Duration::seconds(60) {
                Err("Identity issued in the future.".into())
            } else {
                Ok(())
            }
        });
    let id = tokens.id_token().ok_or(invalid)?;
    let claims = id
        .claims(&verifier, |returned: Option<&Nonce>| {
            if returned.is_some_and(|value| {
                bool::from(value.secret().as_bytes().ct_eq(nonce.secret().as_bytes()))
            }) || (refreshing && returned.is_none())
            {
                Ok(())
            } else {
                Err("The identity nonce does not match this sign-in.".into())
            }
        })
        .map_err(|_| invalid)?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            tokens.access_token(),
            id.signing_alg().map_err(|_| invalid)?,
            id.signing_key(&verifier).map_err(|_| invalid)?,
        )
        .map_err(|_| invalid)?;
        if !bool::from(
            expected
                .as_str()
                .as_bytes()
                .ct_eq(actual.as_str().as_bytes()),
        ) {
            return Err(invalid);
        }
    }
    let handle = claims
        .preferred_username()
        .ok_or("This account needs a unique Relay username at the account service.")?
        .as_str();
    let subject = claims.subject().as_str();
    if !handle_valid(handle)
        || subject.is_empty()
        || subject.len() > 255
        || subject.chars().any(char::is_control)
    {
        return Err("The account service must supply a valid unique Relay username.");
    }
    Ok(Account {
        issuer: provider.issuer.to_string(),
        subject: subject.into(),
        handle: handle.into(),
        expires_at: claims.expiration().timestamp().max(0) as u64,
    })
}

type SavedSession = crate::credential_vault::CentralSession;
const SAVE_WARNING: &str =
    "Relay could not remember your account on this PC. You can stay signed in for this session.";
const RETRY_WARNING: &str = "Reconnecting to your Relay account. Your saved sign-in is kept; you do not need to enter your password again.";
trait SessionStore: Send + Sync {
    fn load(&self) -> Result<Option<SavedSession>>;
    fn save(&self, saved: &SavedSession) -> Result<()>;
    fn forget(&self) -> Result<()>;
}
struct VaultStore(Option<Provider>);
impl SessionStore for VaultStore {
    fn load(&self) -> Result<Option<SavedSession>> {
        let Some(provider) = &self.0 else {
            return Ok(None);
        };
        crate::credential_vault::load_central_session(provider.issuer.as_str(), &provider.client_id)
            .map_err(|_| "Windows could not read your saved Relay account. Try again.")
    }
    fn save(&self, saved: &SavedSession) -> Result<()> {
        crate::credential_vault::save_central_session(saved).map_err(|_| SAVE_WARNING)
    }
    fn forget(&self) -> Result<()> {
        let Some(provider) = &self.0 else {
            return Ok(());
        };
        crate::credential_vault::forget_central_session(
            provider.issuer.as_str(),
            &provider.client_id,
        )
        .map_err(|_| "Windows could not remove your saved Relay account. Try signing out again.")
    }
}
struct PersistenceState {
    enabled: bool,
    current: Option<SavedSession>,
}
struct Persistence {
    state: Mutex<PersistenceState>,
    store: Arc<dyn SessionStore>,
}
impl Persistence {
    fn new(provider: Option<&Provider>) -> Self {
        Self {
            state: Mutex::new(PersistenceState {
                enabled: false,
                current: None,
            }),
            store: Arc::new(VaultStore(provider.cloned())),
        }
    }
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| SAVE_WARNING)?;
        state.enabled = enabled;
        if !enabled {
            self.store.forget()
        } else if let Some(saved) = &state.current {
            self.store.save(saved)
        } else {
            Ok(())
        }
    }
    fn update(&self, saved: SavedSession, cancel: &AtomicBool) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| SAVE_WARNING)?;
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        state.current = Some(saved);
        if state.enabled {
            self.store.save(state.current.as_ref().unwrap())
        } else {
            Ok(())
        }
    }
    fn reject(&self, cancel: &AtomicBool) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| SAVE_WARNING)?;
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        state.current = None;
        self.store.forget()
    }
    fn clear_ram(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.current = None;
        }
    }
}

enum Event {
    Browser(Url),
    Account(Account),
    Proof(LinkProof),
    Failed(&'static str),
    Warning(Option<&'static str>),
}

struct LinkRequest {
    nonce: String,
    expected: Account,
    recent_auth: bool,
}
pub struct LinkProof {
    pub id_token: Zeroizing<String>,
}
struct Job {
    cancel: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
}
struct WorkerPermit;
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
fn run(
    provider: Provider,
    cancel: Arc<AtomicBool>,
    events: &mpsc::SyncSender<Event>,
    port: u16,
) -> Result<()> {
    run_flow(provider, cancel, events, port, None)
}

#[cfg(test)]
fn run_flow(
    provider: Provider,
    cancel: Arc<AtomicBool>,
    events: &mpsc::SyncSender<Event>,
    port: u16,
    link: Option<LinkRequest>,
) -> Result<()> {
    run_flow_with_registration(provider, cancel, events, port, link, false)
}

#[cfg(test)]
fn run_flow_with_registration(
    provider: Provider,
    cancel: Arc<AtomicBool>,
    events: &mpsc::SyncSender<Event>,
    port: u16,
    link: Option<LinkRequest>,
    registration: bool,
) -> Result<()> {
    run_flow_persistent(provider, cancel, events, port, link, registration, None)
}
fn run_flow_persistent(
    provider: Provider,
    cancel: Arc<AtomicBool>,
    events: &mpsc::SyncSender<Event>,
    port: u16,
    link: Option<LinkRequest>,
    registration: bool,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let (client, http) = account_client(&provider, &cancel)?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).map_err(|_| {
        "Another browser sign-in is already using the local callback. Finish it and try again."
    })?;
    let callback = format!(
        "http://{}{CALLBACK_PATH}",
        listener
            .local_addr()
            .map_err(|_| "Cannot create sign-in callback.")?
    );
    let client = client.set_redirect_uri(
        RedirectUrl::new(callback).map_err(|_| "Cannot create sign-in callback.")?,
    );
    let (challenge, pkce) = PkceCodeChallenge::new_random_sha256();
    let requested_nonce = link
        .as_ref()
        .map(|request| Nonce::new(request.nonce.clone()))
        .unwrap_or_else(Nonce::new_random);
    let mut authorization = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            || requested_nonce,
        )
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge);
    if link.as_ref().is_some_and(|l| l.recent_auth) {
        authorization = authorization.set_max_age(Duration::from_secs(300));
    }
    if registration {
        authorization = authorization.add_extra_param("prompt", "create");
    }
    let (url, state, nonce) = authorization.url();
    if cancel.load(Ordering::Acquire) {
        return Err("Sign-in cancelled.");
    }
    events
        .try_send(Event::Browser(url))
        .map_err(|_| "Sign-in window closed.")?;
    let code = wait_for_callback(&listener, &state, &provider, &cancel)?;
    drop(listener);
    if cancel.load(Ordering::Acquire) {
        return Err("Sign-in cancelled.");
    }
    let tokens = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .map_err(|_| "The account service cannot finish sign-in.")?
        .set_pkce_verifier(pkce)
        .request(&http)
        .map_err(|error| {
            crate::connection_errors::hint_or(
                &error,
                "The account service could not finish sign-in. Try again.",
            )
        })?;
    drop(code);
    drop(state);
    let verifier = client.id_token_verifier();
    let original = checked_account(&tokens, &verifier, &nonce, &provider, false)?;
    drop(verifier);
    if let Some(link) = link {
        if original.issuer != link.expected.issuer || original.subject != link.expected.subject {
            return Err(
                "Choose the same Relay account you signed into before linking this community.",
            );
        }
        if cancel.load(Ordering::Acquire) {
            return Err("Link cancelled.");
        }
        let id_token = tokens
            .id_token()
            .ok_or("The account service returned no identity proof.")?
            .to_string();
        events
            .try_send(Event::Proof(LinkProof {
                id_token: Zeroizing::new(id_token),
            }))
            .map_err(|_| "Account page closed.")?;
        return Ok(());
    }
    if cancel.load(Ordering::Acquire) {
        return Err("Sign-in cancelled.");
    }
    let saved = tokens.refresh_token().map(|token| SavedSession {
        issuer: provider.issuer.to_string(),
        client_id: provider.client_id.clone(),
        subject: original.subject.clone(),
        nonce: nonce.secret().clone(),
        refresh_token: token.secret().clone(),
    });
    if let (Some(persistence), Some(saved)) = (&persistence, &saved) {
        if let Err(warning) = persistence.update(saved.clone(), &cancel) {
            let _ = events.try_send(Event::Warning(Some(warning)));
        }
    }
    if saved.is_none() {
        if let Some(persistence) = &persistence {
            if let Err(warning) = persistence.reject(&cancel) {
                let _ = events.try_send(Event::Warning(Some(warning)));
            }
        }
    }
    if saved.is_none()
        && persistence
            .as_ref()
            .is_some_and(|p| p.state.lock().is_ok_and(|s| s.enabled))
    {
        let _ = events.try_send(Event::Warning(Some("This account service did not provide a reusable sign-in. You may need to sign in after restarting Relay.")));
    }
    deliver_account(events, &cancel, original.clone())?;
    drop(tokens);
    let Some(saved) = saved else { return Ok(()) };
    refresh_session(
        client,
        http,
        cancel,
        events,
        saved,
        Some(original),
        persistence,
    )
}

type AccountClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
fn account_client(
    provider: &Provider,
    cancel: &Arc<AtomicBool>,
) -> Result<(AccountClient, Transport)> {
    let http = Transport {
        client: reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "The Relay account connection could not start.")?,
        provider: provider.clone(),
        cancelled: cancel.clone(),
    };
    let metadata = CoreProviderMetadata::discover(
        &IssuerUrl::new(provider.issuer.to_string()).map_err(|_| "Invalid account service.")?,
        &http,
    )
    .map_err(|error| {
        crate::connection_errors::hint_or(
            &error,
            "Cannot reach the Relay account service. Check your connection, then try again.",
        )
    })?;
    // Discovery cannot select an unrelated browser destination or token endpoint.
    if !provider.allows(metadata.authorization_endpoint().url())
        || metadata
            .token_endpoint()
            .is_none_or(|url| !provider.allows(url.url()))
    {
        return Err("The account service returned an unsupported sign-in address.");
    }
    Ok((
        CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(provider.client_id.clone()),
            None,
        ),
        http,
    ))
}

// A paused UI must not turn a valid refresh into a signed-out worker. Warnings
// are best effort/coalesced; signed identity delivery waits with cancellation.
fn deliver_account(
    events: &mpsc::SyncSender<Event>,
    cancel: &AtomicBool,
    account: Account,
) -> Result<()> {
    let mut event = Event::Account(account);
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        match events.try_send(event) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => return Err("Sign-in window closed."),
            Err(mpsc::TrySendError::Full(pending)) => {
                event = pending;
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn retry_pause(cancel: &AtomicBool, attempt: u32) -> bool {
    let seconds = match attempt {
        0 => 1,
        1 => 2,
        2 => 5,
        3 => 10,
        _ => 30,
    };
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !cancel.load(Ordering::Acquire)
}

fn resume_flow(
    provider: Provider,
    cancel: Arc<AtomicBool>,
    events: &mpsc::SyncSender<Event>,
    saved: SavedSession,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        match account_client(&provider, &cancel) {
            Ok((client, http)) => {
                return refresh_session(
                    client,
                    http,
                    cancel,
                    events,
                    saved,
                    None,
                    Some(persistence),
                )
            }
            Err(_) => {
                if attempt == 0 {
                    let _ = events.try_send(Event::Warning(Some(RETRY_WARNING)));
                }
                if !retry_pause(&cancel, attempt) {
                    return Ok(());
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

fn refresh_session(
    client: AccountClient,
    http: Transport,
    cancel: Arc<AtomicBool>,
    events: &mpsc::SyncSender<Event>,
    mut saved: SavedSession,
    mut account: Option<Account>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let nonce = Nonce::new(saved.nonce.clone());
    let verifier = client.id_token_verifier();
    let mut attempt = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        if account
            .as_ref()
            .is_some_and(|a| now < a.expires_at.saturating_sub(30))
        {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let token = openidconnect::RefreshToken::new(saved.refresh_token.clone());
        let result = client
            .exchange_refresh_token(&token)
            .map_err(|_| "This account needs a new sign-in.")?
            .request(&http);
        let refreshed = match result {
            Ok(tokens) => tokens,
            Err(openidconnect::RequestTokenError::ServerResponse(error))
                if *error.error() == openidconnect::core::CoreErrorResponseType::InvalidGrant =>
            {
                if let Some(persistence) = &persistence {
                    persistence.reject(&cancel)?;
                }
                return Err("Your Relay account session ended. Sign in again.");
            }
            Err(_) => {
                if attempt == 0 {
                    let _ = events.try_send(Event::Warning(Some(RETRY_WARNING)));
                }
                if !retry_pause(&cancel, attempt) {
                    return Ok(());
                }
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let next = match checked_account(&refreshed, &verifier, &nonce, &http.provider, true) {
            Ok(next) if next.subject == saved.subject && next.expires_at > now + 30 => next,
            _ => {
                if let Some(persistence) = &persistence {
                    persistence.reject(&cancel)?;
                }
                return Err("The account session changed. Sign in again.");
            }
        };
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(rotated) = refreshed.refresh_token() {
            use zeroize::Zeroize;
            saved.refresh_token.zeroize();
            saved.refresh_token = rotated.secret().clone();
        }
        let warning = persistence
            .as_ref()
            .and_then(|p| p.update(saved.clone(), &cancel).err());
        if warning.is_some() || attempt > 0 || persistence.is_some() {
            let _ = events.try_send(Event::Warning(warning));
        }
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        deliver_account(events, &cancel, next.clone())?;
        account = Some(next);
        attempt = 0;
    }
}

pub struct Controller {
    pub provider: Option<Provider>,
    pub account: Option<Account>,
    pub error: Option<&'static str>,
    pub warning: Option<&'static str>,
    persistence: Arc<Persistence>,
    last_identity: Option<(String, String)>,
    account_changed: bool,
    resuming: bool,
    job: Option<Job>,
    waiting: bool,
    browser_ready: bool,
    link_proof: Option<LinkProof>,
}

impl Default for Controller {
    fn default() -> Self {
        match Provider::configured() {
            Ok(provider) => Self {
                persistence: Arc::new(Persistence::new(provider.as_ref())),
                provider,
                account: None,
                error: None,
                warning: None,
                last_identity: None,
                account_changed: false,
                resuming: false,
                job: None,
                waiting: false,
                browser_ready: false,
                link_proof: None,
            },
            Err(error) => Self {
                persistence: Arc::new(Persistence::new(None)),
                provider: None,
                account: None,
                error: Some(error),
                warning: None,
                last_identity: None,
                account_changed: false,
                resuming: false,
                job: None,
                waiting: false,
                browser_ready: false,
                link_proof: None,
            },
        }
    }
}

impl Controller {
    pub fn waiting(&self) -> bool {
        self.waiting
    }

    pub fn browser_ready(&self) -> bool {
        self.browser_ready
    }

    pub fn set_remember(&mut self, remember: bool) -> Result<()> {
        let result = self.persistence.set_enabled(remember);
        self.warning = result.as_ref().err().copied();
        result
    }
    pub fn resuming(&self) -> bool {
        self.resuming
    }
    /// A different signed global identity must not inherit old community sessions.
    pub fn take_account_change(&mut self) -> bool {
        std::mem::take(&mut self.account_changed)
    }
    #[cfg(test)]
    pub(crate) fn owned(provider: Provider) -> Self {
        Self {
            persistence: Arc::new(Persistence::new(Some(&provider))),
            provider: Some(provider),
            account: None,
            error: None,
            warning: None,
            last_identity: None,
            account_changed: false,
            resuming: false,
            job: None,
            waiting: false,
            browser_ready: false,
            link_proof: None,
        }
    }
    pub fn start_remembered(&mut self, ctx: egui::Context, remember: bool) {
        let _ = self.set_remember(remember);
        self.start(ctx);
    }
    pub fn start_registration_remembered(&mut self, ctx: egui::Context, remember: bool) {
        let _ = self.set_remember(remember);
        self.start_registration(ctx);
    }
    /// Restore via a fresh signed token response; never opens a browser.
    /// True means a worker started, not that a cached identity was trusted.
    pub fn resume(&mut self, ctx: egui::Context, remember: bool) -> bool {
        if let Err(warning) = self.set_remember(remember) {
            self.warning = Some(warning);
        }
        if !remember {
            return false;
        }
        let Some(provider) = self.provider.clone() else {
            return false;
        };
        self.disconnect();
        let saved = match self.persistence.store.load() {
            Ok(Some(saved)) => saved,
            Ok(None) => return false,
            Err(error) => {
                self.error = Some(error);
                return false;
            }
        };
        if saved.issuer != provider.issuer.as_str() || saved.client_id != provider.client_id {
            self.error = Some("The saved Relay account does not match this sign-in service.");
            return false;
        }
        self.last_identity = Some((saved.issuer.clone(), saved.subject.clone()));
        let persistence = self.persistence.clone();
        self.launch_job(ctx, true, move |cancel, tx| {
            resume_flow(provider, cancel, tx, saved, persistence)
        })
    }
    /// Explicit sign-out only. Cancellation and application shutdown keep the vault.
    pub fn forget(&mut self) -> Result<()> {
        self.disconnect();
        self.last_identity = None;
        self.account_changed = false;
        self.set_remember(false)
    }

    pub fn start(&mut self, ctx: egui::Context) {
        self.start_job(ctx, None);
    }
    pub fn start_registration(&mut self, ctx: egui::Context) {
        self.start_job_kind(ctx, None, true);
    }

    pub fn start_link(&mut self, ctx: egui::Context, nonce: String, expected: Account) {
        if nonce.len() > 512 || !nonce.starts_with("relay-link-v1:") {
            self.error = Some("This community returned an invalid linking challenge.");
            return;
        }
        self.start_job(
            ctx,
            Some(LinkRequest {
                nonce,
                expected,
                recent_auth: true,
            }),
        );
    }

    pub fn start_community(&mut self, ctx: egui::Context, nonce: String, expected: Account) {
        if nonce.len() > 512 || !nonce.starts_with("relay-login-v1:") {
            self.error = Some("Invalid server sign-in challenge.");
            return;
        }
        self.start_job(
            ctx,
            Some(LinkRequest {
                nonce,
                expected,
                recent_auth: false,
            }),
        );
    }

    pub fn take_link_proof(&mut self) -> Option<LinkProof> {
        self.link_proof.take()
    }

    fn start_job(&mut self, ctx: egui::Context, link: Option<LinkRequest>) {
        self.start_job_kind(ctx, link, false);
    }
    fn start_job_kind(
        &mut self,
        ctx: egui::Context,
        link: Option<LinkRequest>,
        registration: bool,
    ) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let persistence = link.is_none().then(|| self.persistence.clone());
        self.launch_job(ctx, false, move |cancel, tx| {
            run_flow_persistent(
                provider,
                cancel,
                tx,
                CALLBACK_PORT,
                link,
                registration,
                persistence,
            )
        });
    }
    fn launch_job(
        &mut self,
        ctx: egui::Context,
        resuming: bool,
        task: impl FnOnce(Arc<AtomicBool>, &mpsc::SyncSender<Event>) -> Result<()> + Send + 'static,
    ) -> bool {
        self.disconnect();
        if WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 2).then_some(n + 1)
            })
            .is_err()
        {
            self.error = Some("The previous sign-in is finishing. Try again shortly.");
            return false;
        }
        let (tx, rx) = mpsc::sync_channel(4);
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(Job {
            cancel: cancel.clone(),
            events: rx,
        });
        self.waiting = true;
        self.resuming = resuming;
        ctx.request_repaint();
        thread::spawn(move || {
            let _permit = WorkerPermit;
            if let Err(error) = task(cancel.clone(), &tx) {
                if !cancel.load(Ordering::Acquire) {
                    let _ = tx.try_send(Event::Failed(error));
                }
            }
            ctx.request_repaint();
        });
        true
    }

    /// The caller opens this URL only for its explicit Sign in action. Polling
    /// does not touch browsers, media, community sessions or persistent storage.
    pub fn poll(&mut self) -> Option<Url> {
        let mut browser = None;
        let mut terminal = false;
        if let Some(job) = &self.job {
            loop {
                match job.events.try_recv() {
                    Ok(Event::Browser(url)) => {
                        self.browser_ready = true;
                        browser = Some(url);
                    }
                    Ok(Event::Account(account)) => {
                        let identity = (account.issuer.clone(), account.subject.clone());
                        self.account_changed |= self
                            .last_identity
                            .as_ref()
                            .is_some_and(|last| last != &identity);
                        self.last_identity = Some(identity);
                        self.resuming = false;
                        self.account = Some(account);
                        self.error = None;
                        self.waiting = false;
                    }
                    Ok(Event::Proof(proof)) => {
                        self.link_proof = Some(proof);
                        self.waiting = false;
                        terminal = true;
                        break;
                    }
                    Ok(Event::Warning(warning)) => self.warning = warning,
                    Ok(Event::Failed(error)) => {
                        self.resuming = false;
                        self.error = Some(error);
                        self.waiting = false;
                        terminal = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.waiting = false;
                        terminal = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        if terminal {
            self.job = None;
        }
        if self.account.as_ref().is_some_and(|account| {
            account.expires_at <= chrono::Utc::now().timestamp().max(0) as u64
        }) {
            self.account = None;
            if self.job.is_some() {
                self.resuming = true;
                self.waiting = true;
                self.warning = Some(RETRY_WARNING);
            } else {
                self.error
                    .get_or_insert("Your Relay account session ended. Sign in again.");
            }
        }
        browser
    }

    pub fn disconnect(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Release);
        }
        self.persistence.clear_ram();
        self.resuming = false;
        self.account = None;
        self.error = None;
        self.waiting = false;
        self.browser_ready = false;
        self.link_proof = None;
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
#[path = "central_account_tests.rs"]
mod tests;
