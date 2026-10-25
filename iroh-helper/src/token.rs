//! The host's long-lived key and admission token.
//!
//! The key is the host's identity: it is why an invitation keeps working when
//! the host's address changes. The token is what lets a friend in. Both are
//! created once and kept in the helper's private state folder.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use iroh::SecretKey;
use std::{fs, io::Write, path::{Path, PathBuf}};

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
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let mut file = options.open(path)
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

/// Load the established pair as one identity. Missing halves (or both halves
/// after the public pin was saved) are recovery errors, never a new invitation.
pub fn load_host_identity(state: &Path) -> Result<(SecretKey, String)> {
    host_identity(state, true, None)
}

fn host_identity(state: &Path, allow_create: bool, expected: Option<&str>) -> Result<(SecretKey, String)> {
    let key_path = state.join("host.key");
    let token_path = state.join("admission.token");
    let pin_path = state.join("host.identity");
    let key_exists = key_path.try_exists()?;
    let token_exists = token_path.try_exists()?;
    let pin_exists = pin_path.try_exists()?;
    if !key_exists && !token_exists && !pin_exists {
        anyhow::ensure!(allow_create, "Established host keys are missing; recovery required");
        create_new(&key_path, &random_bytes::<32>()?)?;
        create_new(&token_path, new_token()?.as_bytes())?;
    } else if !key_exists || !token_exists {
        bail!("Established host keys are missing; existing identity retained");
    }
    // Read-only loads: a concurrent deletion cannot trigger key generation.
    let bytes: [u8; 32] = read_limited(&key_path, 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the host key file is damaged"))?;
    let key = SecretKey::from_bytes(&bytes);
    let token = String::from_utf8(read_limited(&token_path, 128)?)?
        .trim()
        .to_owned();
    anyhow::ensure!(
        crate::protocol::valid_token(&token),
        "the admission token file is damaged"
    );
    let pin = key.public().to_string();
    anyhow::ensure!(expected.is_none_or(|expected| expected == pin),"Destination transport key changed");
    if pin_exists {
        anyhow::ensure!(
            read_limited(&pin_path, 64)? == pin.as_bytes(),
            "host identity does not match its saved public pin"
        );
    } else {
        create_new(&pin_path, pin.as_bytes())?;
    }
    Ok((key, token))
}

fn direct(path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_absolute(), "Absolute private state directory required");
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(info) => {
                let redirected = info.file_type().is_symlink();
                #[cfg(windows)]
                let redirected = {
                    use std::os::windows::fs::MetadataExt;
                    redirected || info.file_attributes() & 0x400 != 0
                };
                anyhow::ensure!(!redirected, "Redirected transport identity refused");
                #[cfg(unix)]
                if info.is_file() {
                    use std::os::unix::fs::MetadataExt;
                    anyhow::ensure!(info.nlink() == 1, "Linked transport identity refused");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn valid_endpoint(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The public pin lives outside the key folder. Losing that folder must not
/// silently replace a prepared target with a different endpoint. Resume/start
/// always supplies the independently stored target pin and cannot create keys.
pub fn scoped_host_identity(
    state: &Path, scope: &str, expected: Option<&str>, prepare: bool,
) -> Result<(PathBuf, SecretKey, String)> {
    anyhow::ensure!(scope.len() == 36 && scope.bytes().enumerate().all(|(i,b)|
        if matches!(i,8|13|18|23) { b == b'-' } else { b.is_ascii_digit() || (b'a'..=b'f').contains(&b) }),
        "Canonical transport scope required");
    anyhow::ensure!(expected.is_none_or(valid_endpoint), "Invalid expected transport identity");
    anyhow::ensure!(prepare || expected.is_some(), "Established transport pin required");
    let root = state.join("prepared-hosts");
    let folder = root.join(scope);
    let pin = root.join(format!("{scope}.public"));
    for path in [&folder, &pin, &folder.join("host.key"), &folder.join("admission.token"), &folder.join("host.identity")] {
        direct(path)?;
    }
    let pinned = if pin.try_exists()? {
        let value = String::from_utf8(read_limited(&pin,64)?)?;
        anyhow::ensure!(valid_endpoint(&value), "Invalid saved transport pin");
        Some(value)
    } else { None };
    if let (Some(pin),Some(expected)) = (pinned.as_deref(),expected) {
        anyhow::ensure!(pin == expected, "Destination transport identity differs");
    }
    let fresh = prepare && expected.is_none() && pinned.is_none() && !folder.try_exists()?;
    if fresh {
        fs::create_dir_all(&root)?;
        fs::create_dir(&folder)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root,fs::Permissions::from_mode(0o700))?;
            fs::set_permissions(&folder,fs::Permissions::from_mode(0o700))?;
        }
    }
    let (key,token) = host_identity(&folder,fresh,expected.or(pinned.as_deref()))?;
    let id = key.public().to_string();
    anyhow::ensure!(expected.is_none_or(|expected| expected == id)
        && pinned.as_ref().is_none_or(|pin| *pin == id), "Destination transport key changed");
    if pinned.is_none() { create_new(&pin,id.as_bytes())?; }
    Ok((folder,key,token))
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
    fn established_host_pair_refuses_missing_private_files_instead_of_changing_invitations() {
        let dir = scratch("pinned-pair");
        let (key, token) = load_host_identity(&dir).unwrap();
        let (again, again_token) = load_host_identity(&dir).unwrap();
        assert_eq!(key.public(), again.public());
        assert_eq!(token, again_token);
        fs::remove_file(dir.join("host.key")).unwrap();
        assert!(load_host_identity(&dir).is_err());
        assert!(!dir.join("host.key").exists());
        fs::remove_file(dir.join("admission.token")).unwrap();
        assert!(load_host_identity(&dir).is_err());
        assert!(!dir.join("host.key").exists());
        assert!(!dir.join("admission.token").exists());
        assert_eq!(
            fs::read_to_string(dir.join("host.identity")).unwrap(),
            key.public().to_string()
        );
        let _ = fs::remove_dir_all(dir);
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
