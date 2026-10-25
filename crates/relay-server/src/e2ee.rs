//! Bounded, permission-scoped delivery of opaque MLS protocol messages.
//! This server never has MLS private state or conversation keys. Client-side
//! credential verification, consent and commit validation remain mandatory.
use super::*;
use std::collections::HashSet;

pub const MAX_BATCH_JSON_BYTES: usize = relay_protocol::CRYPTO_MAX_OPERATION_JSON_BYTES;
const MAX_KEY_PACKAGE: usize = 16 * 1024;
const MAX_EVENT_BODY: usize = relay_protocol::CRYPTO_MAX_EVENT_DECODED_BYTES;
const MAX_BATCH_DECODED: usize = relay_protocol::CRYPTO_MAX_OPERATION_DECODED_BYTES;
const MAX_CONTEXT_BODIES: usize = 2 * 1024 * 1024;
const MAX_ROSTER: usize = 64;
const MAX_DEVICES: u32 = 4;
const MAX_UNUSED_PACKAGES: u32 = 32;
#[path = "e2ee/call_control.rs"]
mod call_control;
pub use call_control::{ack_call_request, call_inbox, call_requests, send_call_request};

fn conflict(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, message)
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'.'))
}

fn decoded(value: &str, maximum: usize) -> ApiResult<Vec<u8>> {
    if value.len() > maximum.div_ceil(3) * 4 {
        return Err(bad("Encrypted protocol body exceeds its limit"));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| bad("Expected canonical base64 protocol bytes"))?;
    if bytes.is_empty() || bytes.len() > maximum || STANDARD.encode(&bytes) != value {
        return Err(bad(
            "Expected nonempty bounded canonical base64 protocol bytes",
        ));
    }
    Ok(bytes)
}

pub fn extra_storage_usage(db: &Connection, user: &str) -> rusqlite::Result<(u64, u64)> {
    db.query_row("SELECT
        (SELECT COALESCE(SUM(length(device_id)+length(identity)+length(key_package)),0) FROM crypto_devices)+
        (SELECT COALESCE(SUM(length(package_hash)+length(key_package)),0) FROM crypto_key_packages)+
        (SELECT COALESCE(SUM(length(body)+length(operation_id)+128),0) FROM crypto_events)+
        (SELECT COALESCE(SUM(length(operation_id)+length(request_hash)),0) FROM crypto_operations)+
        (SELECT COUNT(*)*160 FROM crypto_welcome_packages)+(SELECT COUNT(*)*160 FROM crypto_key_package_consumptions)+
        (SELECT COALESCE(SUM(length(request_json)+160),0) FROM crypto_call_requests),
        (SELECT COALESCE(SUM(length(device_id)+length(identity)+length(key_package)),0) FROM crypto_devices WHERE user_id=?1)+
        (SELECT COALESCE(SUM(length(p.package_hash)+length(p.key_package)),0) FROM crypto_key_packages p JOIN crypto_devices d ON d.device_id=p.device_id WHERE d.user_id=?1)+
        (SELECT COALESCE(SUM(length(body)+length(operation_id)+128),0) FROM crypto_events WHERE author_user_id=?1)+
        (SELECT COALESCE(SUM(length(operation_id)+length(request_hash)),0) FROM crypto_operations WHERE author_user_id=?1)+
        (SELECT COUNT(*)*160 FROM crypto_welcome_packages p JOIN crypto_devices d ON d.device_id=p.device_id WHERE d.user_id=?1)+
        (SELECT COUNT(*)*160 FROM crypto_key_package_consumptions p JOIN crypto_devices d ON d.device_id=p.device_id WHERE d.user_id=?1)+
        (SELECT COALESCE(SUM(length(request_json)+160),0) FROM crypto_call_requests WHERE author_user_id=?1)", [user], |r| Ok((r.get(0)?,r.get(1)?)))
}

fn check_quota(state: &AppState, db: &Connection, user: &str) -> ApiResult<()> {
    let (total, owned) = profiles::storage_usage(db, user)?;
    if total > state.0.config.storage_limit_bytes || owned > state.0.config.user_storage_limit_bytes
    {
        return Err(ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "Encrypted protocol storage quota reached",
        ));
    }
    Ok(())
}

