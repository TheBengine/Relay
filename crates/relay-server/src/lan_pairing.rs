//! Short-lived LAN pairing: a host hands a client that found it on the LAN a secure
//! invitation, authenticated by a random 128-bit code which the person shares by hand.
//!
//! Finding a server on the LAN proves nothing, and the plain-HTTP request carries no code
//! and no invitation: only an ID derived from the code, a fresh challenge and a MAC
//! made with a key derived from the code, and the answer is encrypted with another
//! key derived from the code. Whoever does not hold the code learns nothing, and
//! the client still demands a fresh proof from the real host before signing in.
//!
//! Everything lives in memory for at most two minutes. What is kept is the sealed
//! envelope, the MAC key, who created it, the authority it was made under, the hash of
//! the invitation it refers to (to refuse once that invitation is revoked, spent or
//! expired) and at most one bound challenge. The code and the invitation in plain are
//! never kept, logged, echoed or printed; none of these types has a Debug.
use super::*;
use axum::{
    body::Bytes,
    extract::{ConnectInfo, FromRequestParts},
    http::{header, request::Parts},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use hmac::{Hmac, Mac};
use relay_protocol::{
    handover::RoutePin,
    host_locator::Destination,
    lan_pairing::{self as model, Create, Created, Envelope, Purpose, Redeem, Revoke},
};
use std::net::IpAddr;
use zeroize::Zeroizing;

type Tag = Hmac<Sha256>;

const MAX_ATTEMPTS: u32 = 30;
const ATTEMPT_WINDOW_MS: u64 = 60_000;
const MAX_PEERS: usize = 1024;
const PRUNE_EVERY_MS: u64 = 1000;
/// The body limit of a revocation: one ID.
pub(super) const REVOKE_BODY_BYTES: usize = 1024;

/// Who was refused for what is never said: every redemption failure reads the same.
fn unavailable() -> ApiError {
    missing()
}

struct Entry {
    envelope: Envelope,
    auth: Zeroizing<[u8; 32]>,
    creator: String,
    authority: RoutePin,
    invite_hash: String,
    /// Bound by the first valid redemption; a retry must repeat it.
    challenge: Option<String>,
}

#[derive(Default)]
struct Peers {
    seen: std::collections::HashMap<IpAddr, (u64, u32)>,
    pruned_ms: u64,
}

/// The few live entries and the per-peer attempt counters. In memory only.
#[derive(Default)]
pub(super) struct Store {
    entries: Mutex<std::collections::HashMap<String, Entry>>,
    peers: Mutex<Peers>,
}

/// Why an entry was not stored.
enum Refused {
    Full,
    Duplicate,
}

impl Store {
    /// Drops expired entries only, then stores. A full store refuses; it never evicts
    /// a live entry, and an ID which exists (a reused code) is refused.
    fn insert(&self, id: String, entry: Entry, now_ms: u64) -> Result<(), Refused> {
        let mut entries = lock(&self.entries);
        entries.retain(|_, e| e.envelope.expires_ms > now_ms);
        let by_creator = entries
            .values()
            .filter(|e| e.creator == entry.creator)
            .count();
        if entries.len() >= model::MAX_ENTRIES || by_creator >= model::MAX_PER_OWNER {
            return Err(Refused::Full);
        }
        if entries.contains_key(&id) {
            return Err(Refused::Duplicate);
        }
        entries.insert(id, entry);
        Ok(())
    }

    fn remove(&self, id: &str) -> bool {
        lock(&self.entries).remove(id).is_some()
    }

    /// Counts one attempt by this peer. Bounded: at most MAX_PEERS peers are tracked,
    /// expired counters are dropped at most once a second, and a new peer is refused
    /// while the table is full.
    fn admit(&self, ip: IpAddr, now_ms: u64) -> bool {
        let mut peers = lock(&self.peers);
        if now_ms.saturating_sub(peers.pruned_ms) >= PRUNE_EVERY_MS {
            peers
                .seen
                .retain(|_, (start, _)| now_ms.saturating_sub(*start) < ATTEMPT_WINDOW_MS);
            peers.pruned_ms = now_ms;
        }
        if peers.seen.len() >= MAX_PEERS && !peers.seen.contains_key(&ip) {
            return false;
        }
        let counter = peers.seen.entry(ip).or_insert((now_ms, 0));
        if now_ms.saturating_sub(counter.0) >= ATTEMPT_WINDOW_MS {
            *counter = (now_ms, 0);
        }
        if counter.1 >= MAX_ATTEMPTS {
            return false;
        }
        counter.1 += 1;
        true
    }
}

// ---- Shapes ----

/// Canonical unpadded base64url holding exactly `bytes` bytes.
fn decode_exact(value: &str, bytes: usize) -> Option<Vec<u8>> {
    let raw = URL_SAFE_NO_PAD.decode(value).ok()?;
    (raw.len() == bytes && URL_SAFE_NO_PAD.encode(&raw) == value).then_some(raw)
}

fn code_bytes(value: &str) -> Option<Zeroizing<[u8; model::CODE_BYTES]>> {
    let raw = Zeroizing::new(decode_exact(value, model::CODE_BYTES)?);
    let mut code = Zeroizing::new([0u8; model::CODE_BYTES]);
    code.copy_from_slice(&raw);
    Some(code)
}

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn key(server_id: &str, code: &[u8; model::CODE_BYTES], purpose: Purpose) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(Sha256::digest(model::material(server_id, code, purpose)).into())
}

