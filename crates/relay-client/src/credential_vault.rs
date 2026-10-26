//! Optional remembered sessions use the OS credential vault, never preferences files.
use relay_protocol::Session;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct StoredSession {
    #[serde(default)]
    version: u8,
    url: String,
    #[serde(default)]
    server_id: String,
    session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority: Option<crate::host_route::RememberedAuthority>,
}

#[derive(Clone)]
pub struct RememberedSession {
    pub authority: Option<crate::host_route::RememberedAuthority>,
    pub server_id: Option<String>,
    pub session: Session,
}

#[cfg(any(windows, test))]
fn session_target(url: &str, profile: Option<&std::ffi::OsStr>) -> Result<String, String> {
    let url = crate::community::normalize_url(url)?;
    match profile {
        Some(root) => {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(root.to_string_lossy().as_bytes());
            Ok(format!("Relay/profile/{digest:x}/session/{url}"))
        }
        None => Ok(format!("Relay/session/{url}")),
    }
}

impl RememberedSession {
    pub fn confirmed_server_id(&self, expected: Option<&str>) -> Result<String, String> {
        if self
            .server_id
            .as_deref()
            .zip(expected)
            .is_some_and(|(stored, expected)| stored != expected)
        {
            return Err("The saved account and community identity disagree. Confirm the address with the host and sign in again.".into());
        }
        let pin = self.server_id.as_deref().or(expected).ok_or(
            "This older saved login needs its community confirmed. Sign in again; your private history is kept.",
        )?;
        if uuid::Uuid::parse_str(pin).is_err() {
            return Err("The saved community identity is invalid. Sign in again.".into());
        }
        Ok(pin.into())
    }
}

fn encode(url: &str, server_id: &str, session: &Session) -> Result<Vec<u8>, String> {
    encode_bound(url, server_id, session, None)
}
fn encode_bound(
    url: &str,
    server_id: &str,
    session: &Session,
    authority: Option<&crate::host_route::RememberedAuthority>,
) -> Result<Vec<u8>, String> {
    let url = crate::community::normalize_url(url)?;
    if authority.is_some_and(|a| !a.valid_for(&url, server_id)) {
        return Err("Invalid remembered host authority.".into());
    }
    if uuid::Uuid::parse_str(server_id).is_err()
        || session.token.is_empty()
        || session.token.len() > 128
        || session.user.id.is_empty()
    {
        return Err("Invalid session for remembered login.".into());
    }
    let bytes = serde_json::to_vec(&StoredSession {
        version: if authority.is_some() { 2 } else { 1 },
        url,
        server_id: server_id.into(),
        session: session.clone(),
        authority: authority.cloned(),
    })
    .map_err(|_| "Cannot prepare remembered login.")?;
    if bytes.len() > 4096 {
        return Err("The remembered login is too large.".into());
    }
    Ok(bytes)
}
fn decode(url: &str, bytes: &[u8]) -> Result<RememberedSession, String> {
    if bytes.len() > 4096 {
        return Err("The remembered login is invalid.".into());
    }
    let stored: StoredSession =
        serde_json::from_slice(bytes).map_err(|_| "The remembered login is invalid.")?;
    let url = crate::community::normalize_url(url)?;
    if stored.url != url
        || stored.session.token.is_empty()
        || stored.session.token.len() > 128
        || stored.session.user.id.is_empty()
        || stored.version > 2
        || (stored.version == 2) != stored.authority.is_some()
        || stored
            .authority
            .as_ref()
            .is_some_and(|a| !a.valid_for(&url, &stored.server_id))
        || (stored.version == 0 && !stored.server_id.is_empty())
        || (stored.version >= 1 && uuid::Uuid::parse_str(&stored.server_id).is_err())
    {
        return Err("The remembered login belongs to another server.".into());
    }
    Ok(RememberedSession {
        authority: stored.authority,
        server_id: (stored.version >= 1).then_some(stored.server_id),
        session: stored.session,
    })
}

