//! The community's broker for local voice gateways: who may help with voice,
//! for which session, for how long. It issues; the gateway's data plane enforces.
//!
//! * Only a member who holds an offered-and-accepted `VoiceGateway` role AND has
//!   a live voice session registers, for that session's channel, a numeric
//!   private IPv4 address and port, together with the random instance id of the
//!   gateway process (one per worker start). The server stores them and never
//!   connects to the address: no probe, no name lookup.
//! * Everything is bound to that instance. A new instance, a new address or a new
//!   voice session cancels the old registration's leases, so a restarted worker
//!   never receives an old id or key (its sequence counters start again at zero).
//!   Only a renewal of the SAME instance, address and session keeps them.
//! * Only a client with its own live session in the SAME channel can lease that
//!   gateway instance, and only for that session. Each admission gets a fresh
//!   random id and hop key; a renewal while the lease is live keeps both and
//!   restarts the short validity; after expiry or revocation the next lease is
//!   new. The key is independent of the voice ticket and of every E2EE key, and
//!   goes only to the leasing client and to that gateway instance.
//! * The gateway polls its own grants (naming its instance) and gets the
//!   remaining validity. A poll never renews anything.
//! * Every request re-checks account, role, live session, channel and
//!   permission, and then re-checks under the lock that the registration it
//!   validated is still the current one before it issues or returns any key. A
//!   gateway or client that fails is dropped on the spot, and a lease that is
//!   not renewed is gone within `LEASE_TTL_MS` anyway.
//! * Memory only, bounded in registrations, leases, tracked members and request
//!   rate; no schema, no file, no network call.
//!
//! The Friends service uses the same routes and data plane for one narrower thing: the
//! two friends of a LIVE private call. There is no role: a participant who has joined
//! the call registers ITS OWN seat of that exact call, and the other participant, who has
//! also joined it, may lease it. Everything above is additionally bound to the call ID,
//! so a later call on the same room starts with nothing, and every operation re-checks
//! the call itself (accepted, a participant, still friends and unblocked, its room) as
//! well as the seat. Community requests never see a call seat and Friends requests never
//! see a community seat.
use super::*;
use relay_protocol::gateway_broker as wire;
use relay_protocol::hosting::HostRole;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddrV4;
use std::time::{Duration as StdDuration, Instant};
use zeroize::Zeroizing;

const LEASE: StdDuration = StdDuration::from_millis(wire::LEASE_TTL_MS as u64);
const REGISTRATION: StdDuration = StdDuration::from_secs(wire::REGISTRATION_TTL_SECS);
const MAX_REGISTRATIONS: usize = 16;
const MAX_PER_CHANNEL: usize = 4;
const MAX_LEASES: usize = 512;
/// Requests per member in two seconds across all broker routes.
const BURST: usize = 40;
/// Members whose request windows are tracked at once. A new member beyond this
/// is refused until old windows lapse; nothing grows without bound.
const MAX_WINDOWS: usize = 1024;

/// The capability a Friends service announces for the call-scoped broker. The community's
/// own `gateway-broker-v1` stays stripped there.
pub(crate) const FRIENDS_CAPABILITY: &str = "friends-gateway-broker-v1";

#[derive(Clone)]
struct Registration {
    instance: [u8; 16],
    session_id: u64,
    channel_id: String,
    /// The private call this seat belongs to (Friends only); `None` for a community seat.
    call_id: Option<String>,
    token_hash: String,
    address: SocketAddrV4,
    expires: Instant,
}

/// What a request saw and validated; the registration must still be exactly this
/// when keys are issued or returned.
#[derive(Clone, PartialEq, Eq)]
struct Seen {
    instance: [u8; 16],
    session_id: u64,
    call_id: Option<String>,
    address: SocketAddrV4,
}

impl Registration {
    fn seen(&self) -> Seen {
        Seen {
            instance: self.instance,
            session_id: self.session_id,
            call_id: self.call_id.clone(),
            address: self.address,
        }
    }
}

