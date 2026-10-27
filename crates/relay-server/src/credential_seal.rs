//! Login secrets in the community database are useless without this host.
//!
//! The database pages are what a co-host's standby copy receives, word for
//! word, so nothing in them may log anyone in or be cracked offline:
//! * a password hash is stored only sealed (ChaCha20-Poly1305) under a key that
//!   lives in this host's data folder, never in the database;
//! * a session is stored under a keyed verifier (HMAC-SHA256 of the token's
//!   digest with the same host key), and the digest the directory needs is sealed.
//!
//! A copy therefore holds ciphertext and keyed digests only. Moving the whole
//! data folder keeps sign-ins working; a database file restored elsewhere needs
//! a reset or recovery code, exactly like a community copy after a move.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::path::Path;
use zeroize::Zeroizing;

const PASSWORD: &str = "sealed:v1:";
const LOCATOR: &str = "sealed-locator:v1:";
#[cfg(test)]
pub(crate) const TEST_KEY: u8 = 0x5e;

pub(crate) struct Seal {
    key: Zeroizing<[u8; 32]>,
}

impl Seal {
    /// The key is created once, privately, beside the database. Unit tests use
    /// one fixed key, so a test can compute the verifier for a session it made.
    pub(crate) fn open(data: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        #[cfg(test)]
        {
            let _ = data;
            #[allow(clippy::needless_return)] // The non-test block follows.
            return Ok(Self::fixed(TEST_KEY));
        }
        #[cfg(not(test))]
        Self::open_file(data)
    }