pub(crate) fn device(db: &Connection, user: &str, device_id: &str) -> ApiResult<CryptoDevice> {
    if !identifier(device_id) {
        return Err(bad("Invalid device ID"));
    }
    db.query_row("SELECT user_id,device_id,identity,key_package FROM crypto_devices WHERE device_id=?1 AND user_id=?2 AND active=1",
        params![device_id,user], row_device).optional()?.ok_or_else(forbidden)
}

fn row_device(row: &rusqlite::Row<'_>) -> rusqlite::Result<CryptoDevice> {
    Ok(CryptoDevice {
        user_id: row.get(0)?,
        device_id: row.get(1)?,
        identity: row.get(2)?,
        key_package: row.get(3)?,
        invite_attestation: None,
    })
}

pub async fn own_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<CryptoDevice>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let mut statement = db.prepare("SELECT user_id,device_id,identity,key_package FROM crypto_devices WHERE user_id=?1 AND active=1 ORDER BY device_id")?;
    let devices = statement
        .query_map([auth.user.id], row_device)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(devices))
}

/// Public community device identities. No target, reservations, conversation
/// membership, private state or group logs are exposed by this directory.
pub async fn directory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<CryptoDevice>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let mut statement = db.prepare("SELECT d.user_id,d.device_id,d.identity,d.key_package,a.invite_id,a.mac FROM crypto_devices d JOIN membership m ON m.user_id=d.user_id LEFT JOIN device_attestations a ON a.device_id=d.device_id WHERE d.active=1 AND m.status='active' AND (?1=0 OR d.user_id=?2 OR EXISTS(SELECT 1 FROM friendships f WHERE f.accepted=1 AND ((f.sender=?2 AND f.recipient=d.user_id) OR (f.recipient=?2 AND f.sender=d.user_id)))) ORDER BY d.user_id,d.device_id LIMIT 257")?;
    let devices = statement
        .query_map(params![state.0.config.people, auth.user.id], |row| {
            let mut device = row_device(row)?;
            let invite_id: Option<String> = row.get(4)?;
            let mac: Option<String> = row.get(5)?;
            device.invite_attestation = invite_id
                .zip(mac)
                .map(|(invite_id, mac)| InviteAttestation { invite_id, mac });
            Ok(device)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if devices.len() > 256 {
        return Err(conflict("Community has more than 256 active encryption devices; use a scoped conversation context"));
    }
    Ok(Json(devices))
}

fn insert_packages(db: &Connection, device_id: &str, packages: &[String]) -> ApiResult<u32> {
    let mut accepted = 0;
    for package in packages {
        let bytes = decoded(package, MAX_KEY_PACKAGE)?;
        let hash = token_hash_bytes(&bytes);
        accepted += db.execute("INSERT OR IGNORE INTO crypto_key_packages(device_id,package_hash,key_package) VALUES(?1,?2,?3)",
            params![device_id,hash,package])? as u32;
    }
    if unused_packages(db, device_id)? > MAX_UNUSED_PACKAGES {
        return Err(conflict(
            "Device has 32 unused KeyPackages; reserve existing packages before publishing more",
        ));
    }
    Ok(accepted)
}

fn token_hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn unused_packages(db: &Connection, device_id: &str) -> rusqlite::Result<u32> {
    db.query_row(
        "SELECT COUNT(*) FROM crypto_key_packages p WHERE device_id=?1 AND target_kind IS NULL AND NOT EXISTS(SELECT 1 FROM crypto_key_package_consumptions c WHERE c.device_id=p.device_id AND c.package_hash=p.package_hash)",
        [device_id],
        |r| r.get(0),
    )
}

pub async fn register_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<RegisterCryptoDevice>,
) -> ApiResult<Json<CryptoDevice>> {
    let auth = state.auth(&headers)?;
    if !identifier(&input.device_id)
        || input.identity.len() != 64
        || !input.identity.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(bad(
            "Expected bounded device ID and 64 hexadecimal identity fingerprint",
        ));
    }
    decoded(&input.key_package, MAX_KEY_PACKAGE)?;
    input.identity.make_ascii_lowercase();
    let registered = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT user_id,identity FROM crypto_devices WHERE device_id=?1",
                [&input.device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((owner, identity)) = existing {
            if owner != auth.user.id {
                return Err(forbidden());
            }
            if identity != input.identity {
                return Err(conflict(
                    "This device ID is bound to another identity; explicitly enroll a new device",
                ));
            }
        }
        let active_count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM crypto_devices WHERE user_id=?1 AND active=1 AND device_id!=?2",
            params![auth.user.id, input.device_id],
            |r| r.get(0),
        )?;
        if active_count >= MAX_DEVICES {
            return Err(conflict(
                "An account may have at most four active encryption devices",
            ));
        }
        tx.execute("INSERT INTO crypto_devices(device_id,user_id,identity,key_package) VALUES(?1,?2,?3,?4) ON CONFLICT(device_id) DO UPDATE SET key_package=excluded.key_package,active=1",
            params![input.device_id,auth.user.id,input.identity,input.key_package])?;
        insert_packages(&tx, &input.device_id, &[input.key_package])?;
        check_quota(&state, &tx, &auth.user.id)?;
        let registered = device(&tx, &auth.user.id, &input.device_id)?;
        tx.commit()?;
        registered
    };
    state.notify("e2ee-device", None, Some(auth.user.id), None);
    Ok(Json(registered))
}