struct Lease {
    key: Zeroizing<[u8; 32]>,
    session_id: u64,
    client_user: String,
    client_token: String,
    channel_id: String,
    call_id: Option<String>,
    gateway: String,
    gateway_instance: [u8; 16],
    expires: Instant,
}

#[derive(Default)]
struct Book {
    gateways: HashMap<String, Registration>,
    leases: HashMap<[u8; 16], Lease>,
    by_session: HashMap<u64, [u8; 16]>,
    windows: HashMap<String, VecDeque<Instant>>,
    meshes: mesh::Meshes,
}

/// Memory-only broker state. One per server process.
#[derive(Default)]
pub(crate) struct Broker {
    book: Mutex<Book>,
    /// Sessions the voice mesh carries now; see `mesh`.
    coverage: Mutex<mesh::Coverage>,
}

/// A lease to re-check against the live session it was issued for.
struct Probe {
    id: [u8; 16],
    session: u64,
    user: String,
    token: String,
    channel: String,
    call: Option<String>,
}

/// The leasing client, as validated.
struct Client {
    session_id: u64,
    user: String,
    token: String,
    channel: String,
    call: Option<String>,
}

struct Issued {
    id: [u8; 16],
    key: String,
    ttl_ms: u32,
}

fn fresh<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    loop {
        OsRng.fill_bytes(&mut bytes);
        // The data plane refuses an all-zero id or key.
        if bytes.iter().any(|b| *b != 0) {
            return bytes;
        }
    }
}

impl Book {
    fn drop_lease(&mut self, id: &[u8; 16]) {
        if let Some(lease) = self.leases.remove(id) {
            if self.by_session.get(&lease.session_id) == Some(id) {
                self.by_session.remove(&lease.session_id);
            }
        }
    }