    pub(crate) fn open_file(data: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let folder = data.join("credential-seal");
        crate::handover_target::private_directory(&folder)?;
        Ok(Self {
            key: crate::handover_target::seed(&folder, "seal-key.v1", true)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn fixed(byte: u8) -> Self {
        Self {
            key: Zeroizing::new([byte; 32]),
        }
    }

    fn seal(&self, prefix: &str, aad: &str, plain: &[u8]) -> String {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&*self.key));
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: aad.as_bytes() })
            .expect("bounded credential");
        let mut out = nonce.to_vec();
        out.extend(sealed);
        format!("{prefix}{}", URL_SAFE_NO_PAD.encode(out))
    }

    fn open_sealed(&self, prefix: &str, aad: &str, stored: &str) -> Option<Zeroizing<String>> {
        let raw = URL_SAFE_NO_PAD.decode(stored.strip_prefix(prefix)?).ok()?;
        if raw.len() < 12 + 16 {
            return None;
        }
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&*self.key));
        let plain = cipher
            .decrypt(Nonce::from_slice(&raw[..12]), Payload { msg: &raw[12..], aad: aad.as_bytes() })
            .ok()?;
        String::from_utf8(plain).ok().map(Zeroizing::new)
    }

    /// What is stored for a password hash. Bound to the member, so a sealed
    /// value cannot be moved to another account.
    pub(crate) fn seal_password(&self, user_id: &str, phc: &str) -> String {
        self.seal(PASSWORD, &format!("relay-password-v1\0{user_id}"), phc.as_bytes())
    }

    /// The hash to verify against. Older databases still hold plain hashes
    /// until startup seals them; a value from another host opens to nothing.
    pub(crate) fn password(&self, user_id: &str, stored: &str) -> Option<Zeroizing<String>> {
        if stored.starts_with(PASSWORD) {
            self.open_sealed(PASSWORD, &format!("relay-password-v1\0{user_id}"), stored)
        } else if stored.starts_with('$') {
            Some(Zeroizing::new(stored.to_owned()))
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(crate) fn is_sealed_password(stored: &str) -> bool {
        stored.starts_with(PASSWORD)
    }

    /// The stored session verifier for a token digest.
    pub(crate) fn session(&self, digest: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&*self.key).expect("fixed key");
        mac.update(b"relay-session-v1\0");
        mac.update(digest.as_bytes());
        format!("{:x}", mac.finalize().into_bytes())
    }

    /// The token digest the host directory needs, sealed beside the session.
    pub(crate) fn seal_locator(&self, digest: &str) -> String {
        self.seal(LOCATOR, "relay-session-locator-v1", digest.as_bytes())
    }

    pub(crate) fn locator(&self, stored: &str) -> Option<Zeroizing<String>> {
        self.open_sealed(LOCATOR, "relay-session-locator-v1", stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_created_once_privately_and_reused() {
        let temp = tempfile::tempdir().unwrap();
        let first = Seal::open_file(temp.path()).unwrap();
        let again = Seal::open_file(temp.path()).unwrap();
        assert_eq!(first.session("x"), again.session("x"));
        assert_eq!(std::fs::metadata(temp.path().join("credential-seal").join("seal-key.v1")).unwrap().len(), 32);
        assert_ne!(first.session("x"), Seal::fixed(TEST_KEY).session("x"));
    }

    #[test]
    fn sealed_values_open_only_on_this_host_and_for_the_same_member() {
        let host = Seal::fixed(1);
        let other = Seal::fixed(2);
        let phc = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA";
        let stored = host.seal_password("u1", phc);
        assert!(!stored.contains("argon2"), "no hash text in the database");
        assert_eq!(host.password("u1", &stored).as_deref().map(String::as_str), Some(phc));
        assert!(host.password("u2", &stored).is_none(), "bound to the member");
        assert!(other.password("u1", &stored).is_none(), "useless on another host");
        assert_eq!(host.password("u1", phc).as_deref().map(String::as_str), Some(phc), "legacy");
        assert!(host.password("u1", crate::host_copy_secrets::NOT_COPIED).is_none());
        // Session verifiers are keyed: equal tokens match only on this host.
        assert_eq!(host.session("abc"), host.session("abc"));
        assert_ne!(host.session("abc"), other.session("abc"));
        let locator = host.seal_locator("abc");
        assert_eq!(host.locator(&locator).as_deref().map(String::as_str), Some("abc"));
        assert!(other.locator(&locator).is_none());
    }

    #[tokio::test]
    async fn a_page_copy_of_the_database_holds_no_usable_password_hash_or_session() {
        use crate::tests::{request, signup};
        let temp = tempfile::tempdir().unwrap();
        let state = crate::AppState::open(&temp.path().join("host"), "Sealed", 0).unwrap();
        let app = crate::router(state.clone());
        let anna = signup(&app, "Anna", "").await;
        // The same pages a co-host's standby copy receives.
        let copy = temp.path().join("copy.sqlite3");
        state.backup_to(&copy).unwrap();
        let raw = std::fs::read(&copy).unwrap();
        let digest = crate::token_hash(&anna.token);
        for needle in [&b"$argon2"[..], digest.as_bytes()] {
            assert!(!raw.windows(needle.len()).any(|w| w == needle), "usable secret in the copy");
        }
        let db = rusqlite::Connection::open(&copy).unwrap();
        let (hash, verifier, locator): (String, String, String) = db
            .query_row(
                "SELECT u.password_hash,s.token_hash,s.locator FROM users u JOIN sessions s ON s.user_id=u.id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(Seal::is_sealed_password(&hash));
        assert_ne!(verifier, digest);
        assert!(locator.starts_with(LOCATOR));
        // Without this host's key the copy opens nothing.
        assert!(Seal::fixed(9).password(&anna.user.id, &hash).is_none());
        // The host itself still works.
        assert_eq!(
            request(&app, "GET", "/api/bootstrap", Some(&anna.token), serde_json::Value::Null).await.0,
            axum::http::StatusCode::OK
        );
        assert_eq!(
            request(&app, "POST", "/api/login", None, serde_json::json!({"username":"Anna","password":"a-long-test-password"})).await.0,
            axum::http::StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_older_database_is_sealed_at_startup_without_signing_anyone_out() {
        use crate::tests::{request, signup};
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("host");
        let state = crate::AppState::open(&data, "Older", 0).unwrap();
        let anna = signup(&crate::router(state.clone()), "Anna", "").await;
        // Rewrite the rows the way a release before sealing stored them.
        {
            use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
            let plain = Argon2::default()
                .hash_password(b"a-long-test-password", &SaltString::generate(&mut rand::rngs::OsRng))
                .unwrap()
                .to_string();
            let db = crate::lock(&state.0.db);
            db.execute("UPDATE users SET password_hash=?1", [plain]).unwrap();
            db.execute(
                "UPDATE sessions SET token_hash=?1,locator=NULL",
                [crate::token_hash(&anna.token)],
            )
            .unwrap();
        }
        drop(state);
        let state = crate::AppState::open(&data, "Older", 0).unwrap();
        let app = crate::router(state.clone());
        assert_eq!(
            request(&app, "GET", "/api/bootstrap", Some(&anna.token), serde_json::Value::Null).await.0,
            axum::http::StatusCode::OK,
            "the remembered session survives the upgrade"
        );
        let (hash, locator): (String, Option<String>) = crate::lock(&state.0.db)
            .query_row("SELECT u.password_hash,s.locator FROM users u JOIN sessions s ON s.user_id=u.id", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert!(Seal::is_sealed_password(&hash));
        assert!(locator.is_some());
        assert_eq!(
            request(&app, "POST", "/api/login", None, serde_json::json!({"username":"Anna","password":"a-long-test-password"})).await.0,
            axum::http::StatusCode::OK
        );
    }
}
