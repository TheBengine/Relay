//! Administrator-owned file allowances. Client limits are hints; the current
//! database policy and memberships decide both body admission and commit.
use super::*;

pub(super) fn current(db: &Connection) -> ApiResult<UploadPolicy> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM metadata WHERE key='upload_policy'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let policy: UploadPolicy = match raw {
        Some(raw) => serde_json::from_str(&raw).map_err(|_| bad("Upload settings are damaged"))?,
        None => UploadPolicy::default(),
    };
    if !policy.valid() {
        return Err(bad("Upload settings are damaged"));
    }
    Ok(policy)
}

fn owner(db: &Connection, user: &str) -> ApiResult<()> {
    active_user(db, user)?;
    if get_user(db, user)?.role != "admin" {
        return Err(forbidden());
    }
    Ok(())
}

pub(super) fn for_user(db: &Connection, user: &str) -> ApiResult<UploadLimits> {
    active_user(db, user)?;
    let policy = current(db)?;
    let mut q = db.prepare("SELECT r.id FROM community_roles r JOIN member_roles m ON m.role_id=r.id WHERE m.user_id=?1")?;
    let roles = q
        .query_map([user], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(policy.for_roles(
        roles.iter().map(String::as_str),
        get_user(db, user)?.role == "admin",
    ))
}

pub(super) async fn get_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<UploadPolicy>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    owner(&db, &auth.user.id)?;
    let mut policy = current(&db)?;
    // Deleted roles have no authority and need not block the next settings edit.
    let existing: std::collections::HashSet<_> =
        roles::list(&db)?.into_iter().map(|r| r.id).collect();
    policy.role_limits.retain(|role, _| existing.contains(role));
    Ok(Json(policy))
}

pub(super) async fn put_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<UploadPolicy>,
) -> ApiResult<Json<UploadPolicy>> {
    let auth = state.auth(&headers)?;
    if !input.valid() {
        return Err(bad("Choose upload limits from 0 to 25 MiB, with a default no larger than the server maximum"));
    }
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        owner(&tx, &auth.user.id)?;
        let previous = current(&tx)?;
        if input.revision != previous.revision {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Upload settings changed. Reload the current settings before saving.",
            ));
        }
        for role in input.role_limits.keys() {
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM community_roles WHERE id=?1)",
                [role],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(bad(
                    "An upload role no longer exists. Reload the role list.",
                ));
            }
        }
        input.revision = input
            .revision
            .checked_add(1)
            .ok_or_else(|| bad("Upload settings revision is exhausted"))?;
        let raw = serde_json::to_string(&input).map_err(|_| bad("Invalid upload settings"))?;
        tx.execute("INSERT INTO metadata(key,value) VALUES('upload_policy',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [raw])?;
        roles::audit(
            &tx,
            &auth.user.id,
            "uploads.policy.updated",
            "upload_policy",
        )?;
        tx.commit()?;
    }
    state.notify("channels", None, None, None);
    Ok(Json(input))
}

pub(super) fn input_valid(name: &str, mime: &str) -> bool {
    !name.trim().is_empty()
        && name.trim().len() <= 255
        && !name.chars().any(char::is_control)
        && mime.len() <= 128
        && !mime.chars().any(char::is_control)
}

pub(super) fn too_large(_limit: u64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "This file exceeds your account's current upload limit, including encryption overhead",
    )
}

/// Called after reading; never relies on the earlier admission decision.
pub(super) fn commit(
    state: &AppState,
    user: &str,
    name: &str,
    mime: &str,
    bytes: &[u8],
) -> ApiResult<Attachment> {
    if !input_valid(name, mime) || bytes.is_empty() {
        return Err(bad("Invalid filename, MIME type or empty file"));
    }
    let mut db = lock(&state.0.db);
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let limits = for_user(&tx, user)?;
    if limits.effective() == 0 {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "File uploads are disabled for your roles",
        ));
    }
    if bytes.len() as u64 > limits.effective() {
        return Err(too_large(limits.effective()));
    }
    let pending: u64 = tx.query_row(
        "SELECT COALESCE(SUM(size),0) FROM uploads WHERE owner_id=?1 AND message_id IS NULL",
        [user],
        |r| r.get(0),
    )?;
    let (total, user_total) = profiles::storage_usage(&tx, user)?;
    let size = bytes.len() as u64;
    if total.saturating_add(size) > state.0.config.storage_limit_bytes
        || user_total.saturating_add(size) > state.0.config.user_storage_limit_bytes
    {
        return Err(ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "Attachment storage quota reached; remove files or ask the host to raise the limit",
        ));
    }
    if pending.saturating_add(size) > MAX_PENDING_UPLOAD_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Send existing pending files before uploading more (50 MiB pending quota)",
        ));
    }
    let attachment = Attachment {
        id: id(),
        name: name.trim().into(),
        mime: mime.into(),
        size,
        url: String::new(),
    };
    tx.execute(
        "INSERT INTO uploads VALUES(?1,?2,NULL,?3,?4,?5,?6,?7)",
        params![
            attachment.id,
            user,
            attachment.name,
            attachment.mime,
            size,
            bytes,
            now()
        ],
    )?;
    tx.commit()?;
    Ok(Attachment {
        url: format!("/api/uploads/{}", attachment.id),
        ..attachment
    })
}

#[cfg(test)]
mod tests;
