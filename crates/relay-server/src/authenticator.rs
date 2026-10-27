//! Optional RFC 6238 authentication. Seeds are AEAD sealed with a key derived
//! from the account password: database backups remain portable without a new
//! host-wide secret file. Recovery codes are high-entropy, hashed and single-use.
use super::*;
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use serde_json::{json, Value};
use totp_rs::{Algorithm, Builder, Totp};
use zeroize::Zeroizing;

const ATTEMPTS: u32 = 5;
const WINDOW: u64 = 60;
const SETUP_TTL: u64 = 600;

fn storage_error() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Authenticator storage is unavailable. Try again or use a recovery code.",
    )
}
fn code_error() -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, AUTHENTICATOR_INVALID)
}

pub(super) struct Record {
    sealed: Vec<u8>,
    salt: Vec<u8>,
    revision: String,
}
pub(super) struct Proof {
    revision: String,
    step: Option<u64>,
    recovery_hash: Option<String>,
}
pub(super) struct Rewrap {
    previous: String,
    sealed: Vec<u8>,
    salt: Vec<u8>,
}

pub(super) fn record(db: &Connection, user: &str) -> ApiResult<Option<Record>> {
    Ok(db
        .query_row(
            "SELECT sealed,salt,revision FROM authenticator_credentials WHERE user_id=?1",
            [user],
            |row| {
                Ok(Record {
                    sealed: row.get(0)?,
                    salt: row.get(1)?,
                    revision: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn key(password: &str, salt: &[u8]) -> ApiResult<Zeroizing<[u8; 32]>> {
    if salt.len() != 16 {
        return Err(storage_error());
    }
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|_| storage_error())?;
    Ok(key)
}

fn open(user: &str, password: &str, sealed: &[u8], salt: &[u8]) -> ApiResult<Zeroizing<Vec<u8>>> {
    if sealed.len() != 48 {
        return Err(storage_error());
    }
    let key = key(password, salt)?;
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| storage_error())?;
    let aad = format!("relay-authenticator-v1\0{user}");
    cipher
        .decrypt(
            Nonce::from_slice(&sealed[..12]),
            Payload {
                msg: &sealed[12..],
                aad: aad.as_bytes(),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| storage_error())
}

fn seal(user: &str, password: &str, secret: &[u8]) -> ApiResult<(Vec<u8>, Vec<u8>)> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = key(password, &salt)?;
    let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| storage_error())?;
    let aad = format!("relay-authenticator-v1\0{user}");
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: secret,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| storage_error())?;
    Ok((
        [nonce.as_slice(), encrypted.as_slice()].concat(),
        salt.to_vec(),
    ))
}

fn totp(secret: &[u8]) -> ApiResult<Totp> {
    Builder::new()
        .with_secret(secret.to_vec())
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_step_duration(30)
        .with_skew(1)
        .build()
        .map_err(|_| storage_error())
}

fn matched_step(secret: &[u8], code: &str, timestamp: u64) -> ApiResult<Option<u64>> {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    Ok(totp(secret)?.check(code, timestamp))
}

fn recovery_hash(code: &str) -> Option<String> {
    let normalized: String = code
        .chars()
        .filter(|c| *c != '-' && *c != ' ')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (normalized.len() == 32 && normalized.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| token_hash(&format!("relay-recovery-v1\0{normalized}")))
}

fn recovery_codes(db: &Connection, user: &str) -> ApiResult<Vec<String>> {
    db.execute(
        "DELETE FROM authenticator_recovery WHERE user_id=?1",
        [user],
    )?;
    let mut result = Vec::with_capacity(10);
    for _ in 0..10 {
        let mut random = [0u8; 16];
        OsRng.fill_bytes(&mut random);
        let raw: String = random.iter().map(|b| format!("{b:02X}")).collect();
        let code = format!(
            "{}-{}-{}-{}",
            &raw[..8],
            &raw[8..16],
            &raw[16..24],
            &raw[24..]
        );
        db.execute(
            "INSERT INTO authenticator_recovery(user_id,code_hash) VALUES(?1,?2)",
            params![user, recovery_hash(&code).ok_or_else(storage_error)?],
        )?;
        result.push(code);
    }
    Ok(result)
}

/// Password verification has already succeeded. Reserve a bounded attempt
/// before expensive work; do not consume a code until the caller's transaction.
pub(super) async fn prepare(
    state: &AppState,
    user: &str,
    password: Zeroizing<String>,
    code: &str,
) -> ApiResult<Option<Proof>> {
    let row = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let Some(row) = record(&tx, user)? else {
            return Ok(None);
        };
        if code.is_empty() {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                AUTHENTICATOR_REQUIRED,
            ));
        }
        if code.len() > 64 {
            return Err(code_error());
        }
        let (attempts, start): (u32, u64) = tx.query_row(
            "SELECT attempts,window_start FROM authenticator_credentials WHERE user_id=?1",
            [user],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let timestamp = now();
        let fresh =
            timestamp.saturating_sub(start) >= WINDOW || start > timestamp.saturating_add(WINDOW);
        if !fresh && attempts >= ATTEMPTS {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many authenticator attempts. Wait a minute and try again.",
            ));
        }
        tx.execute(
            "UPDATE authenticator_credentials SET attempts=?2,window_start=?3 WHERE user_id=?1",
            params![
                user,
                if fresh { 1 } else { attempts + 1 },
                if fresh { timestamp } else { start }
            ],
        )?;
        tx.commit()?;
        row
    };
    let permit = state
        .0
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Sign-in service is busy. Try again shortly",
            )
        })?;
    let user = user.to_owned();
    let code = Zeroizing::new(code.to_owned());
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // Recovery remains available even if a seed cannot be decrypted; the
        // account password was independently verified and the code is random.
        if let Some(hash) = recovery_hash(&code) {
            return Ok(Some(Proof {
                revision: row.revision,
                step: None,
                recovery_hash: Some(hash),
            }));
        }
        let secret = open(&user, &password, &row.sealed, &row.salt)?;
        let step = matched_step(&secret, code.trim(), now())?.ok_or_else(code_error)?;
        Ok(Some(Proof {
            revision: row.revision,
            step: Some(step),
            recovery_hash: None,
        }))
    })
    .await
    .map_err(|_| storage_error())?
}

