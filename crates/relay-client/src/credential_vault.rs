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
}

pub struct RememberedSession {
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
    let url = crate::community::normalize_url(url)?;
    if uuid::Uuid::parse_str(server_id).is_err()
        || session.token.is_empty()
        || session.token.len() > 128
        || session.user.id.is_empty()
    {
        return Err("Invalid session for remembered login.".into());
    }
    let bytes = serde_json::to_vec(&StoredSession {
        version: 1,
        url,
        server_id: server_id.into(),
        session: session.clone(),
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
        || stored.version > 1
        || (stored.version == 0 && !stored.server_id.is_empty())
        || (stored.version == 1 && uuid::Uuid::parse_str(&stored.server_id).is_err())
    {
        return Err("The remembered login belongs to another server.".into());
    }
    Ok(RememberedSession {
        server_id: (stored.version == 1).then_some(stored.server_id),
        session: stored.session,
    })
}

pub fn save(url: &str, server_id: &str, session: &Session) -> Result<(), String> {
    let bytes = encode(url, server_id, session)?;
    #[cfg(target_os = "windows")]
    {
        native::save(url, &session.user.id, bytes)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = bytes;
        Err(
            "Remembered login requires an OS credential-vault implementation on this platform."
                .into(),
        )
    }
}
pub fn load(url: &str) -> Result<Option<RememberedSession>, String> {
    #[cfg(target_os = "windows")]
    {
        native::load(url)?
            .map(|bytes| decode(url, &bytes))
            .transpose()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
        Ok(None)
    }
}
pub fn forget(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        native::forget(url)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod native {
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::Security::Credentials::*,
    };
    fn target(url: &str) -> Result<Vec<u16>, String> {
        Ok(
            super::session_target(url, std::env::var_os("RELAY_PROFILE_DIR").as_deref())?
                .encode_utf16()
                .chain(Some(0))
                .collect(),
        )
    }
    pub(super) fn save(url: &str, user: &str, mut bytes: Vec<u8>) -> Result<(), String> {
        let result = crate::e2ee::protected::with_credential_lock(|| {
            let mut target = target(url)?;
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
    pub(super) fn load(url: &str) -> Result<Option<Vec<u8>>, String> {
        crate::e2ee::protected::with_credential_lock(|| {
            let target = target(url)?;
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
    pub(super) fn forget(url: &str) -> Result<(), String> {
        crate::e2ee::protected::with_credential_lock(|| {
            let target = target(url)?;
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
