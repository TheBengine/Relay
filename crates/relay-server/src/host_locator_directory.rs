//! A small, independent directory of signed, opaque host locators.
//!
//! The Friends service stores what a community host publishes so that a member
//! who knows the capability ID can find the community's current host after it
//! moved. It stores only the encrypted destination and public host signatures:
//! never an invitation verifier or code, password, admission token or decrypted
//! address. A reader still decrypts the record, verifies its own pinned anchor
//! and demands a fresh proof from the destination before sending any credential,
//! so this directory is a convenience and never a trust root.
//!
//! Two exact POST routes, no listing, no search, no query-string capability and
//! no outbound request. A host proves itself by signature, so neither route takes
//! a bearer. Both are rate limited per ingress before the body is read.
use super::*;
use axum::{
    body::Bytes,
    extract::{ConnectInfo, FromRequestParts},
    http::{header, request::Parts},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use relay_protocol::{
    handover::{self as model, ActivationGrant, HostIdentity, RoutePin},
    host_locator::{self as locator, Lookup, Record},
};
use sha2::{Digest, Sha256};

/// A lookup body is one 64-character ID in a tiny JSON object.
pub(super) const LOOKUP_BODY_BYTES: usize = 1024;
const MAX_HISTORY: usize = model::MAX_ROUTE_MOVES;
/// Live IDs one anchor may hold, and the whole directory.
const MAX_ANCHOR_IDS: i64 = 256;
const MAX_LIVE_IDS: i64 = 32768;
/// Expired rows removed per publish, so cleanup never stalls the database.
const PRUNE_BATCH: i64 = 256;
const FUTURE_SKEW_MS: u64 = 30_000;
const MAX_RATE_KEYS: usize = 4096;
const LOOKUPS_PER_MINUTE: u32 = 120;
const PUBLISHES_PER_MINUTE: u32 = 30;
const MIN_CIPHERTEXT_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;

/// One answer for every record that is not accepted, whatever the reason.
fn refused() -> ApiError {
    bad("This route record was not accepted")
}

// ---- Admission: exact routes, community mode, size, then rate, before any body ----

/// Extracted before the body, so a flood is turned away without reading it.
pub(super) struct Admit<const PUBLISH: bool>;

/// The caller's address as the rest of the server trusts it: the network helper's
/// peer, else the socket, and a forwarded address only from a configured proxy.
fn remote(parts: &Parts, state: &AppState) -> String {
    if let Some(network) = parts.extensions.get::<NetworkIngress>() {
        return format!("network-ingress:{}", network.peer_ip);
    }
    let direct = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|v| v.0.ip());
    let mut remote = direct
        .map(|v| v.to_string())
        .unwrap_or_else(|| "local".into());
    if direct.is_some_and(|ip| state.0.config.trusted_proxy_ips.contains(&ip)) {
        if let Some(header) = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() < 2048)
        {
            if let Some(client) = header
                .split(',')
                .rev()
                .filter_map(|s| s.trim().parse::<std::net::IpAddr>().ok())
                .find(|ip| !state.0.config.trusted_proxy_ips.contains(ip))
            {
                remote = client.to_string();
            }
        }
    }
    remote
}

impl<const PUBLISH: bool> FromRequestParts<AppState> for Admit<PUBLISH> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if !state.0.config.people {
            return Err(missing());
        }
        let limit = if PUBLISH {
            locator::MAX_RECORD_BYTES
        } else {
            LOOKUP_BODY_BYTES
        };
        if parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|length| length > limit)
        {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "This request is too large",
            ));
        }
        let (label, maximum) = if PUBLISH {
            ("publish", PUBLISHES_PER_MINUTE)
        } else {
            ("lookup", LOOKUPS_PER_MINUTE)
        };
        let key = format!("{label}:{}", remote(parts, state));
        let retry_after_ms = {
            // Bounded and pruned; when full it fails closed rather than evicting
            // a source that would then get a fresh allowance.
            let mut rates = lock(&state.0.locator_rates);
            let timestamp = now_ms();
            rates.retain(|_, (start, _)| timestamp.saturating_sub(*start) < 60_000);
            if rates.len() >= MAX_RATE_KEYS && !rates.contains_key(&key) {
                Some(1000)
            } else {
                let record = rates.entry(key).or_insert((timestamp, 0));
                if record.1 >= maximum {
                    Some(
                        record
                            .0
                            .saturating_add(60_000)
                            .saturating_sub(timestamp)
                            .clamp(1, 60_000),
                    )
                } else {
                    record.1 += 1;
                    None
                }
            }
        };
        match retry_after_ms {
            Some(delay) => Err(ApiError::cooldown(
                "rate_limited",
                "Too many requests. Retry after the cooldown",
                delay,
                None,
            )),
            None => Ok(Self),
        }
    }
}

// ---- Verification of a published record ----

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|v| v.to_string() == value)
}