pub(super) fn consume(db: &Connection, user: &str, proof: Option<&Proof>) -> ApiResult<()> {
    let current = record(db, user)?;
    let (Some(current), Some(proof)) = (current, proof) else {
        if record(db, user)?.is_some() {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                AUTHENTICATOR_REQUIRED,
            ));
        }
        return if proof.is_none() {
            Ok(())
        } else {
            Err(code_error())
        };
    };
    if current.revision != proof.revision {
        return Err(code_error());
    }
    if let Some(step) = proof.step {
        if db.execute("UPDATE authenticator_credentials SET last_step=?2,attempts=0,window_start=?3 WHERE user_id=?1 AND last_step<?2",params![user,step,now()])? != 1 { return Err(code_error()); }
    } else if let Some(hash) = &proof.recovery_hash {
        if db.execute(
            "DELETE FROM authenticator_recovery WHERE user_id=?1 AND code_hash=?2",
            params![user, hash],
        )? != 1
        {
            return Err(code_error());
        }
        db.execute(
            "UPDATE authenticator_credentials SET attempts=0,window_start=?2 WHERE user_id=?1",
            params![user, now()],
        )?;
    } else {
        return Err(code_error());
    }
    Ok(())
}

pub(super) fn rewrap(
    user: &str,
    row: Option<Record>,
    old: &str,
    new: &str,
) -> ApiResult<Option<Rewrap>> {
    let Some(row) = row else {
        return Ok(None);
    };
    let secret = open(user, old, &row.sealed, &row.salt)?;
    let (sealed, salt) = seal(user, new, &secret)?;
    Ok(Some(Rewrap {
        previous: row.revision,
        sealed,
        salt,
    }))
}

pub(super) fn apply_rewrap(db: &Connection, user: &str, rewrap: Option<&Rewrap>) -> ApiResult<()> {
    match (record(db, user)?, rewrap) {
        (None, None) => {}
        (Some(row), Some(next)) if row.revision == next.previous => {
            db.execute("UPDATE authenticator_credentials SET sealed=?2,salt=?3,revision=?4 WHERE user_id=?1",params![user,next.sealed,next.salt,id()])?;
        }
        _ => {
            return Err(bad(
                "Authenticator settings changed. Try the password change again.",
            ))
        }
    }
    db.execute("DELETE FROM authenticator_setup WHERE user_id=?1", [user])?;
    Ok(())
}

