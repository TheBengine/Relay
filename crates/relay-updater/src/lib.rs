//! Publisher-verified update discovery and staging; never edits application/profile files.
mod cache;
#[cfg(windows)]
pub mod handoff;
mod publisher_policy;
mod storage;
mod transport;
mod worker;

pub use publisher_policy::bundled_publisher;
pub use worker::{Outcome, Poll, Worker};

use chrono::NaiveDateTime;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use storage::Store;
use tough::{Repository, RepositoryLoader, TargetName, Transport};
use transport::{HttpTransport, RootRecorder};

pub type Result<T> = std::result::Result<T, String>;
pub const APP_ID: &str = "3bed570c-afb7-4f36-8a88-8c6f1772d6af";
const MAX_METADATA: u64 = 2 * 1024 * 1024;
const MAX_RELEASE: u64 = 16 * 1024;
const MAX_INSTALLER: u64 = 512 * 1024 * 1024;

/// Set the app's TLS provider before its first HTTP/WSS connection. TUF's TLS
/// dependency also enables AWS-LC; preserve Relay's existing ring selection.
pub fn initialize_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Clone, Debug)]
pub struct Publisher {
    root: Arc<Vec<u8>>,
    metadata: url::Url,
    targets: url::Url,
    channel: String,
    identity: String,
}

impl Publisher {
    pub fn from_bundled_root(
        root: Vec<u8>,
        metadata: &str,
        targets: &str,
        channel: &str,
    ) -> Result<Self> {
        if root.is_empty()
            || root.len() as u64 > MAX_METADATA
            || !matches!(channel, "stable" | "beta")
        {
            return Err("The publisher configuration is incomplete.".into());
        }
        let metadata = transport::base_url(metadata)?;
        let targets = transport::base_url(targets)?;
        let signed: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_slice(&root).map_err(|e| e.to_string())?;
        signed
            .signed
            .verify_role(&signed)
            .map_err(|e| e.to_string())?;
        let role = signed
            .signed
            .roles
            .get(&tough::schema::RoleType::Root)
            .ok_or("Missing publisher root role.")?;
        if role.threshold.get() < 2 {
            return Err("Publisher roots require at least two independent signatures.".into());
        }
        // Different key IDs can still describe the same public key with extra
        // metadata. Such aliases must not satisfy the independent-root policy.
        let mut root_keys = BTreeSet::new();
        for id in &role.keyids {
            let key = signed
                .signed
                .keys
                .get(id)
                .ok_or("Missing publisher root key.")?;
            let (kind, public): (&str, &[u8]) = match key {
                tough::schema::key::Key::Rsa { keyval, .. } => ("rsa", keyval.public.as_ref()),
                tough::schema::key::Key::Ed25519 { keyval, .. } => {
                    ("ed25519", keyval.public.as_ref())
                }
                tough::schema::key::Key::Ecdsa { keyval, .. }
                | tough::schema::key::Key::EcdsaOld { keyval, .. } => {
                    ("ecdsa", keyval.public.as_ref())
                }
            };
            if !root_keys.insert((kind, public.to_vec())) {
                return Err("Publisher root key aliases are not independent signing keys.".into());
            }
        }
        let identity = digest(
            &serde_json::to_vec(&(digest(&root), metadata.as_str(), targets.as_str(), channel))
                .map_err(|e| e.to_string())?,
        );
        Ok(Self {
            root: Arc::new(root),
            metadata,
            targets,
            channel: channel.into(),
            identity,
        })
    }