    fn drop_gateway(&mut self, user: &str) {
        self.gateways.remove(user);
        let ids: Vec<[u8; 16]> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.gateway == user)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.drop_lease(&id);
        }
    }

    fn sweep(&mut self) {
        let now = Instant::now();
        let dead: Vec<[u8; 16]> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.expires <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.drop_lease(&id);
        }
        let gone: Vec<String> = self
            .gateways
            .iter()
            .filter(|(_, registration)| registration.expires <= now)
            .map(|(user, _)| user.clone())
            .collect();
        for user in gone {
            self.drop_gateway(&user);
        }
    }

    /// Whether the registration is still exactly the one a request validated.
    fn registration_is(&self, user: &str, seen: &Seen) -> bool {
        self.gateways
            .get(user)
            .is_some_and(|r| r.seen() == *seen && r.expires > Instant::now())
    }

    /// Register or renew. The same instance, address and session only renews;
    /// anything else first cancels the old registration and every lease on it.
    fn register(&mut self, user: &str, registration: Registration) -> ApiResult<()> {
        let same = self
            .gateways
            .get(user)
            .is_some_and(|old| old.seen() == registration.seen());
        if !same {
            self.drop_gateway(user);
        }
        let others = self.gateways.iter().filter(|(other, _)| *other != user);
        if others.count() >= MAX_REGISTRATIONS
            || self
                .gateways
                .iter()
                .filter(|(other, r)| *other != user && r.channel_id == registration.channel_id)
                .count()
                >= MAX_PER_CHANNEL
        {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_gateways",
            ));
        }
        self.gateways.insert(user.to_string(), registration);
        Ok(())
    }

    /// Issue or renew a lease, but only if the gateway registration is still
    /// exactly the one that was validated. A live renewal of the same instance
    /// keeps id and key; every other admission is new.
    fn issue_lease(
        &mut self,
        now: Instant,
        gateway: &str,
        seen: &Seen,
        client: &Client,
    ) -> ApiResult<Issued> {
        if !self.registration_is(gateway, seen) {
            return Err(missing());
        }
        let live = self
            .by_session
            .get(&client.session_id)
            .copied()
            .filter(|id| {
                self.leases.get(id).is_some_and(|lease| {
                    lease.expires > now
                        && lease.gateway == gateway
                        && lease.gateway_instance == seen.instance
                        && lease.channel_id == client.channel
                        && lease.call_id == client.call
                        && lease.client_user == client.user
                        && lease.client_token == client.token
                })
            });
        let id = match live {
            Some(id) => {
                if let Some(lease) = self.leases.get_mut(&id) {
                    lease.expires = now + LEASE;
                }
                id
            }
            None => {
                if let Some(old) = self.by_session.get(&client.session_id).copied() {
                    self.drop_lease(&old);
                }
                let on_gateway = self
                    .leases
                    .values()
                    .filter(|lease| lease.gateway == gateway)
                    .count();
                if on_gateway >= wire::MAX_GRANTS_PER_GATEWAY || self.leases.len() >= MAX_LEASES {
                    return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "gateway_full"));
                }
                let id: [u8; 16] = fresh();
                self.leases.insert(
                    id,
                    Lease {
                        key: Zeroizing::new(fresh()),
                        session_id: client.session_id,
                        client_user: client.user.clone(),
                        client_token: client.token.clone(),
                        channel_id: client.channel.clone(),
                        call_id: client.call.clone(),
                        gateway: gateway.to_string(),
                        gateway_instance: seen.instance,
                        expires: now + LEASE,
                    },
                );
                self.by_session.insert(client.session_id, id);
                id
            }
        };
        let lease = self.leases.get(&id).ok_or_else(missing)?;
        Ok(Issued {
            id,
            key: URL_SAFE_NO_PAD.encode(lease.key.as_slice()),
            ttl_ms: lease.expires.saturating_duration_since(now).as_millis() as u32,
        })
    }

    /// The grants of ONE gateway instance, after the revalidation. `None` if the
    /// registration is no longer that instance: a stale poll gets nothing, and no
    /// other instance's grants.
    fn collect_grants(
        &mut self,
        now: Instant,
        gateway: &str,
        instance: &[u8; 16],
        candidates: &[Probe],
        valid: &[[u8; 16]],
    ) -> Option<Vec<wire::GatewayGrant>> {
        if self.gateways.get(gateway).map(|r| r.instance) != Some(*instance) {
            return None;
        }
        for probe in candidates {
            if !valid.contains(&probe.id) {
                self.drop_lease(&probe.id);
            }
        }
        let mut out = Vec::new();
        for id in valid {
            let Some(lease) = self.leases.get(id) else {
                continue;
            };
            let remaining = lease.expires.saturating_duration_since(now).as_millis() as u32;
            if lease.gateway != gateway
                || lease.gateway_instance != *instance
                || remaining == 0
                || out.len() >= wire::MAX_GRANTS_PER_GATEWAY
            {
                continue;
            }
            out.push(wire::GatewayGrant {
                id: wire::encode_id(id),
                session: lease.session_id,
                key: URL_SAFE_NO_PAD.encode(lease.key.as_slice()),
                ttl_ms: remaining.min(wire::LEASE_TTL_MS),
            });
        }
        Some(out)
    }
}