fn lowercase_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Stores the proof a member's device made with the secret of the invitation
/// they joined through. The server cannot compute or verify it (it never sees
/// the secret); it only checks that the caller owns the device and really
/// joined with this invitation, and keeps one proof per device.
pub async fn put_invite_attestation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PutInviteAttestation>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    if !identifier(&input.device_id)
        || !lowercase_hex_64(&input.invite_id)
        || !lowercase_hex_64(&input.mac)
    {
        return Err(bad(
            "Expected a device ID, a 64-character invitation ID and a 64-character lowercase hexadecimal proof",
        ));
    }
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        device(&tx, &auth.user.id, &input.device_id)?;
        let joined: Option<String> = tx
            .query_row(
                "SELECT invite_id FROM member_invitations WHERE user_id=?1",
                [&auth.user.id],
                |r| r.get(0),
            )
            .optional()?;
        if joined.as_deref() != Some(input.invite_id.as_str()) {
            return Err(forbidden());
        }
        let stored = tx.execute(
            "INSERT OR IGNORE INTO device_attestations(device_id,user_id,invite_id,mac,created) VALUES(?1,?2,?3,?4,?5)",
            params![input.device_id, auth.user.id, input.invite_id, input.mac, now()],
        )?;
        if stored != 1 {
            return Err(conflict("This device already has an invitation proof"));
        }
        tx.commit()?;
    }
    state.notify("e2ee-device", None, Some(auth.user.id), None);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn revoke_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    if !identifier(&device_id) {
        return Err(bad("Invalid device ID"));
    }
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let active: Option<bool> = tx
        .query_row(
            "SELECT active FROM crypto_devices WHERE device_id=?1 AND user_id=?2",
            params![device_id, auth.user.id],
            |row| row.get(0),
        )
        .optional()?;
    match active {
        // A lost acknowledgement must be retryable without reactivating a
        // tombstone. Ownership remains required for the idempotent response.
        Some(_) => {}
        None => return Err(forbidden()),
    }
    // Keep the identity and package ledger tombstoned. A deleted ID cannot be
    // reassigned to another signing identity, nor can packages be reused.
    tx.execute(
        "UPDATE crypto_devices SET active=0 WHERE device_id=?1 AND user_id=?2",
        params![device_id, auth.user.id],
    )?;
    // Release only opaque call slots; retain the identity/package tombstones.
    // Revocation and quota cleanup are one durable operation, including retry.
    tx.execute(
        "DELETE FROM crypto_call_requests WHERE author_device_id=?1",
        [&device_id],
    )?;
    tx.commit()?;
    // Serialize with voice_join's DB validation/insertion. Revoking a spare
    // device leaves the active device alone; revoking the selected device
    // invalidates its UDP ticket, frame cache and SFU admission immediately.
    private_calls::reconcile(&state, &db)?;
    let selected = lock(&state.0.voice).values().any(|session| {
        session.user.id == auth.user.id && session.device_id.as_deref() == Some(device_id.as_str())
    });
    if selected {
        state.remove_voice(&db, &auth.user.id, None)?;
    }
    drop(db);
    state.notify("e2ee-device", None, Some(auth.user.id), None);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn publish_packages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PublishCryptoKeyPackages>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    if input.packages.is_empty() || input.packages.len() > 16 {
        return Err(bad("Publish one to sixteen public KeyPackages"));
    }
    let mut total = 0;
    for package in &input.packages {
        total += decoded(package, MAX_KEY_PACKAGE)?.len();
    }
    if total > 256 * 1024 {
        return Err(bad("KeyPackage batch exceeds 256 KiB"));
    }
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    device(&tx, &auth.user.id, &input.device_id)?;
    let accepted = insert_packages(&tx, &input.device_id, &input.packages)?;
    check_quota(&state, &tx, &auth.user.id)?;
    let unused = unused_packages(&tx, &input.device_id)?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"accepted":accepted,"unused":unused}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageStatusQuery {
    device_id: String,
}