/// The identity a key must carry: a UUID derived from the key, as hosts compute it.
fn identity(key: &VerifyingKey) -> HostIdentity {
    let mut hash = Sha256::new();
    hash.update(b"relay-host-identity-v1\0");
    hash.update(key.as_bytes());
    let digest = hash.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().expect("sixteen bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    HostIdentity {
        host_id: uuid::Uuid::from_bytes(bytes).to_string(),
        public_key: URL_SAFE_NO_PAD.encode(key.as_bytes()),
    }
}

fn host_key(host: &HostIdentity) -> Option<VerifyingKey> {
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

fn verify(host: &HostIdentity, signature: &str, message: &[u8]) -> bool {
    if signature.len() != 86 {
        return false;
    }
    let Some(key) = host_key(host) else {
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

fn pin_ok(pin: &RoutePin) -> bool {
    pin.term > 0 && uuid(&pin.server_id) && host_key(&pin.host).is_some()
}

/// One handover edge: the source at `pin` authorised the move and the target
/// attested the image, both by signature. Same rules as a client applies.
fn grant_ok(pin: &RoutePin, grant: &ActivationGrant) -> bool {
    let receipt = &grant.receipt;
    let body = &receipt.body;
    pin.term > 0
        && grant.source_identity == pin.host
        && uuid(&body.source_host_id)
        && body.server_id == pin.server_id
        && body.term == pin.term
        && body.term.checked_add(1) == Some(grant.next_term)
        && body.target_identity != pin.host
        && host_key(&body.target_identity).is_some()
        && uuid(&body.preparation_id)
        && uuid(&body.export_id)
        && body.database_bytes > 0
        && body.database_bytes <= 4 * 1024 * 1024 * 1024
        && hex64(&body.database_sha256)
        && hex64(&body.manifest_sha256)
        && grant.history_sha256.as_deref().is_none_or(hex64)
        && URL_SAFE_NO_PAD
            .decode(&body.challenge)
            .is_ok_and(|v| v.len() == 32)
        && !body.target_url.is_empty()
        && body.target_url.len() <= 2048
        && !body.target_url.chars().any(char::is_control)
        && verify(
            &body.target_identity,
            &receipt.signature,
            &model::receipt_message(body),
        )
        && verify(&pin.host, &grant.signature, &model::grant_message(grant))
}

/// A grant that names its ancestry must name exactly the edges before it. A
/// legacy (v1) grant carries no hash and is accepted as it always was.
fn committed(edge: &ActivationGrant, earlier: &[ActivationGrant]) -> bool {
    edge.history_sha256.as_ref().is_none_or(|hash| {
        *hash == format!("{:x}", Sha256::digest(model::history_message(earlier)))
    })
}

/// Every edge is source-signed and target-attested, each commits to the edges
/// before it, the separately pinned anchor lies on the chain, and the chain ends
/// at exactly `current`. Public evidence, never a shortcut by server ID alone.
fn chain_ok(anchor: &RoutePin, current: &RoutePin, history: &[ActivationGrant]) -> bool {
    if history.len() > MAX_HISTORY || anchor.server_id != current.server_id {
        return false;
    }
    let Some(first) = history.first() else {
        return anchor == current;
    };
    let mut at = RoutePin {
        server_id: anchor.server_id.clone(),
        host: first.source_identity.clone(),
        term: first.receipt.body.term,
    };
    let mut rooted = at == *anchor;
    for (index, edge) in history.iter().enumerate() {
        if !grant_ok(&at, edge) || !committed(edge, &history[..index]) {
            return false;
        }
        at = RoutePin {
            server_id: anchor.server_id.clone(),
            host: edge.receipt.body.target_identity.clone(),
            term: edge.next_term,
        };
        rooted |= at == *anchor;
    }
    rooted && at == *current
}

/// Canonical base64url of `min..=max` bytes; anything re-encoded differently is out.
fn decoded(value: &str, min: usize, max: usize) -> Option<Vec<u8>> {
    if value.len() > max / 3 * 4 + 4 {
        return None;
    }
    let raw = URL_SAFE_NO_PAD.decode(value).ok()?;
    ((min..=max).contains(&raw.len()) && URL_SAFE_NO_PAD.encode(&raw) == value).then_some(raw)
}

fn record_ok(record: &Record, at: u64) -> bool {
    hex64(&record.id)
        && pin_ok(&record.anchor)
        && pin_ok(&record.current)
        && record.current.term <= i64::MAX as u64
        && record.issued_ms > 0
        && record.issued_ms <= at.saturating_add(FUTURE_SKEW_MS)
        && record.expires_ms > at
        && record.expires_ms >= record.issued_ms
        && record.expires_ms - record.issued_ms <= locator::MAX_LIFETIME_MS
        && decoded(&record.nonce, NONCE_BYTES, NONCE_BYTES).is_some()
        && decoded(
            &record.ciphertext,
            MIN_CIPHERTEXT_BYTES,
            locator::MAX_CIPHERTEXT_BYTES,
        )
        .is_some()
        && chain_ok(&record.anchor, &record.current, &record.history)
        && verify(
            &record.current.host,
            &record.signature,
            &locator::record_message(record),
        )
}

// ---- Replacement ----

enum Change {
    /// The very same record again: nothing to write.
    Same,
    Replace,
}

fn text(record: &Record) -> String {
    serde_json::to_string(record).expect("a record always serialises")
}

/// May `new` take the place of `old` for the same ID? Only for the same anchor,
/// and then only as the identical record, a strictly later revision by the same
/// key in the same term over the same ancestry, or a higher term whose complete
/// history extends the old one (so the old current is verifiably its ancestor).
/// Rollback, a conflicting revision, or shortened or rewritten ancestry: never.
fn replace(old: &Record, new: &Record) -> Option<Change> {
    if old.anchor != new.anchor {
        return None;
    }
    if text(old) == text(new) {
        return Some(Change::Same);
    }
    if new.current == old.current {
        return (new.history == old.history && new.issued_ms > old.issued_ms)
            .then_some(Change::Replace);
    }
    let extends = new.history.len() > old.history.len()
        && new.history[..old.history.len()] == old.history[..];
    (new.current.term > old.current.term && extends).then_some(Change::Replace)
}

fn store(db: &mut Connection, record: &Record, at: u64) -> ApiResult<()> {
    let tx = db.transaction()?;
    // Bounded cleanup; an ID that is expired but not yet removed still follows the rules.
    tx.execute(
        "DELETE FROM host_locator_records WHERE id IN (SELECT id FROM host_locator_records WHERE expires_ms<=?1 ORDER BY expires_ms LIMIT ?2)",
        params![at, PRUNE_BATCH],
    )?;
    let anchor = serde_json::to_string(&record.anchor).expect("a pin always serialises");
    let existing: Option<String> = tx
        .query_row(
            "SELECT record FROM host_locator_records WHERE id=?1",
            [&record.id],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(old) => {
            let old: Record = serde_json::from_str(&old).map_err(|_| refused())?;
            match replace(&old, record).ok_or_else(refused)? {
                Change::Same => {}
                Change::Replace => {
                    tx.execute(
                        "UPDATE host_locator_records SET current_key=?2,current_term=?3,issued_ms=?4,expires_ms=?5,record=?6 WHERE id=?1",
                        params![
                            record.id,
                            record.current.host.public_key,
                            record.current.term,
                            record.issued_ms,
                            record.expires_ms,
                            text(record)
                        ],
                    )?;
                }
            }
        }
        None => {
            let live: i64 = tx.query_row(
                "SELECT COUNT(*) FROM host_locator_records WHERE expires_ms>?1",
                [at],
                |r| r.get(0),
            )?;
            let theirs: i64 = tx.query_row(
                "SELECT COUNT(*) FROM host_locator_records WHERE anchor=?1 AND expires_ms>?2",
                params![anchor, at],
                |r| r.get(0),
            )?;
            if live >= MAX_LIVE_IDS || theirs >= MAX_ANCHOR_IDS {
                return Err(refused());
            }
            tx.execute(
                "INSERT INTO host_locator_records(id,anchor,current_key,current_term,issued_ms,expires_ms,record) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    record.id,
                    anchor,
                    record.current.host.public_key,
                    record.current.term,
                    record.issued_ms,
                    record.expires_ms,
                    text(record)
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

// ---- Routes ----

/// A host publishes (or refreshes) its encrypted, signed locator.
pub(super) async fn publish(
    State(state): State<AppState>,
    _admit: Admit<true>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let record: Record = serde_json::from_slice(&body).map_err(|_| refused())?;
    drop(body);
    let at = now_ms();
    if !record_ok(&record, at) {
        return Err(refused());
    }
    store(&mut lock(&state.0.db), &record, at)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Whoever holds the capability ID gets the opaque record, or the same "not
/// found" as for an unknown, malformed or expired ID.
pub(super) async fn lookup(
    State(state): State<AppState>,
    _admit: Admit<false>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let id = serde_json::from_slice::<Lookup>(&body)
        .ok()
        .map(|l| l.id)
        .filter(|id| hex64(id))
        .ok_or_else(missing)?;
    let stored: Option<String> = lock(&state.0.db)
        .query_row(
            "SELECT record FROM host_locator_records WHERE id=?1 AND expires_ms>?2",
            params![id, now_ms()],
            |r| r.get(0),
        )
        .optional()?;
    let record = stored.ok_or_else(missing)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        record,
    ))
}

/// The two exact paths, for the admission layers. Only POST, only on Friends.
pub(super) fn is_route(path: &str, method: &axum::http::Method) -> bool {
    method == axum::http::Method::POST
        && (path == locator::LOOKUP_PATH || path == locator::PUBLISH_PATH)
}