impl Broker {
    /// Drop a member's registration and every lease on it, and any lease they hold
    /// as a client (role withdrawal, step-down, leaving).
    pub(crate) fn cancel_user(&self, user: &str) {
        {
            let mut book = lock(&self.book);
            book.drop_gateway(user);
            let ids: Vec<[u8; 16]> = book
                .leases
                .iter()
                .filter(|(_, lease)| lease.client_user == user)
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                book.drop_lease(&id);
            }
        }
        // The mesh stops carrying them at once; the host delivers again.
        self.mesh_forget(user);
    }

    fn rate(&self, user: &str) -> ApiResult<()> {
        let mut book = lock(&self.book);
        let cutoff = Instant::now()
            .checked_sub(StdDuration::from_secs(2))
            .unwrap_or_else(Instant::now);
        // Admission to the table is bounded: a member not yet tracked is refused
        // when the table is full of recent windows, instead of growing it.
        if !book.windows.contains_key(user) && book.windows.len() >= MAX_WINDOWS {
            book.windows
                .retain(|_, w| w.back().is_some_and(|at| *at >= cutoff));
            if book.windows.len() >= MAX_WINDOWS {
                return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
            }
        }
        let window = book.windows.entry(user.to_string()).or_default();
        while window.front().is_some_and(|at| *at < cutoff) {
            window.pop_front();
        }
        if window.len() >= BURST {
            return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
        }
        window.push_back(Instant::now());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn lease_count(&self) -> usize {
        lock(&self.book).leases.len()
    }

    #[cfg(test)]
    pub(crate) fn gateway_count(&self) -> usize {
        lock(&self.book).gateways.len()
    }

    #[cfg(test)]
    pub(crate) fn window_count(&self) -> usize {
        lock(&self.book).windows.len()
    }

    #[cfg(test)]
    pub(crate) fn expire_leases(&self) {
        for lease in lock(&self.book).leases.values_mut() {
            lease.expires = Instant::now();
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_registrations(&self) {
        for registration in lock(&self.book).gateways.values_mut() {
            registration.expires = Instant::now();
        }
    }
}

// ---------------------------------------------------------------------------
// Facts about live sessions and accounts

/// The caller's own live voice seat: its session, its channel and, on the Friends service,
/// the private call it joined.
struct Seat {
    session_id: u64,
    channel: String,
    call: Option<String>,
}

/// The caller's own live seat for this kind of service. On a community that is a community
/// voice session (never a private-call seat); on the Friends service it is the seat of a
/// private call the caller has joined (never a session without a call).
fn own_seat(state: &AppState, auth: &Auth) -> Option<Seat> {
    let at = now();
    let people = state.0.config.people;
    lock(&state.0.voice)
        .iter()
        .find(|(_, session)| {
            session.user.id == auth.user.id
                && session.token_hash == auth.token_hash
                && session.private_call_id.is_some() == people
                && session.alive_at(at)
        })
        .map(|(id, session)| Seat {
            session_id: *id,
            channel: session.channel_id.clone(),
            call: session.private_call_id.clone(),
        })
}

/// Whether this exact session is still live, in this channel, for this member and
/// sign-in, and the member may still join the channel. Takes the database
/// first, then briefly the voice map (the order used elsewhere).
fn seat_valid(
    state: &AppState,
    db: &Connection,
    session_id: u64,
    user: &str,
    channel: &str,
    token_hash: &str,
    call: Option<&str>,
) -> bool {
    // A call seat belongs to the Friends service and a call-less one to a community.
    if state.0.config.people != call.is_some() {
        return false;
    }
    let at = now();
    let live = lock(&state.0.voice)
        .get(&session_id)
        .is_some_and(|session| {
            session.user.id == user
                && session.channel_id == channel
                && session.token_hash == token_hash
                && session.private_call_id.as_deref() == call
                && session.alive_at(at)
        });
    // For a call: the authoritative admission of that exact call, not the channel alone.
    // It must be accepted, name this user, and its friendship, block state and room
    // must still hold.
    live && community::active_user(db, user).is_ok()
        && call.is_none_or(|id| private_calls::validate_binding(db, id, user, channel).is_ok())
        && channel_access(db, channel, user, JOIN).is_ok()
        && db
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND expires>?2",
                params![token_hash, at],
                |r| r.get::<_, u32>(0),
            )
            .unwrap_or(0)
            > 0
}

/// A community gateway needs its accepted role. A Friends gateway needs none: it is a
/// volunteer for one live call, and its seat and that call are what is checked.
fn gateway_valid(state: &AppState, db: &Connection, user: &str, reg: &Registration) -> bool {
    reg.expires > Instant::now()
        && (reg.call_id.is_some() || hosting::holds_role(db, user, HostRole::VoiceGateway))
        && seat_valid(
            state,
            db,
            reg.session_id,
            user,
            &reg.channel_id,
            &reg.token_hash,
            reg.call_id.as_deref(),
        )
}

/// The only broker requests a Friends service admits, as exact path and method pairs.
/// Nothing else under /api/hosting becomes reachable there.
pub(crate) fn friends_route(path: &str, method: &axum::http::Method) -> bool {
    use axum::http::Method;
    const BASE: &str = "/api/hosting/voice-gateways";
    match path {
        BASE => *method == Method::GET,
        "/api/hosting/voice-gateways/registration" => {
            *method == Method::POST || *method == Method::DELETE
        }
        "/api/hosting/voice-gateways/grants" => *method == Method::GET,
        "/api/hosting/voice-gateways/leases" => {
            *method == Method::POST || *method == Method::DELETE
        }
        _ => false,
    }
}

fn is_gateway_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A request of a finished call: its session is not the caller's live one now.
fn stale_session() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "stale_session")
}