fn lookup_id(server_id: &str, code: &[u8; model::CODE_BYTES]) -> String {
    Sha256::digest(model::material(server_id, code, Purpose::Lookup))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A destination a client may be sent to: HTTPS to a real address, or this host's own
/// iroh route (endpoint, token and relay in the rules the locator uses, scoped to this
/// host and with the one canonical URL). Never plain HTTP, loopback, credentials, a
/// query or a fragment. Nothing is contacted.
fn destination_ok(destination: &Destination, current: &RoutePin) -> bool {
    let raw = &destination.url;
    if !handover::target_url(raw) || !handover::destination_route(raw, destination.iroh.as_ref()) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    if raw.trim_end_matches('/') != parsed.as_str().trim_end_matches('/') {
        return false;
    }
    match &destination.iroh {
        Some(route) => {
            route.scope.as_deref() == Some(current.host.host_id.as_str())
                && parsed.as_str() == format!("http://{}.iroh:4580/", route.endpoint_id)
        }
        None => {
            parsed.scheme() == "https"
                && parsed.host_str().is_some_and(|host| {
                    let host = host.trim_matches(['[', ']']);
                    !host.eq_ignore_ascii_case("localhost")
                        && host.parse::<IpAddr>().map_or(true, |ip| match ip {
                            IpAddr::V4(v) => {
                                !(v.is_loopback()
                                    || v.is_unspecified()
                                    || v.is_link_local()
                                    || v.is_multicast())
                            }
                            IpAddr::V6(v) => {
                                !(v.is_loopback()
                                    || v.is_unspecified()
                                    || v.is_multicast()
                                    || v.is_unicast_link_local())
                            }
                        })
                })
        }
    }
}

/// The authority is the writable current one, exactly.
fn current(state: &AppState) -> ApiResult<RoutePin> {
    if !state.writes_allowed() {
        return Err(forbidden());
    }
    Ok(state.0.handover.route_pin())
}

// ---- Create and revoke: community administrators ----

/// An administrator makes a code's envelope. The code is in the request, never in the
/// answer.
pub(super) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Create>,
) -> ApiResult<Json<Created>> {
    if state.0.config.people {
        return Err(forbidden());
    }
    let auth = state.auth(&headers)?;
    let pin = current(&state)?;
    let code = code_bytes(&input.code).ok_or_else(|| bad("Invalid pairing request"))?;
    let invitation = &input.invitation;
    if invitation.code.is_empty() || invitation.code.len() > 128 {
        return Err(bad("Invalid pairing request"));
    }
    let (server, invite_hash, invite_expiry) = {
        let db = lock(&state.0.db);
        roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
        let server = server_id(&db)?;
        // The invitation must be this community's, under this exact writable authority.
        if invitation.server_id != server
            || invitation.authority != pin
            || !destination_ok(&invitation.destination, &pin)
        {
            return Err(bad("The invitation does not match this community"));
        }
        // Exactly the invitation semantics registration uses.
        let hash = token_hash(invitation.code.trim());
        let expires: u64 = db
            .query_row(
                "SELECT expires FROM invites WHERE code_hash=?1 AND expires>?2 AND remaining_uses>0",
                params![hash, now()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(forbidden)?;
        (server, hash, expires)
    };
    let now_ms = now_ms();
    // Never longer than the invitation itself.
    let expires_ms = now_ms
        .saturating_add(model::LIFETIME_MS)
        .min(invite_expiry.saturating_mul(1000));
    if expires_ms <= now_ms {
        return Err(forbidden());
    }
    let plaintext = Zeroizing::new(
        serde_json::to_vec(&input.invitation).map_err(|_| bad("Invalid pairing request"))?,
    );
    if plaintext.len() + 16 > model::MAX_CIPHERTEXT_BYTES {
        return Err(bad("The invitation is too large"));
    }
    let id = lookup_id(&server, &code);
    let auth_key = key(&server, &code, Purpose::Auth);
    let seal = key(&server, &code, Purpose::Seal);
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let mut envelope = Envelope {
        server_id: server,
        id: id.clone(),
        expires_ms,
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: String::new(),
    };
    let sealed = ChaCha20Poly1305::new_from_slice(&seal[..])
        .map_err(|_| bad("Pairing is unavailable"))?
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: &model::associated_data(&envelope),
            },
        )
        .map_err(|_| bad("Pairing is unavailable"))?;
    envelope.ciphertext = URL_SAFE_NO_PAD.encode(sealed);
    if serde_json::to_vec(&envelope).map_or(true, |raw| raw.len() > model::MAX_ENVELOPE_BYTES) {
        return Err(bad("The invitation is too large"));
    }
    let entry = Entry {
        envelope,
        auth: auth_key,
        creator: auth.user.id.clone(),
        authority: pin.clone(),
        invite_hash,
        challenge: None,
    };
    match state.0.lan_pairing.insert(id.clone(), entry, now_ms) {
        Ok(()) => {}
        Err(Refused::Full) => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many pairing codes are active. Wait for one to expire or revoke it",
            ))
        }
        Err(Refused::Duplicate) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "This pairing code cannot be used",
            ))
        }
    }
    // The authority is checked again after the work: a host which stopped being the
    // writable one in the meantime keeps no entry.
    if current(&state).ok().as_ref() != Some(&pin) {
        state.0.lan_pairing.remove(&id);
        return Err(forbidden());
    }
    Ok(Json(Created { id, expires_ms }))
}

