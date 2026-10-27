//! A safe way back in for members who sign in with a community password.
//!
//! Password hashes never leave the host that set them, so after a move such a
//! member needs one of two codes:
//! * a one-time reset code an administrator creates for them (valid 24 hours), or
//! * a recovery code the member created in advance (valid until used or replaced).
//!
//! Only a SHA-256 digest of each code is stored. A code is 128 random bits, so the
//! digest cannot be guessed back, and unlike a password hash it is safe for the
//! code digests to travel with a community copy: that is what lets them work
//! after a move. Using a code sets a new password, ends every session of the
//! member and removes their second factor (it was sealed with the old password),
//! so they enrol it again. Administrators cannot create codes for each other.
use super::*;
use serde::Serialize;

/// Admin reset codes expire; recovery codes do not.
pub(crate) const RESET_LIFETIME: u64 = 24 * 60 * 60;
const KIND_ADMIN: &str = "admin";
const KIND_RECOVERY: &str = "recovery";

pub(crate) fn migrate(tx: &Connection) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS password_resets(
            user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            kind TEXT NOT NULL CHECK(kind IN('admin','recovery')),
            code_hash TEXT NOT NULL UNIQUE,
            expires INTEGER,
            created INTEGER NOT NULL,
            PRIMARY KEY(user_id,kind));",
    )
}

/// 16 random bytes as 32 lowercase hex characters in groups of four, easy to read aloud.
fn new_code() -> String {
    let mut raw = [0u8; 16];
    OsRng.fill_bytes(&mut raw);
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join("-")
}

/// Accepts the code with or without dashes and spaces, in any case.
fn normalized(code: &str) -> Option<String> {
    let plain: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (plain.len() == 32 && plain.bytes().all(|b| b.is_ascii_hexdigit())).then_some(plain)
}

fn digest(code: &str) -> Option<String> {
    normalized(code).map(|plain| {
        format!(
            "{:x}",
            Sha256::digest(format!("relay-password-reset-v1\0{plain}").as_bytes())
        )
    })
}

fn store(db: &Connection, user: &str, kind: &str, code: &str, expires: Option<u64>) -> ApiResult<()> {
    let hash = digest(code).ok_or_else(|| bad("Invalid code"))?;
    db.execute(
        "INSERT INTO password_resets(user_id,kind,code_hash,expires,created) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(user_id,kind) DO UPDATE SET code_hash=excluded.code_hash,expires=excluded.expires,created=excluded.created",
        params![user, kind, hash, expires, now()],
    )?;
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct IssuedCode {
    code: String,
    expires: Option<u64>,
}

/// An administrator creates a one-time reset code for a member.
pub(super) async fn issue_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user): Path<String>,
) -> ApiResult<Json<IssuedCode>> {
    let auth = state.auth(&headers)?;
    let code = new_code();
    let expires = now() + RESET_LIFETIME;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_ADMIN)?;
        active_user(&tx, &user)?;
        if user == auth.user.id {
            return Err(bad("Create a recovery code for yourself instead"));
        }
        if roles::has(&tx, &user, PERMISSION_ADMIN) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Administrators use their own recovery codes",
            ));
        }
        store(&tx, &user, KIND_ADMIN, &code, Some(expires))?;
        roles::audit(&tx, &auth.user.id, "password.reset_code_created", &user)?;
        tx.commit()?;
    }
    Ok(Json(IssuedCode {
        code,
        expires: Some(expires),
    }))
}

/// A signed-in member creates (or replaces) their own recovery code.
pub(super) async fn issue_recovery(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<IssuedCode>> {
    let auth = state.auth(&headers)?;
    let code = new_code();
    {
        let db = lock(&state.0.db);
        active_user(&db, &auth.user.id)?;
        store(&db, &auth.user.id, KIND_RECOVERY, &code, None)?;
        roles::audit(&db, &auth.user.id, "password.recovery_code_created", &auth.user.id)?;
    }
    Ok(Json(IssuedCode {
        code,
        expires: None,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Redeem {
    username: String,
    code: String,
    new_password: String,
}

/// Anyone with a valid code for this username sets a new password and gets a
/// fresh session. A wrong username, code or expired code gives one answer.
pub(super) async fn redeem(
    State(state): State<AppState>,
    Json(input): Json<Redeem>,
) -> ApiResult<Json<Session>> {
    if input.username.len() > 64 || input.code.len() > 64 {
        return Err(bad("Invalid reset request"));
    }
    if !(10..=256).contains(&input.new_password.len()) {
        return Err(bad("Password must be 10 to 256 bytes"));
    }
    let refused = || ApiError::new(StatusCode::UNAUTHORIZED, "This reset code is not valid");
    let hash = digest(&input.code).ok_or_else(refused)?;
    let user: User = {
        let db = lock(&state.0.db);
        let id: Option<String> = db
            .query_row(
                "SELECT u.id FROM password_resets r JOIN users u ON u.id=r.user_id
                 WHERE u.username=?1 COLLATE NOCASE AND r.code_hash=?2 AND (r.expires IS NULL OR r.expires>?3)",
                params![input.username.trim(), hash, now()],
                |r| r.get(0),
            )
            .optional()?;
        let id = id.ok_or_else(refused)?;
        get_user(&db, &id)?
    };
    password_policy::validate(&user.username, &input.new_password)?;
    let permit = state
        .0
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Sign-in service is busy"))?;
    let new_password = zeroize::Zeroizing::new(input.new_password);
    let sealing = state.clone();
    let sealed_for = user.id.clone();
    let updated = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        Argon2::default()
            .hash_password(new_password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map(|h| sealing.0.seal.seal_password(&sealed_for, &h.to_string()))
    })
    .await
    .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Password operation failed"))?
    .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Password hashing failed"))?;
    let session = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        // The code is spent in the same transaction; a second use finds nothing.
        if tx.execute(
            "DELETE FROM password_resets WHERE user_id=?1 AND code_hash=?2",
            params![user.id, hash],
        )? != 1
        {
            return Err(refused());
        }
        tx.execute(
            "UPDATE users SET password_hash=?2 WHERE id=?1",
            params![user.id, updated],
        )?;
        tx.execute("DELETE FROM sessions WHERE user_id=?1", [&user.id])?;
        tx.execute(
            "DELETE FROM authenticator_credentials WHERE user_id=?1",
            [&user.id],
        )?;
        roles::audit(&tx, &user.id, "password.reset_used", &user.id)?;
        let session = new_session(&tx, &state.0.seal, user.clone())?;
        tx.commit()?;
        session
    };
    state.remove_voice(&lock(&state.0.db), &user.id, None)?;
    Ok(Json(session))
}

#[cfg(test)]
mod tests;