fn instance_of(text: &str) -> ApiResult<[u8; 16]> {
    if !wire::valid_instance(text) {
        return Err(bad("Use the gateway's 32-digit instance id"));
    }
    wire::decode_id(text).ok_or_else(|| bad("Use the gateway's 32-digit instance id"))
}

// ---------------------------------------------------------------------------
// The gateway's side

/// `POST /api/hosting/voice-gateways/registration`
pub(crate) async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<wire::Register>,
) -> ApiResult<Json<wire::Registered>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    let instance = instance_of(&input.instance)?;
    let address = wire::parse_private_addr(&input.ip, input.port).map_err(bad)?;
    let seat = own_seat(&state, &auth).ok_or_else(forbidden)?;
    let (session_id, channel_id, call_id) = (seat.session_id, seat.channel, seat.call);
    // Before anything changes: only the caller's current seat may register.
    if input.session_id != session_id {
        return Err(stale_session());
    }
    {
        let db = lock(&state.0.db);
        // A community seat needs the accepted role; a call seat needs the live call instead,
        // and registering grants no role either way.
        if (call_id.is_none() && !hosting::holds_role(&db, &auth.user.id, HostRole::VoiceGateway))
            || !seat_valid(
                &state,
                &db,
                session_id,
                &auth.user.id,
                &channel_id,
                &auth.token_hash,
                call_id.as_deref(),
            )
        {
            return Err(forbidden());
        }
    }
    let mut book = lock(&state.0.gateways.book);
    // A call may change after the account/role validation above. Hold the live
    // seat through the bounded book update so an old in-flight registration
    // cannot replace the next call's gateway after it has already registered.
    let seats = lock(&state.0.voice);
    if !seats.get(&session_id).is_some_and(|seat| {
        seat.user.id == auth.user.id
            && seat.token_hash == auth.token_hash
            && seat.channel_id == channel_id
            && seat.private_call_id == call_id
            && seat.alive_at(now())
    }) {
        return Err(stale_session());
    }
    book.sweep();
    book.register(
        &auth.user.id,
        Registration {
            instance,
            session_id,
            channel_id: channel_id.clone(),
            call_id: call_id.clone(),
            token_hash: auth.token_hash.clone(),
            address,
            expires: Instant::now() + REGISTRATION,
        },
    )?;
    Ok(Json(wire::Registered {
        channel_id,
        instance: input.instance,
        session_id,
        expires_in_secs: wire::REGISTRATION_TTL_SECS,
        max_grants: wire::MAX_GRANTS_PER_GATEWAY as u32,
    }))
}

/// `DELETE /api/hosting/voice-gateways/registration?instance=…&session_id=…`:
/// drops the caller's registration only if it is exactly that instance and
/// session. A late cleanup of an earlier call or worker therefore cannot remove
/// the registration of the current one; it gets `dropped:false` and nothing changes.
pub(crate) async fn deregister(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<wire::DropRegistration>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    let instance = instance_of(&query.instance)?;
    let mut book = lock(&state.0.gateways.book);
    let matches = book
        .gateways
        .get(&auth.user.id)
        .is_some_and(|r| r.instance == instance && r.session_id == query.session_id);
    if matches {
        book.drop_gateway(&auth.user.id);
    }
    drop(book);
    if matches {
        state.0.gateways.mesh_forget(&auth.user.id);
    }
    Ok(Json(serde_json::json!({"ok":true,"dropped":matches})))
}