pub fn save(url: &str, server_id: &str, session: &Session) -> Result<(), String> {
    save_bound(url, server_id, session, None)
}
pub fn save_bound(
    url: &str,
    server_id: &str,
    session: &Session,
    authority: Option<&crate::host_route::RememberedAuthority>,
) -> Result<(), String> {
    let mut authority = authority.cloned();
    #[cfg(target_os = "windows")]
    if let Some(current) = &mut authority {
        if let Some(previous) = load(url)? {
            if previous.server_id.as_deref() == Some(server_id)
                && previous.session.user.id == session.user.id
            {
                if let Some(old) = previous.authority {
                    for alias in old.previous_urls {
                        if !current.previous_urls.contains(&alias) {
                            current.previous_urls.push(alias);
                        }
                    }
                }
            }
        }
    }
    let mut retired = Vec::new();
    if let Some(current) = &mut authority {
        current.previous_urls.retain(|v| v != url);
        let mut seen = std::collections::HashSet::new();
        current.previous_urls.retain(|v| seen.insert(v.clone()));
        if current.previous_urls.len() > 8 {
            retired = current.previous_urls.split_off(8);
        }
    }
    let bytes = encode_bound(url, server_id, session, authority.as_ref())?;
    #[cfg(target_os = "windows")]
    {
        native::save(
            &session_target(url, profile().as_deref())?,
            &session.user.id,
            bytes,
        )?;
        for alias in retired {
            forget_matching(&alias, server_id, &session.user.id)?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (bytes, retired);
        Err(
            "Remembered login requires an OS credential-vault implementation on this platform."
                .into(),
        )
    }
}

pub fn load(url: &str) -> Result<Option<RememberedSession>, String> {
    #[cfg(target_os = "windows")]
    {
        native::load(&session_target(url, profile().as_deref())?)?
            .map(|bytes| decode(url, &bytes))
            .transpose()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
        Ok(None)
    }
}
#[cfg(target_os = "windows")]
fn forget_matching(url: &str, server_id: &str, user_id: &str) -> Result<(), String> {
    if let Ok(Some(old)) = load(url) {
        if old.server_id.as_deref() == Some(server_id) && old.session.user.id == user_id {
            native::forget(&session_target(url, profile().as_deref())?)?;
        }
    }
    Ok(())
}
pub fn forget(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if let Ok(Some(current)) = load(url) {
            if let (Some(server), Some(authority)) =
                (current.server_id.as_deref(), current.authority.as_ref())
            {
                for alias in &authority.previous_urls {
                    forget_matching(alias, server, &current.session.user.id)?;
                }
            }
        }
        native::forget(&session_target(url, profile().as_deref())?)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
        Ok(())
    }
}

/// The identity a data-copy credential belongs to. A credential for one
/// profile, community address, community, account or copy is never found by
/// another.
pub struct ReplicaKey<'a> {
    pub source_url: &'a str,
    pub server_id: &'a str,
    pub user_id: &'a str,
    pub replica_id: &'a str,
}

/// The dedicated pull token of one copy. It opens only that community's
/// data-copy routes and never leaves the vault except to the copy worker's
/// standard input.
pub struct ReplicaToken {
    pub token: zeroize::Zeroizing<String>,
    pub expires: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredReplicaToken {
    version: u8,
    url: String,
    server_id: String,
    user_id: String,
    replica_id: String,
    token: String,
    expires: u64,
}

fn replica_identity(key: &ReplicaKey<'_>) -> Result<String, String> {
    let url = crate::community::normalize_url(key.source_url)?;
    if uuid::Uuid::parse_str(key.server_id).is_err()
        || uuid::Uuid::parse_str(key.replica_id).is_err()
        || key.user_id.is_empty()
        || key.user_id.len() > 128
    {
        return Err("The copy's identity is invalid.".into());
    }
    Ok(url)
}

#[cfg(any(windows, test))]
fn replica_target(
    key: &ReplicaKey<'_>,
    profile: Option<&std::ffi::OsStr>,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let url = replica_identity(key)?;
    let mut hash = Sha256::new();
    for part in [url.as_str(), key.server_id, key.user_id, key.replica_id] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    let copy = hash.finalize();
    Ok(match profile {
        Some(root) => {
            let digest = Sha256::digest(root.to_string_lossy().as_bytes());
            format!("Relay/profile/{digest:x}/replica/{copy:x}")
        }
        None => format!("Relay/replica/{copy:x}"),
    })
}

fn encode_replica(key: &ReplicaKey<'_>, token: &str, expires: u64) -> Result<Vec<u8>, String> {
    let url = replica_identity(key)?;
    if token.is_empty() || token.len() > 256 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("Invalid copy access.".into());
    }
    let bytes = serde_json::to_vec(&StoredReplicaToken {
        version: 1,
        url,
        server_id: key.server_id.into(),
        user_id: key.user_id.into(),
        replica_id: key.replica_id.into(),
        token: token.into(),
        expires,
    })
    .map_err(|_| "Cannot prepare copy access.")?;
    if bytes.len() > 2048 {
        return Err("The copy access is too large.".into());
    }
    Ok(bytes)
}

