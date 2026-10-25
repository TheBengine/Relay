//! Local operator renewal of the encrypted route hints for every existing invitation
//! and, on its own route, for every remembered session that is still valid.
//!
//! When a host moves, every invitation that is still usable needs a fresh signed
//! record for the new host, under the anchor each holder of that invitation pinned
//! (the host as it was when they joined, or any later one on the same lineage). This
//! endpoint hands the owner's own app those records, four at a time, in a stable
//! order. It is not a community admin or member API: it answers only on this PC,
//! with the private local-control secret, and only while this host is the writable
//! authority it is asked about.
//!
//! The session route lists the sessions an ordinary request would still authenticate (an
//! unexpired row of an active member), by their stored hashes only: no raw token, user
//! ID or name is read into a record. It grants no account permission: the member still
//! needs a fresh proof from the destination before the client sends its token.
//!
//! The page cursor holds an invite (or session) verifier. It is returned only to this local caller,
//! to be passed back here; it never goes to the independent directory or into an
//! invitation. Signed records carry neither the verifier nor any invitation code.
use super::*;
use axum::{
    body::Bytes,
    extract::{ConnectInfo, FromRequestParts},
    http::{header, request::Parts},
};
use relay_protocol::{
    handover::{ActivationGrant, RoutePin, MAX_ROUTE_MOVES},
    host_locator::{self as model, Cursor, Issue, Page, PageRequest, Purpose},
};

/// The request is a pin, a destination and a cursor; nothing needs more.
pub(super) const BODY_BYTES: usize = 4096;
/// The invites read per query: every invite yields at least one pair.
const SCAN_ROUNDS: usize = 2 * model::PAGE_RECORDS;

fn stale() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "This host's authority changed. Ask again.",
    )
}

fn hash_bytes(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (at, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |v: u8| match v {
            b'0'..=b'9' => Some(v - b'0'),
            b'a'..=b'f' => Some(v - b'a' + 10),
            _ => None,
        };
        out[at] = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(out)
}

/// The two exact POST routes, for the one global middleware exemption.
pub(super) fn is_route(path: &str, method: &axum::http::Method) -> bool {
    method == axum::http::Method::POST
        && (path == model::LOCAL_PAGE_PATH || path == model::LOCAL_SESSION_PAGE_PATH)
}

// ---- Admission: this PC, this process's secret, before the body is read ----

/// Extracted before the body. Refuses a Friends service, any network-helper or
/// remote socket, any forwarded or browser-originated request, and anything
/// without the exact local-control secret as the Bearer. A community session, even
/// an administrator's, is not that secret.
pub(super) struct LocalControl;

impl FromRequestParts<AppState> for LocalControl {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if state.0.config.people {
            return Err(forbidden());
        }
        let loopback = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| peer.0.ip().is_loopback());
        if parts.extensions.get::<NetworkIngress>().is_some()
            || !loopback
            || parts.headers.keys().any(|k| {
                let name = k.as_str();
                name == "origin"
                    || name == "forwarded"
                    || name == "x-real-ip"
                    || name.starts_with("x-forwarded-")
            })
        {
            return Err(forbidden());
        }
        let expected = state
            .0
            .config
            .local_control_token
            .as_deref()
            .ok_or_else(forbidden)?;
        let supplied = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(forbidden)?;
        if !bool::from(
            Sha256::digest(expected.as_bytes()).ct_eq(&Sha256::digest(supplied.as_bytes())),
        ) {
            return Err(forbidden());
        }
        if parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
            .is_some_and(|length| length > BODY_BYTES)
        {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "This request is too large",
            ));
        }
        Ok(Self)
    }
}

// ---- Which (invite, anchor) pairs, in what order ----

/// Every pin a holder of an existing invitation may have: each earlier host on this
/// lineage, from the authority's own verified journal history alone, then the
/// current one. Oldest first; a caller can add or choose none of them.
fn lineage(current: &RoutePin, history: &[ActivationGrant]) -> ApiResult<Vec<RoutePin>> {
    if history.len() > MAX_ROUTE_MOVES {
        return Err(stale());
    }
    let mut anchors = Vec::with_capacity(history.len() + 1);
    for grant in history {
        let body = &grant.receipt.body;
        if body.server_id != current.server_id || body.term >= current.term {
            return Err(stale());
        }
        anchors.push(RoutePin {
            server_id: current.server_id.clone(),
            host: grant.source_identity.clone(),
            term: body.term,
        });
    }
    anchors.sort_by_key(|pin| pin.term);
    anchors.dedup_by_key(|pin| pin.term);
    anchors.push(current.clone());
    Ok(anchors)
}

/// A cursor is a canonical lower-case hash and a term of this lineage. It does not
/// have to name an invite that still exists: a revocation between two pages must
/// not stop the walk.
fn check_cursor(after: Option<&Cursor>, lineage: &[RoutePin]) -> ApiResult<()> {
    match after {
        Some(cursor)
            if hash_bytes(&cursor.invite_id).is_none()
                || !lineage.iter().any(|pin| pin.term == cursor.anchor_term) =>
        {
            Err(bad("Invalid page position"))
        }
        _ => Ok(()),
    }
}

struct Pair {
    hash: String,
    expires: u64,
    anchor: RoutePin,
}

/// What each kind of page walks, in hash order. Invitations are the usable invites; sessions
/// are exactly the rows `authenticate_token` would accept: an unexpired session of a user
/// whose membership is active (the joins also drop a session whose user is gone).
struct Source {
    /// One named row, to finish the entry the last page stopped inside.
    one: &'static str,
    /// The next rows after a hash, at most `?3`.
    after: &'static str,
    /// What to say when a stored hash is not a canonical hash.
    damaged: &'static str,
}