/// An administrator ends one code early. Only an exact, existing ID is removed.
pub(super) async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Revoke>,
) -> ApiResult<StatusCode> {
    if state.0.config.people {
        return Err(forbidden());
    }
    let auth = state.auth(&headers)?;
    current(&state)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_ADMIN)?;
    if !hex64(&input.id) || !state.0.lan_pairing.remove(&input.id) {
        return Err(missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- Redeem: the one unauthenticated route, for a peer on the LAN ----

/// Extracted before the body: this is a community on a LAN peer's real socket, with no
/// proxy, browser or network-helper marker, and the peer is within its attempt budget.
pub(super) struct LanPeer;

fn lan_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_link_local() || v.is_loopback(),
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(mapped) => lan_address(IpAddr::V4(mapped)),
            None => v.is_loopback(),
        },
    }
}

impl FromRequestParts<AppState> for LanPeer {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if state.0.config.people || parts.extensions.get::<NetworkIngress>().is_some() {
            return Err(forbidden());
        }
        // The socket's own address; a claimed address in any header is never read.
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0.ip())
            .filter(|ip| lan_address(*ip))
            .ok_or_else(forbidden)?;
        // An explicitly configured reverse proxy is not a directly discovered LAN peer,
        // even when it omitted its forwarding headers.
        if state.0.config.trusted_proxy_ips.contains(&ip) {
            return Err(forbidden());
        }
        if parts.headers.keys().any(|k| {
            let name = k.as_str();
            name == "origin"
                || name == "sec-fetch-site"
                || name == "forwarded"
                || name == "x-real-ip"
                || name.starts_with("x-forwarded-")
        }) {
            return Err(forbidden());
        }
        if !state.0.lan_pairing.admit(ip, now_ms()) {
            return Err(ApiError::cooldown(
                "rate_limited",
                "Too many attempts. Wait a minute",
                ATTEMPT_WINDOW_MS,
                None,
            ));
        }
        if parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|length| length > model::MAX_REDEEM_BYTES)
        {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "This request is too large",
            ));
        }
        Ok(Self)
    }
}