    /// Identifies the bundled root bytes, HTTPS bases and release channel.
    /// This digest is a receipt binding, not a substitute for publisher identity.
    pub fn policy_identity(&self) -> &str {
        &self.identity
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub schema: u32,
    pub app_id: String,
    pub platform: String,
    pub channel: String,
    pub version: String,
    pub sequence: String,
    pub installer_target: String,
    pub installer_sha256: String,
    pub installer_bytes: u64,
    pub notes: String,
    /// Only this declared protocol may be launched by Relay. Older releases
    /// without a protocol can still be downloaded and viewed manually.
    #[serde(default)]
    pub handoff: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InstalledRelease {
    pub version: String,
    pub sequence: String,
}

fn sequence(value: &str) -> Result<()> {
    if value.len() != 14
        || !value.bytes().all(|b| b.is_ascii_digit())
        || NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S").is_err()
    {
        return Err("Invalid release build identifier.".into());
    }
    Ok(())
}

impl Release {
    fn validate(
        &self,
        publisher: &Publisher,
        installed: &InstalledRelease,
        previous: Option<&Release>,
    ) -> Result<()> {
        sequence(&self.sequence)?;
        sequence(&installed.sequence)?;
        let version = semver::Version::parse(&self.version).map_err(|e| e.to_string())?;
        let current = semver::Version::parse(&installed.version).map_err(|e| e.to_string())?;
        if self.schema != 1
            || self.app_id != APP_ID
            || self.platform != "windows-x64"
            || self.channel != publisher.channel
            || self.notes.len() > 4096
            || self.installer_bytes == 0
            || self.installer_bytes > MAX_INSTALLER
            || self.installer_sha256.len() != 64
            || !self
                .installer_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.installer_target
                != format!(
                    "windows-x64/{}/Relay-{}-{}-Setup.exe",
                    self.channel, self.version, self.sequence
                )
            || (publisher.channel == "stable" && !version.pre.is_empty())
            || self.version.len() > 64
            || !version.build.is_empty()
            || self
                .handoff
                .as_deref()
                .is_some_and(|value| value != "wait-parent-v1")
        {
            return Err("The update does not match this app, platform or release channel.".into());
        }
        if version < current || self.sequence < installed.sequence {
            return Err("An older release was refused.".into());
        }
        if self.sequence == installed.sequence && self.version != installed.version {
            return Err("The installed build identifier has a different version.".into());
        }
        if let Some(old) = previous {
            if self.sequence < old.sequence
                || version < semver::Version::parse(&old.version).map_err(|e| e.to_string())?
            {
                return Err("The update feed moved back to an older release.".into());
            }
            if self.sequence == old.sequence && self != old {
                return Err("This build identifier has different release contents.".into());
            }
        }
        Ok(())
    }
}

pub struct Engine {
    publisher: Publisher,
    installed: InstalledRelease,
    directory: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Current,
    Available(Box<Release>),
}

#[derive(Clone, Debug)]
pub struct PreparedInstaller {
    path: PathBuf,
    release: Release,
    valid_until: i64,
}
impl PreparedInstaller {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn release(&self) -> &Release {
        &self.release
    }
    /// Viewing this folder is not approval to execute its files.
    pub fn download_directory(&self) -> Result<&Path> {
        storage::guard(&self.path)?;
        self.path
            .parent()
            .ok_or_else(|| "The download folder is unavailable.".into())
    }
    /// Call again immediately before handoff. No process is started by this library.
    pub fn revalidate(&self) -> Result<()> {
        if chrono::Utc::now().timestamp() >= self.valid_until {
            return Err("Update approval has expired. Check again.".into());
        }
        storage::guard(&self.path)?;
        let (bytes, hash) = storage::file_digest(&self.path, MAX_INSTALLER)?;
        if bytes != self.release.installer_bytes || hash != self.release.installer_sha256 {
            return Err("The downloaded installer changed.".into());
        }
        Ok(())
    }
}

impl Engine {
    pub fn new(
        publisher: Publisher,
        installed: InstalledRelease,
        directory: PathBuf,
    ) -> Result<Self> {
        sequence(&installed.sequence)?;
        semver::Version::parse(&installed.version).map_err(|e| e.to_string())?;
        storage::guard(&directory)?;
        Ok(Self {
            publisher,
            installed,
            directory,
        })
    }
    pub async fn check(&self) -> Result<Check> {
        let transport = HttpTransport::new(&self.publisher)?;
        self.check_with(transport).await
    }
    pub async fn download(&self, approved: &Release) -> Result<PreparedInstaller> {
        let transport = HttpTransport::new(&self.publisher)?;
        self.download_with(approved, transport).await
    }
    async fn load<T: Transport + Clone + 'static>(
        &self,
        store: &Store,
        transport: T,
    ) -> Result<(Repository, tempfile::TempDir)> {
        let candidate = store.candidate()?;
        // Offline revalidation of each cached root transition keeps the bundle as the trust origin.
        let trusted = storage::cached_root(&self.publisher.root, candidate.path())?;
        let roots = Arc::new(Mutex::new(BTreeMap::new()));
        let recorder = RootRecorder::new(transport, self.publisher.metadata.clone(), roots.clone());
        let repository = tokio::time::timeout(
            Duration::from_secs(45),
            RepositoryLoader::new(
                &trusted,
                self.publisher.metadata.clone(),
                self.publisher.targets.clone(),
            )
            .datastore(candidate.path())
            .transport(recorder)
            .expiration_enforcement(tough::ExpirationEnforcement::Safe)
            .limits(tough::Limits {
                max_root_size: MAX_METADATA,
                max_targets_size: MAX_METADATA,
                max_timestamp_size: MAX_METADATA,
                max_snapshot_size: MAX_METADATA,
                max_root_updates: 32,
            })
            .load(),
        )
        .await
        .map_err(|_| "The update check timed out.")?
        .map_err(|e| e.to_string())?;
        if repository.targets().signed.delegations.is_some() {
            return Err("Delegated update targets are not enabled for this channel.".into());
        }
        // The library's max_targets_size can be overridden by a signed snapshot length.
        // Our transport independently caps every metadata response.
        for (version, bytes) in roots.lock().map_err(|_| "Root recording failed.")?.iter() {
            let path = candidate.path().join(format!("chain-{version}.json"));
            storage::guard(&path)?;
            std::fs::write(path, bytes).map_err(|e| e.to_string())?;
        }
        let root = storage::cached_root(&self.publisher.root, candidate.path())?;
        let parsed: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_slice(&root).map_err(|e| e.to_string())?;
        if serde_json::to_value(parsed).map_err(|e| e.to_string())?
            != serde_json::to_value(repository.root()).map_err(|e| e.to_string())?
        {
            return Err(
                "The persisted publisher root chain does not match the verified repository.".into(),
            );
        }
        Ok((repository, candidate))
    }
    async fn release(
        &self,
        repository: &Repository,
        previous: Option<&Release>,
    ) -> Result<Release> {
        let name = TargetName::new(format!(
            "windows-x64/{}/release.json",
            self.publisher.channel
        ))
        .map_err(|e| e.to_string())?;
        let target = repository
            .targets()
            .signed
            .find_target(&name, false)
            .map_err(|e| e.to_string())?;
        if target.length > MAX_RELEASE {
            return Err("Release information is too large.".into());
        }
        let mut stream = repository
            .read_target(&name)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Release information is missing.")?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.try_next().await.map_err(|e| e.to_string())? {
            if bytes.len() + chunk.len() > MAX_RELEASE as usize {
                return Err("Release information is too large.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        // Only parse after EOF: tough authenticates the target at the end of its stream.
        let release: Release = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        release.validate(&self.publisher, &self.installed, previous)?;
        let installer = TargetName::new(&release.installer_target).map_err(|e| e.to_string())?;
        let target = repository
            .targets()
            .signed
            .find_target(&installer, false)
            .map_err(|e| e.to_string())?;
        if target.length != release.installer_bytes
            || target.hashes.sha256.as_ref() != decode_hash(&release.installer_sha256)?.as_slice()
        {
            return Err("Release information does not match the signed installer target.".into());
        }
        Ok(release)
    }
    async fn check_with<T: Transport + Clone + 'static>(&self, transport: T) -> Result<Check> {
        let store = Store::open(&self.directory, &self.publisher.identity)?;
        let (repository, candidate) = self.load(&store, transport).await?;
        let release = tokio::time::timeout(
            Duration::from_secs(30),
            self.release(&repository, store.head().and_then(|h| h.highest.as_ref())),
        )
        .await
        .map_err(|_| "Release information timed out.")??;
        let current = release.sequence == self.installed.sequence;
        store.commit(candidate, &release)?;
        Ok(if current {
            Check::Current
        } else {
            Check::Available(Box::new(release))
        })
    }
    async fn download_with<T: Transport + Clone + 'static>(
        &self,
        approved: &Release,
        transport: T,
    ) -> Result<PreparedInstaller> {
        let store = Store::open(&self.directory, &self.publisher.identity)?;
        let (repository, candidate) = self.load(&store, transport).await?;
        let release = tokio::time::timeout(
            Duration::from_secs(30),
            self.release(&repository, store.head().and_then(|h| h.highest.as_ref())),
        )
        .await
        .map_err(|_| "Release information timed out.")??;
        if &release != approved || release.sequence <= self.installed.sequence {
            return Err("The offered update changed. Check again before installing.".into());
        }
        let valid_until = [
            repository.root().signed.expires.as_second(),
            repository.timestamp().signed.expires.as_second(),
            repository.snapshot().signed.expires.as_second(),
            repository.targets().signed.expires.as_second(),
        ]
        .into_iter()
        .min()
        .ok_or("No metadata expiration.")?;
        // Advance the authenticated metadata/observed-release rollback floor
        // before payload IO. An interrupted payload is not an installed release,
        // but must not make already verified newer metadata disappear.
        store.checkpoint(&candidate, &release)?;
        let target = TargetName::new(&release.installer_target).map_err(|e| e.to_string())?;
        let downloads = store.downloads()?;
        let mut cache = cache::Cache::open(downloads.clone(), &self.publisher.identity)?;
        let destination = downloads.join(format!(
            "Relay-{}-{}-Setup.exe",
            release.version, release.sequence
        ));
        storage::guard(&destination)?;
        if destination.exists() {
            let ready = PreparedInstaller {
                path: destination,
                release,
                valid_until,
            };
            ready.revalidate()?;
            cache.record(&ready.release, &ready.path)?;
            return Ok(ready);
        }
        let mut temporary = cache.stage(&release)?;
        let operation = async {
            let stream = repository
                .read_target(&target)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("The installer is unavailable.")?;
            futures_util::pin_mut!(stream);
            let mut count = 0u64;
            while let Some(chunk) = stream.try_next().await.map_err(|e| e.to_string())? {
                count = count
                    .checked_add(chunk.len() as u64)
                    .ok_or("Installer length overflow.")?;
                if count > release.installer_bytes || count > MAX_INSTALLER {
                    return Err("The installer exceeded its signed size.".into());
                }
                temporary
                    .file()
                    .write_all(&chunk)
                    .map_err(|e| e.to_string())?;
                storage::checkpoint("download-partial");
            }
            if count != release.installer_bytes {
                return Err("The installer download was incomplete.".into());
            }
            temporary.file().flush().map_err(|e| e.to_string())?;
            temporary
                .file()
                .as_file()
                .sync_all()
                .map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        };
        tokio::time::timeout(Duration::from_secs(120), operation)
            .await
            .map_err(|_| "The installer download timed out.")??;
        let (count, hash) = storage::file_digest(temporary.path(), MAX_INSTALLER)?;
        if count != release.installer_bytes || hash != release.installer_sha256 {
            return Err("The installer could not be verified.".into());
        }
        storage::guard(&destination)?;
        if destination.exists() {
            if storage::file_digest(&destination, MAX_INSTALLER)? != (count, hash) {
                return Err("The existing staged installer has different bytes.".into());
            }
        } else {
            temporary.persist(&destination)?;
        }
        cache.record(&release, &destination)?;
        let ready = PreparedInstaller {
            path: destination,
            release,
            valid_until,
        };
        ready.revalidate()?;
        Ok(ready)
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn decode_hash(value: &str) -> Result<Vec<u8>> {
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests;