fn source(purpose: Purpose) -> Source {
    match purpose {
        Purpose::Invitation => Source {
            one: "SELECT expires FROM invites WHERE code_hash=?1 AND expires>?2 AND remaining_uses>0",
            after: "SELECT code_hash,expires FROM invites WHERE code_hash>?1 AND expires>?2 AND remaining_uses>0 ORDER BY code_hash LIMIT ?3",
            damaged: "Invitation storage needs attention before recovery addresses can be renewed",
        },
        Purpose::Session => Source {
            one: "SELECT s.expires FROM sessions s JOIN users u ON u.id=s.user_id JOIN membership m ON m.user_id=u.id WHERE s.token_hash=?1 AND s.expires>?2 AND m.status='active'",
            after: "SELECT s.token_hash,s.expires FROM sessions s JOIN users u ON u.id=s.user_id JOIN membership m ON m.user_id=u.id WHERE s.token_hash>?1 AND s.expires>?2 AND m.status='active' ORDER BY s.token_hash LIMIT ?3",
            damaged: "Session storage needs attention before recovery addresses can be renewed",
        },
    }
}

/// Up to PAGE_RECORDS pairs after the cursor, in (hash, anchor term) order, from usable
/// entries of one kind only. Reads at most a handful of rows; never the table.
fn select(
    db: &Connection,
    purpose: Purpose,
    after: Option<&Cursor>,
    lineage: &[RoutePin],
    at: u64,
) -> ApiResult<Vec<Pair>> {
    let source = source(purpose);
    let mut pairs: Vec<Pair> = Vec::with_capacity(model::PAGE_RECORDS);
    let mut bound = String::new();
    if let Some(cursor) = after {
        // The rest of the entry the last page stopped inside, if it still exists.
        let expires: Option<u64> = db
            .query_row(source.one, params![cursor.invite_id, at], |row| row.get(0))
            .optional()?;
        if let Some(expires) = expires {
            for anchor in lineage.iter().filter(|pin| pin.term > cursor.anchor_term) {
                if pairs.len() == model::PAGE_RECORDS {
                    break;
                }
                pairs.push(Pair {
                    hash: cursor.invite_id.clone(),
                    expires,
                    anchor: anchor.clone(),
                });
            }
        }
        bound = cursor.invite_id.clone();
    }
    for _ in 0..SCAN_ROUNDS {
        let room = model::PAGE_RECORDS - pairs.len();
        if room == 0 {
            break;
        }
        let mut query = db.prepare(source.after)?;
        let rows = query
            .query_map(params![bound, at, room as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let Some((last, _)) = rows.last() else {
            break;
        };
        bound = last.clone();
        for (hash, expires) in rows {
            // A stored hash that is not canonical cannot be walked past: stop rather
            // than report the walk finished.
            if hash_bytes(&hash).is_none() {
                return Err(bad(source.damaged));
            }
            for anchor in lineage {
                if pairs.len() == model::PAGE_RECORDS {
                    break;
                }
                pairs.push(Pair {
                    hash: hash.clone(),
                    expires,
                    anchor: anchor.clone(),
                });
            }
        }
    }
    Ok(pairs)
}

/// A full page means there may be more: continue after its last pair. Anything
/// shorter was the end, so it carries no position.
fn next(pairs: &[Pair]) -> Option<Cursor> {
    (pairs.len() == model::PAGE_RECORDS)
        .then(|| pairs.last())
        .flatten()
        .map(|pair| Cursor {
            invite_id: pair.hash.clone(),
            anchor_term: pair.anchor.term,
        })
}

// ---- The endpoint ----

/// Renewal pages for the usable invitations.
pub(super) async fn page(
    State(state): State<AppState>,
    _local: LocalControl,
    body: Bytes,
) -> ApiResult<Json<Page>> {
    serve(state, body, Purpose::Invitation)
}

/// Renewal pages for the remembered sessions that are still valid.
pub(super) async fn session_page(
    State(state): State<AppState>,
    _local: LocalControl,
    body: Bytes,
) -> ApiResult<Json<Page>> {
    serve(state, body, Purpose::Session)
}

fn serve(state: AppState, body: Bytes, purpose: Purpose) -> ApiResult<Json<Page>> {
    let input: PageRequest = serde_json::from_slice(&body).map_err(|_| bad("Invalid request"))?;
    drop(body);
    let authority = &state.0.handover;
    let pin = authority.route_pin();
    let status = authority.status();
    // Only the writable authority it was asked about, exactly: not a retired or
    // frozen source, and not any other key or term.
    if !status.writes_allowed || !authority.writes_allowed() {
        return Err(forbidden());
    }
    if input.current != pin
        || status.term != pin.term
        || status.server_id != pin.server_id
        || status.source_identity.as_ref() != Some(&pin.host)
    {
        return Err(stale());
    }
    let anchors = lineage(&pin, &status.history)?;
    check_cursor(input.after.as_ref(), &anchors)?;
    // The same lock order as issuing a single hint: the table first, then the journal.
    let db = lock(&state.0.db);
    let pairs = select(&db, purpose, input.after.as_ref(), &anchors, now())?;
    let mut records = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        let hash = hash_bytes(&pair.hash).ok_or_else(|| bad("Invalid page position"))?;
        let issue = Issue {
            invite_id: pair.hash.clone(),
            anchor: pair.anchor.clone(),
            destination: input.destination.clone(),
        };
        let record = match purpose {
            Purpose::Invitation => authority.issue_locator(issue, hash, pair.expires)?,
            Purpose::Session => authority.issue_session_locator(issue, hash, pair.expires)?,
        };
        // Signed under the journal lock; if the host changed meanwhile, this page is
        // stale and nothing signed by another host is returned.
        if record.current != input.current {
            return Err(stale());
        }
        records.push(record);
    }
    if !authority.writes_allowed() || authority.route_pin() != input.current {
        return Err(stale());
    }
    let page = Page {
        current: input.current,
        next: next(&pairs),
        records,
    };
    if serde_json::to_vec(&page).map_or(true, |raw| raw.len() > model::MAX_PAGE_BYTES) {
        return Err(bad("The page is too large"));
    }
    Ok(Json(page))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{new_invite, signup};
    use axum::{body::Body, http::Request};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        ChaCha20Poly1305, Nonce,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use http_body_util::BodyExt;
    use relay_protocol::handover::{HostIdentity, Receipt, ReceiptBody};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    const CONTROL: &str = "fixture-local-control-secret-0123456789";
    const DESTINATION: &str = "https://source.example";

    // ---- pure selection over a bare invites table ----

    fn table() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE invites(code_hash TEXT PRIMARY KEY,expires INTEGER NOT NULL,remaining_uses INTEGER NOT NULL DEFAULT 1,label TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        db
    }
    fn hash(n: u128) -> String {
        format!("{n:064x}")
    }
    fn invite(db: &Connection, n: u128, expires: u64, uses: u32) {
        db.execute(
            "INSERT INTO invites(code_hash,expires,remaining_uses) VALUES(?1,?2,?3)",
            params![hash(n), expires, uses],
        )
        .unwrap();
    }
    fn identity(seed: u8) -> HostIdentity {
        HostIdentity {
            host_id: uuid::Uuid::from_bytes([seed; 16]).to_string(),
            public_key: format!("key-{seed}"),
        }
    }
    fn grant(server: &str, from: &HostIdentity, term: u64, to: &HostIdentity) -> ActivationGrant {
        ActivationGrant {
            receipt: Receipt {
                body: ReceiptBody {
                    server_id: server.into(),
                    source_host_id: uuid::Uuid::new_v4().to_string(),
                    term,
                    preparation_id: "p".into(),
                    export_id: "e".into(),
                    target_identity: to.clone(),
                    target_url: "https://new-host.example/".into(),
                    database_sha256: "a".repeat(64),
                    manifest_sha256: "b".repeat(64),
                    database_bytes: 1,
                    challenge: "c".into(),
                },
                signature: "s".into(),
            },
            source_identity: from.clone(),
            next_term: term + 1,
            history_sha256: None,
            signature: "s".into(),
        }
    }
    /// A lineage of `moves` earlier hosts and the current one, returned with its history.
    fn chain(moves: u8) -> (RoutePin, Vec<ActivationGrant>) {
        let server = uuid::Uuid::new_v4().to_string();
        let history: Vec<_> = (0..moves)
            .map(|i| grant(&server, &identity(i + 1), i as u64 + 1, &identity(i + 2)))
            .collect();
        let current = RoutePin {
            server_id: server,
            host: identity(moves + 1),
            term: moves as u64 + 1,
        };
        (current, history)
    }
    fn walk(db: &Connection, anchors: &[RoutePin], at: u64) -> Vec<Vec<(String, u64)>> {
        walk_of(db, Purpose::Invitation, anchors, at)
    }
    fn walk_of(
        db: &Connection,
        purpose: Purpose,
        anchors: &[RoutePin],
        at: u64,
    ) -> Vec<Vec<(String, u64)>> {
        let mut pages = vec![];
        let mut after: Option<Cursor> = None;
        loop {
            check_cursor(after.as_ref(), anchors).unwrap();
            let pairs = select(db, purpose, after.as_ref(), anchors, at).unwrap();
            pages.push(
                pairs
                    .iter()
                    .map(|p| (p.hash.clone(), p.anchor.term))
                    .collect(),
            );
            after = next(&pairs);
            if after.is_none() {
                return pages;
            }
            assert!(pages.len() < 20, "the walk must end");
        }
    }

    #[test]
    fn anchors_come_only_from_the_journal_history_oldest_first_and_end_with_the_current_pin() {
        let (current, mut history) = chain(3);
        history.reverse();
        let anchors = lineage(&current, &history).unwrap();
        assert_eq!(
            anchors.iter().map(|p| p.term).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(anchors.last(), Some(&current));
        assert!(anchors.iter().all(|p| p.server_id == current.server_id));
        // No history: only the current pin.
        let (alone, none) = chain(0);
        assert_eq!(lineage(&alone, &none).unwrap(), vec![alone.clone()]);
        // A grant from another community, from a later term, or too many moves: stale.
        let mut foreign = history.clone();
        foreign[0].receipt.body.server_id = uuid::Uuid::new_v4().to_string();
        assert!(lineage(&current, &foreign).is_err());
        let mut later = history.clone();
        later[0].receipt.body.term = current.term;
        assert!(lineage(&current, &later).is_err());
        let (long, too_many) = chain(MAX_ROUTE_MOVES as u8 + 1);
        assert!(lineage(&long, &too_many).is_err());
    }

    #[test]
    fn four_record_pages_cover_every_usable_invite_and_anchor_once_in_order() {
        let db = table();
        let (current, history) = chain(2);
        let anchors = lineage(&current, &history).unwrap();
        for n in [30, 10, 20] {
            invite(&db, n, 5_000, 1);
        }
        // Expired and spent invitations are never offered.
        invite(&db, 15, 50, 1);
        invite(&db, 25, 5_000, 0);
        let pages = walk(&db, &anchors, 100);
        assert_eq!(
            pages.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![4, 4, 1],
            "9 pairs, 4 at a time"
        );
        let flat: Vec<_> = pages.concat();
        let expected: Vec<_> = [10u128, 20, 30]
            .iter()
            .flat_map(|n| (1..=3).map(move |t| (hash(*n), t)))
            .collect();
        assert_eq!(flat, expected);
        // A full last page is followed by an empty one that invents nothing.
        let db = table();
        for n in [1, 2] {
            invite(&db, n, 5_000, 1);
        }
        let (current, history) = chain(3);
        let anchors = lineage(&current, &history).unwrap();
        let pages = walk(&db, &anchors, 100);
        assert_eq!(
            pages.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![4, 4, 0]
        );
        // No invitation at all: one empty page, no cursor.
        assert_eq!(walk(&table(), &anchors, 100), vec![vec![]]);
    }

    #[test]
    fn the_walk_continues_after_the_invite_it_stopped_at_is_revoked_or_spent() {
        let db = table();
        let (current, history) = chain(2);
        let anchors = lineage(&current, &history).unwrap();
        for n in [1, 2, 3, 4] {
            invite(&db, n, 5_000, 1);
        }
        let first = select(&db, Purpose::Invitation, None, &anchors, 100).unwrap();
        let cursor = next(&first).unwrap();
        assert_eq!(
            (cursor.invite_id.as_str(), cursor.anchor_term),
            (hash(2).as_str(), 1)
        );
        // Revoked between the pages: the walk goes on with the next hash, skipping the rest of it.
        db.execute("DELETE FROM invites WHERE code_hash=?1", [hash(2)])
            .unwrap();
        let second = select(&db, Purpose::Invitation, Some(&cursor), &anchors, 100).unwrap();
        let seen: Vec<_> = second
            .iter()
            .map(|p| (p.hash.clone(), p.anchor.term))
            .collect();
        assert_eq!(
            seen,
            vec![(hash(3), 1), (hash(3), 2), (hash(3), 3), (hash(4), 1)]
        );
        // Still alive: the rest of its anchors come first.
        db.execute(
            "INSERT INTO invites(code_hash,expires,remaining_uses) VALUES(?1,5000,1)",
            [hash(2)],
        )
        .unwrap();
        let again = select(&db, Purpose::Invitation, Some(&cursor), &anchors, 100).unwrap();
        assert_eq!(
            again
                .iter()
                .map(|p| (p.hash.clone(), p.anchor.term))
                .collect::<Vec<_>>(),
            vec![(hash(2), 2), (hash(2), 3), (hash(3), 1), (hash(3), 2)]
        );
        // A cursor that names a hash that never existed is fine too.
        let cursor = Cursor {
            invite_id: hash(0x5),
            anchor_term: 3,
        };
        assert!(check_cursor(Some(&cursor), &anchors).is_ok());
    }

    #[test]
    fn a_cursor_must_be_canonical_and_name_a_term_of_this_lineage() {
        let (current, history) = chain(2);
        let anchors = lineage(&current, &history).unwrap();
        let ok = Cursor {
            invite_id: hash(7),
            anchor_term: 2,
        };
        assert!(check_cursor(Some(&ok), &anchors).is_ok());
        assert!(check_cursor(None, &anchors).is_ok());
        for (id, term) in [
            (hash(7).to_uppercase().replace('0', "A"), 2),
            (hash(7)[1..].to_string(), 2),
            (format!("{}0", hash(7)), 2),
            ("g".repeat(64), 2),
            (String::new(), 2),
            (hash(7), 0),
            (hash(7), 4),
            (hash(7), u64::MAX),
        ] {
            let bad = Cursor {
                invite_id: id,
                anchor_term: term,
            };
            assert!(check_cursor(Some(&bad), &anchors).is_err());
        }
    }

    // ---- what each selected pair becomes ----

    fn capability_key(anchor: &RoutePin, hash: &[u8; 32]) -> [u8; 32] {
        Sha256::digest(model::capability_material(anchor, hash, true)).into()
    }
    fn session_key(anchor: &RoutePin, hash: &[u8; 32]) -> [u8; 32] {
        Sha256::digest(model::session_material(anchor, hash, true)).into()
    }
    fn open(record: &model::Record, anchor: &RoutePin, hash: &[u8; 32]) -> Option<Vec<u8>> {
        open_with(record, capability_key(anchor, hash))
    }
    fn open_session(record: &model::Record, anchor: &RoutePin, hash: &[u8; 32]) -> Option<Vec<u8>> {
        open_with(record, session_key(anchor, hash))
    }
    fn open_with(record: &model::Record, key: [u8; 32]) -> Option<Vec<u8>> {
        let nonce = URL_SAFE_NO_PAD.decode(&record.nonce).ok()?;
        let ciphertext = URL_SAFE_NO_PAD.decode(&record.ciphertext).ok()?;
        ChaCha20Poly1305::new_from_slice(&key)
            .ok()?
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &model::associated_data(record),
                },
            )
            .ok()
    }

    #[test]
    fn every_anchor_gets_a_record_only_its_own_invite_and_anchor_can_open() {
        let key = SigningKey::from_bytes(&[91; 32]);
        let (mut current, history) = chain(3);
        current.host = crate::handover_target::identity(&key.verifying_key());
        let anchors = lineage(&current, &history).unwrap();
        let code_hash = hash_bytes(&hash(0xabc)).unwrap();
        let destination = model::Destination {
            url: "https://new-host.example/".into(),
            iroh: None,
        };
        let mut records = vec![];
        for anchor in &anchors {
            let sealed = crate::handover_locator::seal(
                Issue {
                    invite_id: hash(0xabc),
                    anchor: anchor.clone(),
                    destination: model::Destination {
                        url: destination.url.clone(),
                        iroh: None,
                    },
                },
                code_hash,
                now() + 3600,
                current.clone(),
                history.clone(),
                |message| URL_SAFE_NO_PAD.encode(key.sign(message).to_bytes()),
            )
            .unwrap();
            records.push(sealed);
        }
        let ids: std::collections::HashSet<_> = records.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids.len(), anchors.len(), "one directory ID per anchor");
        for (index, record) in records.iter().enumerate() {
            // Signed by the current host, whatever the anchor.
            assert_eq!(record.current, current);
            assert!(crate::handover_target::verify(
                &relay_protocol::handover::HostProof {
                    identity: current.host.clone(),
                    signature: record.signature.clone(),
                },
                &model::record_message(record)
            ));
            // The old anchor's capability opens its own record, and only that one.
            let plain = open(record, &anchors[index], &code_hash).expect("own capability opens");
            let found: model::Destination = serde_json::from_slice(&plain).unwrap();
            assert_eq!(found.url, destination.url);
            for (other, anchor) in anchors.iter().enumerate() {
                if other != index {
                    assert!(open(record, anchor, &code_hash).is_none());
                }
            }
            assert!(open(record, &anchors[index], &hash_bytes(&hash(0xabd)).unwrap()).is_none());
            // The verifier stays private. Public signed delegation history still names its target URL.
            let raw = serde_json::to_string(record).unwrap();
            assert!(!raw.contains(&hash(0xabc)));
        }
    }

    // ---- the endpoint, with a real authority ----

    fn local() -> (tempfile::TempDir, AppState, Router) {
        let data = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            local_control_token: Some(CONTROL.into()),
            ..Default::default()
        };
        let state = AppState::open_with_config(data.path(), "locator page", 0, config).unwrap();
        let app = router(state.clone());
        (data, state, app)
    }

    async fn call(
        app: &Router,
        bearer: Option<&str>,
        peer: Option<&str>,
        body: Vec<u8>,
        extra: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        send(app, model::LOCAL_PAGE_PATH, bearer, peer, body, extra).await
    }

    async fn send(
        app: &Router,
        path: &str,
        bearer: Option<&str>,
        peer: Option<&str>,
        body: Vec<u8>,
        extra: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder()
            .header("host", "127.0.0.1")
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        if let Some(peer) = peer {
            request = request.extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
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

    fn wanted(state: &AppState, after: Option<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "current": state.0.handover.route_pin(),
            "destination": {"url": DESTINATION},
            "after": after,
        }))
        .unwrap()
    }

    async fn local_page(app: &Router, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
        call(app, Some(CONTROL), Some("127.0.0.1:50000"), body, &[]).await
    }

    #[tokio::test]
    async fn the_local_owner_pages_existing_invites_four_at_a_time_and_the_records_open_for_their_invite(
    ) {
        let (_data, state, app) = local();
        let owner = signup(&app, "Owner", "").await;
        let mut codes = vec![];
        for _ in 0..5 {
            codes.push(new_invite(&app, &owner).await);
        }
        // Not usable: expired, and spent.
        {
            let db = lock(&state.0.db);
            for (name, expires, uses) in [("old", now() - 10, 1u32), ("spent", now() + 3600, 0)] {
                db.execute(
                    "INSERT INTO invites(code_hash,expires,remaining_uses) VALUES(?1,?2,?3)",
                    params![token_hash(name), expires, uses],
                )
                .unwrap();
            }
        }
        let pin = state.0.handover.route_pin();
        let mut hashes: Vec<_> = codes.iter().map(|c| token_hash(c)).collect();
        hashes.sort();
        let (status, raw) = local_page(&app, wanted(&state, None)).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&raw));
        let first: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(first["records"].as_array().unwrap().len(), 4);
        assert_eq!(first["next"]["invite_id"], hashes[3]);
        assert_eq!(first["next"]["anchor_term"], pin.term);
        // The verifier appears only in the cursor, never in a signed record or the link.
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert_eq!(text.matches(&hashes[3]).count(), 1);
        for code in &codes {
            assert!(!text.contains(code.as_str()));
        }
        assert!(!text.contains("source.example"));
        // The owner revokes the invite the cursor names; the walk still ends cleanly.
        lock(&state.0.db)
            .execute("DELETE FROM invites WHERE code_hash=?1", [&hashes[3]])
            .unwrap();
        let (status, raw) = local_page(&app, wanted(&state, Some(first["next"].clone()))).await;
        assert_eq!(status, StatusCode::OK);
        let second: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(second["records"].as_array().unwrap().len(), 1);
        assert!(second["next"].is_null());
        // Each record belongs to one invite and opens with its capability alone.
        let mut seen = vec![];
        for record in first["records"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["records"].as_array().unwrap())
        {
            let record: model::Record = serde_json::from_value(record.clone()).unwrap();
            assert_eq!(record.current, pin);
            assert!(crate::handover_target::verify(
                &relay_protocol::handover::HostProof {
                    identity: pin.host.clone(),
                    signature: record.signature.clone(),
                },
                &model::record_message(&record)
            ));
            let owner_of: Vec<_> = hashes
                .iter()
                .filter(|h| open(&record, &pin, &hash_bytes(h).unwrap()).is_some())
                .collect();
            assert_eq!(owner_of.len(), 1);
            seen.push(owner_of[0].clone());
        }
        // The record for the revoked invite was signed before it was revoked; the walk
        // lost nothing and repeated nothing, in stable hash order.
        assert_eq!(seen, hashes);
    }

    #[tokio::test]
    async fn only_the_private_local_control_secret_on_this_pc_opens_the_endpoint() {
        let (_data, state, app) = local();
        let owner = signup(&app, "Owner", "").await;
        new_invite(&app, &owner).await;
        let body = wanted(&state, None);
        let ok = "127.0.0.1:50000";
        assert_eq!(
            call(&app, Some(CONTROL), Some(ok), body.clone(), &[])
                .await
                .0,
            StatusCode::OK
        );
        let refused: Vec<(&str, (StatusCode, Vec<u8>))> = vec![
            (
                "no credentials",
                call(&app, None, Some(ok), body.clone(), &[]).await,
            ),
            (
                "admin session",
                call(&app, Some(&owner.token), Some(ok), body.clone(), &[]).await,
            ),
            (
                "wrong secret",
                call(
                    &app,
                    Some("not-the-secret-0123456789abcdef0123"),
                    Some(ok),
                    body.clone(),
                    &[],
                )
                .await,
            ),
            (
                "secret as a header only",
                call(
                    &app,
                    None,
                    Some(ok),
                    body.clone(),
                    &[("x-relay-local-control", CONTROL)],
                )
                .await,
            ),
            (
                "no socket information",
                call(&app, Some(CONTROL), None, body.clone(), &[]).await,
            ),
            (
                "remote socket",
                call(
                    &app,
                    Some(CONTROL),
                    Some("100.64.0.9:50000"),
                    body.clone(),
                    &[],
                )
                .await,
            ),
            (
                "forwarded",
                call(
                    &app,
                    Some(CONTROL),
                    Some(ok),
                    body.clone(),
                    &[("x-forwarded-for", "127.0.0.1")],
                )
                .await,
            ),
            (
                "browser origin",
                call(
                    &app,
                    Some(CONTROL),
                    Some(ok),
                    body.clone(),
                    &[("origin", "https://evil.example")],
                )
                .await,
            ),
        ];
        for (name, (status, raw)) in refused {
            assert_eq!(status, StatusCode::FORBIDDEN, "{name}");
            assert!(!String::from_utf8_lossy(&raw).contains("records"), "{name}");
        }
        // The network helper's marker never opens it, even with the right secret.
        let helper = NetworkIngress {
            peer_ip: "100.64.0.9".parse().unwrap(),
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .header("host", "127.0.0.1")
                    .method("POST")
                    .uri(model::LOCAL_PAGE_PATH)
                    .header("authorization", format!("Bearer {CONTROL}"))
                    .extension(ConnectInfo(
                        "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
                    ))
                    .extension(helper)
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // Only the exact POST: other methods and neighbouring paths stay closed.
        for (method, uri) in [
            ("GET", model::LOCAL_PAGE_PATH),
            ("PUT", model::LOCAL_PAGE_PATH),
            ("POST", "/api/local/handover/locator-pages"),
            ("POST", "/api/local/handover"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .header("host", "127.0.0.1")
                        .method(method)
                        .uri(uri)
                        .header("authorization", format!("Bearer {CONTROL}"))
                        .extension(ConnectInfo(
                            "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
                        ))
                        .body(Body::from(body.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(!response.status().is_success(), "{method} {uri}");
        }
        // A Friends service never serves it, with or without the secret.
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
                local_control_token: Some(CONTROL.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let app = router(friends.clone());
        assert_eq!(
            call(&app, Some(CONTROL), Some(ok), wanted(&friends, None), &[])
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn another_host_a_stale_term_a_frozen_source_and_bad_requests_are_refused() {
        let (_data, state, app) = local();
        let owner = signup(&app, "Owner", "").await;
        new_invite(&app, &owner).await;
        let pin = state.0.handover.route_pin();
        let ask = |current: Value, after: Value| {
            serde_json::to_vec(
                &json!({"current": current, "destination": {"url": DESTINATION}, "after": after}),
            )
            .unwrap()
        };
        let mut later = serde_json::to_value(&pin).unwrap();
        later["term"] = json!(pin.term + 1);
        let mut other = serde_json::to_value(&pin).unwrap();
        other["host"]["public_key"] = json!(URL_SAFE_NO_PAD.encode([5u8; 32]));
        let mut elsewhere = serde_json::to_value(&pin).unwrap();
        elsewhere["server_id"] = json!(uuid::Uuid::new_v4().to_string());
        for (name, current) in [
            ("later term", later),
            ("another host key", other),
            ("another community", elsewhere),
        ] {
            let (status, _) = local_page(&app, ask(current, Value::Null)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{name}");
        }
        // Bad bodies and cursors: 400, and an oversized body is refused before it is read.
        let current = serde_json::to_value(&pin).unwrap();
        let hash = token_hash("x");
        for (name, body) in [
            ("not json", b"nope".to_vec()),
            ("unknown field", serde_json::to_vec(&json!({"current": current, "destination": {"url": DESTINATION}, "history": []})).unwrap()),
            ("no destination", serde_json::to_vec(&json!({"current": current})).unwrap()),
            ("upper-case cursor", ask(current.clone(), json!({"invite_id": hash.to_uppercase(), "anchor_term": pin.term}))),
            ("short cursor", ask(current.clone(), json!({"invite_id": &hash[1..], "anchor_term": pin.term}))),
            ("term of another lineage", ask(current.clone(), json!({"invite_id": hash, "anchor_term": pin.term + 5}))),
            ("term zero", ask(current.clone(), json!({"invite_id": hash, "anchor_term": 0}))),
        ] {
            let (status, _) = local_page(&app, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        }
        let (status, _) = local_page(&app, vec![b' '; BODY_BYTES + 1]).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        // Without the secret, even an oversized or malformed body is answered by the secret check.
        let (status, _) = call(
            &app,
            None,
            Some("127.0.0.1:50000"),
            vec![b' '; BODY_BYTES + 1],
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // A source that stopped being the writable authority signs nothing.
        assert_eq!(
            local_page(&app, wanted(&state, None)).await.0,
            StatusCode::OK
        );
        state
            .0
            .handover
            .begin_restore(state.server_id().unwrap())
            .unwrap();
        let (status, raw) = local_page(&app, wanted(&state, None)).await;
        assert!(status.is_client_error(), "{status}");
        assert!(!String::from_utf8_lossy(&raw).contains("records"));
    }

    // ---- the session route ----

    const SESSION_PATH: &str = model::LOCAL_SESSION_PAGE_PATH;

    fn accounts() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE users(id TEXT PRIMARY KEY);
             CREATE TABLE membership(user_id TEXT PRIMARY KEY,status TEXT NOT NULL DEFAULT 'active');
             CREATE TABLE sessions(token_hash TEXT PRIMARY KEY,user_id TEXT NOT NULL,expires INTEGER NOT NULL);",
        )
        .unwrap();
        db
    }
    fn member(db: &Connection, user: &str, status: &str) {
        db.execute("INSERT INTO users(id) VALUES(?1)", [user])
            .unwrap();
        db.execute(
            "INSERT INTO membership(user_id,status) VALUES(?1,?2)",
            [user, status],
        )
        .unwrap();
    }
    fn session_row(db: &Connection, n: u128, user: &str, expires: u64) {
        db.execute(
            "INSERT INTO sessions(token_hash,user_id,expires) VALUES(?1,?2,?3)",
            params![hash(n), user, expires],
        )
        .unwrap();
    }

    #[test]
    fn only_sessions_a_request_would_still_accept_are_walked_in_hash_order_across_every_anchor() {
        let db = accounts();
        member(&db, "active-a", "active");
        member(&db, "active-b", "active");
        member(&db, "removed", "suspended");
        let (current, history) = chain(2);
        let anchors = lineage(&current, &history).unwrap();
        // 1, 6, 7, 8 are valid; 2 expired; 3 belongs to an inactive member; 5 to nobody.
        session_row(&db, 8, "active-b", 5_000);
        session_row(&db, 1, "active-a", 5_000);
        session_row(&db, 2, "active-a", 50);
        session_row(&db, 3, "removed", 5_000);
        session_row(&db, 5, "ghost", 5_000);
        session_row(&db, 6, "active-b", 5_000);
        session_row(&db, 7, "active-a", 5_000);
        let pages = walk_of(&db, Purpose::Session, &anchors, 100);
        assert_eq!(
            pages.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![4, 4, 4, 0],
            "12 pairs, 4 at a time, then the empty page that ends the walk"
        );
        let expected: Vec<_> = [1u128, 6, 7, 8]
            .iter()
            .flat_map(|n| (1..=3).map(move |t| (hash(*n), t)))
            .collect();
        assert_eq!(pages.concat(), expected);
        // Logged out (row gone) or removed between two pages: the walk goes on past it.
        let first = select(&db, Purpose::Session, None, &anchors, 100).unwrap();
        let cursor = next(&first).unwrap();
        assert_eq!(
            (cursor.invite_id.as_str(), cursor.anchor_term),
            (hash(6).as_str(), 1)
        );
        db.execute("DELETE FROM sessions WHERE token_hash=?1", [hash(6)])
            .unwrap();
        db.execute(
            "UPDATE membership SET status='removed' WHERE user_id='active-a'",
            [],
        )
        .unwrap();
        let second = select(&db, Purpose::Session, Some(&cursor), &anchors, 100).unwrap();
        assert_eq!(
            second
                .iter()
                .map(|p| (p.hash.clone(), p.anchor.term))
                .collect::<Vec<_>>(),
            vec![(hash(8), 1), (hash(8), 2), (hash(8), 3)],
            "only the remaining valid session; the removed member's sessions are skipped"
        );
        // A stored hash that is not a hash stops the walk instead of ending it quietly.
        db.execute(
            "INSERT INTO sessions(token_hash,user_id,expires) VALUES('not-a-hash','active-b',5000)",
            [],
        )
        .unwrap();
        assert!(select(&db, Purpose::Session, Some(&cursor), &anchors, 100).is_err());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
    }

    #[test]
    fn a_malformed_stored_invitation_stops_the_walk_instead_of_claiming_it_finished() {
        let db = table();
        let (current, history) = chain(0);
        let anchors = lineage(&current, &history).unwrap();
        db.execute(
            "INSERT INTO invites(code_hash,expires,remaining_uses) VALUES('1-invalid',999,1)",
            [],
        )
        .unwrap();
        invite(&db, 0xf, 999, 1);
        assert!(select(&db, Purpose::Invitation, None, &anchors, 1).is_err());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM invites", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn exactly_the_two_posts_are_the_local_pages() {
        let post = axum::http::Method::POST;
        assert!(is_route(model::LOCAL_PAGE_PATH, &post));
        assert!(is_route(SESSION_PATH, &post));
        for path in [
            "/api/local/handover/session-locator-pages",
            "/api/local/handover/locator-page/",
            "/api/local/handover",
            "/api/local/shutdown",
            "/api/local",
        ] {
            assert!(!is_route(path, &post), "{path}");
        }
        for method in [
            axum::http::Method::GET,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
        ] {
            assert!(!is_route(SESSION_PATH, &method));
        }
    }

    #[tokio::test]
    async fn the_session_route_pages_valid_sessions_and_its_records_open_only_with_session_material(
    ) {
        let (_data, state, app) = local();
        let owner = signup(&app, "Sessionowner", "").await;
        let mut sessions = vec![owner.clone()];
        for n in 0..5 {
            let code = new_invite(&app, &owner).await;
            sessions.push(signup(&app, &format!("Sessionmember{n}"), &code).await);
        }
        {
            let db = lock(&state.0.db);
            // One member is no longer active; the owner also has an old, expired session.
            db.execute(
                "UPDATE membership SET status='suspended' WHERE user_id=?1",
                [&sessions[5].user.id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO sessions(token_hash,user_id,expires,created_at) VALUES(?1,?2,?3,?4)",
                params![
                    token_hash("expired"),
                    owner.user.id,
                    now() - 10,
                    now() - 100
                ],
            )
            .unwrap();
        }
        let pin = state.0.handover.route_pin();
        let mut hashes: Vec<_> = sessions[..5].iter().map(|s| token_hash(&s.token)).collect();
        hashes.sort();
        let ask = |after: Option<Value>| wanted(&state, after);
        let page = |body: Vec<u8>| {
            let app = app.clone();
            async move {
                send(
                    &app,
                    SESSION_PATH,
                    Some(CONTROL),
                    Some("127.0.0.1:50000"),
                    body,
                    &[],
                )
                .await
            }
        };
        let (status, raw) = page(ask(None)).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&raw));
        let first: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(first["records"].as_array().unwrap().len(), 4);
        assert_eq!(first["next"]["invite_id"], hashes[3]);
        // No raw token, user ID, name or destination anywhere, and the verifier only in the cursor.
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert_eq!(text.matches(&hashes[3]).count(), 1);
        for session in &sessions {
            assert!(!text.contains(&session.token) && !text.contains(&session.user.id));
            assert!(!text.contains(&session.user.username));
        }
        assert!(!text.contains("source.example"));
        // The session the cursor names is logged out between pages; the walk still ends.
        lock(&state.0.db)
            .execute("DELETE FROM sessions WHERE token_hash=?1", [&hashes[3]])
            .unwrap();
        let (status, raw) = page(ask(Some(first["next"].clone()))).await;
        assert_eq!(status, StatusCode::OK);
        let second: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(second["records"].as_array().unwrap().len(), 1);
        assert!(second["next"].is_null());
        // Each record belongs to one session, opens with SESSION material alone, and its ID is
        // the session ID: the invitation key for the same bytes cannot open it.
        let mut seen = vec![];
        for record in first["records"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["records"].as_array().unwrap())
        {
            let record: model::Record = serde_json::from_value(record.clone()).unwrap();
            assert_eq!(record.current, pin);
            assert!(crate::handover_target::verify(
                &relay_protocol::handover::HostProof {
                    identity: pin.host.clone(),
                    signature: record.signature.clone(),
                },
                &model::record_message(&record)
            ));
            let owners: Vec<_> = hashes
                .iter()
                .filter(|h| open_session(&record, &pin, &hash_bytes(h).unwrap()).is_some())
                .collect();
            assert_eq!(owners.len(), 1);
            let bytes = hash_bytes(owners[0]).unwrap();
            assert!(
                open(&record, &pin, &bytes).is_none(),
                "an invitation key does not open it"
            );
            let session_id = format!(
                "{:x}",
                Sha256::digest(model::session_material(&pin, &bytes, false))
            );
            let invitation_id = format!(
                "{:x}",
                Sha256::digest(model::capability_material(&pin, &bytes, false))
            );
            assert_eq!(record.id, session_id);
            assert_ne!(record.id, invitation_id);
            seen.push(owners[0].clone());
        }
        assert_eq!(
            seen, hashes,
            "stable hash order; only the five valid sessions"
        );
    }

    #[tokio::test]
    async fn the_session_route_has_the_same_local_only_admission_as_the_invitation_route() {
        let (_data, state, app) = local();
        let owner = signup(&app, "Sessionowner", "").await;
        let body = wanted(&state, None);
        let ok = "127.0.0.1:50000";
        for path in [model::LOCAL_PAGE_PATH, SESSION_PATH] {
            assert_eq!(
                send(&app, path, Some(CONTROL), Some(ok), body.clone(), &[])
                    .await
                    .0,
                StatusCode::OK,
                "{path}"
            );
            let refused = [
                send(&app, path, None, Some(ok), body.clone(), &[]).await,
                send(&app, path, Some(&owner.token), Some(ok), body.clone(), &[]).await,
                send(
                    &app,
                    path,
                    Some("not-the-secret-0123456789abcdef0123"),
                    Some(ok),
                    body.clone(),
                    &[],
                )
                .await,
                send(&app, path, Some(CONTROL), None, body.clone(), &[]).await,
                send(
                    &app,
                    path,
                    Some(CONTROL),
                    Some("100.64.0.9:50000"),
                    body.clone(),
                    &[],
                )
                .await,
                send(
                    &app,
                    path,
                    Some(CONTROL),
                    Some(ok),
                    body.clone(),
                    &[("x-forwarded-for", "127.0.0.1")],
                )
                .await,
                send(
                    &app,
                    path,
                    Some(CONTROL),
                    Some(ok),
                    body.clone(),
                    &[("origin", "https://evil.example")],
                )
                .await,
            ];
            for (status, raw) in refused {
                assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
                assert!(!String::from_utf8_lossy(&raw).contains("records"));
            }
            // The network helper's marker never opens it, even with the right secret.
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .header("host", "127.0.0.1")
                        .method("POST")
                        .uri(path)
                        .header("authorization", format!("Bearer {CONTROL}"))
                        .extension(ConnectInfo(ok.parse::<SocketAddr>().unwrap()))
                        .extension(NetworkIngress {
                            peer_ip: "100.64.0.9".parse().unwrap(),
                        })
                        .body(Body::from(body.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            // Other methods stay closed.
            for method in ["GET", "PUT", "DELETE"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .header("host", "127.0.0.1")
                            .method(method)
                            .uri(path)
                            .header("authorization", format!("Bearer {CONTROL}"))
                            .extension(ConnectInfo(ok.parse::<SocketAddr>().unwrap()))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(!response.status().is_success(), "{method} {path}");
            }
        }
        // A Friends service serves neither page, with or without the secret.
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
                local_control_token: Some(CONTROL.into()),
                ..Default::default()
            },
        )
        .unwrap();
        let friends_app = router(friends.clone());
        for path in [model::LOCAL_PAGE_PATH, SESSION_PATH] {
            assert_eq!(
                send(
                    &friends_app,
                    path,
                    Some(CONTROL),
                    Some(ok),
                    wanted(&friends, None),
                    &[]
                )
                .await
                .0,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
    }
}