/// `GET /api/hosting/voice-gateways/grants?instance=<32 hex>`: the grants of
/// that gateway instance, each with its remaining validity. Polling never renews
/// a lease. A request for an instance that is not the registered one gets `409`
/// and nothing else.
pub(crate) async fn grants(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<wire::GrantsQuery>,
) -> ApiResult<Json<wire::GatewayGrants>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    let instance = instance_of(&query.instance)?;
    let registration = {
        let mut book = lock(&state.0.gateways.book);
        book.sweep();
        book.gateways.get(&auth.user.id).cloned()
    };
    let Some(registration) = registration else {
        return Err(forbidden());
    };
    if registration.instance != instance {
        return Err(ApiError::new(StatusCode::CONFLICT, "stale_instance"));
    }
    let candidates: Vec<Probe> = {
        let book = lock(&state.0.gateways.book);
        book.leases
            .iter()
            .filter(|(_, lease)| {
                lease.gateway == auth.user.id && lease.gateway_instance == instance
            })
            .map(|(id, lease)| Probe {
                id: *id,
                session: lease.session_id,
                user: lease.client_user.clone(),
                token: lease.client_token.clone(),
                channel: lease.channel_id.clone(),
                call: lease.call_id.clone(),
            })
            .collect()
    };
    let (gateway_ok, valid): (bool, Vec<[u8; 16]>) = {
        let db = lock(&state.0.db);
        let gateway_ok = gateway_valid(&state, &db, &auth.user.id, &registration);
        let valid = if gateway_ok {
            candidates
                .iter()
                .filter(|probe| {
                    probe.channel == registration.channel_id
                        && probe.call == registration.call_id
                        && seat_valid(
                            &state,
                            &db,
                            probe.session,
                            &probe.user,
                            &probe.channel,
                            &probe.token,
                            probe.call.as_deref(),
                        )
                })
                .map(|probe| probe.id)
                .collect()
        } else {
            Vec::new()
        };
        (gateway_ok, valid)
    };
    let mut book = lock(&state.0.gateways.book);
    if !gateway_ok {
        if book.registration_is(&auth.user.id, &registration.seen()) {
            book.drop_gateway(&auth.user.id);
        }
        return Err(forbidden());
    }
    // Recheck the instance under the lock: a gateway that restarted meanwhile
    // must not have its predecessor's grants published to this request.
    let out = book
        .collect_grants(
            Instant::now(),
            &auth.user.id,
            &instance,
            &candidates,
            &valid,
        )
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "stale_instance"))?;
    Ok(Json(wire::GatewayGrants { grants: out }))
}

// ---------------------------------------------------------------------------
// The client's side

/// `GET /api/hosting/voice-gateways`: gateways registered for the channel of the
/// caller's own live session. Members elsewhere see nothing.
pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<wire::GatewayList>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    let seat = own_seat(&state, &auth).ok_or_else(forbidden)?;
    let (session_id, channel_id, call_id) = (seat.session_id, seat.channel, seat.call);
    let candidates: Vec<(String, Registration)> = {
        let mut book = lock(&state.0.gateways.book);
        book.sweep();
        book.gateways
            .iter()
            .filter(|(user, r)| {
                r.channel_id == channel_id && r.call_id == call_id && **user != auth.user.id
            })
            .map(|(user, r)| (user.clone(), r.clone()))
            .collect()
    };
    let (caller_ok, live): (bool, Vec<bool>) = {
        let db = lock(&state.0.db);
        (
            seat_valid(
                &state,
                &db,
                session_id,
                &auth.user.id,
                &channel_id,
                &auth.token_hash,
                call_id.as_deref(),
            ),
            candidates
                .iter()
                .map(|(user, r)| gateway_valid(&state, &db, user, r))
                .collect(),
        )
    };
    if !caller_ok {
        return Err(forbidden());
    }
    let mut book = lock(&state.0.gateways.book);
    let mut gateways = Vec::new();
    for ((user, registration), ok) in candidates.iter().zip(live) {
        if !ok {
            if book.registration_is(user, &registration.seen()) {
                book.drop_gateway(user);
            }
            continue;
        }
        // Only what is still current is published: a worker that restarted since
        // the snapshot is not listed under its predecessor's instance.
        if book.registration_is(user, &registration.seen()) {
            gateways.push(wire::GatewayOffer {
                gateway_id: user.clone(),
                instance: wire::encode_id(&registration.instance),
                ip: registration.address.ip().to_string(),
                port: registration.address.port(),
            });
        }
    }
    gateways.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    Ok(Json(wire::GatewayList { gateways }))
}