fn decode_replica(key: &ReplicaKey<'_>, bytes: &[u8]) -> Result<ReplicaToken, String> {
    if bytes.len() > 2048 {
        return Err("The saved copy access is invalid.".into());
    }
    let stored: StoredReplicaToken =
        serde_json::from_slice(bytes).map_err(|_| "The saved copy access is invalid.")?;
    let url = replica_identity(key)?;
    if stored.version != 1
        || stored.url != url
        || stored.server_id != key.server_id
        || stored.user_id != key.user_id
        || stored.replica_id != key.replica_id
        || stored.token.is_empty()
        || stored.token.len() > 256
    {
        return Err("The saved copy access belongs to another copy.".into());
    }
    Ok(ReplicaToken {
        token: zeroize::Zeroizing::new(stored.token),
        expires: stored.expires,
    })
}

pub fn save_replica_token(key: &ReplicaKey<'_>, token: &str, expires: u64) -> Result<(), String> {
    let bytes = encode_replica(key, token, expires)?;
    #[cfg(target_os = "windows")]
    {
        native::save(
            &replica_target(key, profile().as_deref())?,
            key.user_id,
            bytes,
        )
        .map_err(|_| "Windows could not save the access for this copy.".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = bytes;
        Err("Keeping a copy requires an OS credential vault on this platform.".into())
    }
}
pub fn load_replica_token(key: &ReplicaKey<'_>) -> Result<Option<ReplicaToken>, String> {
    #[cfg(target_os = "windows")]
    {
        native::load(&replica_target(key, profile().as_deref())?)
            .map_err(|_| "Windows could not read the access for this copy.".to_string())?
            .map(|bytes| decode_replica(key, &bytes))
            .transpose()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = key;
        Ok(None)
    }
}
pub fn forget_replica_token(key: &ReplicaKey<'_>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        native::forget(&replica_target(key, profile().as_deref())?)
            .map_err(|_| "Windows could not remove the access for this copy.".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = key;
        Ok(())
    }
}

/// A purpose-separated central account refresh credential. Never stored in preferences.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CentralSession {
    pub issuer: String,
    pub client_id: String,
    pub subject: String,
    pub nonce: String,
    pub refresh_token: String,
}
impl Drop for CentralSession {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.refresh_token.zeroize();
        self.nonce.zeroize();
    }
}

fn central_target(issuer: &str, client_id: &str, profile: Option<&std::ffi::OsStr>) -> String {
    use sha2::{Digest, Sha256};
    let mut identity = Sha256::new();
    identity.update(issuer.as_bytes());
    identity.update([0]);
    identity.update(client_id.as_bytes());
    let binding = format!("{:x}", identity.finalize());
    match profile {
        Some(profile) => format!(
            "Relay/profile/{:x}/central-account-v1/{binding}",
            Sha256::digest(profile.to_string_lossy().as_bytes())
        ),
        None => format!("Relay/central-account-v1/{binding}"),
    }
}
fn decode_central(issuer: &str, client_id: &str, raw: &[u8]) -> Result<CentralSession, String> {
    if raw.len() > 4096 {
        return Err("The saved Relay account is invalid.".into());
    }
    let saved: CentralSession =
        serde_json::from_slice(raw).map_err(|_| "The saved Relay account is invalid.")?;
    if saved.issuer != issuer
        || saved.client_id != client_id
        || saved.subject.is_empty()
        || saved.subject.len() > 255
        || saved.subject.chars().any(char::is_control)
        || saved.nonce.is_empty()
        || saved.nonce.len() > 512
        || saved.refresh_token.is_empty()
        || saved.refresh_token.len() > 3500
    {
        return Err("The saved Relay account does not match this sign-in service.".into());
    }
    Ok(saved)
}

