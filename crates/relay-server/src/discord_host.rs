//! Exact host credential; deliberately separate from accounts, MLS and backups.
use super::*;
use axum::{extract::FromRequestParts, http::request::Parts};
use serde::Serialize;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

#[cfg(test)]
pub(crate) mod tests;

const VAULT_ERROR: &str = "Windows could not access the Discord credential. Try again on the host.";
const UNSUPPORTED: &str = "Saving Discord credentials requires the Windows host.";

#[derive(Serialize)]
pub(super) struct CredentialStatus {
    supported: bool,
    source: &'static str,
    error: Option<&'static str>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    server_id: String,
    path_hash: String,
    removed: bool,
    token: Zeroizing<String>,
}

enum Loaded {
    Unloaded,
    Absent,
    Removed,
    Saved(Zeroizing<String>),
    Failed,
}

pub(super) struct Store {
    target: String,
    server_id: String,
    path_hash: String,
    loaded: Loaded,
    environment: Option<Zeroizing<String>>,
    revision: u64,
    #[cfg(test)]
    backing: Option<Zeroizing<Vec<u8>>>,
}

fn valid_token(value: &str) -> bool {
    (20..=512).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl Store {
    pub(super) fn new(
        data: &FsPath,
        server_id: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let canonical = std::fs::canonicalize(data)?;
        let path = canonical.to_str().ok_or("Unsupported host storage path")?;
        #[cfg(windows)]
        let path = path.to_lowercase();
        let path_hash = token_hash(&path);
        let target = format!("Relay/discord-host/v1/{path_hash}/{server_id}");
        Ok(Self {
            target,
            server_id,
            path_hash,
            loaded: Loaded::Unloaded,
            environment: None,
            revision: 0,
            #[cfg(test)]
            backing: None,
        })
    }

    pub(super) fn environment(&mut self, token: Option<String>) {
        self.environment = token
            .map(Zeroizing::new)
            .map(|v| Zeroizing::new(v.trim().to_owned()))
            .filter(|v| valid_token(v));
    }

    fn decode(&self, bytes: Option<Zeroizing<Vec<u8>>>) -> Loaded {
        let Some(bytes) = bytes else {
            return Loaded::Absent;
        };
        let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
            return Loaded::Failed;
        };
        if record.version != 1
            || record.server_id != self.server_id
            || record.path_hash != self.path_hash
            || (record.removed && !record.token.is_empty())
            || (!record.removed && !valid_token(&record.token))
        {
            return Loaded::Failed;
        }
        if record.removed {
            Loaded::Removed
        } else {
            Loaded::Saved(record.token)
        }
    }

    fn load(&mut self) {
        if !matches!(self.loaded, Loaded::Unloaded) {
            return;
        }
        #[cfg(test)]
        let bytes: Result<_, ()> = Ok(self.backing.clone());
        #[cfg(all(windows, not(test)))]
        let bytes = native::read(&self.target);
        #[cfg(all(not(windows), not(test)))]
        let bytes: Result<Option<Zeroizing<Vec<u8>>>, ()> = Ok(None);
        self.loaded = match bytes {
            Ok(v) => self.decode(v),
            Err(_) => Loaded::Failed,
        };
    }

    pub(super) fn current(&mut self) -> (Option<Zeroizing<String>>, u64) {
        self.load();
        let token = match &self.loaded {
            Loaded::Saved(v) => Some(v.clone()),
            Loaded::Absent => self.environment.clone(),
            _ => None,
        };
        (token, self.revision)
    }

    pub(super) fn host_binding(&self) -> String {
        format!("{}/{}", self.path_hash, self.server_id)
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    fn status(&mut self) -> CredentialStatus {
        self.load();
        CredentialStatus {
            supported: cfg!(windows),
            source: match &self.loaded {
                Loaded::Saved(_) => "saved",
                Loaded::Absent if self.environment.is_some() => "environment",
                Loaded::Removed => "removed",
                _ => "none",
            },
            error: matches!(self.loaded, Loaded::Failed).then_some(VAULT_ERROR),
        }
    }

    fn write(&mut self, token: Option<Zeroizing<String>>) -> ApiResult<CredentialStatus> {
        if !cfg!(windows) && !cfg!(test) {
            return Err(bad(UNSUPPORTED));
        }
        let record = Record {
            version: 1,
            server_id: self.server_id.clone(),
            path_hash: self.path_hash.clone(),
            removed: token.is_none(),
            token: token.unwrap_or_default(),
        };
        let mut bytes = Zeroizing::new(serde_json::to_vec(&record).map_err(|_| bad(VAULT_ERROR))?);
        #[cfg(test)]
        {
            self.backing = Some(bytes.clone());
        }
        #[cfg(all(windows, not(test)))]
        native::write(&self.target, &mut bytes).map_err(|_| bad(VAULT_ERROR))?;
        bytes.zeroize();
        // An explicit host choice retires the old in-memory environment key too.
        self.environment = None;
        self.loaded = if record.removed {
            Loaded::Removed
        } else {
            Loaded::Saved(record.token)
        };
        self.revision = self.revision.wrapping_add(1);
        Ok(self.status())
    }

    pub(super) fn retry(&mut self) -> CredentialStatus {
        // Re-read only this exact namespace; also retries a failed vault read.
        self.loaded = Loaded::Unloaded;
        self.revision = self.revision.wrapping_add(1);
        self.status()
    }
}

// Extract before decoding any secret body. The ordinary session alone cannot
// change host secrets. Direct peer, private process proof and concrete identities
// are all required, even if the host trusts a reverse proxy for ordinary API use.
struct HostAccess {
    auth: Auth,
}
impl FromRequestParts<AppState> for HostAccess {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .ok_or_else(forbidden)?;
        if !peer.0.ip().is_loopback()
            || parts.headers.keys().any(|k| {
                let name = k.as_str();
                name == "origin" || name == "forwarded" || name.starts_with("x-forwarded-")
            })
        {
            return Err(forbidden());
        }
        let value = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
        let expected = state
            .0
            .config
            .local_control_token
            .as_deref()
            .ok_or_else(forbidden)?;
        let supplied = value("x-relay-local-control").ok_or_else(forbidden)?;
        if !bool::from(
            Sha256::digest(expected.as_bytes()).ct_eq(&Sha256::digest(supplied.as_bytes())),
        ) {
            return Err(forbidden());
        }
        let auth = state.auth(&parts.headers)?;
        let db = lock(&state.0.db);
        active_user(&db, &auth.user.id)?;
        roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
        if value("x-relay-instance-id") != Some(state.0.instance_id.as_str())
            || value("x-relay-server-id") != Some(server_id(&db)?.as_str())
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "The local host changed. Refresh its connection.",
            ));
        }
        Ok(Self { auth })
    }
}

