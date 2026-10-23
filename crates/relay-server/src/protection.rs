//! Community protection mode and server-managed media keys for accepted open calls.
//! Protected content remains opaque; in open mode the host has access to media keys.
//! Each client must accept the exact announced revision before using open media.
use super::*;

pub(super) const MAX_OPEN_CALL_PEERS: usize = 32;

pub(super) fn open_device(token_hash: &str) -> String {
    format!("open-{}", &token_hash[..32])
}

#[derive(serde::Serialize, Deserialize)]
struct OpenEpoch {
    fingerprint: String,
    epoch: u64,
}

/// Server-managed media protection: the host has these keys. This is not E2EE.
/// Every request must name the accepted current revision and a live voice seat.
pub(super) async fn open_call(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<VoiceJoin>,
) -> ApiResult<Json<OpenCallMaterial>> {
    use hmac::Mac;
    let auth = state.auth(&headers)?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let protection = current(&tx)?;
    if protection.level != ProtectionLevel::Open || input.open_revision != Some(protection.revision)
    {
        return Err(forbidden());
    }
    channel_access(&tx, &input.channel_id, &auth.user.id, JOIN)?;
    let mut peers: Vec<_> = lock(&state.0.voice)
        .values()
        .filter(|v| v.channel_id == input.channel_id && v.alive_at(now()))
        .map(|v| (v.user.id.clone(), v.token_hash.clone()))
        .collect();
    peers.sort();
    peers.dedup();
    if peers.len() > MAX_OPEN_CALL_PEERS
        || !peers
            .iter()
            .any(|(user, token)| user == &auth.user.id && token == &auth.token_hash)
    {
        return Err(forbidden());
    }
    let server = server_id(&tx)?;
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(&state.0.instance_id, protection.revision, &peers))
                .map_err(|_| bad("Invalid call context"))?
        )
    );
    let epoch_key = format!("open_call_epoch:{}", input.channel_id);
    let previous: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [&epoch_key],
            |r| r.get(0),
        )
        .optional()?;
    let previous: Option<OpenEpoch> = previous
        .map(|v| serde_json::from_str(&v))
        .transpose()
        .map_err(|_| bad("Open call state is damaged"))?;
    let epoch = match previous {
        Some(previous) if previous.fingerprint == fingerprint => previous.epoch,
        previous => previous
            .map_or(0, |p| p.epoch)
            .checked_add(1)
            .ok_or_else(|| bad("Call generation limit reached"))?,
    };
    tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![epoch_key,serde_json::to_string(&OpenEpoch{fingerprint,epoch}).map_err(|_|bad("Invalid call context"))?])?;
    let secret: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key='open_call_secret'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let secret = match secret {
        Some(secret) => secret,
        None => {
            let secret = random_token();
            tx.execute(
                "INSERT INTO metadata(key,value) VALUES('open_call_secret',?1)",
                [&secret],
            )?;
            secret
        }
    };
    let secret = URL_SAFE_NO_PAD
        .decode(secret)
        .map_err(|_| bad("Open call secret is damaged"))?;
    if secret.len() != 32 {
        return Err(bad("Open call secret is damaged"));
    }
    let derive = |user: &str, device: &str, kind: &str| -> ApiResult<String> {
        let context = serde_json::to_vec(&(
            "relay-server-managed-call-v1",
            &server,
            &input.channel_id,
            protection.revision,
            epoch,
            user,
            device,
            kind,
        ))
        .map_err(|_| bad("Invalid call context"))?;
        let mut mac = <hmac::Hmac<Sha256> as Mac>::new_from_slice(&secret)
            .map_err(|_| bad("Invalid call secret"))?;
        mac.update(&context);
        Ok(STANDARD.encode(mac.finalize().into_bytes()))
    };
    let mut senders = Vec::new();
    for (user, token) in peers {
        let device = open_device(&token);
        senders.push(OpenCallSender {
            opus: derive(&user, &device, "opus")?,
            rtc: derive(&user, &device, "rtc")?,
            user_id: user,
            device_id: device,
        });
    }
    tx.commit()?;
    Ok(Json(OpenCallMaterial {
        revision: protection.revision,
        epoch,
        group_id: format!("server-managed:{server}/{}", input.channel_id),
        senders,
    }))
}
#[cfg(test)]
mod tests;

pub(super) fn current(db: &Connection) -> ApiResult<CommunityProtection> {
    let value: Option<String> = db
        .query_row(
            "SELECT value FROM metadata WHERE key='protection'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        Some(value) => {
            serde_json::from_str(&value).map_err(|_| bad("Protection settings are damaged"))
        }
        None => Ok(CommunityProtection::default()),
    }
}

/// Only an administrator may change the mode. The caller names the revision it
/// saw, so two administrators cannot overwrite each other unnoticed.
pub(super) async fn put_protection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<SetProtection>,
) -> ApiResult<Json<CommunityProtection>> {
    let auth = state.auth(&headers)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        active_user(&tx, &auth.user.id)?;
        if get_user(&tx, &auth.user.id)?.role != "admin" {
            return Err(forbidden());
        }
        let now = current(&tx)?;
        if now.revision != input.expected_revision {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "The protection setting changed. Refresh and try again.",
            ));
        }
        if now.level == input.level {
            return Ok(Json(now));
        }
        let next = CommunityProtection {
            level: input.level,
            revision: now.revision + 1,
            changed_by: Some(auth.user.id.clone()),
        };
        let json = serde_json::to_string(&next).map_err(|_| bad("Invalid protection setting"))?;
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES('protection',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [json],
        )?;
        roles::audit(
            &tx,
            &auth.user.id,
            "server.protection",
            match next.level {
                ProtectionLevel::Protected => "protected",
                ProtectionLevel::Open => "open",
            },
        )?;
        // Mode changes end old voice/video seats. No client is silently moved
        // from a protected call to one whose keys the server can read.
        let users: Vec<_> = lock(&state.0.voice)
            .values()
            .map(|v| v.user.id.clone())
            .collect();
        tx.execute("UPDATE private_call_recovery SET state='ended',retained_until=?1 WHERE state='accepted'", [super::now().saturating_add(30)])?;
        tx.execute("DELETE FROM voice_resume_leases", [])?;
        let saved_calls = private_calls::durable(&tx)?;
        tx.commit()?;
        // No runtime seat changes until the entire mode/end operation commits.
        private_calls::committed(&state, saved_calls);
        for user in users {
            state.remove_voice_runtime(&user, None);
        }
        next
    };
    // Every member's client re-reads the bootstrap, which carries the new mode.
    state.notify("channels", None, None, None);
    Ok(Json(result))
}
