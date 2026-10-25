//! A public address is only a hint until the current host delegates to a key
//! and that exact destination proves possession with a fresh challenge. Never
//! send a bearer, password or account proof during these checks.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use relay_protocol::handover::{self as model, HostIdentity, RoutePin, RouteProof};
use sha2::{Digest, Sha256};
use std::{io::Read, time::Duration};
#[path = "host_route_cache.rs"]
mod cache;

pub const UNAVAILABLE: &str = "The current host cannot answer the route check yet.";
/// The exact pinned host answered but is paused for a move it has not granted.
/// It is still this community's authority: a remembered login may reopen it
/// read-only to finish or cancel the move. A first login still needs a writable host.
pub const PAUSED: &str = "This community's host is paused for a move. Sign-ins resume when the move is finished or cancelled.";
pub const REFUSED: &str = "Relay could not confirm the new host. Your existing account, drafts and server identity have been kept.";

fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|v| v.to_string() == value)
}
fn identity(key: &VerifyingKey) -> HostIdentity {
    let mut hash = Sha256::new();
    hash.update(b"relay-host-identity-v1\0");
    hash.update(key.as_bytes());
    let digest = hash.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    HostIdentity {
        host_id: uuid::Uuid::from_bytes(bytes).to_string(),
        public_key: URL_SAFE_NO_PAD.encode(key.as_bytes()),
    }
}
fn key(host: &HostIdentity) -> Option<VerifyingKey> {
    if host.public_key.len() != 43 {
        return None;
    }
    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&host.public_key)
        .ok()?
        .try_into()
        .ok()?;
    let key = VerifyingKey::from_bytes(&bytes).ok()?;
    (identity(&key) == *host).then_some(key)
}
pub(crate) fn verify(host: &HostIdentity, signature: &str, message: &[u8]) -> bool {
    if signature.len() != 86 {
        return false;
    }
    let Some(key) = key(host) else {
        return false;
    };
    let Ok(raw) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&raw) else {
        return false;
    };
    key.verify_strict(message, &signature).is_ok()
}
pub fn valid_pin(pin: &RoutePin) -> bool {
    pin.term > 0 && uuid(&pin.server_id) && key(&pin.host).is_some()
}

/// Trust a bootstrap's first pin only on its existing authenticated transport.
/// Subsequent snapshots cannot silently rotate a host key or authority term.
pub fn bootstrap_pin(
    url: &str,
    server_id: &str,
    incoming: Option<&RoutePin>,
    previous: Option<&RoutePin>,
) -> Result<Option<RoutePin>, String> {
    if let Some(previous) = previous {
        if incoming != Some(previous) {
            return Err(REFUSED.into());
        }
    }
    let Some(pin) = incoming else {
        return Ok(None);
    };
    if pin.server_id != server_id || !valid_pin(pin) {
        return Err(REFUSED.into());
    }
    if crate::community::account_destination(url).is_err() {
        return Ok(None);
    }
    Ok(Some(pin.clone()))
}

pub(crate) fn safe_destination(raw: &str) -> Result<String, String> {
    let normalized = crate::community::account_destination(raw).map_err(|_| REFUSED)?;
    let parsed = url::Url::parse(&normalized).map_err(|_| REFUSED)?;
    if parsed.path() != "/" || raw.len() > 2048 {
        return Err(REFUSED.into());
    }
    let host = parsed.host_str().ok_or(REFUSED)?;
    // A target URL is shared among all community members. Loopback cannot
    // name the same new host from their different PCs.
    if host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| match ip {
                std::net::IpAddr::V4(v) => {
                    v.is_loopback() || v.is_unspecified() || v.is_link_local() || v.is_multicast()
                }
                std::net::IpAddr::V6(v) => {
                    v.is_loopback()
                        || v.is_unspecified()
                        || v.is_multicast()
                        || v.is_unicast_link_local()
                }
            })
    {
        return Err(REFUSED.into());
    }
    Ok(normalized)
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
fn grant(pin: &RoutePin, grant: &model::ActivationGrant) -> bool {
    let r = &grant.receipt;
    let b = &r.body;
    grant.source_identity == pin.host
        // The source's durable database marker predates the signing key and
        // need not equal its key-derived host ID. Both signatures authenticate
        // the marker; the trusted key and term remain the route authority.
        && uuid(&b.source_host_id)
        && b.server_id == pin.server_id
        && b.term == pin.term
        && b.term.checked_add(1) == Some(grant.next_term)
        && b.target_identity != pin.host
        && key(&b.target_identity).is_some()
        && uuid(&b.preparation_id)
        && uuid(&b.export_id)
        && b.database_bytes > 0
        && b.database_bytes <= 4 * 1024 * 1024 * 1024
        && digest(&b.database_sha256)
        && digest(&b.manifest_sha256)
        && grant.history_sha256.as_deref().is_none_or(digest)
        && URL_SAFE_NO_PAD
            .decode(&b.challenge)
            .is_ok_and(|v| v.len() == 32)
        && safe_destination(&b.target_url).is_ok()
        && verify(&b.target_identity, &r.signature, &model::receipt_message(b))
        && verify(&pin.host, &grant.signature, &model::grant_message(grant))
}
pub(crate) fn valid_grant(pin: &RoutePin, value: &model::ActivationGrant) -> bool {
    valid_pin(pin) && grant(pin, value)
}
pub(crate) fn committed_history(value:&model::ActivationGrant,history:&[model::ActivationGrant])->bool {
    history.len() <= MAX_MOVES && value.history_sha256.as_ref().is_none_or(|hash|
        *hash == format!("{:x}",Sha256::digest(model::history_message(history))))
}
fn proof(pin: &RoutePin, challenge: &str, reply: &RouteProof) -> bool {
    valid_pin(pin)
        && challenge.len() == 43
        && reply.state.challenge == challenge
        && history_connects(pin,&reply.state.pin,&reply.state.history)
        && verify(
            &reply.state.pin.host,
            &reply.signature,
            &model::route_message(&reply.state),
        )
}