fn current_admin(db: &Connection, access: &HostAccess) -> ApiResult<()> {
    // A logout/permission change between header extraction and JSON decoding
    // must not authorize a queued vault mutation.
    active_user(db, &access.auth.user.id)?;
    roles::require(db, &access.auth.user.id, PERMISSION_ADMIN)?;
    let live: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3)",
        params![access.auth.token_hash, access.auth.user.id, now()],
        |r| r.get(0),
    )?;
    if !live {
        return Err(unauthorized());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    token: Zeroizing<String>,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/local/discord-credentials",
            get(status).put(save).delete(remove).post(retry),
        )
        .layer(DefaultBodyLimit::max(4096))
}

async fn status(
    State(state): State<AppState>,
    access: HostAccess,
) -> ApiResult<Json<CredentialStatus>> {
    let db = lock(&state.0.db);
    current_admin(&db, &access)?;
    Ok(Json(lock(&state.0.discord_credential).status()))
}
async fn save(
    State(state): State<AppState>,
    access: HostAccess,
    Json(mut input): Json<Input>,
) -> ApiResult<Json<CredentialStatus>> {
    input.token = Zeroizing::new(input.token.trim().to_owned());
    if !valid_token(&input.token) {
        return Err(bad("Paste the bot token from Discord's Developer Portal."));
    }
    let result = {
        let db = lock(&state.0.db);
        current_admin(&db, &access)?;
        lock(&state.0.discord_credential).write(Some(input.token))?
    };
    state
        .0
        .discord_changes
        .send_modify(|v| *v = v.wrapping_add(1));
    Ok(Json(result))
}
async fn remove(
    State(state): State<AppState>,
    access: HostAccess,
) -> ApiResult<Json<CredentialStatus>> {
    let result = {
        let db = lock(&state.0.db);
        current_admin(&db, &access)?;
        // Empty tombstone intentionally suppresses legacy environment credentials
        // after restart; there is no credential in the tombstone.
        lock(&state.0.discord_credential).write(None)?
    };
    state
        .0
        .discord_changes
        .send_modify(|v| *v = v.wrapping_add(1));
    Ok(Json(result))
}
async fn retry(
    State(state): State<AppState>,
    access: HostAccess,
) -> ApiResult<Json<CredentialStatus>> {
    let result = {
        let db = lock(&state.0.db);
        current_admin(&db, &access)?;
        lock(&state.0.discord_credential).retry()
    };
    state
        .0
        .discord_changes
        .send_modify(|v| *v = v.wrapping_add(1));
    Ok(Json(result))
}

#[cfg(all(windows, not(test)))]
mod native {
    use super::*;
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::Security::Credentials::*,
    };
    pub(super) fn read(target: &str) -> Result<Option<Zeroizing<Vec<u8>>>, ()> {
        let name: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let mut pointer = std::ptr::null_mut();
        if let Err(error) =
            unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, 0, &mut pointer) }
        {
            return if error.code().0 as u32 == 0x80070490 {
                Ok(None)
            } else {
                Err(())
            };
        }
        if pointer.is_null() {
            return Err(());
        }
        unsafe {
            let value = &*pointer;
            let result = if value.CredentialBlobSize == 0
                || value.CredentialBlobSize > 4096
                || value.CredentialBlob.is_null()
            {
                Err(())
            } else {
                let bytes = std::slice::from_raw_parts_mut(
                    value.CredentialBlob,
                    value.CredentialBlobSize as usize,
                );
                let copy = Zeroizing::new(bytes.to_vec());
                bytes.zeroize();
                Ok(Some(copy))
            };
            CredFree(pointer.cast());
            result
        }
    }
    pub(super) fn write(target: &str, bytes: &mut [u8]) -> Result<(), ()> {
        let mut name: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        unsafe { CredWriteW(&credential, 0) }.map_err(|_| ())
    }
}
