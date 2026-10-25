//! Public, immutable delegation records. These are hints until their source
//! signature matches a separately trusted pin. No session or provider secret.
use super::*;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 16 * 1024;
const MAX_RECORDS: usize = 128;
pub(super) const STORAGE: &str =
    "Relay could not retain the signed host move. Check this profile's storage and try again.";

pub(super) struct Cache {
    root: PathBuf,
}
fn direct(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(STORAGE.into());
    }
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(info) => {
                let mut redirected = info.file_type().is_symlink();
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    redirected |= info.file_attributes() & 0x400 != 0;
                }
                if redirected {
                    return Err(STORAGE.into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(STORAGE.into()),
        }
    }
    Ok(())
}
fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    direct(path)?;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(STORAGE.into()),
    };
    let info = file.metadata().map_err(|_| STORAGE)?;
    if !info.is_file() || info.len() > MAX_BYTES {
        return Err(STORAGE.into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| STORAGE)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(STORAGE.into());
    }
    Ok(Some(bytes))
}
impl Cache {
    pub(super) fn for_profile() -> Result<Self, String> {
        let base = std::env::var_os("RELAY_PROFILE_DIR")
            .or_else(|| std::env::var_os("RELAY_CONFIG_DIR"))
            .map(PathBuf::from)
            .or_else(|| {
                directories::ProjectDirs::from("org", "Relay", "Relay")
                    .map(|v| v.config_dir().to_owned())
            })
            .ok_or(STORAGE)?;
        Ok(Self {
            root: base.join("host-moves"),
        })
    }
    #[cfg(test)]
    pub(super) fn at(root: PathBuf) -> Self {
        Self { root }
    }
    fn path(&self, source: &str, pin: &RoutePin, suffix: &str) -> Result<PathBuf, String> {
        if !valid_pin(pin) {
            return Err(REFUSED.into());
        }
        let source = crate::community::normalize_url(source)?;
        let mut hash = Sha256::new();
        hash.update(b"relay-signed-host-move-v1\0");
        for part in [
            source.as_str(),
            pin.server_id.as_str(),
            pin.host.host_id.as_str(),
            pin.host.public_key.as_str(),
            &pin.term.to_string(),
        ] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        Ok(self.root.join(format!("{:x}.{suffix}", hash.finalize())))
    }
}
impl Delegations for Cache {
    fn load(&self, source: &str, pin: &RoutePin) -> Result<Option<model::ActivationGrant>, String> {
        let Some(bytes) = read(&self.path(source, pin, "json")?)? else {
            return Ok(None);
        };
        let found: model::ActivationGrant = serde_json::from_slice(&bytes).map_err(|_| REFUSED)?;
        if !grant(pin, &found) {
            return Err(REFUSED.into());
        }
        Ok(Some(found))
    }
    fn store(
        &mut self,
        source: &str,
        pin: &RoutePin,
        value: &model::ActivationGrant,
    ) -> Result<(), String> {
        if !grant(pin, value) {
            return Err(REFUSED.into());
        }
        direct(&self.root)?;
        fs::create_dir_all(&self.root).map_err(|_| STORAGE)?;
        let lock_path = self.root.join("cache.lock");
        direct(&lock_path)?;
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|_| STORAGE)?;
        lease.try_lock().map_err(|_| STORAGE)?;
        if let Some(old) = self.load(source, pin)? {
            return if old == *value {
                Ok(())
            } else {
                Err(REFUSED.into())
            };
        }
        // A bounded scan of this owned cache only; never walk a child directory.
        let mut records = 0;
        for (index, entry) in fs::read_dir(&self.root)
            .map_err(|_| STORAGE)?
            .take(MAX_RECORDS * 2 + 3)
            .enumerate()
        {
            if index >= MAX_RECORDS * 2 + 2 {
                return Err(STORAGE.into());
            }
            let entry = entry.map_err(|_| STORAGE)?;
            if entry.file_name().to_string_lossy().ends_with(".json") {
                records += 1;
            }
            if records >= MAX_RECORDS {
                return Err(STORAGE.into());
            }
        }
        let path = self.path(source, pin, "json")?;
        let pending = self.path(source, pin, "pending")?;
        let bytes = serde_json::to_vec(value).map_err(|_| STORAGE)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(STORAGE.into());
        }
        if let Some(old) = read(&pending)? {
            // Only this exact scratch name is replaceable. A different complete,
            // valid source grant is evidence of a conflict and is kept intact.
            if let Ok(previous) = serde_json::from_slice::<model::ActivationGrant>(&old) {
                if grant(pin, &previous) && previous != *value {
                    return Err(REFUSED.into());
                }
            }
            fs::remove_file(&pending).map_err(|_| STORAGE)?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&pending).map_err(|_| STORAGE)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| STORAGE)?;
        drop(file);
        fs::hard_link(&pending, &path).map_err(|_| STORAGE)?;
        fs::remove_file(&pending).map_err(|_| STORAGE)?;
        #[cfg(unix)]
        fs::File::open(&self.root)
            .and_then(|v| v.sync_all())
            .map_err(|_| STORAGE)?;
        Ok(())
    }
}