/// A history is public evidence, never a trust root. It must contain the
/// separately pinned authority and end at the fresh proof's exact key/term.
pub(crate) fn history_connects(pin:&RoutePin,end:&RoutePin,history:&[model::ActivationGrant])->bool {
    if !valid_pin(pin) || !valid_pin(end) || pin.server_id != end.server_id
        || history.len() > MAX_MOVES { return false; }
    let Some(first)=history.first() else { return pin == end; };
    let mut current=RoutePin{server_id:pin.server_id.clone(),host:first.source_identity.clone(),term:first.receipt.body.term};
    let mut trusted=current == *pin;
    for (index,edge) in history.iter().enumerate() {
        if !grant(&current,edge) { return false; }
        if !committed_history(edge,&history[..index]) { return false; }
        current=RoutePin{server_id:pin.server_id.clone(),host:edge.receipt.body.target_identity.clone(),term:edge.next_term};
        trusted |= current == *pin;
    }
    trusted && current == *end
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RememberedAuthority {
    pub pin: RoutePin,
    pub account_provider: Option<relay_protocol::AccountProvider>,
    #[serde(default)]
    pub previous_urls: Vec<String>,
}
impl RememberedAuthority {
    pub fn valid_for(&self, url: &str, server: &str) -> bool {
        self.pin.server_id == server
            && valid_pin(&self.pin)
            && self.previous_urls.len() <= MAX_MOVES
            && self.previous_urls.iter().all(|v| {
                crate::community::account_destination(v)
                    .is_ok_and(|canonical| canonical == *v && canonical != url)
            })
            && crate::community::account_destination(url).is_ok()
            && self.account_provider.as_ref().is_none_or(|p| {
                [&p.issuer, &p.client_id, &p.community_url]
                    .into_iter()
                    .all(|v| v.len() <= 2048 && !v.chars().any(char::is_control))
            })
    }
}

pub fn alias_history(source: &str, previous: &[String]) -> Vec<String> {
    let mut aliases = Vec::new();
    for url in std::iter::once(source).chain(previous.iter().map(String::as_str)) {
        if !aliases.iter().any(|v| v == url) {
            aliases.push(url.to_string());
        }
    }
    // One extra entry lets the vault retire the oldest credential only after
    // the new binding was saved. On disk each record retains at most eight.
    aliases.truncate(MAX_MOVES + 1);
    aliases
}

#[derive(Clone)]
pub struct VerifiedMove {
    pub url: String,
    pub pin: RoutePin,
}

const MAX_MOVES: usize = model::MAX_ROUTE_MOVES;
trait Delegations {
    fn load(&self, source: &str, pin: &RoutePin) -> Result<Option<model::ActivationGrant>, String>;
    fn store(
        &mut self,
        source: &str,
        pin: &RoutePin,
        value: &model::ActivationGrant,
    ) -> Result<(), String>;
}
#[cfg(test)]
fn resolve(
    source: &str,
    pin: &RoutePin,
    fetch: impl FnMut(&str, &str) -> Result<RouteProof, String>,
) -> Result<Option<VerifiedMove>, String> {
    struct Empty;
    impl Delegations for Empty {
        fn load(&self, _: &str, _: &RoutePin) -> Result<Option<model::ActivationGrant>, String> {
            Ok(None)
        }
        fn store(
            &mut self,
            _: &str,
            _: &RoutePin,
            _: &model::ActivationGrant,
        ) -> Result<(), String> {
            Ok(())
        }
    }
    resolve_cached(source, pin, fetch, &mut Empty)
}
fn resolve_cached(
    source: &str,
    pin: &RoutePin,
    mut fetch: impl FnMut(&str, &str) -> Result<RouteProof, String>,
    cache: &mut impl Delegations,
) -> Result<Option<VerifiedMove>, String> {
    if !valid_pin(pin) {
        return Err(REFUSED.into());
    }
    let mut at = crate::community::normalize_url(source)?;
    let mut current = pin.clone();
    let mut seen = std::collections::HashSet::new();
    seen.insert((at.clone(), pin.host.host_id.clone(), pin.term));
    for step in 0..=MAX_MOVES {
        let mut nonce = [0u8; 32];
        getrandom::getrandom(&mut nonce).map_err(|_| REFUSED)?;
        let challenge = URL_SAFE_NO_PAD.encode(nonce);
        let remembered = cache.load(&at, &current)?;
        let moved = if let Some(saved) = remembered {
            saved
        } else {
            match fetch(&at, &challenge) {
                Ok(reply) => {
                    if !proof(&current, &challenge, &reply) {
                        return Err(REFUSED.into());
                    }
                    if reply.state.pin != current {
                        if reply.state.pin.term.checked_sub(pin.term).is_none_or(|n|n > MAX_MOVES as u64)
                            || reply.state.history.last().is_none_or(|g|
                                !safe_destination(&g.receipt.body.target_url).is_ok_and(|url|url == at)) {
                            return Err(REFUSED.into());
                        }
                        // A new address can prove the whole ancestry without
                        // asking any old machine to come back online. Retain
                        // only already verified immutable grants, before auth.
                        let mut history_at=at.clone();
                        for edge in &reply.state.history {
                            if edge.source_identity != current.host || edge.receipt.body.term != current.term { continue; }
                            cache.store(&history_at,&current,edge)?;
                            current=RoutePin{server_id:pin.server_id.clone(),host:edge.receipt.body.target_identity.clone(),term:edge.next_term};
                            history_at=safe_destination(&edge.receipt.body.target_url)?;
                        }
                        if current != reply.state.pin || !seen.insert((at.clone(),current.host.host_id.clone(),current.term)) {
                            return Err(REFUSED.into());
                        }
                    }
                    match reply.state.moved {
                        None => {
                            return if !reply.state.writes_allowed && step == 0 && current == *pin {
                                Err(PAUSED.into())
                            } else if !reply.state.writes_allowed {
                                Err(UNAVAILABLE.into())
                            } else if step == 0 && current == *pin {
                                Ok(None)
                            } else if reply.state.writes_allowed {
                                Ok(Some(VerifiedMove {
                                    url: at,
                                    pin: current,
                                }))
                            } else {
                                Err(UNAVAILABLE.into())
                            };
                        }
                        Some(moved) => {
                            if reply.state.writes_allowed || !grant(&current, &moved) {
                                return Err(REFUSED.into());
                            }
                            // Save before probing the destination: a stopped source
                            // is no longer required after this immutable evidence.
                            cache.store(&at, &current, &moved)?;
                            moved
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        };
        if step == MAX_MOVES || !grant(&current, &moved) {
            return Err(REFUSED.into());
        }
        if moved.next_term.checked_sub(pin.term).is_none_or(|n|n > MAX_MOVES as u64) {
            return Err(REFUSED.into());
        }
        let next = safe_destination(&moved.receipt.body.target_url)?;
        if !seen.insert((
            next.clone(),
            moved.receipt.body.target_identity.host_id.clone(),
            moved.next_term,
        )) {
            return Err(REFUSED.into());
        }
        current = RoutePin {
            server_id: pin.server_id.clone(),
            host: moved.receipt.body.target_identity,
            term: moved.next_term,
        };
        at = next;
    }
    Err(REFUSED.into())
}

pub(crate) fn discover(
    source: &str,
    pin: &RoutePin,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Option<VerifiedMove>, String> {
    let mut cache = cache::Cache::for_profile()?;
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|_| REFUSED)?;
    resolve_cached(
        source,
        pin,
        |url, challenge| {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err(UNAVAILABLE.into());
            }
            fetch_route(&http,url,challenge)
        },
        &mut cache,
    )
}

/// An invitation root must prove the exact selected destination before any
/// password, invite or central-account proof is sent. Authentication cannot
/// silently change URLs; the anonymous setup check handles that first.
pub(crate) fn confirm_login(url: &str, pins: &[RoutePin]) -> Result<Option<RoutePin>, String> {
    if pins.is_empty() {
        return Ok(None);
    }
    crate::community::account_destination(url).map_err(|_| REFUSED)?;
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|_| REFUSED)?;
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).map_err(|_| REFUSED)?;
    let challenge = URL_SAFE_NO_PAD.encode(nonce);
    let reply = fetch_route(&http, url, &challenge)?;
    for pin in pins {
        confirm_reply(url, pin, &challenge, &reply)?;
    }
    Ok(Some(reply.state.pin))
}
pub(crate) fn confirm_reply(
    url: &str,
    pin: &RoutePin,
    challenge: &str,
    reply: &RouteProof,
) -> Result<RoutePin, String> {
    if !proof(pin, challenge, reply)
        || !reply.state.writes_allowed
        || reply.state.moved.is_some()
        || reply
            .state
            .pin
            .term
            .checked_sub(pin.term)
            .is_none_or(|n| n > MAX_MOVES as u64)
        || (reply.state.pin != *pin
            && reply.state.history.last().is_none_or(|edge| {
                !safe_destination(&edge.receipt.body.target_url).is_ok_and(|target| target == url)
            }))
    {
        return Err(REFUSED.into());
    }
    Ok(reply.state.pin.clone())
}
fn fetch_route(
    http: &reqwest::blocking::Client,
    url: &str,
    challenge: &str,
) -> Result<RouteProof, String> {
    crate::embedded_network::ensure_route(url).map_err(|_| UNAVAILABLE)?;
    let response = http
        .post(crate::embedded_network::endpoint(&format!(
            "{url}/api/hosting/handover/route"
        )))
        .json(&model::RouteChallenge {
            challenge: challenge.into(),
        })
        .send()
        .map_err(|_| UNAVAILABLE)?;
    if !response.status().is_success() {
        return Err(UNAVAILABLE.into());
    }
    let mut bytes = Vec::new();
    response
        .take(model::CONTROL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| REFUSED)?;
    if bytes.len() as u64 > model::CONTROL_BYTES {
        return Err(REFUSED.into());
    }
    serde_json::from_slice(&bytes).map_err(|_| REFUSED.into())
}

/// An unavailable old route may use the session's anonymous lookup capability.
/// A live host, identity refusal or cancellation is never overridden by a directory.
fn with_session_fallback(
    result: Result<Option<VerifiedMove>, String>,
    session: Option<&crate::host_locator::ReadCapability>,
    cancel: &std::sync::atomic::AtomicBool,
    lookup: impl FnOnce(&crate::host_locator::ReadCapability) -> Result<VerifiedMove, String>,
) -> Result<Option<VerifiedMove>, String> {
    if matches!(&result, Err(error) if error == UNAVAILABLE)
        && !cancel.load(std::sync::atomic::Ordering::Acquire)
    {
        if let Some(capability) = session { return lookup(capability).map(Some); }
    }
    result
}

/// One bounded background probe per active connection. The network worker stays
/// free to process Leave/mute/logout while HTTPS or iroh is being checked.
pub struct Job {
    receiver: std::sync::mpsc::Receiver<Result<Option<VerifiedMove>, String>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Job {
    pub(crate) fn start(
        source: String,
        pin: RoutePin,
        session: Option<crate::host_locator::ReadCapability>,
        work: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Self, String> {
        Self::spawn(work, wake, move |cancel| {
            with_session_fallback(discover(&source, &pin, cancel), session.as_ref(), cancel,
                |capability| crate::host_locator::discover_capability(capability, cancel))
        })
    }
    fn spawn(
        work: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        wake: impl FnOnce() + Send + 'static,
        task: impl FnOnce(&std::sync::atomic::AtomicBool) -> Result<Option<VerifiedMove>, String>
            + Send
            + 'static,
    ) -> Result<Self, String> {
        use std::sync::atomic::Ordering;
        work.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            (v < 4).then_some(v + 1)
        })
        .map_err(|_| UNAVAILABLE)?;
        struct Permit(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Permit {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let permit = Permit(work);
        let (send, receiver) = std::sync::mpsc::sync_channel(1);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = cancel.clone();
        std::thread::Builder::new()
            .name("relay-host-route".into())
            .spawn(move || {
                let result = task(&stopped);
                if !stopped.load(Ordering::Acquire) {
                    let _ = send.try_send(result);
                }
                drop(permit);
                wake();
            })
            .map_err(|_| UNAVAILABLE)?;
        Ok(Self { receiver, cancel })
    }
    #[cfg(test)]
    pub(crate) fn ready(result: Result<Option<VerifiedMove>, String>) -> Self {
        let (send, receiver) = std::sync::mpsc::sync_channel(1);
        send.send(result).ok();
        Self {
            receiver,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
    pub fn poll(&self) -> Option<Result<Option<VerifiedMove>, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(UNAVAILABLE.into())),
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    pub(crate) fn sign(key: &SigningKey, state: model::RouteState) -> RouteProof {
        RouteProof {
            signature: URL_SAFE_NO_PAD.encode(key.sign(&model::route_message(&state)).to_bytes()),
            state,
        }
    }
    pub(crate) fn fixture() -> (SigningKey, SigningKey, RoutePin, model::ActivationGrant) {
        let a = SigningKey::from_bytes(&[41; 32]);
        let b = SigningKey::from_bytes(&[42; 32]);
        let pin = RoutePin {
            server_id: uuid::Uuid::new_v4().to_string(),
            host: identity(&a.verifying_key()),
            term: 3,
        };
        let body = model::ReceiptBody {
            server_id: pin.server_id.clone(),
            source_host_id: uuid::Uuid::new_v4().to_string(),
            term: pin.term,
            preparation_id: uuid::Uuid::new_v4().to_string(),
            export_id: uuid::Uuid::new_v4().to_string(),
            target_identity: identity(&b.verifying_key()),
            target_url: "https://target.example/".into(),
            database_sha256: "a".repeat(64),
            manifest_sha256: "b".repeat(64),
            database_bytes: 45,
            challenge: URL_SAFE_NO_PAD.encode([4; 32]),
        };
        let receipt = model::Receipt {
            signature: URL_SAFE_NO_PAD.encode(b.sign(&model::receipt_message(&body)).to_bytes()),
            body,
        };
        let mut g = model::ActivationGrant {
            receipt,
            source_identity: pin.host.clone(),
            next_term: 4,
            history_sha256: None,
            signature: String::new(),
        };
        g.signature = URL_SAFE_NO_PAD.encode(a.sign(&model::grant_message(&g)).to_bytes());
        (a, b, pin, g)
    }
    #[test]
    fn two_fresh_key_proofs_follow_exactly_the_signed_next_host() {
        let (a, b, pin, g) = fixture();
        let mut urls = vec![];
        let next = resolve("https://source.example", &pin, |url, challenge| {
            urls.push((url.to_string(), challenge.to_string()));
            let (key, pin, writes, moved) = if urls.len() == 1 {
                (&a, pin.clone(), false, Some(g.clone()))
            } else {
                (
                    &b,
                    RoutePin {
                        server_id: pin.server_id.clone(),
                        host: identity(&b.verifying_key()),
                        term: 4,
                    },
                    true,
                    None,
                )
            };
            Ok(sign(
                key,
                model::RouteState {
                    history: Vec::new(),
                    pin,
                    challenge: challenge.into(),
                    writes_allowed: writes,
                    moved,
                },
            ))
        })
        .unwrap()
        .unwrap();
        assert_eq!(next.url, "https://target.example");
        assert_eq!(next.pin.term, 4);
        assert_eq!(urls.len(), 2);
        assert_ne!(urls[0].1, urls[1].1);
    }
    #[test]
    fn unsigned_redirects_replays_wrong_keys_terms_and_unready_targets_are_refused() {
        for fault in 0..7 {
            let (a, b, pin, g) = fixture();
            let mut count = 0;
            assert!(
                resolve("https://source.example", &pin, |_, challenge| {
                    count += 1;
                    let mut state = if count == 1 {
                        model::RouteState {
                            history: Vec::new(),
                            pin: pin.clone(),
                            challenge: challenge.into(),
                            writes_allowed: false,
                            moved: Some(g.clone()),
                        }
                    } else {
                        model::RouteState {
                            history: Vec::new(),
                            pin: RoutePin {
                                server_id: pin.server_id.clone(),
                                host: identity(&b.verifying_key()),
                                term: 4,
                            },
                            challenge: challenge.into(),
                            writes_allowed: true,
                            moved: None,
                        }
                    };
                    if count == 1 && fault == 0 {
                        state.moved.as_mut().unwrap().receipt.body.target_url =
                            "https://attacker.example".into();
                    }
                    if count == 1 && fault == 1 {
                        state.writes_allowed = true;
                    }
                    if count == 2 && fault == 2 {
                        state.challenge = URL_SAFE_NO_PAD.encode([99; 32]);
                    }
                    if count == 2 && fault == 3 {
                        state.pin.term = 3;
                    }
                    if count == 2 && fault == 4 {
                        state.writes_allowed = false;
                    }
                    if count == 2 && fault == 5 {
                        state.pin.server_id = uuid::Uuid::new_v4().to_string();
                    }
                    Ok(sign(if count == 1 || fault == 6 { &a } else { &b }, state))
                })
                .is_err(),
                "fault {fault}"
            );
        }
    }
    #[test]
    fn learning_an_authority_requires_an_authenticated_route_and_never_silently_rotates() {
        let (_, _, pin, _) = fixture();
        assert_eq!(
            bootstrap_pin("http://192.168.1.8", &pin.server_id, Some(&pin), None).unwrap(),
            None
        );
        assert_eq!(
            bootstrap_pin("https://source.example", &pin.server_id, Some(&pin), None).unwrap(),
            Some(pin.clone())
        );
        let mut changed = pin.clone();
        changed.term += 1;
        assert!(bootstrap_pin(
            "https://source.example",
            &pin.server_id,
            Some(&changed),
            Some(&pin)
        )
        .is_err());
        assert!(bootstrap_pin("https://source.example", &pin.server_id, None, Some(&pin)).is_err());
        for url in [
            "http://192.168.1.8",
            "https://127.0.0.1/",
            "https://localhost/",
            "https://host.example/path",
            "https://user:pass@host.example/",
        ] {
            assert!(safe_destination(url).is_err(), "{url}");
        }
    }
    #[test]
    fn a_slow_probe_does_not_block_the_worker_and_cancellation_releases_its_work_slot() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc,
        };
        let work = Arc::new(AtomicUsize::new(0));
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (done, done_rx) = mpsc::channel();
        let job = Job::spawn(
            work.clone(),
            move || {
                let _ = done.send(());
            },
            move |cancel| {
                entered.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(cancel.load(Ordering::Acquire));
                Ok(None)
            },
        )
        .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(job.poll().is_none());
        assert_eq!(work.load(Ordering::Acquire), 1);
        drop(job);
        release.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(work.load(Ordering::Acquire), 0);
        work.store(4, Ordering::Release);
        assert!(Job::spawn(
            work.clone(),
            || {},
            |_| panic!("must not start beyond work limit")
        )
        .is_err());
        assert_eq!(work.load(Ordering::Acquire), 4);
    }
    #[test]
    fn disk_delegation_survives_restart_without_the_source_but_never_substitutes_for_a_fresh_target_proof(
    ) {
        let (a, b, pin, g) = fixture();
        let temp = tempfile::tempdir().unwrap();
        let mut cache = cache::Cache::at(temp.path().to_owned());
        let source = "https://source.example";
        let first = resolve_cached(
            source,
            &pin,
            |url, challenge| {
                if url != source {
                    return Err(UNAVAILABLE.into());
                }
                Ok(sign(
                    &a,
                    model::RouteState {
                        history: Vec::new(),
                        pin: pin.clone(),
                        challenge: challenge.into(),
                        writes_allowed: false,
                        moved: Some(g.clone()),
                    },
                ))
            },
            &mut cache,
        );
        assert!(first.is_err());
        assert_eq!(cache.load(source, &pin).unwrap(), Some(g.clone()));
        drop(cache);
        let mut cache = cache::Cache::at(temp.path().to_owned());
        let target = RoutePin {
            server_id: pin.server_id.clone(),
            host: identity(&b.verifying_key()),
            term: 4,
        };
        let next = resolve_cached(
            source,
            &pin,
            |url, challenge| {
                if url == source {
                    return Err(UNAVAILABLE.into());
                }
                Ok(sign(
                    &b,
                    model::RouteState {
                        history: Vec::new(),
                        pin: target.clone(),
                        challenge: challenge.into(),
                        writes_allowed: true,
                        moved: None,
                    },
                ))
            },
            &mut cache,
        )
        .unwrap()
        .unwrap();
        assert_eq!(next.pin, target);
        assert!(resolve_cached(source, &pin, |_, _| Err(UNAVAILABLE.into()), &mut cache).is_err());
        // Even a fresh old-key claim cannot undo a cached irreversible grant.
        assert!(resolve_cached(
            source,
            &pin,
            |_, challenge| Ok(sign(
                &a,
                model::RouteState {
                    history: Vec::new(),
                    pin: pin.clone(),
                    challenge: challenge.into(),
                    writes_allowed: true,
                    moved: None,
                }
            )),
            &mut cache
        )
        .is_err());
        let path = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|v| v.unwrap().path())
            .find(|v| v.extension().is_some_and(|x| x == "json"))
            .unwrap();
        let mut forged = g.clone();
        forged.receipt.body.target_url = "https://attacker.example".into();
        let raw = serde_json::to_vec(&forged).unwrap();
        std::fs::write(&path, &raw).unwrap();
        assert!(cache.load(source, &pin).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        assert!(cache
            .load("https://another-source.example", &pin)
            .unwrap()
            .is_none());
    }
    fn delegated(
        template: &model::ActivationGrant,
        source: &RoutePin,
        signer: &SigningKey,
        target: &SigningKey,
        url: &str,
    ) -> model::ActivationGrant {
        let mut g = template.clone();
        g.receipt.body.term = source.term;
        g.receipt.body.source_host_id = source.host.host_id.clone();
        g.receipt.body.target_identity = identity(&target.verifying_key());
        g.receipt.body.target_url = url.into();
        g.receipt.signature = URL_SAFE_NO_PAD.encode(
            target
                .sign(&model::receipt_message(&g.receipt.body))
                .to_bytes(),
        );
        g.source_identity = source.host.clone();
        g.next_term = source.term + 1;
        g.signature = URL_SAFE_NO_PAD.encode(signer.sign(&model::grant_message(&g)).to_bytes());
        g
    }
    #[test]
    fn a_new_host_proves_saved_ancestry_without_contacting_old_hosts() {
        let (a,b,pin,mut g)=fixture();
        assert!(serde_json::to_value(&g).unwrap().get("history_sha256").is_none(),"legacy grant encoding is unchanged");
        g.history_sha256=Some(format!("{:x}",Sha256::digest(model::history_message(&[]))));
        g.signature=URL_SAFE_NO_PAD.encode(a.sign(&model::grant_message(&g)).to_bytes());
        let c=SigningKey::from_bytes(&[43;32]);
        let second=RoutePin{server_id:pin.server_id.clone(),host:identity(&b.verifying_key()),term:g.next_term};
        let mut next=delegated(&g,&second,&b,&c,"https://final.example");
        next.history_sha256=Some(format!("{:x}",Sha256::digest(model::history_message(&[g.clone()]))));
        next.signature=URL_SAFE_NO_PAD.encode(b.sign(&model::grant_message(&next)).to_bytes());
        let end=RoutePin{server_id:pin.server_id.clone(),host:identity(&c.verifying_key()),term:next.next_term};
        let history=vec![g.clone(),next.clone()];
        let mut alternative=g.clone();
        alternative.receipt.body.export_id=uuid::Uuid::new_v4().to_string();
        alternative.receipt.signature=URL_SAFE_NO_PAD.encode(b.sign(&model::receipt_message(&alternative.receipt.body)).to_bytes());
        alternative.signature=URL_SAFE_NO_PAD.encode(a.sign(&model::grant_message(&alternative)).to_bytes());
        assert!(valid_grant(&pin,&alternative));
        assert!(!history_connects(&pin,&end,&[alternative,next.clone()]),"a later source commits to the exact preceding grant, not just the same keys");
        let dir=tempfile::tempdir().unwrap();
        let mut cache=cache::Cache::at(dir.path().to_owned());
        let found=resolve_cached("https://final.example",&pin,|url,challenge| {
            assert_eq!(url,"https://final.example","no lookup of either old host");
            Ok(sign(&c,model::RouteState{pin:end.clone(),challenge:challenge.into(),
                writes_allowed:true,moved:None,history:history.clone()}))
        },&mut cache).unwrap().unwrap();
        assert_eq!(found.pin,end);
        assert_eq!(found.url,"https://final.example");
        // A later lookup can consume the retained grants and only ask the
        // current host for a fresh, exact-key proof. No password/bearer exists.
        let resumed=resolve_cached("https://final.example",&pin,|url,challenge| {
            assert_eq!(url,"https://final.example");
            Ok(sign(&c,model::RouteState{pin:end.clone(),challenge:challenge.into(),
                writes_allowed:true,moved:None,history:Vec::new()}))
        },&mut cache).unwrap().unwrap();
        assert_eq!(resumed.pin,end);
        for change in 0..7 {
            let mut bad=history.clone();
            match change {
                0=>bad.clear(),
                1=>{bad.remove(0);},
                2=>bad.reverse(),
                3=>bad[0].signature="A".repeat(86),
                4=>bad[1].next_term+=1,
                5=>bad=vec![g.clone();MAX_MOVES+1],
                _=>bad[1].receipt.body.server_id=uuid::Uuid::new_v4().to_string(),
            }
            assert!(!history_connects(&pin,&end,&bad),"history mutation {change}");
        }
        let wrong=RoutePin{host:identity(&SigningKey::from_bytes(&[46;32]).verifying_key()),..pin.clone()};
        assert!(!history_connects(&wrong,&end,&history),"a matching server UUID is not a trust root");
        let challenge=URL_SAFE_NO_PAD.encode([7;32]);
        let mut replay=sign(&c,model::RouteState{pin:end.clone(),challenge:challenge.clone(),
            writes_allowed:true,moved:None,history:history.clone()});
        assert!(proof(&pin,&challenge,&replay));
        replay.state.challenge=URL_SAFE_NO_PAD.encode([8;32]);
        assert!(!proof(&pin,&challenge,&replay));
        assert!(resolve("https://wrong.example",&pin,|_,challenge|Ok(sign(&c,
            model::RouteState{pin:end.clone(),challenge:challenge.into(),writes_allowed:true,moved:None,history:history.clone()}))).is_err(),
            "the final URL must be the delegated destination");
    }

    #[test]
    fn the_pinned_host_paused_for_a_move_is_reported_as_paused_not_unavailable() {
        let (a, _, pin, _) = fixture();
        let paused = |writes_allowed: bool| {
            resolve("https://source.example", &pin, |_, challenge| {
                Ok(sign(
                    &a,
                    model::RouteState {
                        history: Vec::new(),
                        pin: pin.clone(),
                        challenge: challenge.into(),
                        writes_allowed,
                        moved: None,
                    },
                ))
            })
        };
        assert!(matches!(paused(false), Err(error) if error == PAUSED));
        assert!(matches!(paused(true), Ok(None)));
        // The live probe and a remembered resume treat it as "not moved".
        let fallback = with_session_fallback(
            Err(PAUSED.into()),
            None,
            &std::sync::atomic::AtomicBool::new(false),
            |_| unreachable!("a reachable paused host needs no lookup"),
        );
        assert!(matches!(fallback, Err(error) if error == PAUSED));
    }

    #[test]
    fn signed_move_chains_are_bounded_and_reject_replayed_authorities() {
        let (a, b, pin, g) = fixture();
        let c = SigningKey::from_bytes(&[43; 32]);
        let second = RoutePin {
            server_id: pin.server_id.clone(),
            host: identity(&b.verifying_key()),
            term: 4,
        };
        for cycle in [false, true] {
            let g2 = delegated(
                &g,
                &second,
                &b,
                &c,
                if cycle {
                    "https://source.example"
                } else {
                    "https://final.example"
                },
            );
            let result = resolve("https://source.example", &pin, |url, challenge| {
                let (key, head, moved) = if url == "https://source.example" {
                    (&a, pin.clone(), Some(g.clone()))
                } else if url == "https://target.example" {
                    (&b, second.clone(), Some(g2.clone()))
                } else {
                    (
                        &c,
                        RoutePin {
                            server_id: pin.server_id.clone(),
                            host: identity(&c.verifying_key()),
                            term: 5,
                        },
                        None,
                    )
                };
                Ok(sign(
                    key,
                    model::RouteState {
                        history: Vec::new(),
                        pin: head,
                        challenge: challenge.into(),
                        writes_allowed: moved.is_none(),
                        moved,
                    },
                ))
            });
            if cycle {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().unwrap().pin.term, 5);
            }
        }
        let mut count = 0;
        let mut current = pin.clone();
        let mut signer = a;
        let result = resolve("https://source.example", &pin, |_, challenge| {
            let target = SigningKey::from_bytes(&[50 + count as u8; 32]);
            let g = delegated(
                &g,
                &current,
                &signer,
                &target,
                &format!("https://node-{count}.example"),
            );
            let reply = sign(
                &signer,
                model::RouteState {
                    history: Vec::new(),
                    pin: current.clone(),
                    challenge: challenge.into(),
                    writes_allowed: false,
                    moved: Some(g.clone()),
                },
            );
            current = RoutePin {
                server_id: pin.server_id.clone(),
                host: identity(&target.verifying_key()),
                term: g.next_term,
            };
            signer = target;
            count += 1;
            Ok(reply)
        });
        assert!(result.is_err());
        assert_eq!(count, MAX_MOVES + 1);
    }

    #[test]
    fn a_cached_chain_can_return_to_an_old_address_under_a_new_authority_term() {
        let (a, b, pin, g) = fixture();
        let c = SigningKey::from_bytes(&[45; 32]);
        let second = RoutePin {
            server_id: pin.server_id.clone(),
            host: identity(&b.verifying_key()),
            term: 4,
        };
        let back = delegated(&g, &second, &b, &c, "https://source.example");
        let temp = tempfile::tempdir().unwrap();
        let mut cache = cache::Cache::at(temp.path().to_owned());
        cache.store("https://source.example", &pin, &g).unwrap();
        cache
            .store("https://target.example", &second, &back)
            .unwrap();
        let final_pin = RoutePin {
            server_id: pin.server_id.clone(),
            host: identity(&c.verifying_key()),
            term: 5,
        };
        let mut requests = 0;
        let result = resolve_cached(
            "https://source.example",
            &pin,
            |url, nonce| {
                requests += 1;
                assert_eq!(url, "https://source.example");
                Ok(sign(
                    &c,
                    model::RouteState {
                        history: Vec::new(),
                        pin: final_pin.clone(),
                        challenge: nonce.into(),
                        writes_allowed: true,
                        moved: None,
                    },
                ))
            },
            &mut cache,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.pin, final_pin);
        assert_eq!(requests, 1);
        let _ = a;
    }
}

#[cfg(test)]
mod invitation_proof_tests {
    use super::*;
    #[test]
    fn first_login_requires_a_live_writable_host_at_the_exact_delegated_address() {
        let (a, b, pin, g) = tests::fixture();
        let challenge = URL_SAFE_NO_PAD.encode([8; 32]);
        let mut state = model::RouteState {
            pin: pin.clone(),
            challenge: challenge.clone(),
            writes_allowed: true,
            moved: None,
            history: Vec::new(),
        };
        assert_eq!(
            confirm_reply(
                "https://source.example",
                &pin,
                &challenge,
                &tests::sign(&a, state.clone())
            )
            .unwrap(),
            pin
        );
        state.writes_allowed = false;
        let frozen = tests::sign(&a, state.clone());
        assert!(confirm_reply("https://source.example", &pin, &challenge, &frozen).is_err());
        assert!(resolve("https://source.example", &pin, |_, challenge| { let mut fresh=frozen.state.clone();fresh.challenge=challenge.into();Ok(tests::sign(&a,fresh)) }).is_err());
        let end = RoutePin {
            server_id: pin.server_id.clone(),
            host: g.receipt.body.target_identity.clone(),
            term: g.next_term,
        };
        state = model::RouteState {
            pin: end.clone(),
            challenge: challenge.clone(),
            writes_allowed: true,
            moved: None,
            history: vec![g.clone()],
        };
        let reply = tests::sign(&b, state.clone());
        assert_eq!(
            confirm_reply("https://target.example", &pin, &challenge, &reply).unwrap(),
            end
        );
        assert!(confirm_reply("https://unrelated.example", &pin, &challenge, &reply).is_err());
        assert!(confirm_reply(
            "https://target.example",
            &pin,
            &URL_SAFE_NO_PAD.encode([9; 32]),
            &reply
        )
        .is_err());
        state.history.clear();
        assert!(confirm_reply(
            "https://target.example",
            &pin,
            &challenge,
            &tests::sign(&b, state)
        )
        .is_err());
    }
}

#[cfg(test)]
mod session_fallback_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    #[test]
    fn only_unavailability_uses_the_saved_sessions_locator() {
        let (_,_,pin,_) = super::tests::fixture();
        let capability = crate::host_locator::ReadCapability::session(&pin,"synthetic-session").unwrap();
        let cancel = AtomicBool::new(false);
        let moved = with_session_fallback(Err(UNAVAILABLE.into()),Some(&capability),&cancel,|_|Ok(VerifiedMove{url:"https://new.example".into(),pin:pin.clone()})).unwrap().unwrap();
        assert_eq!(moved.url,"https://new.example");
        assert!(with_session_fallback(Err(REFUSED.into()),Some(&capability),&cancel,|_|panic!("identity refusal must not fall back")).is_err());
        assert!(with_session_fallback(Ok(None),Some(&capability),&cancel,|_|panic!("live host must not fall back")).unwrap().is_none());
        assert!(with_session_fallback(Err(UNAVAILABLE.into()),None,&cancel,|_|panic!("missing capability must not look up")).is_err());
        cancel.store(true,std::sync::atomic::Ordering::Release);
        assert!(with_session_fallback(Err(UNAVAILABLE.into()),Some(&capability),&cancel,|_|panic!("cancelled lookup")).is_err());
    }
}