pub(super) async fn reauthenticate(
    state: &AppState,
    user: &str,
    password: &str,
) -> ApiResult<String> {
    if password.len() > 256 || password.is_empty() {
        return Err(unauthorized());
    }
    let hash: String = lock(&state.0.db).query_row(
        "SELECT password_hash FROM users WHERE id=?1",
        [user],
        |r| r.get(0),
    )?;
    let expected = state
        .0
        .seal
        .password(user, &hash)
        .map(|h| h.to_string())
        .unwrap_or_default();
    let password = Zeroizing::new(password.to_owned());
    let permit = state
        .0
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Sign-in service is busy. Try again shortly",
            )
        })?;
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        PasswordHash::new(&expected).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
    })
    .await
    .map_err(|_| storage_error())?;
    if !valid {
        return Err(unauthorized());
    }
    Ok(hash)
}

pub(super) fn current_auth(db: &Connection, auth: &Auth, password_hash: &str) -> ApiResult<()> {
    active_user(db, &auth.user.id)?;
    let current: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=?1 AND s.user_id=?2 AND s.expires>?3 AND u.password_hash=?4)",params![auth.token_hash,auth.user.id,now(),password_hash],|r|r.get(0))?;
    if current {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

fn revoke_others(db: &Connection, auth: &Auth) -> ApiResult<Vec<String>> {
    let mut q =
        db.prepare("SELECT token_hash FROM sessions WHERE user_id=?1 AND token_hash!=?2")?;
    let tokens = q
        .query_map(params![auth.user.id, auth.token_hash], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    db.execute(
        "DELETE FROM sessions WHERE user_id=?1 AND token_hash!=?2",
        params![auth.user.id, auth.token_hash],
    )?;
    Ok(tokens)
}

type PrivateJson = (HeaderMap, Json<Value>);
fn private(value: Value) -> PrivateJson {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    (headers, Json(value))
}

pub(super) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<PrivateJson> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let remaining: u32 = db.query_row(
        "SELECT COUNT(*) FROM authenticator_recovery WHERE user_id=?1",
        [&auth.user.id],
        |r| r.get(0),
    )?;
    Ok(private(
        json!({"enabled":record(&db,&auth.user.id)?.is_some(),"recovery_codes_remaining":remaining}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    action: String,
    password: Zeroizing<String>,
    #[serde(default)]
    code: Zeroizing<String>,
    #[serde(default)]
    setup_id: Option<String>,
}

pub(super) async fn change(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Change>,
) -> ApiResult<PrivateJson> {
    let auth = state.auth(&headers)?;
    let hash = reauthenticate(&state, &auth.user.id, &input.password).await?;
    match input.action.as_str() {
        "setup" => {
            if record(&lock(&state.0.db), &auth.user.id)?.is_some() {
                return Err(bad(
                    "Authenticator is already enabled. Disable it before replacing it.",
                ));
            }
            let mut secret = Zeroizing::new(vec![0u8; 20]);
            OsRng.fill_bytes(secret.as_mut());
            let setup_id = id();
            let user = auth.user.id.clone();
            let password = input.password;
            let permit = state
                .0
                .password_jobs
                .clone()
                .try_acquire_owned()
                .map_err(|_| {
                    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Sign-in service is busy")
                })?;
            let (sealed, salt, secret) = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let (sealed, salt) = seal(&user, &password, &secret)?;
                Ok::<_, ApiError>((sealed, salt, secret))
            })
            .await
            .map_err(|_| storage_error())??;
            let safe_name: String = state
                .0
                .name
                .chars()
                .filter(|c| !c.is_control())
                .map(|c| if c == ':' { '-' } else { c })
                .take(48)
                .collect();
            let totp = Builder::new()
                .with_secret(secret.to_vec())
                .with_algorithm(Algorithm::SHA1)
                .with_account_name(format!(
                    "{} ({})",
                    auth.user.username,
                    &server_id(&lock(&state.0.db))?[..8]
                ))
                .with_issuer(Some(format!("Relay - {safe_name}")))
                .build()
                .map_err(|_| storage_error())?;
            let uri = totp.to_url().map_err(|_| storage_error())?;
            let base32 = totp.secret().to_base32();
            let mut db = lock(&state.0.db);
            let tx = db.transaction()?;
            current_auth(&tx, &auth, &hash)?;
            if record(&tx, &auth.user.id)?.is_some() {
                return Err(bad(
                    "Authenticator settings changed. Refresh and try again.",
                ));
            }
            tx.execute("INSERT INTO authenticator_setup(user_id,session_hash,setup_id,sealed,salt,expires) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(user_id) DO UPDATE SET session_hash=excluded.session_hash,setup_id=excluded.setup_id,sealed=excluded.sealed,salt=excluded.salt,expires=excluded.expires,attempts=0,window_start=0",params![auth.user.id,auth.token_hash,setup_id,sealed,salt,now().saturating_add(SETUP_TTL)])?;
            tx.commit()?;
            Ok(private(
                json!({"setup_id":setup_id,"secret":base32,"otpauth_uri":uri,"expires_in":SETUP_TTL}),
            ))
        }
        "enable" => {
            let setup_id = input
                .setup_id
                .ok_or_else(|| bad("Start authenticator setup first"))?;
            let pending:Option<(Vec<u8>,Vec<u8>)>=lock(&state.0.db).query_row("SELECT sealed,salt FROM authenticator_setup WHERE user_id=?1 AND session_hash=?2 AND setup_id=?3 AND expires>?4",params![auth.user.id,auth.token_hash,setup_id,now()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let (sealed, salt) =
                pending.ok_or_else(|| bad("Authenticator setup expired. Start again."))?;
            {
                let mut db = lock(&state.0.db);
                let tx = db.transaction()?;
                let (attempts,start):(u32,u64)=tx.query_row("SELECT attempts,window_start FROM authenticator_setup WHERE user_id=?1 AND session_hash=?2 AND setup_id=?3",params![auth.user.id,auth.token_hash,setup_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                let timestamp = now();
                let fresh = timestamp.saturating_sub(start) >= WINDOW
                    || start > timestamp.saturating_add(WINDOW);
                if !fresh && attempts >= ATTEMPTS {
                    return Err(ApiError::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        "Too many authenticator attempts. Wait a minute and try again.",
                    ));
                }
                tx.execute(
                    "UPDATE authenticator_setup SET attempts=?2,window_start=?3 WHERE user_id=?1",
                    params![
                        auth.user.id,
                        if fresh { 1 } else { attempts + 1 },
                        if fresh { timestamp } else { start }
                    ],
                )?;
                tx.commit()?;
            }
            let user = auth.user.id.clone();
            let password = input.password;
            let code = input.code;
            let encoded = sealed.clone();
            let salt_copy = salt.clone();
            let permit = state
                .0
                .password_jobs
                .clone()
                .try_acquire_owned()
                .map_err(|_| {
                    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Sign-in service is busy")
                })?;
            let step = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let secret = open(&user, &password, &encoded, &salt_copy)?;
                matched_step(&secret, code.trim(), now())?.ok_or_else(code_error)
            })
            .await
            .map_err(|_| storage_error())??;
            let (codes, revoked) = {
                let mut db = lock(&state.0.db);
                let tx = db.transaction()?;
                current_auth(&tx, &auth, &hash)?;
                let current:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM authenticator_setup WHERE user_id=?1 AND session_hash=?2 AND setup_id=?3 AND expires>?4)",params![auth.user.id,auth.token_hash,setup_id,now()],|r|r.get(0))?;
                if !current || record(&tx, &auth.user.id)?.is_some() {
                    return Err(bad("Authenticator setup changed. Start again."));
                }
                tx.execute("INSERT INTO authenticator_credentials(user_id,sealed,salt,revision,last_step) VALUES(?1,?2,?3,?4,?5)",params![auth.user.id,sealed,salt,id(),step])?;
                let codes = recovery_codes(&tx, &auth.user.id)?;
                let revoked = revoke_others(&tx, &auth)?;
                tx.execute(
                    "DELETE FROM authenticator_setup WHERE user_id=?1",
                    [&auth.user.id],
                )?;
                tx.commit()?;
                (codes, revoked)
            };
            for token in revoked {
                state.remove_voice(&lock(&state.0.db), &auth.user.id, Some(&token))?;
            }
            Ok(private(json!({"enabled":true,"recovery_codes":codes})))
        }
        "disable" | "recovery" => {
            let proof = prepare(&state, &auth.user.id, input.password, &input.code)
                .await?
                .ok_or_else(|| bad("Authenticator is not enabled"))?;
            let (codes, revoked) = {
                let mut db = lock(&state.0.db);
                let tx = db.transaction()?;
                current_auth(&tx, &auth, &hash)?;
                consume(&tx, &auth.user.id, Some(&proof))?;
                let codes = if input.action == "recovery" {
                    recovery_codes(&tx, &auth.user.id)?
                } else {
                    tx.execute(
                        "DELETE FROM authenticator_credentials WHERE user_id=?1",
                        [&auth.user.id],
                    )?;
                    Vec::new()
                };
                tx.execute(
                    "DELETE FROM authenticator_setup WHERE user_id=?1",
                    [&auth.user.id],
                )?;
                let revoked = revoke_others(&tx, &auth)?;
                tx.commit()?;
                (codes, revoked)
            };
            for token in revoked {
                state.remove_voice(&lock(&state.0.db), &auth.user.id, Some(&token))?;
            }
            Ok(private(
                json!({"enabled":input.action=="recovery","recovery_codes":codes}),
            ))
        }
        _ => Err(bad("Choose setup, enable, disable or recovery")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{request, signup};
    const PASSWORD: &str = "a-long-test-password";

    #[test]
    fn rfc6238_vectors_recovery_normalization_and_sealed_storage_binding() {
        let secret = b"12345678901234567890";
        for (timestamp, expected) in [
            (59, "287082"),
            (1111111109, "081804"),
            (1111111111, "050471"),
            (1234567890, "005924"),
            (2000000000, "279037"),
            (20000000000, "353130"),
        ] {
            assert_eq!(
                totp(secret).unwrap().generate(timestamp).to_string(),
                expected
            );
            assert_eq!(
                matched_step(secret, expected, timestamp).unwrap(),
                Some(timestamp / 30)
            );
        }
        assert_eq!(matched_step(secret, "12x456", 59).unwrap(), None);
        let (sealed, salt) = seal("alice", PASSWORD, secret).unwrap();
        assert!(!sealed.windows(secret.len()).any(|w| w == secret));
        assert_eq!(&**open("alice", PASSWORD, &sealed, &salt).unwrap(), secret);
        assert!(open("bob", PASSWORD, &sealed, &salt).is_err());
        assert!(open("alice", "wrong-password", &sealed, &salt).is_err());
        assert_eq!(
            recovery_hash("01234567-89abcdef-01234567-89abcdef"),
            recovery_hash("0123456789ABCDEF0123456789ABCDEF")
        );
        assert!(recovery_hash("123456").is_none());
    }

    async fn setup(app: &Router, session: &Session) -> Value {
        let (status, data) = request(
            app,
            "POST",
            "/api/authenticator",
            Some(&session.token),
            json!({"action":"setup","password":PASSWORD}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{data}");
        data
    }
    fn setup_code(data: &Value, at: u64) -> String {
        let token = Totp::from_url(data["otpauth_uri"].as_str().unwrap()).unwrap();
        token.generate(at).to_string()
    }
    async fn enable(app: &Router, session: &Session, data: &Value) -> Value {
        let code = setup_code(data, now());
        let (status, result) = request(
            app,
            "POST",
            "/api/authenticator",
            Some(&session.token),
            json!({"action":"enable","password":PASSWORD,"setup_id":data["setup_id"],"code":code}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        result
    }
    async fn login(app: &Router, password: &str, factor: Option<&str>) -> (StatusCode, Value) {
        request(
            app,
            "POST",
            "/api/login",
            None,
            json!({"username":"alice","password":password,"invite":"","second_factor":factor}),
        )
        .await
    }

    #[tokio::test]
    async fn optional_enrollment_login_replay_recovery_and_disable() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "authenticator-fixture", 0).unwrap();
        let app = router(state.clone());
        let user = signup(&app, "alice", "").await;
        assert_eq!(login(&app, PASSWORD, None).await.0, StatusCode::OK);
        let data = setup(&app, &user).await;
        assert!(
            record(&lock(&state.0.db), &user.user.id).unwrap().is_none(),
            "setup alone never enables a factor"
        );
        let result = enable(&app, &user, &data).await;
        assert_eq!(result["recovery_codes"].as_array().unwrap().len(), 10);
        assert_eq!(
            login(&app, PASSWORD, None).await.1["error"],
            AUTHENTICATOR_REQUIRED
        );
        assert_eq!(
            login(&app, "wrong-password", None).await.1["error"],
            "Incorrect username or password"
        );
        let code = setup_code(&data, now().saturating_add(30));
        assert_eq!(login(&app, PASSWORD, Some(&code)).await.0, StatusCode::OK);
        assert_eq!(
            login(&app, PASSWORD, Some(&code)).await.0,
            StatusCode::UNAUTHORIZED,
            "a TOTP step works once"
        );
        let recovery = result["recovery_codes"][0].as_str().unwrap();
        let (first, second) = tokio::join!(
            login(&app, PASSWORD, Some(recovery)),
            login(&app, PASSWORD, Some(recovery))
        );
        assert_eq!(
            [first.0, second.0]
                .iter()
                .filter(|s| **s == StatusCode::OK)
                .count(),
            1,
            "single-use recovery is atomic"
        );
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/authenticator",
                Some(&user.token),
                Value::Null
            )
            .await
            .1["recovery_codes_remaining"],
            9
        );
        let (status, disabled) = request(
            &app,
            "POST",
            "/api/authenticator",
            Some(&user.token),
            json!({"action":"disable","password":PASSWORD,"code":result["recovery_codes"][1]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{disabled}");
        assert_eq!(login(&app, PASSWORD, None).await.0, StatusCode::OK);
        assert!(record(&lock(&state.0.db), &user.user.id).unwrap().is_none());
    }

    #[tokio::test]
    async fn setup_bound_to_session_expires_and_attempts_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "setup-fixture", 0).unwrap();
        let app = router(state.clone());
        let user = signup(&app, "alice", "").await;
        let other: Session = serde_json::from_value(login(&app, PASSWORD, None).await.1).unwrap();
        let data = setup(&app, &user).await;
        let body = json!({"action":"enable","password":PASSWORD,"setup_id":data["setup_id"],"code":setup_code(&data,now())});
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/authenticator",
                Some(&other.token),
                body.clone()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        for _ in 0..ATTEMPTS {
            assert_eq!(request(&app,"POST","/api/authenticator",Some(&user.token),json!({"action":"enable","password":PASSWORD,"setup_id":data["setup_id"],"code":"not-a-code"})).await.0,StatusCode::UNAUTHORIZED);
        }
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/authenticator",
                Some(&user.token),
                body.clone()
            )
            .await
            .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        lock(&state.0.db)
            .execute("UPDATE authenticator_setup SET expires=0", [])
            .unwrap();
        assert_eq!(
            request(&app, "POST", "/api/authenticator", Some(&user.token), body)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn password_change_rewraps_and_recovery_replacement_invalidates_old_codes() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "rewrap-fixture", 0).unwrap();
        let app = router(state.clone());
        let user = signup(&app, "alice", "").await;
        let setup = setup(&app, &user).await;
        let enabled = enable(&app, &user, &setup).await;
        let (status, replacement) = request(
            &app,
            "POST",
            "/api/authenticator",
            Some(&user.token),
            json!({"action":"recovery","password":PASSWORD,"code":enabled["recovery_codes"][0]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replacement}");
        assert_eq!(
            login(&app, PASSWORD, enabled["recovery_codes"][1].as_str())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let before = record(&lock(&state.0.db), &user.user.id).unwrap().unwrap();
        let (status, result) = request(
            &app,
            "PATCH",
            "/api/password",
            Some(&user.token),
            json!({"current_password":PASSWORD,"new_password":"my-updated-long-password"}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{result}");
        let after = record(&lock(&state.0.db), &user.user.id).unwrap().unwrap();
        assert_ne!(before.sealed, after.sealed);
        assert!(open(&user.user.id, PASSWORD, &after.sealed, &after.salt).is_err());
        assert!(open(
            &user.user.id,
            "my-updated-long-password",
            &after.sealed,
            &after.salt
        )
        .is_ok());
        assert_eq!(
            login(&app, "my-updated-long-password", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            login(
                &app,
                "my-updated-long-password",
                replacement["recovery_codes"][0].as_str()
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            login(&app, PASSWORD, replacement["recovery_codes"][1].as_str())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        // The sealed seed carries all storage material inside the ordinary DB;
        // reopening it needs no host-specific MFA key file.
        drop(app);
        drop(state);
        let reopened = AppState::open(temp.path(), "rewrap-fixture", 0).unwrap();
        let app = router(reopened);
        assert_eq!(
            login(
                &app,
                "my-updated-long-password",
                replacement["recovery_codes"][1].as_str()
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn active_factor_brute_force_limit_does_not_reset_on_rejected_codes() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "limit-fixture", 0).unwrap();
        let app = router(state.clone());
        let user = signup(&app, "alice", "").await;
        let setup = setup(&app, &user).await;
        let enabled = enable(&app, &user, &setup).await;
        for _ in 0..ATTEMPTS {
            assert_eq!(
                login(&app, PASSWORD, Some("not-a-code")).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            login(&app, PASSWORD, enabled["recovery_codes"][0].as_str())
                .await
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        lock(&state.0.db)
            .execute("UPDATE authenticator_credentials SET window_start=0", [])
            .unwrap();
        assert_eq!(
            login(&app, PASSWORD, enabled["recovery_codes"][0].as_str())
                .await
                .0,
            StatusCode::OK
        );
    }
}