pub async fn package_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(input): Query<PackageStatusQuery>,
) -> ApiResult<Json<CryptoKeyPackageStatus>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    device(&db, &auth.user.id, &input.device_id)?;
    Ok(Json(CryptoKeyPackageStatus {
        unused:unused_packages(&db,&input.device_id)?,
        reserved:db.query_row("SELECT COUNT(*) FROM crypto_key_packages WHERE device_id=?1 AND target_kind IS NOT NULL",[&input.device_id],|row|row.get(0))?,
        consumed:db.query_row("SELECT COUNT(*) FROM crypto_key_package_consumptions WHERE device_id=?1",[&input.device_id],|row|row.get(0))?,
        device_id:input.device_id,
    }))
}

pub async fn consume_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ConsumeCryptoKeyPackage>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    if input.package_hash.len() != 64
        || !input
            .package_hash
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
        || input.welcome_sequence == 0
        || input.welcome_sequence > i64::MAX as u64
    {
        return Err(bad("Invalid consumed KeyPackage hash or Welcome sequence"));
    }
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    device(&tx, &auth.user.id, &input.device_id)?;
    let (kind, id, _) = roster(&state, &tx, &auth.user.id, &input.target)?;
    let group_id: String = tx
        .query_row(
            "SELECT group_id FROM crypto_groups WHERE target_kind=?1 AND target_id=?2",
            params![kind, id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| conflict("No addressed Welcome exists for this conversation"))?;
    let published:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM crypto_welcome_packages p JOIN crypto_events e ON e.group_id=p.group_id AND e.sequence=p.sequence WHERE p.group_id=?1 AND p.sequence=?2 AND p.device_id=?3 AND p.package_hash=?4 AND e.kind='welcome' AND e.recipient_device_id=?3)",params![group_id,input.welcome_sequence,input.device_id,input.package_hash],|row|row.get(0))?;
    if !published {
        return Err(conflict(
            "The consumed package does not match its addressed Welcome",
        ));
    }
    let receipt:Option<(String,String,String,u64)>=tx.query_row("SELECT target_kind,target_id,group_id,welcome_sequence FROM crypto_key_package_consumptions WHERE device_id=?1 AND package_hash=?2",params![input.device_id,input.package_hash],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    if let Some(receipt) = receipt {
        if receipt != (kind, id, group_id, input.welcome_sequence) {
            return Err(conflict("This package was consumed for another Welcome"));
        }
        return Ok(StatusCode::NO_CONTENT);
    }
    let assigned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM crypto_key_packages WHERE device_id=?1 AND package_hash=?2 AND target_kind=?3 AND target_id=?4)",params![input.device_id,input.package_hash,kind,id],|row|row.get(0))?;
    if !assigned {
        return Err(conflict(
            "The package is not reserved for this conversation",
        ));
    }
    tx.execute(
        "INSERT INTO crypto_key_package_consumptions VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            input.device_id,
            input.package_hash,
            group_id,
            input.welcome_sequence,
            kind,
            id
        ],
    )?;
    // Clearing the reservation frees its unique target slot. The durable
    // consumption ledger permanently excludes these bytes from unused stock.
    tx.execute("UPDATE crypto_key_packages SET target_kind=NULL,target_id=NULL WHERE device_id=?1 AND package_hash=?2",params![input.device_id,input.package_hash])?;
    check_quota(&state, &tx, &auth.user.id)?;
    tx.commit()?;
    let audience = input
        .target
        .peer_id
        .as_ref()
        .map(|peer| vec![auth.user.id.clone(), peer.clone()]);
    state.notify(
        "e2ee",
        input.target.channel_id,
        Some(auth.user.id),
        audience,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Returns the current permission roster, never a cached historic roster.
pub(crate) fn roster(
    state: &AppState,
    db: &Connection,
    user: &str,
    target: &CryptoTarget,
) -> ApiResult<(String, String, Vec<User>)> {
    active_user(db, user)?;
    match (&target.channel_id, &target.peer_id) {
        (Some(channel), None) => {
            let channel_info = get_channel(db, channel)?;
            let permission = match channel_info.kind.as_str() {
                "text" => READ,
                "voice" => JOIN,
                _ => return Err(bad("Unsupported encrypted channel kind")),
            };
            channel_access(db, channel, user, permission)?;
            if channel_info.kind == "text" {
                if let Some(group) = conversations::info(db, channel)?.filter(|c| c.kind == "group")
                {
                    let mut result = vec![];
                    for member in &group.member_ids {
                        if channel_access(db, channel, member, permission).is_ok() {
                            result.push(get_user(db, member)?);
                        }
                    }
                    if result.len() > MAX_ROSTER {
                        return Err(conflict(
                            "Encrypted conversations support at most 64 authorized members",
                        ));
                    }
                    return Ok(("channel".into(), channel.clone(), result));
                }
            }
            let live_users: HashSet<String> = if channel_info.kind == "voice" {
                lock(&state.0.voice)
                    .values()
                    .filter(|session| session.channel_id == *channel && session.alive_at(now()))
                    .map(|session| session.user.id.clone())
                    .collect()
            } else {
                HashSet::new()
            };
            let mut statement = db.prepare("SELECT u.id,u.username,u.role,p.display_name,p.bio,p.version,p.avatar IS NOT NULL,p.appearance FROM users u LEFT JOIN user_profiles p ON p.user_id=u.id JOIN membership m ON m.user_id=u.id WHERE m.status='active' ORDER BY u.id")?;
            let mut result = vec![];
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let member = row_user(row)?;
                if channel_access(db, channel, &member.id, permission).is_ok()
                    && (channel_info.kind != "voice"
                        || member.id == user
                        || live_users.contains(&member.id))
                {
                    result.push(member);
                    if result.len() > MAX_ROSTER {
                        return Err(conflict(
                            "Encrypted conversations support at most 64 authorized members",
                        ));
                    }
                }
            }
            Ok(("channel".into(), channel.clone(), result))
        }
        (None, Some(peer)) if peer != user => {
            social::direct_access(db, user, peer)?;
            active_user(db, peer)?;
            let mut members = vec![get_user(db, user)?, get_user(db, peer)?];
            members.sort_by(|a, b| a.id.cmp(&b.id));
            Ok((
                "direct".into(),
                format!("{}:{}", members[0].id, members[1].id),
                members,
            ))
        }
        _ => Err(bad("Choose exactly one channel_id or another peer_id")),
    }
}