pub(crate) fn load_central_session(
    issuer: &str,
    client_id: &str,
) -> Result<Option<CentralSession>, String> {
    #[cfg(windows)]
    {
        let raw = native::load(&central_target(issuer, client_id, profile().as_deref()))?;
        raw.map(|raw| {
            let raw = zeroize::Zeroizing::new(raw);
            decode_central(issuer, client_id, &raw)
        })
        .transpose()
    }
    #[cfg(not(windows))]
    {
        let _ = (issuer, client_id);
        Ok(None)
    }
}
pub(crate) fn save_central_session(saved: &CentralSession) -> Result<(), String> {
    let mut raw = zeroize::Zeroizing::new(
        serde_json::to_vec(saved).map_err(|_| "Windows could not save your Relay account.")?,
    );
    // Apply exactly the same bounds before writing as when reading the vault.
    decode_central(&saved.issuer, &saved.client_id, &raw)?;
    if raw.len() > 4096 {
        let _wipe = zeroize::Zeroizing::new(raw);
        return Err("The account session is too large to remember safely.".into());
    }
    #[cfg(windows)]
    {
        native::save(
            &central_target(&saved.issuer, &saved.client_id, profile().as_deref()),
            &saved.subject,
            std::mem::take(&mut *raw),
        )
    }
    #[cfg(not(windows))]
    {
        let _wipe = zeroize::Zeroizing::new(raw);
        Err("Remembering Relay accounts requires the Windows credential vault.".into())
    }
}
pub(crate) fn forget_central_session(issuer: &str, client_id: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        native::forget(&central_target(issuer, client_id, profile().as_deref()))
    }
    #[cfg(not(windows))]
    {
        let _ = (issuer, client_id);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn profile() -> Option<std::ffi::OsString> {
    std::env::var_os("RELAY_PROFILE_DIR")
}

#[cfg(target_os = "windows")]
mod native {
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::Security::Credentials::*,
    };
    fn wide(target: &str) -> Vec<u16> {
        target.encode_utf16().chain(Some(0)).collect()
    }
    pub(super) fn save(target: &str, user: &str, mut bytes: Vec<u8>) -> Result<(), String> {
        let result = crate::e2ee::protected::with_credential_lock(|| {
            let mut target = wide(target);
            let mut username: Vec<_> = user.encode_utf16().chain(Some(0)).collect();
            let credential = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target.as_mut_ptr()),
                UserName: PWSTR(username.as_mut_ptr()),
                CredentialBlobSize: bytes.len() as u32,
                CredentialBlob: bytes.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };
            unsafe { CredWriteW(&credential, 0) }
                .map_err(|_| "Windows could not save the remembered login.".to_string())
        });
        bytes.fill(0);
        result
    }
    pub(super) fn load(target: &str) -> Result<Option<Vec<u8>>, String> {
        crate::e2ee::protected::with_credential_lock(|| {
            let target = wide(target);
            let mut credential = std::ptr::null_mut();
            let result = unsafe {
                CredReadW(
                    PCWSTR(target.as_ptr()),
                    CRED_TYPE_GENERIC,
                    0,
                    &mut credential,
                )
            };
            if let Err(error) = result {
                if error.code().0 as u32 == 0x80070490 {
                    return Ok(None);
                }
                return Err("Windows could not read the remembered login.".into());
            }
            let bytes = unsafe {
                let value = &*credential;
                let result = if value.CredentialBlobSize > 4096 || value.CredentialBlob.is_null() {
                    Err("The remembered login is invalid.".to_string())
                } else {
                    Ok(std::slice::from_raw_parts(
                        value.CredentialBlob,
                        value.CredentialBlobSize as usize,
                    )
                    .to_vec())
                };
                CredFree(credential.cast());
                result
            }?;
            Ok(Some(bytes))
        })
    }
    pub(super) fn forget(target: &str) -> Result<(), String> {
        crate::e2ee::protected::with_credential_lock(|| {
            let target = wide(target);
            match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) } {
                Ok(()) => Ok(()),
                Err(error) if error.code().0 as u32 == 0x80070490 => Ok(()),
                Err(_) => Err("Windows could not remove the remembered login.".into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_vault_is_distinct_without_changing_default_login_target() {
        use std::ffi::OsStr;
        let url = "http://127.0.0.1:5717";
        let default = session_target(url, None).unwrap();
        assert_eq!(default, "Relay/session/http://127.0.0.1:5717");
        let first = session_target(url, Some(OsStr::new("profile-one"))).unwrap();
        let second = session_target(url, Some(OsStr::new("profile-two"))).unwrap();
        assert_ne!(first, default);
        assert_ne!(first, second);
        assert_eq!(
            first,
            session_target("http://127.0.0.1:5717/", Some(OsStr::new("profile-one"))).unwrap()
        );
        assert!(session_target("not a community", Some(OsStr::new("profile-one"))).is_err());
    }
    #[test]
    fn remembered_session_is_bound_to_canonical_server_and_bounded() {
        let session = Session {
            token: "synthetic-fixture-token".into(),
            user: relay_protocol::User {
                id: "fixture-user".into(),
                ..Default::default()
            },
        };
        let pin = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";
        let bytes = encode("HTTPS://ONE.EXAMPLE/", pin, &session).unwrap();
        assert_eq!(
            decode("https://one.example", &bytes)
                .unwrap()
                .session
                .user
                .id,
            "fixture-user"
        );
        assert!(decode("https://two.example", &bytes).is_err());
        assert!(decode("https://one.example", &vec![0; 4097]).is_err());
        assert_eq!(
            decode("https://one.example", &bytes)
                .unwrap()
                .confirmed_server_id(None)
                .unwrap(),
            pin
        );
        assert!(encode("https://one.example", "", &session).is_err());
    }
    #[test]
    fn legacy_login_requires_a_known_pin_and_unknown_versions_never_resume() {
        let pin = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";
        let legacy = serde_json::json!({"url":"https://one.example","session":{"token":"fixture-token","user":{"id":"fixture-user","username":"fixture","role":"member"}}});
        let old = decode("https://one.example", &serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(old.confirmed_server_id(None).is_err());
        assert_eq!(old.confirmed_server_id(Some(pin)).unwrap(), pin);
        let encoded = encode("https://one.example", pin, &old.session).unwrap();
        let current = decode("https://one.example", &encoded).unwrap();
        assert!(current
            .confirmed_server_id(Some("f79a7de8-05d0-4297-b56d-7f34cc03a56d"))
            .is_err());
        let mut invalid: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        for version in [0, 2, 255] {
            invalid["version"] = serde_json::json!(version);
            assert!(decode(
                "https://one.example",
                &serde_json::to_vec(&invalid).unwrap()
            )
            .is_err());
        }
    }

    #[test]
    fn authority_v2_is_bound_to_the_exact_account_url_and_server_and_keeps_only_public_host_material(
    ) {
        let (_, _, pin, _) = crate::host_route::tests::fixture();
        let session = Session {
            token: "synthetic-bound-token".into(),
            user: relay_protocol::User {
                id: "synthetic-user".into(),
                ..Default::default()
            },
        };
        let authority = crate::host_route::RememberedAuthority {
            pin: pin.clone(),
            account_provider: None,
            previous_urls: vec!["https://old.example".into()],
        };
        let raw = encode_bound(
            "https://current.example",
            &pin.server_id,
            &session,
            Some(&authority),
        )
        .unwrap();
        let decoded = decode("https://current.example", &raw).unwrap();
        assert_eq!(decoded.authority.as_ref().unwrap().pin, pin);
        assert_eq!(
            decoded.authority.unwrap().previous_urls,
            authority.previous_urls
        );
        assert!(decode("https://different.example", &raw).is_err());
        let mut wrong = authority.clone();
        wrong.pin.server_id = uuid::Uuid::new_v4().to_string();
        assert!(encode_bound(
            "https://current.example",
            &pin.server_id,
            &session,
            Some(&wrong)
        )
        .is_err());
        let mut wire: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        wire["version"] = serde_json::json!(1);
        assert!(decode(
            "https://current.example",
            &serde_json::to_vec(&wire).unwrap()
        )
        .is_err());
        wire["version"] = serde_json::json!(2);
        wire["authority"]["pin"]["host"]["public_key"] = serde_json::json!("forged");
        assert!(decode(
            "https://current.example",
            &serde_json::to_vec(&wire).unwrap()
        )
        .is_err());
    }
    const SERVER: &str = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";
    const COPY: &str = "f79a7de8-05d0-4297-b56d-7f34cc03a56d";
    fn key<'a>(url: &'a str, server: &'a str, user: &'a str, copy: &'a str) -> ReplicaKey<'a> {
        ReplicaKey {
            source_url: url,
            server_id: server,
            user_id: user,
            replica_id: copy,
        }
    }
    #[test]
    fn copy_access_is_bound_to_profile_community_account_and_copy() {
        use std::ffi::OsStr;
        let base = key("https://one.example", SERVER, "alice", COPY);
        let default = replica_target(&base, None).unwrap();
        let first = replica_target(&base, Some(OsStr::new("profile-one"))).unwrap();
        assert_ne!(default, first);
        assert_ne!(
            first,
            replica_target(&base, Some(OsStr::new("profile-two"))).unwrap()
        );
        assert_eq!(
            first,
            replica_target(
                &key("HTTPS://ONE.EXAMPLE/", SERVER, "alice", COPY),
                Some(OsStr::new("profile-one"))
            )
            .unwrap()
        );
        for other in [
            key("https://two.example", SERVER, "alice", COPY),
            key("https://one.example", COPY, "alice", COPY),
            key("https://one.example", SERVER, "bob", COPY),
            key("https://one.example", SERVER, "alice", SERVER),
        ] {
            assert_ne!(default, replica_target(&other, None).unwrap());
        }
        // Parts are length-prefixed, so a value cannot be shifted into the next one.
        assert_ne!(
            replica_target(&key("https://one.example", SERVER, "ab", COPY), None).unwrap(),
            replica_target(&key("https://one.example", SERVER, "a", COPY), None).unwrap()
        );
        assert!(replica_target(&key("https://one.example", "x", "alice", COPY), None).is_err());
        assert!(replica_target(&key("https://one.example", SERVER, "", COPY), None).is_err());
        assert!(replica_target(&key("not a community", SERVER, "alice", COPY), None).is_err());
    }
    #[test]
    fn stored_copy_access_is_exact_and_bounded() {
        let own = key("https://one.example", SERVER, "alice", COPY);
        let bytes = encode_replica(&own, "synthetic-pull-token", 42).unwrap();
        let found = decode_replica(&own, &bytes).unwrap();
        assert_eq!(found.token.as_str(), "synthetic-pull-token");
        assert_eq!(found.expires, 42);
        for other in [
            key("https://one.example", SERVER, "bob", COPY),
            key("https://one.example", SERVER, "alice", SERVER),
            key("https://one.example", COPY, "alice", COPY),
            key("https://two.example", SERVER, "alice", COPY),
        ] {
            assert!(decode_replica(&other, &bytes).is_err());
        }
        assert!(encode_replica(&own, "", 1).is_err());
        assert!(encode_replica(&own, &"x".repeat(257), 1).is_err());
        assert!(encode_replica(&own, "has space", 1).is_err());
        assert!(decode_replica(&own, &vec![0; 2049]).is_err());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["version"] = serde_json::json!(2);
        assert!(decode_replica(&own, &serde_json::to_vec(&value).unwrap()).is_err());
    }
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "writes and removes only a uniquely named synthetic Relay test credential in the OS vault"]
    fn actual_vault_roundtrip_never_uses_existing_user_credentials() {
        let url = format!("https://{}.relay-test.invalid", uuid::Uuid::new_v4());
        let session = Session {
            token: "synthetic-vault-fixture".into(),
            user: relay_protocol::User {
                id: "fixture-user".into(),
                ..Default::default()
            },
        };
        let pin = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";
        save(&url, pin, &session).unwrap();
        let found = load(&url).unwrap().unwrap();
        assert_eq!(found.session.token, session.token);
        assert_eq!(found.confirmed_server_id(None).unwrap(), pin);
        forget(&url).unwrap();
        assert!(load(&url).unwrap().is_none());
    }
}

#[cfg(test)]
mod central_session_tests {
    use super::*;
    #[test]
    fn central_credentials_are_bound_to_issuer_client_and_profile_and_not_community_sessions() {
        use std::ffi::OsStr;
        let issuer = "https://accounts.example/realms/relay";
        let target = central_target(issuer, "relay-desktop", None);
        assert!(target.starts_with("Relay/central-account-v1/"));
        assert_ne!(
            target,
            session_target("http://127.0.0.1:4580", None).unwrap()
        );
        assert_ne!(target, central_target(issuer, "another-client", None));
        assert_ne!(
            target,
            central_target("https://other.example/realm", "relay-desktop", None)
        );
        assert_ne!(
            central_target(issuer, "relay-desktop", Some(OsStr::new("C:/owned/a"))),
            central_target(issuer, "relay-desktop", Some(OsStr::new("C:/owned/b")))
        );
        let saved = CentralSession {
            issuer: issuer.into(),
            client_id: "relay-desktop".into(),
            subject: "owned-subject".into(),
            nonce: "owned-nonce".into(),
            refresh_token: "owned-refresh".into(),
        };
        let raw = serde_json::to_vec(&saved).unwrap();
        assert!(decode_central(issuer, "relay-desktop", &raw).is_ok());
        assert!(decode_central(issuer, "another-client", &raw).is_err());
        assert!(decode_central("https://other.example/realm", "relay-desktop", &raw).is_err());
    }
}