/// The sealed envelope for a valid request, or the same plain "not found" for every
/// other reason. Failed authentication consumes nothing; the first valid request binds
/// its challenge, and only that challenge may fetch the envelope again.
pub(super) async fn redeem(
    State(state): State<AppState>,
    _peer: LanPeer,
    body: Bytes,
) -> ApiResult<Json<Envelope>> {
    let request: Redeem = serde_json::from_slice(&body).map_err(|_| bad("Invalid request"))?;
    drop(body);
    let challenge = decode_exact(&request.challenge, model::CHALLENGE_BYTES);
    let tag = decode_exact(&request.tag, 32);
    if !hex64(&request.id) || challenge.is_none() || tag.is_none() {
        return Err(unavailable());
    }
    let tag = tag.unwrap_or_default();
    // Locks: the invites table first, then the entries (creation never nests them).
    let db = lock(&state.0.db);
    if request.server_id != server_id(&db).map_err(|_| unavailable())? {
        return Err(unavailable());
    }
    let now_ms = now_ms();
    let mut entries = lock(&state.0.lan_pairing.entries);
    let Some(entry) = entries.get_mut(&request.id) else {
        return Err(unavailable());
    };
    if entry.envelope.expires_ms <= now_ms {
        entries.remove(&request.id);
        return Err(unavailable());
    }
    let mut mac = <Tag as Mac>::new_from_slice(&entry.auth[..]).map_err(|_| unavailable())?;
    mac.update(&model::redeem_message(&request));
    // Constant time; a wrong MAC leaves the entry as it was.
    if mac.verify_slice(&tag).is_err() {
        return Err(unavailable());
    }
    // The host must still be the writable authority the entry was made under, and the
    // invitation must still be usable right now (not revoked, expired or used up).
    let authority_ok =
        state.writes_allowed() && state.0.handover.route_pin() == entry.authority;
    let invite_ok = db
        .query_row(
            "SELECT 1 FROM invites WHERE code_hash=?1 AND expires>?2 AND remaining_uses>0",
            params![entry.invite_hash, now()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| unavailable())?
        .is_some();
    if !authority_ok || !invite_ok {
        if !invite_ok {
            entries.remove(&request.id);
        }
        return Err(unavailable());
    }
    match entry.challenge.clone() {
        Some(bound) if bound != request.challenge => return Err(unavailable()),
        Some(_) => {}
        None => entry.challenge = Some(request.challenge.clone()),
    }
    Ok(Json(entry.envelope.clone()))
}

/// The exact POST for the one global middleware exemption (a redemption has no session).
pub(super) fn is_redeem_route(path: &str, method: &axum::http::Method) -> bool {
    method == axum::http::Method::POST && path == model::REDEEM_PATH
}

/// The pairing-only LAN listener's router: the ordinary router behind one outer filter
/// which lets exactly `POST REDEEM_PATH` through and answers everything else with the
/// same plain "not found", before any body is read and before any session is looked
/// at. It is not a proxy and gives no loopback privilege: the ordinary routes are still
/// there but cannot be reached through this filter, and the redemption keeps its own LAN
/// admission, per-peer budget and MAC. The host check, request budget and shutdown of the
/// ordinary router stay in force beneath it.
pub(super) fn narrow(router: Router) -> Router {
    router.layer(middleware::from_fn(only_redemption))
}

async fn only_redemption(request: axum::extract::Request, next: Next) -> Response {
    if is_redeem_route(request.uri().path(), request.method()) {
        next.run(request).await
    } else {
        unavailable().into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{new_invite, request, signup};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use relay_protocol::lan_pairing::Invitation;
    use serde_json::{json, Value};
    use std::net::Ipv4Addr;
    use tower::ServiceExt;

    const DESTINATION: &str = "https://host.example";
    const LAN: &str = "192.168.1.20:50000";

    struct Lab {
        _data: tempfile::TempDir,
        state: AppState,
        app: Router,
        owner: Session,
        invite: String,
    }

    async fn lab() -> Lab {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "pairing", 0).unwrap();
        let app = router(state.clone());
        let owner = signup(&app, "Pairowner", "").await;
        let invite = new_invite(&app, &owner).await;
        Lab {
            _data: data,
            state,
            app,
            owner,
            invite,
        }
    }

    fn random_code() -> (String, [u8; model::CODE_BYTES]) {
        let mut raw = [0u8; model::CODE_BYTES];
        OsRng.fill_bytes(&mut raw);
        (URL_SAFE_NO_PAD.encode(raw), raw)
    }

    impl Lab {
        fn invitation(&self, invite: &str) -> Invitation {
            Invitation {
                server_id: self.state.server_id().unwrap(),
                code: invite.to_owned(),
                authority: self.state.0.handover.route_pin(),
                destination: Destination {
                    url: DESTINATION.into(),
                    iroh: None,
                },
            }
        }
        async fn create(&self, code: &str, invitation: &Invitation) -> (StatusCode, Value) {
            request(
                &self.app,
                "POST",
                model::CREATE_PATH,
                Some(&self.owner.token),
                json!({"code": code, "invitation": invitation}),
            )
            .await
        }
        /// A signed redemption the way the client makes it.
        fn redeem_body(&self, code: &[u8; model::CODE_BYTES], challenge: &[u8]) -> Redeem {
            self.redeem_as(code, code, challenge)
        }
        /// A request naming the entry of `id_code` but signed with the key of `mac_code`.
        fn redeem_as(
            &self,
            id_code: &[u8; model::CODE_BYTES],
            mac_code: &[u8; model::CODE_BYTES],
            challenge: &[u8],
        ) -> Redeem {
            let server = self.state.server_id().unwrap();
            let mut redeem = Redeem {
                server_id: server.clone(),
                id: lookup_id(&server, id_code),
                challenge: URL_SAFE_NO_PAD.encode(challenge),
                tag: String::new(),
            };
            let auth = key(&server, mac_code, Purpose::Auth);
            let mut mac = <Tag as Mac>::new_from_slice(&auth[..]).unwrap();
            mac.update(&model::redeem_message(&redeem));
            redeem.tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
            redeem
        }
        async fn redeem(
            &self,
            redeem: &Redeem,
            peer: Option<&str>,
            extra: &[(&str, &str)],
        ) -> (StatusCode, Vec<u8>) {
            self.send(serde_json::to_vec(redeem).unwrap(), peer, extra, None)
                .await
        }
        async fn send(
            &self,
            body: Vec<u8>,
            peer: Option<&str>,
            extra: &[(&str, &str)],
            helper: Option<NetworkIngress>,
        ) -> (StatusCode, Vec<u8>) {
            let mut builder = Request::builder()
                .header("host", "192.168.1.5")
                .method("POST")
                .uri(model::REDEEM_PATH)
                .header("content-type", "application/json");
            for (name, value) in extra {
                builder = builder.header(*name, *value);
            }
            if let Some(peer) = peer {
                builder = builder.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            }
            if let Some(helper) = helper {
                builder = builder.extension(helper);
            }
            let response = self
                .app
                .clone()
                .oneshot(builder.body(Body::from(body)).unwrap())
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
    }

    fn open(
        envelope: &Envelope,
        server: &str,
        code: &[u8; model::CODE_BYTES],
    ) -> Option<Invitation> {
        let nonce = URL_SAFE_NO_PAD.decode(&envelope.nonce).ok()?;
        let sealed = URL_SAFE_NO_PAD.decode(&envelope.ciphertext).ok()?;
        let seal = key(server, code, Purpose::Seal);
        let plain = ChaCha20Poly1305::new_from_slice(&seal[..])
            .ok()?
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &sealed,
                    aad: &model::associated_data(envelope),
                },
            )
            .ok()?;
        serde_json::from_slice(&plain).ok()
    }

    #[tokio::test]
    async fn a_code_creates_one_envelope_which_only_the_matching_challenge_and_code_can_fetch() {
        let lab = lab().await;
        let (code, raw) = random_code();
        let invitation = lab.invitation(&lab.invite);
        let (status, created) = lab.create(&code, &invitation).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let server = lab.state.server_id().unwrap();
        assert_eq!(created["id"], lookup_id(&server, &raw));
        // The reply names an ID and a time, never the code, the invitation or the destination.
        let text = created.to_string();
        assert!(
            !text.contains(&code) && !text.contains(&lab.invite) && !text.contains("host.example")
        );
        let ttl = created["expires_ms"].as_u64().unwrap() - now_ms();
        assert!(ttl <= model::LIFETIME_MS && ttl > 100_000);
        // A wrong code's MAC is refused and consumes nothing.
        let (_, wrong_raw) = random_code();
        let forged = lab.redeem_as(&raw, &wrong_raw, &[7u8; 16]);
        let unknown = lab.redeem_as(&wrong_raw, &wrong_raw, &[7u8; 16]);
        for attempt in [&forged, &unknown] {
            let (status, _) = lab.redeem(attempt, Some(LAN), &[]).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        // The right code gets the sealed envelope, which opens to the invitation.
        let first = lab.redeem_body(&raw, &[1u8; 16]);
        let (status, body) = lab.redeem(&first, Some(LAN), &[]).await;
        assert_eq!(status, StatusCode::OK);
        let envelope: Envelope = serde_json::from_slice(&body).unwrap();
        let opened = open(&envelope, &server, &raw).expect("the code opens it");
        assert_eq!(
            (opened.code.as_str(), opened.destination.url.as_str()),
            (lab.invite.as_str(), DESTINATION)
        );
        // The plain request and the answer show neither the code, the invitation nor the key.
        let wire =
            String::from_utf8_lossy(&body).into_owned() + &serde_json::to_string(&first).unwrap();
        assert!(
            !wire.contains(&code) && !wire.contains(&lab.invite) && !wire.contains("host.example")
        );
        // The same challenge again: the very same envelope. Another challenge: refused.
        let (status, again) = lab.redeem(&first, Some(LAN), &[]).await;
        assert_eq!((status, again), (StatusCode::OK, body));
        let other = lab.redeem_body(&raw, &[2u8; 16]);
        assert_eq!(
            lab.redeem(&other, Some(LAN), &[]).await.0,
            StatusCode::NOT_FOUND
        );
        // A code of the right shape which was never made, and the same code made twice.
        let (_, stranger) = random_code();
        let stranger = lab.redeem_body(&stranger, &[1u8; 16]);
        assert_eq!(
            lab.redeem(&stranger, Some(LAN), &[]).await.0,
            StatusCode::NOT_FOUND
        );
        let (status, _) = lab.create(&code, &invitation).await;
        assert_eq!(status, StatusCode::CONFLICT, "a code is used once");
    }

    #[tokio::test]
    async fn expiry_revocation_a_revoked_invitation_and_the_writer_barrier_end_an_entry() {
        let lab = lab().await;
        let server = lab.state.server_id().unwrap();
        // Never longer than two minutes, and never longer than the invitation itself.
        let (code, raw) = random_code();
        let (status, created) = lab.create(&code, &lab.invitation(&lab.invite)).await;
        assert_eq!(status, StatusCode::OK);
        let id = created["id"].as_str().unwrap().to_owned();
        lock(&lab.state.0.db)
            .execute(
                "UPDATE invites SET expires=?1 WHERE code_hash=?2",
                params![now() + 30, token_hash(&lab.invite)],
            )
            .unwrap();
        let (short, _) = random_code();
        let (status, created) = lab.create(&short, &lab.invitation(&lab.invite)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(created["expires_ms"].as_u64().unwrap() <= (now() + 30) * 1000);
        // Expiry: the entry is gone when its time has passed.
        lock(&lab.state.0.lan_pairing.entries)
            .get_mut(&id)
            .unwrap()
            .envelope
            .expires_ms = now_ms() - 1;
        let body = lab.redeem_body(&raw, &[1u8; 16]);
        assert_eq!(
            lab.redeem(&body, Some(LAN), &[]).await.0,
            StatusCode::NOT_FOUND
        );
        assert!(!lock(&lab.state.0.lan_pairing.entries).contains_key(&id));
        // Revocation by an administrator removes exactly that entry.
        let (code2, raw2) = random_code();
        let invite2 = new_invite(&lab.app, &lab.owner).await;
        let (_, made) = lab.create(&code2, &lab.invitation(&invite2)).await;
        let id2 = made["id"].as_str().unwrap().to_owned();
        let (status, _) = request(
            &lab.app,
            "POST",
            model::REVOKE_PATH,
            Some(&lab.owner.token),
            json!({"id": "f".repeat(64)}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = request(
            &lab.app,
            "POST",
            model::REVOKE_PATH,
            Some(&lab.owner.token),
            json!({"id": id2}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let gone = lab.redeem_body(&raw2, &[1u8; 16]);
        assert_eq!(
            lab.redeem(&gone, Some(LAN), &[]).await.0,
            StatusCode::NOT_FOUND
        );
        // The invitation is revoked after the envelope was fetched once: a retry with the
        // very same challenge is now refused, because it is checked on every redemption.
        let (code3, raw3) = random_code();
        let invite3 = new_invite(&lab.app, &lab.owner).await;
        lab.create(&code3, &lab.invitation(&invite3)).await;
        let fetch = lab.redeem_body(&raw3, &[3u8; 16]);
        assert_eq!(lab.redeem(&fetch, Some(LAN), &[]).await.0, StatusCode::OK);
        lock(&lab.state.0.db)
            .execute(
                "DELETE FROM invites WHERE code_hash=?1",
                [token_hash(&invite3)],
            )
            .unwrap();
        assert_eq!(
            lab.redeem(&fetch, Some(LAN), &[]).await.0,
            StatusCode::NOT_FOUND
        );
        // A spent invitation is refused the same way; creating for one is refused outright.
        let (code4, _) = random_code();
        let spent = new_invite(&lab.app, &lab.owner).await;
        lock(&lab.state.0.db)
            .execute(
                "UPDATE invites SET remaining_uses=0 WHERE code_hash=?1",
                [token_hash(&spent)],
            )
            .unwrap();
        assert_eq!(
            lab.create(&code4, &lab.invitation(&spent)).await.0,
            StatusCode::FORBIDDEN
        );
        // A pin which is not this host's current one is refused, before and after.
        let (code5, raw5) = random_code();
        let invite5 = new_invite(&lab.app, &lab.owner).await;
        let mut moved = lab.invitation(&invite5);
        moved.authority.term += 1;
        assert_eq!(lab.create(&code5, &moved).await.0, StatusCode::BAD_REQUEST);
        lab.create(&code5, &lab.invitation(&invite5)).await;
        let held = lab.redeem_body(&raw5, &[5u8; 16]);
        lab.state.0.handover.begin_restore(server.clone()).unwrap();
        assert!(!lab.redeem(&held, Some(LAN), &[]).await.0.is_success());
        let (code6, _) = random_code();
        assert!(!lab
            .create(&code6, &lab.invitation(&invite5))
            .await
            .0
            .is_success());
    }

    #[tokio::test]
    async fn only_an_administrator_on_a_community_creates_and_only_a_real_lan_peer_redeems() {
        let lab = lab().await;
        let (code, raw) = random_code();
        let invitation = lab.invitation(&lab.invite);
        assert_eq!(lab.create(&code, &invitation).await.0, StatusCode::OK);
        // A member who is not an administrator, and nobody at all.
        let member_invite = new_invite(&lab.app, &lab.owner).await;
        let member = signup(&lab.app, "Pairmember", &member_invite).await;
        let (other, _) = random_code();
        let (status, _) = request(
            &lab.app,
            "POST",
            model::CREATE_PATH,
            Some(&member.token),
            json!({"code": other, "invitation": invitation}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request(
            &lab.app,
            "POST",
            model::CREATE_PATH,
            None,
            json!({"code": other, "invitation": invitation}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = request(
            &lab.app,
            "POST",
            model::REVOKE_PATH,
            Some(&member.token),
            json!({"id": "a".repeat(64)}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // A malformed code, or an invitation for another community, never makes an entry.
        let prefixed = format!("LAN-{code}");
        let padded = format!("{code}=");
        for bad_code in ["short", prefixed.as_str(), padded.as_str()] {
            let (status, _) = lab.create(bad_code, &invitation).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad_code}");
        }
        let mut elsewhere = invitation.clone();
        elsewhere.server_id = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            lab.create(&other, &elsewhere).await.0,
            StatusCode::BAD_REQUEST
        );
        // Destinations which are not allowed: plain HTTP, loopback, credentials, query, fragment.
        for url in [
            "http://host.example",
            "https://127.0.0.1",
            "https://localhost",
            "https://user:pw@host.example",
            "https://host.example/?x=1",
            "https://host.example/#f",
            "https://169.254.1.1",
        ] {
            let mut bad = invitation.clone();
            bad.destination.url = url.into();
            assert_eq!(
                lab.create(&other, &bad).await.0,
                StatusCode::BAD_REQUEST,
                "{url}"
            );
        }

        // The redeeming request: real LAN socket only, no proxy, browser or helper marker.
        let body = lab.redeem_body(&raw, &[1u8; 16]);
        for peer in [
            "192.168.1.20:5",
            "10.0.0.7:5",
            "172.16.3.4:5",
            "169.254.9.9:5",
            "127.0.0.1:5",
        ] {
            assert_eq!(
                lab.redeem(&body, Some(peer), &[]).await.0,
                StatusCode::OK,
                "{peer}"
            );
        }
        for (name, peer, extra) in [
            ("no socket", None, vec![]),
            ("public address", Some("203.0.113.9:5"), vec![]),
            ("public ipv6", Some("[2001:db8::1]:5"), vec![]),
            (
                "forwarded",
                Some(LAN),
                vec![("x-forwarded-for", "192.168.1.9")],
            ),
            (
                "forwarded header",
                Some(LAN),
                vec![("forwarded", "for=192.168.1.9")],
            ),
            ("real ip", Some(LAN), vec![("x-real-ip", "192.168.1.9")]),
            (
                "browser origin",
                Some(LAN),
                vec![("origin", "https://evil.example")],
            ),
            (
                "browser fetch",
                Some(LAN),
                vec![("sec-fetch-site", "cross-site")],
            ),
        ] {
            let (status, reply) = lab.redeem(&body, peer, &extra).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{name}");
            assert!(
                !String::from_utf8_lossy(&reply).contains("ciphertext"),
                "{name}"
            );
        }
        let helper = NetworkIngress {
            peer_ip: "100.64.0.9".parse().unwrap(),
        };
        let (status, _) = lab
            .send(
                serde_json::to_vec(&body).unwrap(),
                Some(LAN),
                &[],
                Some(helper),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "the network helper's marker");
        // The create route is not open to the LAN: it needs a session, even from a peer.
        let response = lab
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .header("host", "192.168.1.5")
                    .method("POST")
                    .uri(model::CREATE_PATH)
                    .extension(ConnectInfo(LAN.parse::<SocketAddr>().unwrap()))
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // Nothing in the body before it is bounded: over the cap is refused before parsing.
        let (status, _) = lab
            .send(
                vec![b' '; model::MAX_REDEEM_BYTES + 1],
                Some(LAN),
                &[],
                None,
            )
            .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        for malformed in [
            b"nope".to_vec(),
            serde_json::to_vec(&json!({"server_id": "x"})).unwrap(),
            serde_json::to_vec(
                &json!({"server_id":"x","id":"y","challenge":"z","tag":"t","code":"c"}),
            )
            .unwrap(),
        ] {
            let (status, _) = lab.send(malformed, Some(LAN), &[], None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        // Wrong shapes of otherwise valid fields read as the same "not found".
        for variant in 0..4 {
            let mut bad = lab.redeem_body(&raw, &[1u8; 16]);
            match variant {
                0 => bad.server_id = "another".into(),
                1 => bad.id = bad.id.to_uppercase(),
                2 => bad.challenge = "short".into(),
                _ => bad.tag = "short".into(),
            }
            assert_eq!(
                lab.redeem(&bad, Some(LAN), &[]).await.0,
                StatusCode::NOT_FOUND,
                "variant {variant}"
            );
        }
        // A Friends service has no such routes.
        let data = tempfile::tempdir().unwrap();
        let provider = account_identity::Provider::new(
            "https://accounts.example.test/realms/relay-accounts",
            "relay-desktop",
            "https://people.example.test",
        )
        .unwrap();
        let friends = AppState::open_with_config(
            data.path(),
            "Relay friends",
            0,
            ServerConfig {
                people: true,
                account_provider: Some(provider),
                ..Default::default()
            },
        )
        .unwrap();
        let app = router(friends);
        for path in [model::CREATE_PATH, model::REDEEM_PATH, model::REVOKE_PATH] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .header("host", "192.168.1.5")
                        .method("POST")
                        .uri(path)
                        .extension(ConnectInfo(LAN.parse::<SocketAddr>().unwrap()))
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(!response.status().is_success(), "{path}");
        }
        // The capability is announced by a community, never by Friends.
        let (_, boot) = request(
            &lab.app,
            "GET",
            "/api/bootstrap",
            Some(&lab.owner.token),
            Value::Null,
        )
        .await;
        assert!(boot["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == model::CAPABILITY));
    }

    #[test]
    fn entries_peers_and_capacity_are_bounded_and_never_evict_a_live_one() {
        let store = Store::default();
        let pin = RoutePin {
            server_id: uuid::Uuid::new_v4().to_string(),
            host: relay_protocol::handover::HostIdentity {
                host_id: uuid::Uuid::new_v4().to_string(),
                public_key: "k".into(),
            },
            term: 1,
        };
        let entry = |creator: &str, expires_ms: u64| Entry {
            envelope: Envelope {
                server_id: "s".into(),
                id: String::new(),
                expires_ms,
                nonce: "n".into(),
                ciphertext: "c".into(),
            },
            auth: Zeroizing::new([1u8; 32]),
            creator: creator.into(),
            authority: pin.clone(),
            invite_hash: "h".into(),
            challenge: None,
        };
        let now = 1_000_000;
        // 8 per creator, 32 in all, and nothing live is evicted to make room.
        for n in 0..model::MAX_PER_OWNER {
            assert!(store
                .insert(format!("a{n}"), entry("alice", now + 100), now)
                .is_ok());
        }
        assert!(matches!(
            store.insert("a9".into(), entry("alice", now + 100), now),
            Err(Refused::Full)
        ));
        for owner in ["bob", "cara", "dave"] {
            for n in 0..model::MAX_PER_OWNER {
                assert!(store
                    .insert(format!("{owner}{n}"), entry(owner, now + 100), now)
                    .is_ok());
            }
        }
        assert_eq!(lock(&store.entries).len(), model::MAX_ENTRIES);
        assert!(matches!(
            store.insert("eve0".into(), entry("eve", now + 100), now),
            Err(Refused::Full)
        ));
        assert_eq!(
            lock(&store.entries).len(),
            model::MAX_ENTRIES,
            "no live entry was dropped"
        );
        // A reused ID is refused while it lives; expired entries are the only ones pruned.
        assert!(matches!(
            store.insert("a0".into(), entry("alice", now + 100), now + 50),
            Err(Refused::Full) | Err(Refused::Duplicate)
        ));
        assert!(store
            .insert("eve0".into(), entry("eve", now + 500), now + 200)
            .is_ok());
        assert_eq!(
            lock(&store.entries).len(),
            1,
            "everything expired was pruned, the new one kept"
        );
        assert!(matches!(
            store.insert("eve0".into(), entry("eve", now + 500), now + 201),
            Err(Refused::Duplicate)
        ));
        assert!(store.remove("eve0") && !store.remove("eve0"));
        // Thirty attempts per peer per minute; others are unaffected.
        let peer: IpAddr = "192.168.1.9".parse().unwrap();
        for _ in 0..MAX_ATTEMPTS {
            assert!(store.admit(peer, now));
        }
        assert!(!store.admit(peer, now + 1));
        assert!(store.admit("192.168.1.10".parse().unwrap(), now + 1));
        assert!(store.admit(peer, now + ATTEMPT_WINDOW_MS), "a new minute");
        // At most 1024 peers are tracked: a new one is refused while the table is full.
        let full = Store::default();
        for n in 0..MAX_PEERS as u32 {
            assert!(full.admit(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n)), now));
        }
        assert!(!full.admit("10.9.9.9".parse().unwrap(), now + 1));
        assert!(
            full.admit(IpAddr::V4(Ipv4Addr::from(0x0a00_0000)), now + 1),
            "a tracked peer still counts"
        );
        // Pruning happens at most once a second, and frees the table once counters expire.
        assert!(full.admit(
            "10.9.9.9".parse().unwrap(),
            now + ATTEMPT_WINDOW_MS + PRUNE_EVERY_MS
        ));
        assert!(lock(&full.peers).seen.len() <= MAX_PEERS);
    }
    #[tokio::test]
    async fn a_configured_proxy_cannot_redeem_without_forwarded_headers() {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open_with_config(
            data.path(),
            "pairing proxy",
            0,
            ServerConfig {
                trusted_proxy_ips: vec!["127.0.0.1".parse().unwrap()],
                ..Default::default()
            },
        )
        .unwrap();
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(model::REDEEM_PATH)
                    .header("host", "127.0.0.1")
                    .extension(ConnectInfo(
                        "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
                    ))
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Every other route and method is answered "not found" before its body is read,
    /// even with a real administrator session, and nothing behind it runs.
    #[tokio::test]
    async fn the_pairing_only_router_reaches_nothing_but_the_redemption_even_with_an_admin_session()
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        let lab = lab().await;
        let (code, raw) = random_code();
        assert_eq!(
            lab.create(&code, &lab.invitation(&lab.invite)).await.0,
            StatusCode::OK
        );
        let app = crate::lan_pairing_router(lab.state.clone());
        let invites = |lab: &Lab| -> i64 {
            lock(&lab.state.0.db)
                .query_row("SELECT COUNT(*) FROM invites", [], |r| r.get(0))
                .unwrap()
        };
        let before = invites(&lab);
        for (method, path) in [
            ("GET", "/api/health"),
            ("GET", "/health"),
            ("POST", "/api/login"),
            ("POST", "/api/account-login"),
            ("GET", "/api/bootstrap"),
            ("POST", "/api/invites"),
            ("GET", "/api/invites"),
            ("POST", model::CREATE_PATH),
            ("POST", model::REVOKE_PATH),
            ("GET", model::REDEEM_PATH),
            ("PUT", model::REDEEM_PATH),
            ("DELETE", model::REDEEM_PATH),
            ("OPTIONS", model::REDEEM_PATH),
            ("POST", "/api/lan-pairing/redeem/"),
            ("POST", "/api/lan-pairing/redeem/extra"),
            ("POST", "/api/lan-pairing"),
            ("POST", "/api/local/shutdown"),
            ("POST", "/api/local/handover/locator-page"),
            ("POST", "/api/uploads"),
            ("GET", "/api/events"),
            ("GET", "/api/media/frames"),
            ("POST", "/api/voice/join"),
            ("GET", "/rtc"),
            ("GET", "/"),
        ] {
            let polled = Arc::new(AtomicBool::new(false));
            let seen = polled.clone();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .header("host", "192.168.1.5")
                        .method(method)
                        .uri(path)
                        .header("authorization", format!("Bearer {}", lab.owner.token))
                        .extension(ConnectInfo(LAN.parse::<SocketAddr>().unwrap()))
                        .body(Body::from_stream(futures_util::stream::poll_fn(
                            move |_| {
                                seen.store(true, Ordering::SeqCst);
                                std::task::Poll::Ready(None::<Result<Bytes, std::io::Error>>)
                            },
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
            assert!(
                !polled.load(Ordering::SeqCst),
                "{method} {path} read its body"
            );
        }
        // Nothing behind the filter ran: no invitation made, no entry added, no shutdown.
        assert_eq!(invites(&lab), before);
        assert_eq!(lock(&lab.state.0.lan_pairing.entries).len(), 1);
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            lab.state.shutdown_requested()
        )
        .await
        .is_err());
        // The redemption itself works here with the same LAN admission as anywhere.
        let redeem = |peer: &'static str, body: Vec<u8>| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .header("host", "192.168.1.5")
                            .method("POST")
                            .uri(model::REDEEM_PATH)
                            .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()))
                            .body(Body::from(body))
                            .unwrap(),
                    )
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
        };
        let good = serde_json::to_vec(&lab.redeem_body(&raw, &[1u8; 16])).unwrap();
        let (status, body) = redeem(LAN, good.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let envelope: Envelope = serde_json::from_slice(&body).unwrap();
        assert!(open(&envelope, &lab.state.server_id().unwrap(), &raw).is_some());
        assert_eq!(
            redeem("203.0.113.9:5", good.clone()).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            redeem(LAN, vec![b' '; model::MAX_REDEEM_BYTES + 1]).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    /// A real socket on an OS-chosen port: the port which would be advertised is the one
    /// bound, and over actual TCP only the redemption is served.
    #[tokio::test]
    async fn a_real_ephemeral_listener_serves_only_the_redemption() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let lab = lab().await;
        let (code, raw) = random_code();
        assert_eq!(
            lab.create(&code, &lab.invitation(&lab.invite)).await.0,
            StatusCode::OK
        );
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 0, "the OS chose and holds a real port");
        let app = crate::lan_pairing_router(lab.state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        let exchange = |request: String| async move {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            socket.write_all(request.as_bytes()).await.unwrap();
            let mut answer = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                socket.read_to_end(&mut answer),
            )
            .await
            .expect("an answer arrives")
            .unwrap();
            String::from_utf8_lossy(&answer).into_owned()
        };
        let post = |path: &str, body: &str, extra: &str| {
            format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let admin = format!("Authorization: Bearer {}\r\n", lab.owner.token);
        let denied = exchange(format!(
            "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n{admin}Connection: close\r\n\r\n"
        ))
        .await;
        assert!(denied.starts_with("HTTP/1.1 404"), "{denied}");
        let denied = exchange(post("/api/invites", "{}", &admin)).await;
        assert!(denied.starts_with("HTTP/1.1 404"), "{denied}");
        let good = serde_json::to_string(&lab.redeem_body(&raw, &[1u8; 16])).unwrap();
        let answer = exchange(post(model::REDEEM_PATH, &good, "")).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        let body = answer.split("\r\n\r\n").nth(1).unwrap();
        let envelope: Envelope = serde_json::from_str(body).unwrap();
        assert!(open(&envelope, &lab.state.server_id().unwrap(), &raw).is_some());
        // The ordinary API of the same state is not on this socket and is unchanged.
        assert_eq!(lock(&lab.state.0.lan_pairing.entries).len(), 1);
        server.abort();
    }
}