fn reserve_package(
    db: &Connection,
    device_id: &str,
    kind: &str,
    target: &str,
) -> ApiResult<String> {
    if let Some(package)=db.query_row("SELECT key_package FROM crypto_key_packages WHERE device_id=?1 AND target_kind=?2 AND target_id=?3",
        params![device_id,kind,target],|r|r.get::<_,String>(0)).optional()? { return Ok(package); }
    let next: Option<(String,String)> = db.query_row("SELECT package_hash,key_package FROM crypto_key_packages p WHERE device_id=?1 AND target_kind IS NULL AND NOT EXISTS(SELECT 1 FROM crypto_key_package_consumptions c WHERE c.device_id=p.device_id AND c.package_hash=p.package_hash) ORDER BY rowid LIMIT 1",[device_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    match next {
        Some((hash, package)) => {
            db.execute("UPDATE crypto_key_packages SET target_kind=?3,target_id=?4 WHERE device_id=?1 AND package_hash=?2 AND target_kind IS NULL",params![device_id,hash,kind,target])?;
            Ok(package)
        }
        None => Ok(String::new()),
    }
}

fn context_for(
    state: &AppState,
    db: &Connection,
    user: &str,
    target: &CryptoTarget,
    device_id: &str,
    after: u64,
) -> ApiResult<CryptoContext> {
    if after > i64::MAX as u64 {
        return Err(bad("Invalid encryption event cursor"));
    }
    device(db, user, device_id)?;
    let (kind, id, roster) = roster(state, db, user, target)?;
    let voice_devices = selected_voice_devices(state, db, &kind, &id)?;
    let mut devices = vec![];
    for member in &roster {
        let members_devices = if let Some(selected) = &voice_devices {
            let active_device = match selected.get(&member.id) {
                Some(device) => device.as_deref(),
                None if member.id == user => Some(device_id),
                None => None,
            };
            if member.id == user && active_device.is_some_and(|active| active != device_id) {
                return Err(forbidden());
            }
            if member.id == user && selected.contains_key(user) && active_device.is_none() {
                return Err(forbidden());
            }
            match active_device {
                Some(id) => vec![device(db, &member.id, id)?],
                None => vec![], // Legacy callers never acquire protected call keys.
            }
        } else {
            let mut statement = db.prepare("SELECT user_id,device_id,identity,key_package FROM crypto_devices WHERE user_id=?1 AND active=1 ORDER BY device_id")?;
            let result = statement
                .query_map([&member.id], row_device)?
                .collect::<Result<Vec<_>, _>>()?;
            result
        };
        for mut device in members_devices {
            device.key_package = reserve_package(db, &device.device_id, &kind, &id)?;
            devices.push(device);
        }
    }
    let group: Option<(String, u64)> = db
        .query_row(
            "SELECT group_id,sequence FROM crypto_groups WHERE target_kind=?1 AND target_id=?2",
            params![kind, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut events = vec![];
    if let Some((group_id, _)) = &group {
        let mut statement = db.prepare("SELECT sequence,group_id,epoch,kind,author_user_id,author_device_id,recipient_device_id,body,operation_id FROM crypto_events WHERE group_id=?1 AND sequence>?2 AND (kind!='welcome' OR recipient_device_id=?3) ORDER BY sequence LIMIT 128")?;
        let mut rows = statement.query(params![group_id, after, device_id])?;
        let mut encoded_size = 0;
        while let Some(row) = rows.next()? {
            let body: String = row.get(7)?;
            if encoded_size + body.len() > MAX_CONTEXT_BODIES {
                break;
            }
            encoded_size += body.len();
            events.push(CryptoEvent {
                sequence: row.get(0)?,
                group_id: row.get(1)?,
                epoch: row.get(2)?,
                kind: row.get(3)?,
                author_user_id: row.get(4)?,
                author_device_id: row.get(5)?,
                recipient_device_id: row.get(6)?,
                body,
                operation_id: row.get(8)?,
            });
        }
    }
    let (group_id, sequence) = group.map_or((None, 0), |(id, sequence)| (Some(id), sequence));
    Ok(CryptoContext {
        server_id: server_id(db)?,
        target: target.clone(),
        roster,
        devices,
        group_id,
        sequence,
        events,
    })
}

fn selected_voice_devices(
    state: &AppState,
    db: &Connection,
    kind: &str,
    target_id: &str,
) -> ApiResult<Option<HashMap<String, Option<String>>>> {
    if kind != "channel" || get_channel(db, target_id)?.kind != "voice" {
        return Ok(None);
    }
    Ok(Some(
        lock(&state.0.voice)
            .values()
            .filter(|session| session.channel_id == target_id && session.alive_at(now()))
            .map(|session| (session.user.id.clone(), session.device_id.clone()))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextQuery {
    channel_id: Option<String>,
    peer_id: Option<String>,
    device_id: String,
    #[serde(default)]
    after: u64,
}

pub async fn context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ContextQuery>,
) -> ApiResult<Json<CryptoContext>> {
    let auth = state.auth(&headers)?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let context = context_for(
        &state,
        &tx,
        &auth.user.id,
        &CryptoTarget {
            channel_id: query.channel_id,
            peer_id: query.peer_id,
        },
        &query.device_id,
        query.after,
    )?;
    tx.commit()?;
    Ok(Json(context))
}

fn validate_events(input: &AppendCryptoEvents, initial: bool) -> ApiResult<()> {
    if !identifier(&input.group_id)
        || !identifier(&input.operation_id)
        || !identifier(&input.device_id)
        || input.events.is_empty()
        || input.events.len() > 66
        || input.expected_sequence > i64::MAX as u64
        || input.epoch > i64::MAX as u64
    {
        return Err(bad(
            "Invalid encrypted operation identifiers, counters or event count",
        ));
    }
    let mut commits = 0;
    let mut creates = 0;
    let mut total = 0;
    let mut recipients = HashSet::new();
    for (index, event) in input.events.iter().enumerate() {
        match event.kind.as_str() {
            "create"
                if initial
                    && index == 0
                    && event.body.is_empty()
                    && event.recipient_device_id.is_none() =>
            {
                creates += 1
            }
            "commit" if index == usize::from(initial) && event.recipient_device_id.is_none() => {
                commits += 1;
                total += decoded(&event.body, MAX_EVENT_BODY)?.len();
            }
            "welcome" if index > usize::from(initial) => {
                let recipient = event
                    .recipient_device_id
                    .as_ref()
                    .filter(|id| identifier(id))
                    .ok_or_else(|| bad("Welcome requires one bounded recipient device"))?;
                if !recipients.insert(recipient) {
                    return Err(bad("Duplicate Welcome recipient"));
                }
                total += decoded(&event.body, MAX_EVENT_BODY)?.len();
            }
            _ => {
                return Err(bad(
                    "Expected optional initial create, exactly one commit, then recipient Welcomes",
                ))
            }
        }
        if total > MAX_BATCH_DECODED {
            return Err(bad("Encrypted operation exceeds 1 MiB decoded"));
        }
    }
    if commits != 1 || creates != usize::from(initial) || recipients.len() > 64 {
        return Err(bad("Expected exactly one commit and at most 64 Welcomes"));
    }
    Ok(())
}

pub async fn append_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AppendCryptoEvents>,
) -> ApiResult<Json<CryptoContext>> {
    let auth = state.auth(&headers)?;
    let request_hash = token_hash(&serde_json::to_string(&input)?);
    let context = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        device(&tx, &auth.user.id, &input.device_id)?;
        let (kind, target_id, current_roster) = roster(&state, &tx, &auth.user.id, &input.target)?;
        let selected = selected_voice_devices(&state, &tx, &kind, &target_id)?;
        if selected
            .as_ref()
            .and_then(|devices| devices.get(&auth.user.id))
            .is_some_and(|device| device.as_deref() != Some(input.device_id.as_str()))
        {
            return Err(forbidden());
        }
        let existing: Option<(String,u64,u64)> = tx.query_row("SELECT group_id,sequence,epoch FROM crypto_groups WHERE target_kind=?1 AND target_id=?2",params![kind,target_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let initial = existing.is_none();
        let initial_batch = input
            .events
            .first()
            .is_some_and(|event| event.kind == "create");
        validate_events(&input, initial_batch)?;
        if let Some((group_id, sequence, epoch)) = &existing {
            if group_id != &input.group_id {
                return Err(conflict(
                    "This conversation already has another encryption group",
                ));
            }
            let receipt: Option<(String,String,String)> = tx.query_row("SELECT author_user_id,author_device_id,request_hash FROM crypto_operations WHERE group_id=?1 AND operation_id=?2",params![input.group_id,input.operation_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((user, device, hash)) = receipt {
                if user != auth.user.id || device != input.device_id || hash != request_hash {
                    return Err(conflict(
                        "Encryption operation ID already used for another batch",
                    ));
                }
                let context = context_for(
                    &state,
                    &tx,
                    &auth.user.id,
                    &input.target,
                    &input.device_id,
                    input.expected_sequence,
                )?;
                tx.commit()?;
                return Ok(Json(context));
            }
            if *sequence != input.expected_sequence || epoch.checked_add(1) != Some(input.epoch) {
                return Err(conflict(
                    "Encryption sequence or epoch changed; refresh before committing",
                ));
            }
        } else {
            if input.expected_sequence != 0 || input.epoch > 1 {
                return Err(conflict(
                    "Initial encryption group must start at sequence zero and epoch zero or one",
                ));
            }
            let duplicate: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM crypto_groups WHERE group_id=?1)",
                [&input.group_id],
                |r| r.get(0),
            )?;
            if duplicate {
                return Err(conflict(
                    "Encryption group ID already belongs to another conversation",
                ));
            }
            tx.execute(
                "INSERT INTO crypto_groups(target_kind,target_id,group_id) VALUES(?1,?2,?3)",
                params![kind, target_id, input.group_id],
            )?;
        }
        if initial_batch != initial {
            return Err(conflict(
                "Create is required only for the initial encryption group claim",
            ));
        }
        let mut welcome_hashes = HashMap::new();
        for event in &input.events {
            if let Some(recipient) = &event.recipient_device_id {
                let owner: Option<String> = tx
                    .query_row(
                        "SELECT user_id FROM crypto_devices WHERE device_id=?1 AND active=1",
                        [recipient],
                        |r| r.get(0),
                    )
                    .optional()?;
                let owner = owner
                    .filter(|owner| current_roster.iter().any(|u| u.id == *owner))
                    .ok_or_else(forbidden)?;
                if let Some(selected) = &selected {
                    let expected = match selected.get(&owner) {
                        Some(device) => device.as_deref(),
                        None if owner == auth.user.id => Some(input.device_id.as_str()),
                        None => None,
                    };
                    if expected != Some(recipient.as_str()) {
                        return Err(forbidden());
                    }
                }
                let package = reserve_package(&tx, recipient, &kind, &target_id)?;
                if package.is_empty() {
                    return Err(conflict(
                        "A Welcome recipient has no reserved KeyPackage; refresh device pool",
                    ));
                }
                let hash = token_hash_bytes(&decoded(&package, MAX_KEY_PACKAGE)?);
                let already_published:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM crypto_welcome_packages WHERE device_id=?1 AND package_hash=?2)",params![recipient,hash],|row|row.get(0))?;
                if already_published {
                    return Err(conflict("This one-use package already has a Welcome; wait for its consumption acknowledgement"));
                }
                welcome_hashes.insert(recipient.clone(), hash);
            }
        }
        let mut sequence = input.expected_sequence;
        for event in &input.events {
            sequence = sequence
                .checked_add(1)
                .filter(|s| *s <= i64::MAX as u64)
                .ok_or_else(|| conflict("Encryption sequence exhausted"))?;
            tx.execute(
                "INSERT INTO crypto_events VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    input.group_id,
                    sequence,
                    input.epoch,
                    event.kind,
                    auth.user.id,
                    input.device_id,
                    event.recipient_device_id,
                    event.body,
                    input.operation_id
                ],
            )?;
            if let Some(recipient) = &event.recipient_device_id {
                let hash = welcome_hashes
                    .get(recipient)
                    .expect("validated Welcome package");
                tx.execute(
                    "INSERT INTO crypto_welcome_packages VALUES(?1,?2,?3,?4)",
                    params![input.group_id, sequence, recipient, hash],
                )?;
            }
        }
        tx.execute(
            "UPDATE crypto_groups SET sequence=?2,epoch=?3 WHERE group_id=?1",
            params![input.group_id, sequence, input.epoch],
        )?;
        tx.execute(
            "INSERT INTO crypto_operations VALUES(?1,?2,?3,?4,?5)",
            params![
                input.group_id,
                input.operation_id,
                auth.user.id,
                input.device_id,
                request_hash
            ],
        )?;
        check_quota(&state, &tx, &auth.user.id)?;
        let result = context_for(
            &state,
            &tx,
            &auth.user.id,
            &input.target,
            &input.device_id,
            input.expected_sequence,
        )?;
        tx.commit()?;
        result
    };
    let audience = input
        .target
        .peer_id
        .as_ref()
        .map(|peer| vec![auth.user.id.clone(), peer.clone()]);
    state.notify(
        "e2ee",
        input.target.channel_id,
        Some(auth.user.id),
        audience,
    );
    Ok(Json(context))
}

/// Only this tagged content gets a larger message envelope budget. The server
/// does not parse or decrypt its MLS payload, and never claims it is trusted.
pub fn valid_message_content(content: &str) -> bool {
    match content.strip_prefix("relay-e2ee:v1:") {
        Some(body) => decoded(body, 48 * 1024).is_ok(),
        None => content.len() <= 10000,
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
#[path = "e2ee/voice_tests.rs"]
mod voice_tests;