/// `POST /api/hosting/voice-gateways/leases`: lease the caller's OWN live session
/// from one gateway instance of the same channel. A live lease of the same
/// instance is renewed with the same id and key; otherwise a new id and key are
/// issued.
pub(crate) async fn lease(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<wire::LeaseRequest>,
) -> ApiResult<Json<wire::LeaseGrant>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    if !is_gateway_id(&input.gateway_id) || input.gateway_id == auth.user.id {
        return Err(forbidden());
    }
    let instance = instance_of(&input.instance)?;
    let seat = own_seat(&state, &auth).ok_or_else(forbidden)?;
    let (session_id, channel_id, call_id) = (seat.session_id, seat.channel, seat.call);
    // Before anything changes: only the caller's current seat may lease.
    if input.session_id != session_id {
        return Err(stale_session());
    }
    let registration = {
        let mut book = lock(&state.0.gateways.book);
        book.sweep();
        book.gateways.get(&input.gateway_id).cloned()
    };
    // The instance the client names must be the registered one.
    let registration = registration
        .filter(|r| r.channel_id == channel_id && r.call_id == call_id && r.instance == instance)
        .ok_or_else(missing)?;
    let (caller_ok, gateway_ok) = {
        let db = lock(&state.0.db);
        (
            seat_valid(
                &state,
                &db,
                session_id,
                &auth.user.id,
                &channel_id,
                &auth.token_hash,
                call_id.as_deref(),
            ),
            gateway_valid(&state, &db, &input.gateway_id, &registration),
        )
    };
    if !caller_ok {
        return Err(forbidden());
    }
    let mut book = lock(&state.0.gateways.book);
    if !gateway_ok {
        if book.registration_is(&input.gateway_id, &registration.seen()) {
            book.drop_gateway(&input.gateway_id);
        }
        return Err(missing());
    }
    let client = Client {
        session_id,
        user: auth.user.id.clone(),
        token: auth.token_hash.clone(),
        channel: channel_id,
        call: call_id,
    };
    // Issued only if the registration is still exactly the one validated above.
    let issued = book.issue_lease(
        Instant::now(),
        &input.gateway_id,
        &registration.seen(),
        &client,
    )?;
    Ok(Json(wire::LeaseGrant {
        lease_id: wire::encode_id(&issued.id),
        gateway_id: input.gateway_id,
        instance: input.instance,
        session_id,
        key: issued.key,
        ttl_ms: issued.ttl_ms,
        ip: registration.address.ip().to_string(),
        port: registration.address.port(),
    }))
}

/// `DELETE /api/hosting/voice-gateways/leases?session_id=…&lease_id=…`: the
/// client lets go of ITS OWN lease, and only if both the session and the lease id
/// match. A late cleanup of an earlier call cannot drop the current call's lease.
pub(crate) async fn release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<wire::DropLease>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    state.0.gateways.rate(&auth.user.id)?;
    let id = wire::decode_id(&query.lease_id).ok_or_else(|| bad("Use the 32-digit lease id"))?;
    let mut book = lock(&state.0.gateways.book);
    let matches = book.leases.get(&id).is_some_and(|lease| {
        lease.client_user == auth.user.id && lease.session_id == query.session_id
    });
    if matches {
        book.drop_lease(&id);
    }
    Ok(Json(serde_json::json!({"ok":true,"dropped":matches})))
}

/// The voice mesh between homes, built on these registrations and leases.
mod mesh;
pub(crate) use mesh::{member as mesh_member, report as mesh_report};

#[cfg(test)]
mod tests;
