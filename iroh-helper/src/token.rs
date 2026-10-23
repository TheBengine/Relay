//! The host's long-lived key and admission token.
//!
//! The key is the host's identity: it is why an invitation keeps working when
//! the host's address changes. The token is what lets a friend in. Both are
//! created once and kept in the helper's private state folder.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use iroh::SecretKey;
use std::{fs, io::Write, path::Path};

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("no secure random source"))?;
    Ok(bytes)
}

/// 43 URL-safe characters from 32 random bytes.
pub fn new_token() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

/// Writes a new file and never replaces one: a second run must not lose the
/// identity or token a friend's invitation depends on.
fn create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > limit {
        bail!("{} is not an ordinary small file", path.display());
    }
    Ok(fs::read(path)?)
}

/// The host's secret key, created on first use.
pub fn load_or_create_key(state: &Path) -> Result<SecretKey> {
    let path = state.join("host.key");
    if !path.exists() {
        create_new(&path, &random_bytes::<32>()?)?;
    }
    let bytes = read_limited(&path, 64)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("the host key file is damaged"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// The admission token, created on first use.
pub fn load_or_create_token(state: &Path) -> Result<String> {
    let path = state.join("admission.token");
    if !path.exists() {
        create_new(&path, new_token()?.as_bytes())?;
    }
    let text = String::from_utf8(read_limited(&path, 128)?)?;
    let token = text.trim().to_owned();
    if !crate::protocol::valid_token(&token) {
        bail!("the admission token file is damaged");
    }
    Ok(token)
}

/// Replaces the token. The old one stops working for every friend at once.
pub fn rotate_token(state: &Path) -> Result<String> {
    let path = state.join("admission.token");
    let token = new_token()?;
    let pending = state.join("admission.token.new");
    let _ = fs::remove_file(&pending);
    create_new(&pending, token.as_bytes())?;
    fs::rename(&pending, &path)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("relay-iroh-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tokens_are_random_long_and_acceptable_to_the_protocol() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(crate::protocol::valid_token(&a));
    }

    #[test]
    fn the_identity_and_token_survive_a_restart_and_are_never_replaced_by_loading() {
        let dir = scratch("identity");
        let key = load_or_create_key(&dir).unwrap();
        let token = load_or_create_token(&dir).unwrap();
        assert_eq!(load_or_create_key(&dir).unwrap().public(), key.public());
        assert_eq!(load_or_create_token(&dir).unwrap(), token);
        let rotated = rotate_token(&dir).unwrap();
        assert_ne!(rotated, token);
        assert_eq!(load_or_create_token(&dir).unwrap(), rotated);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn damaged_or_odd_files_are_refused_instead_of_replaced() {
        let dir = scratch("damaged");
        fs::write(dir.join("host.key"), b"too short").unwrap();
        assert!(load_or_create_key(&dir).is_err());
        assert_eq!(
            fs::read(dir.join("host.key")).unwrap(),
            b"too short",
            "a damaged key is left for the user to see"
        );
        fs::write(dir.join("admission.token"), "not a valid token!").unwrap();
        assert!(load_or_create_token(&dir).is_err());
        fs::write(dir.join("big.key"), vec![0u8; 100]).unwrap();
        assert!(read_limited(&dir.join("big.key"), 64).is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
