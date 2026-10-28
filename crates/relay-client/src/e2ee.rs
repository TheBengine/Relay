//! The private device actor and protected local state. HTTP never receives keys.
use crate::invite_proof::{self, InviteProof};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use relay_crypto::{Device, Identity, PublicDevice};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use zeroize::Zeroizing;

pub const MESSAGE_PREFIX: &str = "relay-e2ee:v1:";
pub(crate) fn check_control_delivery(
    batch: &relay_protocol::AppendCryptoEvents,
) -> Result<(), String> {
    let oversized = || {
        "This key update exceeds the server's delivery limit. Your protected encryption state was preserved.".to_string()
    };
    if batch.events.len() > 66
        || serde_json::to_vec(batch).map_err(|e| e.to_string())?.len()
            > relay_protocol::CRYPTO_MAX_OPERATION_JSON_BYTES
    {
        return Err(oversized());
    }
    let mut total = 0usize;
    for event in &batch.events {
        let bytes = STANDARD
            .decode(&event.body)
            .map_err(|_| "Invalid saved key update body.".to_string())?;
        if bytes.len() > relay_protocol::CRYPTO_MAX_EVENT_DECODED_BYTES {
            return Err(oversized());
        }
        total += bytes.len();
        if total > relay_protocol::CRYPTO_MAX_OPERATION_DECODED_BYTES {
            return Err(oversized());
        }
    }
    Ok(())
}
#[cfg(all(test, windows))]
#[path = "e2ee_capacity_tests.rs"]
mod capacity_tests;
#[cfg(all(test, windows))]
#[path = "e2ee_delivery_tests.rs"]
mod delivery_tests;
#[cfg(all(test, windows))]
#[path = "e2ee_invitation_tests.rs"]
mod invitation_tests;
#[cfg(all(test, windows))]
#[path = "e2ee_trust_tests.rs"]
mod trust_tests;
#[cfg(all(test, windows))]
#[path = "e2ee_tests.rs"]
mod tests;
const MAX_PRIVATE_BYTES: usize = 64 * 1024 * 1024;
#[path = "e2ee_call.rs"]
mod call;
pub(crate) use call::device_not_approved;
#[cfg(test)]
pub(crate) use call::VERIFY_BEFORE_CALL;
#[path = "e2ee_history.rs"]
mod history;
#[path = "e2ee_history_cache.rs"]
mod history_cache;
const PREFERENCES_MAGIC: &[u8] = b"RLYPREF1";
pub fn protect_local_preferences(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Local preferences exceed their protected storage limit.".into());
    }
    let mut wire = PREFERENCES_MAGIC.to_vec();
    wire.extend(protected::protect(bytes, "Relay/preferences/v1")?);
    Ok(wire)
}
pub fn unprotect_local_preferences(wire: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
    if wire.len() > 4 * 1024 * 1024 + 1024 * 1024 {
        return Err("Local preferences exceed their protected storage limit.".into());
    }
    if let Some(blob) = wire.strip_prefix(PREFERENCES_MAGIC) {
        protected::unprotect(blob, "Relay/preferences/v1")
    } else if wire.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{') {
        // Legacy JSON is accepted for a one-time in-memory migration only.
        Ok(Zeroizing::new(wire.to_vec()))
    } else {
        Err("Local preferences could not be unlocked; the existing file was preserved.".into())
    }
}

#[derive(Clone)]
pub struct SenderKeys {
    pub device_id: String,
    pub opus: Zeroizing<[u8; 32]>,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub rtc: Zeroizing<[u8; 32]>,
}
pub struct CallKeys {
    pub group_id: String,
    pub scope: String,
    pub epoch: u64,
    pub local_user_id: String,
    pub local_device_id: String,
    pub sender_keys: HashMap<String, SenderKeys>,
    // In-memory replay state belongs to verified key material, not a UDP worker.
    // It is never serialized with device keys or copied as a recovery snapshot.
    pub(crate) voice_replay: Mutex<crate::media::VoiceReplay>,
}
impl std::fmt::Debug for SenderKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SenderKeys([redacted])")
    }
}
impl std::fmt::Debug for CallKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallKeys")
            .field("epoch", &self.epoch)
            .field("verified_devices", &self.sender_keys.len())
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct CryptoUiStatus {
    pub own_device_id: String,
    pub own_fingerprint: String,
    pub peers: Vec<CryptoPeerStatus>,
    pub own_devices: Vec<CryptoPeerStatus>,
    pub status: String,
    /// Presentation only: local verification is not proof that an earlier
    /// encryption operation recovered. Never infer this from diagnostic text.
    pub problem: Option<String>,
    pub supported: bool,
    pub history: Option<HistoryUsage>,
    /// Devices of other people approved because they proved the secret of the
    /// invitation they joined through (shown as "Verified via invitation").
    pub via_invitation: Vec<String>,
    /// Devices whose invitation proof did not check out; they stay unverified.
    pub invitation_unconfirmed: Vec<String>,
    /// Key changes and new own devices to tell the user about, until dismissed.
    /// Changes that need the user's decision are `CryptoPeerStatus::needs_review`.
    pub notices: Vec<TrustNotice>,
}

/// Something about another device's key the user should know. It never blocks
/// messaging by itself; see `CryptoPeerStatus::needs_review` for that.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustNotice {
    pub user_id: String,
    pub device_id: String,
    pub fingerprint: String,
    pub kind: TrustNoticeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustNoticeKind {
    /// A device of someone not verified here presented a new key, which is
    /// now trusted in place of the old one.
    CodeChanged,
    /// Another device was added to this account.
    NewOwnDevice,
}

const MAX_TRUST_NOTICES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CryptoProblemSource {
    Initialization,
    CallInbox,
    Directory,
    Conversation(String),
    DeviceAction(String),
    Call(String),
    Other,
}
const MAX_UI_PROBLEMS: usize = 32;
const MAX_UI_PROBLEM_SOURCE_BYTES: usize = 4096;
const MAX_UI_PROBLEM_TEXT_BYTES: usize = 2048;
const UI_PROBLEM_OVERFLOW: &str =
    "Several encryption checks still need attention. Restart Relay before checking again.";

struct CryptoProblem {
    source: CryptoProblemSource,
    generation: u64,
    text: String,
}

impl CryptoProblemSource {
    fn exceeds_ui_limit(&self) -> bool {
        match self {
            Self::Conversation(scope) | Self::DeviceAction(scope) | Self::Call(scope) => {
                scope.len() > MAX_UI_PROBLEM_SOURCE_BYTES
            }
            _ => false,
        }
    }
}
#[derive(Clone, Debug)]
pub struct HistoryUsage {
    pub archived_bytes: u64,
    pub byte_limit: u64,
    pub segments: usize,
    pub segment_limit: usize,
    pub records: usize,
}
#[derive(Clone, Debug)]
pub struct CryptoPeerStatus {
    pub user_id: String,
    pub name: String,
    pub device_id: String,
    pub fingerprint: String,
    /// Security codes were compared (or an invitation proof checked out) for
    /// exactly this key. Shown as the "Verified" badge.
    pub verified: bool,
    /// This key is trusted, verified or trusted on first use.
    pub trusted: bool,
    /// An earlier key of this device is trusted here, not this one.
    pub changed: bool,
    /// Nothing is shared with this device until the user accepts its new code
    /// or verifies it: the person was verified by hand.
    pub needs_review: bool,
}

/// The trust flags of one listed device, as this device sees it.
pub(crate) fn peer_trust(device: &Device, public: &PublicDevice) -> (bool, bool, bool, bool) {
    let trust = device.trust(public);
    let changed = trust.is_none() && device.expected_fingerprint(&public.identity.device_id).is_some();
    let needs_review = trust.is_none()
        && device.first_use_decision(public) == relay_crypto::FirstUse::Held;
    (
        trust == Some(relay_crypto::Trust::Verified),
        trust.is_some(),
        changed,
        needs_review,
    )
}
#[derive(Clone, Debug)]
pub enum MessageProtection {
    Protected,
    Legacy,
    Unavailable(String),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EncryptedAttachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub file: relay_crypto::files::FileKey,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum MessageAction {
    Reaction {
        message_id: String,
        target_client_id: Option<String>,
        emoji: String,
        active: bool,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub scope: String,
    pub client_id: String,
    pub content: String,
    pub reply_to_id: Option<String>,
    pub mentions: Vec<String>,
    pub attachments: Vec<EncryptedAttachment>,
    #[serde(default)]
    pub action: Option<MessageAction>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct CachedEnvelope {
    pub author: Identity,
    pub envelope: Envelope,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub sequence: u64,
    /// Local order of this ciphertext's first authenticated delivery.
    /// Re-reading an older cached version does not make it current again.
    #[serde(default)]
    pub delivery_revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PrivateState {
    version: u32,
    revision: u64,
    identity: Identity,
    crypto: Vec<u8>,
    advertised: Option<PublicDevice>,
    cursors: HashMap<String, u64>,
    cached: HashMap<String, CachedEnvelope>,
    outbox: HashMap<String, (String, String)>,
    files: HashMap<String, EncryptedAttachment>,
    #[serde(default)]
    control_outbox: HashMap<String, relay_protocol::AppendCryptoEvents>,
    #[serde(default)]
    pending_pool: Vec<PublicDevice>,
    #[serde(default)]
    application_cursors: HashMap<String, u64>,
    #[serde(default)]
    consumption_outbox: Vec<relay_protocol::ConsumeCryptoKeyPackage>,
    #[serde(default)]
    completed: HashMap<String, (String, String)>,
    #[serde(default)]
    completed_order: Vec<String>,
    #[serde(default)]
    revocation_outbox: Vec<String>,
    #[serde(default)]
    history: Vec<history::Segment>,
    #[serde(default)]
    delivery_clock: u64,
    #[serde(default)]
    call: call::CallState,
    /// One-time secrets of the invitations this device created (host side).
    #[serde(default)]
    invitations: Vec<InviteRecord>,
    /// Device IDs approved through an invitation proof.
    #[serde(default)]
    via_invitation: Vec<String>,
    /// Key changes not yet dismissed by the user.
    #[serde(default)]
    trust_notices: Vec<TrustNotice>,
    /// Set after the first complete directory read. Before it, every device is
    /// a first contact and no "new device on your account" notice is shown.
    #[serde(default)]
    trust_started: bool,
}

const MAX_ACTIVE_INVITATIONS: usize = 64;
const MAX_VIA_INVITATION: usize = 256;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A host-side record of a one-time invitation secret. Never printed.
#[derive(Clone, Serialize, Deserialize)]
struct InviteRecord {
    /// The server's invitation ID.
    id: String,
    /// Unpadded URL-safe base64 of the 32-byte secret.
    secret: String,
    expires: u64,
    used: bool,
}

/// A short-lived retry of the exact public identity a joined invitation named.
/// No invitation secret/MAC, persistence or trust override lives here.
#[derive(Clone)]
struct PendingInvitedHost {
    device_id: String,
    fingerprint: String,
    deadline: std::time::Instant,
}

pub struct CryptoSession {
    pub device: Device,
    state: PrivateState,
    path: PathBuf,
    pub directory: HashMap<String, PublicDevice>,
    pub status: CryptoUiStatus,
    problems: Vec<CryptoProblem>,
    problem_overflow: bool,
    problem_generation: u64,
    pending_invited_host: Option<PendingInvitedHost>,
    history_cache: history_cache::Cache,
    call_contexts: Mutex<HashMap<String, Arc<CallKeys>>>,
    // Freshness belongs to this actor lifetime, never to a restored checkpoint.
    fresh_call_scopes: HashSet<String>,
    call_refresh_operations: HashMap<String, String>,
    call_challenges: HashMap<String, [u8; 32]>,
    call_challenge_attempts: HashMap<String, std::time::Instant>,
    call_inbox_cursor: usize,
    _lock: std::fs::File,
    #[cfg(windows)]
    _binding_lease: protected::BindingLease,
}

impl CryptoSession {
    pub(crate) fn problem_generation(&self) -> u64 {
        self.problem_generation
    }

    pub(crate) fn note_problem(&mut self, source: CryptoProblemSource, error: &str) {
        let Some(generation) = self.problem_generation.checked_add(1) else {
            self.problem_overflow = true;
            self.publish_problem_summary();
            return;
        };
        self.problem_generation = generation;
        if source.exceeds_ui_limit() {
            self.problem_overflow = true;
        } else {
            // Replace only this source. Other unresolved operations remain
            // tracked even when a later operation fails and then recovers.
            if let Some(index) = self.problems.iter().position(|p| p.source == source) {
                self.problems.remove(index);
            }
            if self.problems.len() < MAX_UI_PROBLEMS {
                let mut end = error.len().min(MAX_UI_PROBLEM_TEXT_BYTES);
                while !error.is_char_boundary(end) {
                    end -= 1;
                }
                self.problems.push(CryptoProblem {
                    source,
                    generation,
                    text: error[..end].into(),
                });
            } else {
                // Never evict an unresolved source to show a false all-clear.
                // This actor-local overflow latch clears only with a new actor.
                self.problem_overflow = true;
            }
        }
        self.publish_problem_summary();
    }

    fn publish_problem_summary(&mut self) {
        self.status.problem = if self.problem_overflow {
            Some(UI_PROBLEM_OVERFLOW.into())
        } else {
            self.problems.last().map(|problem| problem.text.clone())
        };
        if let Some(problem) = &self.status.problem {
            self.status.status = problem.clone();
        }
    }

    pub(crate) fn finish_problem_action(
        &mut self,
        source: &CryptoProblemSource,
        generation: u64,
        success: &str,
    ) {
        let mut recovered = false;
        self.problems.retain(|problem| {
            let remove = &problem.source == source && problem.generation <= generation;
            recovered |= remove;
            !remove
        });
        self.publish_problem_summary();
        if self.status.problem.is_none()
            && (recovered
                || matches!(
                    source,
                    CryptoProblemSource::Conversation(_) | CryptoProblemSource::Call(_)
                ))
        {
            self.status.status = success.into();
        }
    }

    fn default_root() -> Result<PathBuf, String> {
        if let Some(root) = std::env::var_os("RELAY_CRYPTO_DIR") {
            let root = PathBuf::from(root);
            if !root.is_absolute() {
                return Err("The encryption storage override must be an absolute path.".into());
            }
            return Ok(root);
        }
        directories::ProjectDirs::from("org", "Relay", "Relay")
            .map(|dirs| dirs.data_local_dir().join("encryption"))
            .ok_or("Cannot locate protected device storage.".into())
    }
    pub fn open(server_id: &str, user_id: &str) -> Result<Self, String> {
        let root = Self::default_root()?;
        Self::open_at(root, server_id, user_id)
    }
    /// Only the explicit, typed UI recovery action may replace a device identity.
    pub fn reset(server_id: &str, user_id: &str) -> Result<Self, String> {
        let root = Self::default_root()?;
        Self::open_at_mode(root, server_id, user_id, true)
    }
    pub fn storage_root(&self) -> Result<PathBuf, String> {
        self.path
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or("Invalid protected device storage.".into())
    }
    #[cfg(test)]
    pub fn reset_at(root: PathBuf, server_id: &str, user_id: &str) -> Result<Self, String> {
        Self::open_at_mode(root, server_id, user_id, true)
    }
    pub fn reset_replacing(
        root: PathBuf,
        server_id: &str,
        user_id: &str,
        retiring_device: Option<&str>,
    ) -> Result<Self, String> {
        Self::open_at_internal(root, server_id, user_id, true, retiring_device)
    }
    fn open_at(root: PathBuf, server_id: &str, user_id: &str) -> Result<Self, String> {
        Self::open_at_mode(root, server_id, user_id, false)
    }
    fn open_at_mode(
        root: PathBuf,
        server_id: &str,
        user_id: &str,
        reset: bool,
    ) -> Result<Self, String> {
        Self::open_at_internal(root, server_id, user_id, reset, None)
    }
    fn open_at_internal(
        root: PathBuf,
        server_id: &str,
        user_id: &str,
        reset: bool,
        retiring_device: Option<&str>,
    ) -> Result<Self, String> {
        let binding = format!("Relay/crypto/v1/{server_id}/{user_id}");
        #[cfg(windows)]
        let binding_lease = protected::lock_binding(&binding)?;
        let path = root.join(format!("{:x}.device", Sha256::digest(binding.as_bytes())));
        std::fs::create_dir_all(&root).map_err(|_| "Cannot create protected device storage.")?;
        let mut lock_options = std::fs::OpenOptions::new();
        lock_options
            .create(true)
            .truncate(false)
            .read(true)
            .write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            lock_options.share_mode(0);
        }
        let lock = lock_options.open(path.with_extension("lock"))
            .map_err(|_| "Another Relay process is using this account's encryption keys. Close it before signing in here.")?;
        if reset && path.exists() {
            let length = std::fs::metadata(&path)
                .map_err(|_| "Cannot preserve the previous protected device state.")?
                .len();
            if length > MAX_PRIVATE_BYTES as u64 + 1024 * 1024 {
                return Err("The previous protected state exceeds its archive limit; it was preserved in place.".into());
            }
            let archive = root.join(format!(
                "{}.{}.retired",
                path.file_stem().unwrap().to_string_lossy(),
                uuid::Uuid::new_v4()
            ));
            let mut source = std::fs::File::open(&path)
                .map_err(|_| "Cannot preserve the previous protected device state.")?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(archive)
                .map_err(|_| "Cannot create the previous device's protected archive.")?;
            std::io::copy(&mut source, &mut output)
                .and_then(|_| output.sync_all())
                .map_err(|_| "Cannot finish the previous device's protected archive.")?;
        }
        let state = if path.exists() && !reset {
            let metadata =
                std::fs::metadata(&path).map_err(|_| "Cannot read encrypted device storage.")?;
            if !metadata.is_file() || metadata.len() > MAX_PRIVATE_BYTES as u64 + 1024 * 1024 {
                return Err("Encrypted device storage exceeds its limit.".into());
            }
            let blob = std::fs::read(&path).map_err(|_| "Cannot read encrypted device storage.")?;
            let plain = protected::unprotect(&blob, &binding)?;
            if plain.len() > MAX_PRIVATE_BYTES {
                return Err("Encrypted device storage exceeds its limit.".into());
            }
            let state: PrivateState = serde_json::from_slice(&plain).map_err(|_| {
                "Cannot read this device's encryption state. Its keys were not reset."
            })?;
            if !matches!(state.version, 1..=3)
                || (state.version != 3
                    && (!state.call.is_empty()
                        || !state.invitations.is_empty()
                        || !state.via_invitation.is_empty()))
                || state.invitations.len() > MAX_ACTIVE_INVITATIONS
                || state.via_invitation.len() > MAX_VIA_INVITATION
                || state.trust_notices.len() > MAX_TRUST_NOTICES
                || state
                    .invitations
                    .iter()
                    .any(|r| r.id.len() != 64 || invite_proof::decode_secret(&r.secret).is_none())
                || !state.call.valid()
                || (state.version == 1 && !state.history.is_empty())
                || state.identity.server_id != server_id
                || state.identity.user_id != user_id
                || state.cached.len() > 8192
                || state.outbox.len() > 128
                || state.files.len() > 1024
                || state.cursors.len() > 128
                || state.control_outbox.len() > 128
                || state.pending_pool.len() > 16
                || state.application_cursors.len() > 128
                || state.consumption_outbox.len() > 128
                || state.completed.len() > 512
                || state.completed_order.len() > 512
                || state.revocation_outbox.len() > 4
            {
                return Err(
                    "This encryption state belongs to another account or is invalid.".into(),
                );
            }
            protected::check_identity(&binding, &state.identity.device_id)?;
            state
        } else {
            if !reset && protected::head(&binding)?.is_some() {
                return Err("This device's encryption state is missing. Restore it or explicitly create a new device; Relay will not reuse its old identity.".into());
            }
            let identity = Identity {
                server_id: server_id.into(),
                user_id: user_id.into(),
                device_id: uuid::Uuid::new_v4().to_string(),
            };
            let device = Device::new(identity.clone())?;
            PrivateState {
                version: 2,
                revision: if reset {
                    protected::head(&binding)?.unwrap_or(0)
                } else {
                    0
                },
                identity,
                crypto: device.checkpoint()?.to_vec(),
                advertised: None,
                cursors: HashMap::new(),
                cached: HashMap::new(),
                outbox: HashMap::new(),
                files: HashMap::new(),
                control_outbox: HashMap::new(),
                pending_pool: Vec::new(),
                application_cursors: HashMap::new(),
                consumption_outbox: Vec::new(),
                completed: HashMap::new(),
                completed_order: Vec::new(),
                revocation_outbox: retiring_device.into_iter().map(str::to_string).collect(),
                history: Vec::new(),
                delivery_clock: 0,
                call: call::CallState::default(),
                invitations: Vec::new(),
                via_invitation: Vec::new(),
                trust_notices: Vec::new(),
                trust_started: false,
            }
        };
        if let Some(head) = protected::head(&binding)? {
            if state.revision < head {
                return Err("An older encryption snapshot was restored. Sending is blocked to prevent key reuse.".into());
            }
        }
        let device = Device::restore(&state.crypto, &state.identity)?;
        history::verify_files(&path, &state.identity, &state.history)?;
        let mut session = Self {
            device,
            state,
            path,
            directory: HashMap::new(),
            status: CryptoUiStatus::default(),
            problems: Vec::new(),
            problem_overflow: false,
            problem_generation: 0,
            pending_invited_host: None,
            history_cache: history_cache::Cache::default(),
            call_contexts: Mutex::default(),
            fresh_call_scopes: HashSet::new(),
            call_refresh_operations: HashMap::new(),
            call_challenges: HashMap::new(),
            call_challenge_attempts: HashMap::new(),
            call_inbox_cursor: 0,
            _lock: lock,
            #[cfg(windows)]
            _binding_lease: binding_lease,
        };
        if session.state.advertised.is_none() {
            session.mutate(|device, state| {
                let public = device.key_package()?;
                state.advertised = Some(public);
                Ok(())
            })?;
        }
        session.status.own_device_id = session.device.identity().device_id.clone();
        session.status.own_fingerprint = session.device.fingerprint()?;
        session.status.supported = true;
        session.status.via_invitation = session.state.via_invitation.clone();
        session.status.notices = session.state.trust_notices.clone();
        session.status.status = "Messages are end-to-end encrypted. Compare security codes any time to verify someone.".into();
        Ok(session)
    }
    #[cfg(test)]
    pub(crate) fn open_for_test(
        root: PathBuf,
        server_id: &str,
        user_id: &str,
    ) -> Result<Self, String> {
        Self::open_at(root, server_id, user_id)
    }
    #[cfg(all(test, windows))]
    pub(crate) fn remove_test_checkpoint(&self) {
        protected::remove_head(&self.binding());
    }
    #[cfg(test)]
    pub(crate) fn checkpoint_revision_for_test(&self) -> u64 {
        self.state.revision
    }
    #[cfg(all(test, windows))]
    pub(crate) fn seed_owned_history_measurement(&mut self, scope: &str, count: usize) {
        let root = PathBuf::from(
            std::env::var_os("RELAY_TEST_WORK_ROOT").expect("Owned fixture root required"),
        );
        assert!(self
            .storage_root()
            .unwrap()
            .canonicalize()
            .unwrap()
            .starts_with(root.canonicalize().unwrap()));
        assert!(matches!(count, 1000 | 16000 | 64000));
        assert!(self.device.is_active(scope).unwrap());
        tests::fill_history(self, scope, count);
    }
    fn binding(&self) -> String {
        format!(
            "Relay/crypto/v1/{}/{}",
            self.device.identity().server_id,
            self.device.identity().user_id
        )
    }
    /// Mutations and their outbox/cache changes are protected in one checkpoint.
    pub fn mutate<T>(
        &mut self,
        operation: impl FnOnce(&mut Device, &mut PrivateState) -> Result<T, String>,
    ) -> Result<T, String> {
        let binding = self.binding();
        let path = &self.path;
        let state = std::cell::RefCell::new(self.state.clone());
        let result = self.device.transact(
            |device| operation(device, &mut state.borrow_mut()),
            |snapshot| {
                let mut state = state.borrow_mut();
                // Old clients must reject this checkpoint instead of silently
                // ignoring archived send IDs and encrypting them a second time.
                state.version = if state.call.is_empty()
                    && state.invitations.is_empty()
                    && state.via_invitation.is_empty()
                {
                    2
                } else {
                    3
                };
                state.crypto = snapshot.to_vec();
                state.revision = state
                    .revision
                    .checked_add(1)
                    .ok_or("The encryption state counter is exhausted.")?;
                history::compact(&mut state, path, &binding)?;
                save(&state, path, &binding)
            },
        );
        if result.is_ok() {
            self.state = state.into_inner();
        }
        result
    }
    #[cfg(test)]
    pub fn with_device<T>(
        &mut self,
        operation: impl FnOnce(&mut Device) -> Result<T, String>,
    ) -> Result<T, String> {
        self.mutate(|device, _| operation(device))
    }
    pub fn advertised(&self) -> PublicDevice {
        self.state.advertised.as_ref().unwrap().clone()
    }
    pub fn key_packages(&mut self, count: usize) -> Result<Vec<PublicDevice>, String> {
        if count > 16 {
            return Err("Too many device key packages requested.".into());
        }
        if !self.state.pending_pool.is_empty() {
            return Ok(self.state.pending_pool.clone());
        }
        self.mutate(|device, state| {
            let packages = (0..count)
                .map(|_| device.key_package())
                .collect::<Result<Vec<_>, _>>()?;
            state.pending_pool = packages.clone();
            Ok(packages)
        })
    }
    pub fn pool_committed(&mut self) -> Result<(), String> {
        self.mutate(|_, state| {
            state.pending_pool.clear();
            Ok(())
        })
    }
    pub fn cursor(&self, scope: &str) -> u64 {
        self.state.cursors.get(scope).copied().unwrap_or(0)
    }
    pub fn set_cursor(&mut self, scope: &str, sequence: u64) -> Result<(), String> {
        self.mutate(|_, state| {
            state.cursors.insert(scope.into(), sequence);
            Ok(())
        })
    }
    /// Applying an MLS event and acknowledging its delivery cursor are one durable mutation.
    pub fn apply_control(
        &mut self,
        scope: &str,
        event: &relay_protocol::CryptoEvent,
        server_id: &str,
        consumption: Option<relay_protocol::ConsumeCryptoKeyPackage>,
    ) -> Result<(), String> {
        let own_device = self.device.identity().device_id.clone();
        let pending_rotation = self.device.pending_key_rotation(scope)?;
        let challenge = self.call_challenges.get(scope).copied();
        let fresh = self.mutate(|device, state| {
            let mut fresh = false;
            if event.sequence <= state.cursors.get(scope).copied().unwrap_or(0) {
                return Err("The group-key delivery cursor was replayed.".into());
            }
            let existing = device.is_active(scope)?;
            match event.kind.as_str() {
                "create" => {},
                "welcome" if !existing => {
                    if event.recipient_device_id.as_deref() != Some(own_device.as_str()) {
                        return Err("The group welcome is addressed to another device.".into());
                    }
                    let bytes = STANDARD.decode(&event.body).map_err(|_| "Malformed group welcome.")?;
                    let controller = Identity { server_id: server_id.into(), user_id: event.author_user_id.clone(), device_id: event.author_device_id.clone() };
                    device.join(scope, &event.group_id, controller, &bytes)?;
                    if let Some(receipt) = consumption {
                        if state.consumption_outbox.len() >= 128 { return Err("Key package receipt storage is full.".into()); }
                        state.consumption_outbox.push(receipt);
                    }
                    if device.group_state(scope).is_none_or(|g| g.epoch != event.epoch) {
                        return Err("The group welcome's epoch does not match its delivery context.".into());
                    }
                },
                "welcome" => {},
                "commit" if existing => {
                    let bytes = STANDARD.decode(&event.body).map_err(|_| "Malformed group-key update.")?;
                    let (received, proofs) = device.receive_with_call_proofs(scope, &bytes)?;
                    if !matches!(received, relay_crypto::Received::Membership {epoch} if epoch == event.epoch) {
                        return Err("Unexpected chat in the group-key stream.".into());
                    }
                    fresh = proofs.iter().any(|proof| proof.identity == *device.identity()
                        && challenge == Some(proof.nonce));
                    if let Some(rotation) = &pending_rotation {
                        let batch = state.control_outbox.get(scope)
                            .ok_or("The saved call-key update is missing its delivery operation.")?;
                        if batch.events.len() != 1
                            || batch.events[0].kind != "commit"
                            || batch.events[0].body != STANDARD.encode(&rotation.commit)
                            || batch.epoch != rotation.epoch
                        {
                            return Err("The saved call-key update does not match its pending rotation.".into());
                        }
                        if device.pending_key_rotation(scope)?.is_none() {
                            // Only a successfully authenticated competing commit
                            // can retire this outbox. Never cancel on an HTTP 409 alone.
                            state.control_outbox.remove(scope);
                        }
                    }
                },
                "commit" => {},
                _ => return Err("Unsupported group-key event.".into()),
            }
            state.cursors.insert(scope.into(), event.sequence);
            Ok(fresh)
        })?;
        if event.kind == "welcome" {
            self.fresh_call_scopes.remove(scope);
            self.call_challenges.remove(scope);
        }
        if fresh {
            self.fresh_call_scopes.insert(scope.into());
        }
        if pending_rotation.is_some() && self.pending_control(scope).is_none() {
            self.call_refresh_operations.remove(scope);
        }
        Ok(())
    }
    pub fn pending_consumptions(&self) -> Vec<relay_protocol::ConsumeCryptoKeyPackage> {
        self.state.consumption_outbox.clone()
    }
    pub fn pending_revocations(&self) -> Vec<String> {
        self.state.revocation_outbox.clone()
    }
    pub fn committed_revocation(&mut self, device_id: &str) -> Result<(), String> {
        self.mutate(|_, state| {
            state.revocation_outbox.retain(|id| id != device_id);
            Ok(())
        })
    }
    pub fn committed_consumption(
        &mut self,
        receipt: &relay_protocol::ConsumeCryptoKeyPackage,
    ) -> Result<(), String> {
        self.mutate(|_, state| {
            // Welcome sequences are local to each group. An ACK must retire
            // only its exact saved operation, including the selected package.
            state.consumption_outbox.retain(|c| {
                !(c.target == receipt.target
                    && c.device_id == receipt.device_id
                    && c.package_hash == receipt.package_hash
                    && c.welcome_sequence == receipt.welcome_sequence)
            });
            Ok(())
        })
    }
    pub fn application_cursor(&self, scope: &str) -> u64 {
        self.state
            .application_cursors
            .get(scope)
            .copied()
            .unwrap_or(0)
    }
    pub fn application_committed(&mut self, scope: &str, sequence: u64) -> Result<(), String> {
        if sequence <= self.application_cursor(scope) {
            return Ok(());
        }
        self.mutate(|_, state| {
            state.application_cursors.insert(scope.into(), sequence);
            Ok(())
        })
    }
    pub fn pending_control(&self, scope: &str) -> Option<relay_protocol::AppendCryptoEvents> {
        self.state.control_outbox.get(scope).cloned()
    }
    fn guard_control_ack(&self, scope: &str) -> Result<(), String> {
        if self.state.control_outbox.contains_key(scope) {
            return Err("Waiting for confirmation of this conversation's key update. New messages, edits and calls are paused; saved retries were preserved.".into());
        }
        Ok(())
    }
    pub fn needs_controller_call_refresh(&self, scope: &str) -> bool {
        self.device.group_state(scope).is_some_and(|group| {
            group.controller == *self.device.identity() && !self.fresh_call_scopes.contains(scope)
        })
    }
    pub fn stage_call_refresh(
        &mut self,
        scope: &str,
        target: relay_protocol::CryptoTarget,
        expected_sequence: u64,
    ) -> Result<relay_protocol::AppendCryptoEvents, String> {
        let own = self.device.identity();
        let expected_scope = match (&target.channel_id, &target.peer_id) {
            (Some(channel), None) => format!("{}/channel/{channel}", own.server_id),
            (None, Some(peer)) => {
                let mut users = [own.user_id.as_str(), peer.as_str()];
                users.sort_unstable();
                format!("{}/dm/{}/{}", own.server_id, users[0], users[1])
            }
            _ => return Err("The protected call has an invalid conversation target.".into()),
        };
        if scope != expected_scope || self.cursor(scope) != expected_sequence {
            return Err("Refresh this conversation before preparing new call keys.".into());
        }
        if let Some(pending) = self.pending_control(scope) {
            if pending.target != target || pending.expected_sequence != expected_sequence {
                return Err("Finish the saved key update before preparing new call keys.".into());
            }
            // An operation restored from another actor is retried, never adopted
            // as this actor's locally generated entropy.
            return Ok(pending);
        }
        let batch = self.mutate(|device, state| {
            let members = device.members(scope)?;
            let proofs = state.call.proofs.entry(scope.into()).or_default();
            proofs.retain(|proof| members.contains(&proof.identity));
            let change = device.stage_key_rotation_with_proofs(scope, proofs)?;
            let group = device
                .group_state(scope)
                .ok_or("The call's keys are missing.")?;
            let batch = relay_protocol::AppendCryptoEvents {
                target,
                device_id: device.identity().device_id.clone(),
                group_id: group.group_id.clone(),
                expected_sequence,
                epoch: change.epoch,
                operation_id: uuid::Uuid::new_v4().to_string(),
                events: vec![relay_protocol::CryptoEventBody {
                    kind: "commit".into(),
                    recipient_device_id: None,
                    body: STANDARD.encode(change.commit),
                }],
            };
            state.control_outbox.insert(scope.into(), batch.clone());
            Ok(batch)
        })?;
        self.call_refresh_operations
            .insert(scope.into(), batch.operation_id.clone());
        Ok(batch)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn stage_membership(
        &mut self,
        scope: &str,
        target: relay_protocol::CryptoTarget,
        group_id: String,
        expected_sequence: u64,
        add: &[PublicDevice],
        remove: &[String],
        create: bool,
    ) -> Result<relay_protocol::AppendCryptoEvents, String> {
        if !add.is_empty() && !remove.is_empty() {
            return Err("Membership additions and removals require separate epochs.".into());
        }
        if let Some(pending) = self.pending_control(scope) {
            return Ok(pending);
        }
        self.mutate(|device, state| {
            let mut events = Vec::new();
            if create {
                device.create_group(scope, &group_id)?;
                events.push(relay_protocol::CryptoEventBody {
                    kind: "create".into(),
                    ..Default::default()
                });
            }
            if !remove.is_empty() {
                let change = device.remove_members(scope, remove)?;
                events.push(relay_protocol::CryptoEventBody {
                    kind: "commit".into(),
                    body: STANDARD.encode(change.commit),
                    ..Default::default()
                });
            }
            if !add.is_empty() {
                let change = device.add_members(scope, add)?;
                events.push(relay_protocol::CryptoEventBody {
                    kind: "commit".into(),
                    body: STANDARD.encode(change.commit),
                    ..Default::default()
                });
                let welcome = change
                    .welcome
                    .ok_or("No encrypted group welcome was produced.")?;
                for peer in add {
                    events.push(relay_protocol::CryptoEventBody {
                        kind: "welcome".into(),
                        recipient_device_id: Some(peer.identity.device_id.clone()),
                        body: STANDARD.encode(&welcome),
                    });
                }
            }
            if create && add.is_empty() {
                let change = device.rotate_group(scope)?;
                events.push(relay_protocol::CryptoEventBody {
                    kind: "commit".into(),
                    body: STANDARD.encode(change.commit),
                    ..Default::default()
                });
            }
            if events.is_empty() {
                return Err("No group-key change was requested.".into());
            }
            let batch = relay_protocol::AppendCryptoEvents {
                target,
                device_id: device.identity().device_id.clone(),
                group_id,
                expected_sequence,
                epoch: device.group_state(scope).unwrap().epoch,
                operation_id: uuid::Uuid::new_v4().to_string(),
                events,
            };
            check_control_delivery(&batch)?;
            state.control_outbox.insert(scope.into(), batch.clone());
            Ok(batch)
        })
    }
    pub fn committed_control(&mut self, scope: &str, sequence: u64) -> Result<(), String> {
        let operation = self.pending_control(scope);
        let rotation = self.device.pending_key_rotation(scope)?;
        let confirmed_proofs = self.device.pending_call_proofs(scope)?;
        let fresh_operation = rotation.is_some()
            && operation.as_ref().is_some_and(|batch| {
                self.call_refresh_operations.get(scope) == Some(&batch.operation_id)
            });
        self.mutate(|device, state| {
            if let Some(rotation) = rotation {
                let batch = state
                    .control_outbox
                    .get(scope)
                    .ok_or("The saved call-key update is missing its delivery operation.")?;
                if batch.events.len() != 1
                    || batch.events[0].kind != "commit"
                    || batch.events[0].recipient_device_id.is_some()
                    || batch.events[0].body != STANDARD.encode(&rotation.commit)
                    || batch.epoch != rotation.epoch
                    || batch.group_id != device.group_state(scope).unwrap().group_id
                    || batch.device_id != device.identity().device_id
                    || batch.expected_sequence.checked_add(1) != Some(sequence)
                {
                    return Err(
                        "The call-key confirmation does not match the saved operation.".into(),
                    );
                }
                device.confirm_key_rotation(scope, &rotation.commit)?;
                if let Some(proofs) = state.call.proofs.get_mut(scope) {
                    proofs.retain(|proof| !confirmed_proofs.contains(proof));
                }
            }
            state.control_outbox.remove(scope);
            state.cursors.insert(scope.into(), sequence);
            Ok(())
        })?;
        self.call_refresh_operations.remove(scope);
        if fresh_operation {
            self.fresh_call_scopes.insert(scope.into());
        }
        Ok(())
    }
    pub fn discard_initial_claim(&mut self, scope: &str) -> Result<(), String> {
        if self
            .state
            .control_outbox
            .get(scope)
            .is_none_or(|p| p.events.first().is_none_or(|e| e.kind != "create"))
        {
            return Err("This group has no unpublished initial claim.".into());
        }
        if self
            .state
            .cached
            .values()
            .any(|c| c.envelope.scope == scope)
        {
            return Err(
                "A conversation containing published messages cannot be reset automatically."
                    .into(),
            );
        }
        self.mutate(|device, state| {
            device.forget_initial_claim(scope)?;
            state.control_outbox.remove(scope);
            state.cursors.insert(scope.into(), 0);
            Ok(())
        })
    }
    pub fn approve(&mut self, device_id: &str, fingerprint: &str) -> Result<(), String> {
        self.cancel_invited_host_retry(device_id);
        let public = self
            .directory
            .get(device_id)
            .ok_or("Refresh the device list before verifying this device.")?
            .clone();
        self.mutate(|device, state| {
            if fingerprint != public.fingerprint {
                return Err("The device fingerprint does not match. No key was shared.".into());
            }
            // Existing MLS members may be listed without an unused KeyPackage:
            // a matching pin only changes its recorded source to "verified".
            // A changed key was compared by hand here, so it replaces the pin.
            if device.expected_fingerprint(device_id).is_some() && device.trust(&public).is_none() {
                device.accept_new_key(&public, Some(fingerprint))?;
            } else {
                device.approve(&public, fingerprint)?;
            }
            state.via_invitation.retain(|id| id != device_id);
            state.trust_notices.retain(|n| n.device_id != device_id);
            Ok(())
        })?;
        self.status.via_invitation.retain(|id| id != device_id);
        self.refresh_trust_status();
        Ok(())
    }
    /// Removes the "Verified" mark. The same key stays trusted, as if it had
    /// been trusted on first use, so conversations keep working.
    pub fn unapprove(&mut self, device_id: &str) -> Result<(), String> {
        self.cancel_invited_host_retry(device_id);
        self.mutate(|device, state| {
            device.unverify(device_id)?;
            state.via_invitation.retain(|id| id != device_id);
            Ok(())
        })?;
        self.status.via_invitation.retain(|id| id != device_id);
        self.refresh_trust_status();
        Ok(())
    }
    /// One click: trust the new security code of a device that needed review.
    /// It is trusted, not verified; comparing codes later adds the badge.
    pub fn accept_new_key(&mut self, device_id: &str, fingerprint: &str) -> Result<(), String> {
        self.cancel_invited_host_retry(device_id);
        let public = self
            .directory
            .get(device_id)
            .ok_or("Refresh the device list before accepting this security code.")?
            .clone();
        if public.fingerprint != fingerprint {
            return Err("This device's security code changed again. Review the new code.".into());
        }
        if public.identity == *self.device.identity() {
            return Err("You cannot replace the key of this device.".into());
        }
        if self.device.trust(&public).is_none() {
            self.mutate(|device, state| {
                device.accept_new_key(&public, None)?;
                state.via_invitation.retain(|id| id != device_id);
                state.trust_notices.retain(|n| n.device_id != device_id);
                Ok(())
            })?;
        } else {
            self.dismiss_notice(device_id)?;
        }
        self.status.via_invitation = self.state.via_invitation.clone();
        self.refresh_trust_status();
        Ok(())
    }
    pub fn trust_notices(&self) -> Vec<TrustNotice> {
        self.state.trust_notices.clone()
    }
    /// The user read a key-change notice. Trust does not change.
    pub fn dismiss_notice(&mut self, device_id: &str) -> Result<(), String> {
        if self.state.trust_notices.iter().any(|n| n.device_id == device_id) {
            self.mutate(|_, state| {
                state.trust_notices.retain(|n| n.device_id != device_id);
                Ok(())
            })?;
        }
        self.status.notices = self.state.trust_notices.clone();
        Ok(())
    }

    /// Trust on first use for every listed device. New people and new devices
    /// of people nobody verified are trusted at once; a person the user
    /// verified by hand never gets a new device or key trusted here. Returns
    /// the notices this pass created. Writes a checkpoint only on a change.
    pub fn trust_directory(&mut self, complete: bool) -> Result<Vec<TrustNotice>, String> {
        let mut listed: Vec<PublicDevice> = self.directory.values().cloned().collect();
        listed.sort_by(|a, b| a.identity.device_id.cmp(&b.identity.device_id));
        // A listing without a KeyPackage only becomes an in-memory candidate;
        // one with a foreign community or malformed code is not trusted at all.
        for public in &listed {
            if self.device.first_use_decision(public) == relay_crypto::FirstUse::Deferred {
                // A refused listing stays untrusted; there is nothing to undo.
                self.device.trust_on_first_use(public).ok();
            }
        }
        // Only valid listings count as a change, so an unusable entry does not
        // cause a checkpoint write on every directory read.
        let changes: Vec<_> = listed
            .into_iter()
            .filter(|public| {
                matches!(
                    self.device.first_use_decision(public),
                    relay_crypto::FirstUse::Trusted | relay_crypto::FirstUse::KeyReplaced
                ) && self.device.listing_is_valid(public)
            })
            .collect();
        let start = complete && !self.state.trust_started;
        if changes.is_empty() && !start {
            self.refresh_trust_status();
            return Ok(Vec::new());
        }
        if complete && self.device.pin_count() + changes.len() > relay_crypto::MAX_PINS * 3 / 4 {
            let keep: HashSet<String> = self.directory.keys().cloned().collect();
            self.mutate(|device, _| device.forget_unused_first_use(&keep))?;
        }
        let own = self.device.identity().clone();
        let started = self.state.trust_started;
        let created = self.mutate(|device, state| {
            let mut created = Vec::new();
            for public in &changes {
                let decision = device.trust_on_first_use(public)?;
                let own_account = public.identity.user_id == own.user_id;
                let kind = match decision {
                    // A new key on an own device is, to its owner, a new device.
                    relay_crypto::FirstUse::KeyReplaced if own_account => {
                        TrustNoticeKind::NewOwnDevice
                    }
                    relay_crypto::FirstUse::KeyReplaced => TrustNoticeKind::CodeChanged,
                    relay_crypto::FirstUse::Trusted if started && own_account => {
                        TrustNoticeKind::NewOwnDevice
                    }
                    _ => continue,
                };
                let notice = TrustNotice {
                    user_id: public.identity.user_id.clone(),
                    device_id: public.identity.device_id.clone(),
                    fingerprint: public.fingerprint.clone(),
                    kind,
                };
                state.trust_notices.retain(|n| n.device_id != notice.device_id);
                if state.trust_notices.len() >= MAX_TRUST_NOTICES {
                    state.trust_notices.remove(0);
                }
                state.trust_notices.push(notice.clone());
                created.push(notice);
            }
            if complete {
                state.trust_started = true;
            }
            Ok(created)
        })?;
        self.refresh_trust_status();
        Ok(created)
    }

    /// Recomputes the trust flags of the listed devices after a pin changed.
    pub(crate) fn refresh_trust_status(&mut self) {
        for peer in self
            .status
            .peers
            .iter_mut()
            .chain(self.status.own_devices.iter_mut())
        {
            if peer.device_id == self.device.identity().device_id {
                continue;
            }
            if let Some(public) = self.directory.get(&peer.device_id) {
                (peer.verified, peer.trusted, peer.changed, peer.needs_review) =
                    peer_trust(&self.device, public);
            }
        }
        self.status.notices = self.state.trust_notices.clone();
    }
    /// Listed devices of this person that wait for the user's review.
    #[cfg(test)]
    pub fn devices_needing_review(&self, user_id: &str) -> Vec<String> {
        self.directory
            .values()
            .filter(|public| {
                public.identity.user_id == user_id
                    && self.device.trust(public).is_none()
                    && self.device.first_use_decision(public) == relay_crypto::FirstUse::Held
            })
            .map(|public| public.identity.device_id.clone())
            .collect()
    }

    /// Host side: remembers a new one-time secret for the invitation the server
    /// just created and returns what the link must carry. Fails (so the caller
    /// sends an ordinary invitation) if too many unused verified invitations are open.
    pub fn issue_invitation(
        &mut self,
        invite_id: &str,
        expires: u64,
    ) -> Result<InviteProof, String> {
        let device_id = self.device.identity().device_id.clone();
        let fingerprint = self.status.own_fingerprint.clone();
        let mut secret = [0u8; invite_proof::SECRET_BYTES];
        getrandom::getrandom(&mut secret)
            .map_err(|_| "Cannot create a secret for this invitation.".to_string())?;
        let proof = InviteProof::new(invite_id, &device_id, &fingerprint, secret)?;
        let encoded = proof.secret_for_link();
        let now = unix_now();
        self.mutate(|_, state| {
            state.invitations.retain(|r| !r.used && r.expires > now);
            if state.invitations.len() >= MAX_ACTIVE_INVITATIONS {
                return Err("Too many unused verified invitations are open.".into());
            }
            state.invitations.push(InviteRecord {
                id: invite_id.into(),
                secret: encoded.to_string(),
                expires,
                used: false,
            });
            Ok(())
        })?;
        Ok(proof)
    }

    /// True while any invitation created here could still be redeemed, so the
    /// worker knows to look for proofs in the directory.
    pub fn has_active_invitations(&self) -> bool {
        let now = unix_now();
        self.state
            .invitations
            .iter()
            .any(|r| !r.used && r.expires > now)
    }

    /// Host side: approves devices whose invitation proof checks out against a
    /// secret created here. Returns the IDs approved in this pass. A device that
    /// is already approved, whose identity changed, or that belongs to this
    /// account is never touched, and a proof that does not verify only leaves
    /// the device unapproved (listed in `status.invitation_unconfirmed`).
    pub fn apply_invitation_attestations(
        &mut self,
        devices: &[relay_protocol::CryptoDevice],
    ) -> Result<Vec<String>, String> {
        let server_id = self.device.identity().server_id.clone();
        let own_user = self.device.identity().user_id.clone();
        let now = unix_now();
        let mut approved = Vec::new();
        let mut unconfirmed = Vec::new();
        for entry in devices {
            let Some(attestation) = &entry.invite_attestation else {
                continue;
            };
            if entry.user_id == own_user {
                continue;
            }
            let Some(public) = self.directory.get(&entry.device_id).cloned() else {
                continue;
            };
            // Only a device trusted on first use with this exact key, or one
            // seen for the first time, may become verified by its proof.
            if self.device.is_verified(&public)
                || (self.device.expected_fingerprint(&entry.device_id).is_some()
                    && self.device.trust(&public).is_none())
            {
                continue;
            }
            let Some(secret) = self
                .state
                .invitations
                .iter()
                .find(|r| r.id == attestation.invite_id && !r.used && r.expires > now)
                .and_then(|r| invite_proof::decode_secret(&r.secret))
            else {
                continue;
            };
            if !invite_proof::verify(
                &secret,
                &server_id,
                &attestation.invite_id,
                &entry.user_id,
                &entry.device_id,
                &public.fingerprint,
                &attestation.mac,
            ) {
                unconfirmed.push(entry.device_id.clone());
                continue;
            }
            let invite_id = attestation.invite_id.clone();
            let device_id = entry.device_id.clone();
            self.mutate(|device, state| {
                device.approve(&public, &public.fingerprint)?;
                if let Some(record) = state.invitations.iter_mut().find(|r| r.id == invite_id) {
                    record.used = true;
                }
                if !state.via_invitation.contains(&device_id) {
                    if state.via_invitation.len() >= MAX_VIA_INVITATION {
                        return Err("Too many devices were approved through invitations.".into());
                    }
                    state.via_invitation.push(device_id.clone());
                }
                Ok(())
            })?;
            self.status.via_invitation = self.state.via_invitation.clone();
            approved.push(entry.device_id.clone());
        }
        self.status.invitation_unconfirmed = unconfirmed;
        Ok(approved)
    }

    /// Friend side: approves the host's device named in the invitation, but only
    /// if the directory shows exactly the device ID and fingerprint the link
    /// carried and nothing else is already pinned for that device.
    pub fn approve_invited_host(&mut self, proof: &InviteProof) -> Result<bool, String> {
        self.approve_invited_host_identity(&proof.host_device_id, &proof.host_fingerprint)
    }

    /// Called only after the attestation request received its response. A later
    /// directory read can complete the same pin, without retaining the secret
    /// or sending that one-time attestation again.
    pub(super) fn remember_invited_host(&mut self, proof: &InviteProof, now: std::time::Instant) {
        self.pending_invited_host = None;
        let pinned = self.device.expected_fingerprint(&proof.host_device_id);
        // Trust on first use may already hold exactly the named key; the
        // invitation can still upgrade it to verified.
        let upgradable = pinned == Some(proof.host_fingerprint.as_str())
            && self.device.pin_trust(&proof.host_device_id) == Some(relay_crypto::Trust::FirstUse);
        if pinned.is_none() || upgradable {
            self.pending_invited_host = Some(PendingInvitedHost {
                device_id: proof.host_device_id.clone(),
                fingerprint: proof.host_fingerprint.clone(),
                deadline: now + std::time::Duration::from_secs(30),
            });
        }
    }

    pub(super) fn has_pending_invited_host(&mut self, now: std::time::Instant) -> bool {
        if self
            .pending_invited_host
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            self.pending_invited_host = None;
        }
        self.pending_invited_host.is_some()
    }

    pub(super) fn retry_invited_host(&mut self, now: std::time::Instant) -> Result<bool, String> {
        if !self.has_pending_invited_host(now) {
            return Ok(false);
        }
        let pending = self.pending_invited_host.as_ref().unwrap().clone();
        self.approve_invited_host_identity(&pending.device_id, &pending.fingerprint)
    }

    fn cancel_invited_host_retry(&mut self, device_id: &str) {
        if self
            .pending_invited_host
            .as_ref()
            .is_some_and(|pending| pending.device_id == device_id)
        {
            self.pending_invited_host = None;
        }
    }

    fn approve_invited_host_identity(
        &mut self,
        host_device_id: &str,
        host_fingerprint: &str,
    ) -> Result<bool, String> {
        let Some(public) = self.directory.get(host_device_id).cloned() else {
            return Ok(false);
        };
        // A missing entry may arrive later. A present mismatch or explicit
        // existing pin is final; never keep waiting until a different key fits.
        self.cancel_invited_host_retry(host_device_id);
        if public.fingerprint != host_fingerprint
            || self.device.is_verified(&public)
            || (self.device.expected_fingerprint(host_device_id).is_some()
                && self.device.trust(&public).is_none())
        {
            return Ok(false);
        }
        let device_id = host_device_id.to_owned();
        self.mutate(|device, state| {
            device.approve(&public, &public.fingerprint)?;
            if !state.via_invitation.contains(&device_id) {
                if state.via_invitation.len() >= MAX_VIA_INVITATION {
                    return Err("Too many devices were approved through invitations.".into());
                }
                state.via_invitation.push(device_id.clone());
            }
            Ok(())
        })?;
        self.status.via_invitation = self.state.via_invitation.clone();
        Ok(true)
    }
    pub fn encrypted_content(&mut self, envelope: Envelope) -> Result<String, String> {
        self.prepare_content(envelope, false)
    }
    pub fn encrypted_edit_content(&mut self, envelope: Envelope) -> Result<String, String> {
        self.prepare_content(envelope, true)
    }
    fn prepare_content(&mut self, envelope: Envelope, edit: bool) -> Result<String, String> {
        if self.device.is_blocked() {
            return Err("Encryption is paused because its state could not be saved.".into());
        }
        validate_action(&envelope)?;
        if envelope.version != 1
            || envelope.client_id.is_empty()
            || envelope.client_id.len() > 128
            || envelope.content.len() > 10000
            || envelope.attachments.len() > 8
            || envelope.mentions.len() > 64
        {
            return Err("The encrypted message is invalid or too large.".into());
        }
        let body = Zeroizing::new(
            serde_json::to_vec(&envelope).map_err(|_| "Cannot prepare this encrypted message.")?,
        );
        let request_hash = format!("{:x}", Sha256::digest(&body));
        if let Some((old_hash, wire)) = self.state.outbox.get(&envelope.client_id) {
            if old_hash == &request_hash {
                return Ok(wire.clone());
            }
            // An explicit edit can supersede a pending edit after a definite
            // rejection or uncertain response. Keep the old ciphertext cached:
            // a receiver may already have consumed it before the response was
            // lost, so its MLS generation must never be rolled back or reused.
            let same_edit = edit
                && envelope.action.is_none()
                && self
                    .state
                    .cached
                    .get(&cipher_hash(wire))
                    .is_some_and(|cached| {
                        cached.author == *self.device.identity()
                            && cached.envelope.scope == envelope.scope
                            && cached.envelope.client_id == envelope.client_id
                            && cached.envelope.action.is_none()
                    });
            if !same_edit {
                return Err("This message send ID already belongs to different content.".into());
            }
        }
        if !self.state.outbox.contains_key(&envelope.client_id) {
            if let Some((old_hash, wire)) = self.state.completed.get(&envelope.client_id) {
                if old_hash == &request_hash {
                    return Ok(wire.clone());
                }
                if !edit {
                    return Err("This message send ID already belongs to different content.".into());
                }
            } else if !edit
                && history::contains_send(
                    &self.state,
                    &self.path,
                    &self.binding(),
                    &envelope.client_id,
                )?
            {
                return Err(
                    "This send ID is outside the retained retry window. Its message was preserved."
                        .into(),
                );
            }
        }
        self.guard_control_ack(&envelope.scope)?;
        self.mutate(|device, state| {
            if state.outbox.len() >= 128 && !state.outbox.contains_key(&envelope.client_id) {
                return Err("Protected message storage is full.".into());
            }
            let wire = format!(
                "{MESSAGE_PREFIX}{}",
                STANDARD.encode(device.encrypt(&envelope.scope, &body)?)
            );
            let hash = cipher_hash(&wire);
            state.cached.insert(
                hash,
                CachedEnvelope {
                    author: device.identity().clone(),
                    envelope: envelope.clone(),
                    message_id: None,
                    sequence: 0,
                    delivery_revision: 0,
                },
            );
            state
                .outbox
                .insert(envelope.client_id.clone(), (request_hash, wire.clone()));
            Ok(wire)
        })
    }
    pub fn committed(&mut self, client_id: &str) -> Result<(), String> {
        if !self.state.outbox.contains_key(client_id) {
            return Ok(());
        }
        self.mutate(|_, state| {
            if let Some((_, wire)) = state.outbox.get(client_id) {
                if let Some(cached) = state.cached.get(&cipher_hash(wire)) {
                    for attachment in &cached.envelope.attachments {
                        state.files.remove(&attachment.id);
                    }
                }
            }
            if let Some(receipt) = state.outbox.remove(client_id) {
                state.completed.insert(client_id.into(), receipt);
                state.completed_order.retain(|id| id != client_id);
                state.completed_order.push(client_id.into());
                if state.completed_order.len() > 512 {
                    let expired = state.completed_order.remove(0);
                    state.completed.remove(&expired);
                }
            }
            Ok(())
        })
    }
    pub fn decrypt_content(&mut self, scope: &str, wire: &str) -> Result<CachedEnvelope, String> {
        self.decrypt_contents(scope, &[wire]).remove(0)
    }
    /// One durable checkpoint per bounded page. Each rejected input rolls back
    /// its own MLS attempt before the next message is processed.
    pub fn decrypt_contents(
        &mut self,
        scope: &str,
        wires: &[&str],
    ) -> Vec<Result<CachedEnvelope, String>> {
        if wires.len() > 200 {
            return vec![Err("Protected message page exceeds its limit.".into()); wires.len()];
        }
        let hashes: Vec<_> = wires.iter().map(|wire| cipher_hash(wire)).collect();
        let existing = hashes
            .iter()
            .map(|hash| history::lookup(&self.state, &self.path, &self.binding(), hash))
            .collect::<Result<Vec<_>, _>>();
        let existing = match existing {
            Ok(entries) => entries,
            Err(error) => return vec![Err(error); wires.len()],
        };
        if existing.iter().all(Option::is_some) {
            return existing
                .iter()
                .map(|cached| checked_cached(cached.as_ref().unwrap(), scope))
                .collect();
        }
        let result = self.mutate(|device, state| {
            let mut results = Vec::with_capacity(wires.len());
            for ((wire, hash), old) in wires.iter().zip(&hashes).zip(&existing) {
                if let Some(cached) = old.as_ref().or_else(|| state.cached.get(hash)) {
                    results.push(checked_cached(cached, scope));
                    continue;
                }
                let result = (|| {
                    let encoded = wire
                        .strip_prefix(MESSAGE_PREFIX)
                        .ok_or("This is a legacy unencrypted message.")?;
                    if encoded.len() > 64 * 1024 {
                        return Err("The encrypted message exceeds its limit.".into());
                    }
                    let bytes = STANDARD
                        .decode(encoded)
                        .map_err(|_| "The encrypted message is malformed.")?;
                    // The inner transaction is never externally published. The
                    // enclosing checkpoint must succeed before results escape.
                    let cached = device
                        .transact(|device| receive_envelope(device, scope, &bytes), |_| Ok(()))?;
                    state.cached.insert(hash.clone(), cached.clone());
                    Ok(cached)
                })();
                results.push(result);
            }
            Ok(results)
        });
        result.unwrap_or_else(|error| vec![Err(error); wires.len()])
    }
    pub fn add_file(&mut self, file: EncryptedAttachment) -> Result<(), String> {
        self.mutate(|_, state| {
            if state.files.len() >= 1024 && !state.files.contains_key(&file.id) {
                return Err("Protected attachment storage is full.".into());
            }
            state.files.insert(file.id.clone(), file);
            Ok(())
        })
    }
    pub fn file(&self, id: &str) -> Result<Option<EncryptedAttachment>, String> {
        if let Some(file) = self.state.files.get(id) {
            return Ok(Some(file.clone()));
        }
        for entry in history::records(&self.state, &self.path, self.binding(), None) {
            if let Some(file) = entry?
                .envelope
                .attachments
                .into_iter()
                .find(|file| file.id == id)
            {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }
    pub fn bind_delivery(
        &mut self,
        wire: &str,
        message_id: &str,
        sequence: u64,
    ) -> Result<(), String> {
        self.bind_deliveries(&[(wire, message_id, sequence)])
            .remove(0)
    }
    pub fn bind_deliveries(&mut self, deliveries: &[(&str, &str, u64)]) -> Vec<Result<(), String>> {
        if deliveries.len() > 200 {
            return vec![
                Err("Protected delivery page exceeds its limit.".into());
                deliveries.len()
            ];
        }
        let mut updates = Vec::new();
        let mut pending: HashMap<String, (String, u64)> = HashMap::new();
        let mut results: Vec<_> = deliveries
            .iter()
            .map(|(wire, message_id, sequence)| {
                let hash = cipher_hash(wire);
                let cached = history::lookup(&self.state, &self.path, &self.binding(), &hash)?
                    .ok_or("The encrypted message has not been authenticated.".to_string())?;
                if message_id.is_empty() || message_id.len() > 128 || *sequence == 0 {
                    return Err("The encrypted message's delivery identity is invalid.".into());
                }
                if let Some(old) = &cached.message_id {
                    if old != message_id || cached.sequence != *sequence {
                        return Err("The encrypted message's delivery identity changed.".into());
                    }
                } else if let Some((id, old_sequence)) = pending.get(&hash) {
                    if id != message_id || old_sequence != sequence {
                        return Err(
                            "The same encrypted message received conflicting delivery identities."
                                .into(),
                        );
                    }
                } else {
                    pending.insert(hash.clone(), ((*message_id).to_string(), *sequence));
                    updates.push((hash, cached, (*message_id).to_string(), *sequence));
                }
                Ok(())
            })
            .collect();
        if !updates.is_empty() {
            let result = self.mutate(|_, state| {
                let mut revision = state
                    .cached
                    .values()
                    .map(|cached| cached.delivery_revision)
                    .max()
                    .unwrap_or(0)
                    .max(state.delivery_clock);
                for (hash, mut cached, id, sequence) in updates {
                    revision = revision
                        .checked_add(1)
                        .ok_or("The authenticated delivery counter is exhausted.")?;
                    cached.message_id = Some(id);
                    cached.sequence = sequence;
                    cached.delivery_revision = revision;
                    state.cached.insert(hash, cached);
                }
                state.delivery_clock = revision;
                Ok(())
            });
            if let Err(error) = result {
                for result in &mut results {
                    if result.is_ok() {
                        *result = Err(error.clone());
                    }
                }
            }
        }
        results
    }
    pub fn cached_envelopes<'a>(
        &'a self,
        scope: &'a str,
    ) -> impl Iterator<Item = Result<CachedEnvelope, String>> + 'a {
        history::records_cached(
            &self.state,
            &self.path,
            self.binding(),
            scope,
            &self.history_cache,
        )
        .filter(|entry| match entry {
            Ok(cached) => cached.message_id.is_some(),
            Err(_) => true,
        })
    }
    /// Read-only history snapshot. Never exports device/MLS checkpoints,
    /// approvals, pending sends or live media material.
    pub fn history_backup(&self) -> Result<crate::recovery::HistoryArchive, String> {
        let mut archive = crate::recovery::HistoryArchive::new(
            self.state.identity.clone(),
            self.device.fingerprint()?,
        );
        for entry in history::records(&self.state, &self.path, self.binding(), None) {
            let entry = entry?;
            if entry.message_id.is_some() {
                archive.push(entry)?;
            }
        }
        archive.validate(None)?;
        Ok(archive)
    }
    pub fn history_usage(&self) -> HistoryUsage {
        history::usage(&self.state)
    }
    pub fn pending_action_id(&self, scope: &str, action: &MessageAction) -> Option<String> {
        self.state.outbox.iter().find_map(|(id, (_, wire))| {
            self.state
                .cached
                .get(&cipher_hash(wire))
                .filter(|c| c.envelope.scope == scope && c.envelope.action.as_ref() == Some(action))
                .map(|_| id.clone())
        })
    }
    pub fn call_keys(
        &self,
        scope: &str,
        expected_users: &[String],
    ) -> Result<std::sync::Arc<CallKeys>, String> {
        self.guard_control_ack(scope)?;
        let group = self
            .device
            .group_state(scope)
            .ok_or("The call's encryption keys are not ready.")?;
        let members = self.device.members(scope)?;
        let mut sender_keys = HashMap::new();
        for member in members {
            if !expected_users.contains(&member.user_id)
                || sender_keys.contains_key(&member.user_id)
            {
                return Err("The protected call's membership has changed or has multiple devices for one account. Wait for its key manager to update the call.".into());
            }
            let opus = self.device.media_key(scope, "opus", &member)?;
            let rtc = self.device.media_key(scope, "rtc", &member)?;
            sender_keys.insert(
                member.user_id.clone(),
                SenderKeys {
                    device_id: member.device_id,
                    opus: Zeroizing::new(
                        opus.as_slice()
                            .try_into()
                            .map_err(|_| "Invalid call encryption key.")?,
                    ),
                    rtc: Zeroizing::new(
                        rtc.as_slice()
                            .try_into()
                            .map_err(|_| "Invalid call encryption key.")?,
                    ),
                },
            );
        }
        if expected_users
            .iter()
            .any(|id| !sender_keys.contains_key(id))
        {
            return Err("Verify every participant's device and wait for the call's key manager to approve membership.".into());
        }
        retain_call_context(
            &self.call_contexts,
            CallKeys {
                group_id: group.group_id.clone(),
                scope: scope.into(),
                epoch: group.epoch,
                local_user_id: self.device.identity().user_id.clone(),
                local_device_id: self.device.identity().device_id.clone(),
                sender_keys,
                voice_replay: Mutex::default(),
            },
        )
    }
}
// Device supports at most 128 MLS groups. Keep the latest verified call context
// per scope within that same bound; never evict a live epoch's replay window.
fn retain_call_context(
    contexts: &Mutex<HashMap<String, Arc<CallKeys>>>,
    keys: CallKeys,
) -> Result<Arc<CallKeys>, String> {
    let mut contexts = contexts
        .lock()
        .map_err(|_| "Protected call state is unavailable. Leave the call and reconnect.")?;
    if let Some(previous) = contexts.get(&keys.scope) {
        let same_material = previous.group_id == keys.group_id
            && previous.epoch == keys.epoch
            && previous.local_user_id == keys.local_user_id
            && previous.local_device_id == keys.local_device_id
            && previous.sender_keys.len() == keys.sender_keys.len()
            && previous.sender_keys.iter().all(|(user, sender)| {
                keys.sender_keys.get(user).is_some_and(|next| {
                    sender.device_id == next.device_id
                        && sender.opus == next.opus
                        && sender.rtc == next.rtc
                })
            });
        if same_material {
            return Ok(previous.clone());
        }
        if previous.group_id == keys.group_id && keys.epoch <= previous.epoch {
            return Err("The call's encryption context changed without a fresh epoch. Leave the call and wait for its keys to update.".into());
        }
    } else if contexts.len() >= 128 {
        return Err(
            "Protected call state reached its conversation limit. Existing calls remain protected."
                .into(),
        );
    }
    let keys = Arc::new(keys);
    contexts.insert(keys.scope.clone(), keys.clone());
    Ok(keys)
}
fn checked_cached(cached: &CachedEnvelope, scope: &str) -> Result<CachedEnvelope, String> {
    if cached.envelope.scope != scope {
        return Err("Encrypted message belongs to another conversation.".into());
    }
    Ok(cached.clone())
}
fn receive_envelope(
    device: &mut Device,
    scope: &str,
    wire: &[u8],
) -> Result<CachedEnvelope, String> {
    let relay_crypto::Received::Application { author, bytes } = device.receive(scope, wire)? else {
        return Err("A group-key update was received as chat content.".into());
    };
    let bytes = Zeroizing::new(bytes);
    let envelope: Envelope =
        serde_json::from_slice(&bytes).map_err(|_| "The encrypted message payload is invalid.")?;
    if envelope.version != 1
        || envelope.scope != scope
        || envelope.content.len() > 10000
        || envelope.client_id.is_empty()
        || envelope.client_id.len() > 128
        || envelope.mentions.len() > 64
        || envelope.attachments.len() > 8
        || envelope.attachments.iter().any(|file| {
            file.id.is_empty()
                || file.id.len() > 128
                || file.name.len() > 512
                || file.mime.len() > 128
                || file.file.binding.len() > 512
                || file.file.size > relay_crypto::files::MAX_FILE_BYTES as u64
        })
    {
        return Err(
            "The encrypted message payload has invalid context or exceeds its limits.".into(),
        );
    }
    validate_action(&envelope)?;
    Ok(CachedEnvelope {
        author,
        envelope,
        message_id: None,
        sequence: 0,
        delivery_revision: 0,
    })
}
fn validate_action(envelope: &Envelope) -> Result<(), String> {
    if let Some(MessageAction::Reaction {
        message_id,
        target_client_id,
        emoji,
        ..
    }) = &envelope.action
    {
        if !envelope.content.is_empty()
            || !envelope.attachments.is_empty()
            || !envelope.mentions.is_empty()
            || envelope.reply_to_id.is_some()
            || message_id.is_empty()
            || message_id.len() > 128
            || target_client_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 128)
            || emoji.is_empty()
            || emoji.len() > 128
            || emoji.chars().any(char::is_control)
        {
            return Err("The encrypted reaction has invalid context or exceeds its limits.".into());
        }
    }
    Ok(())
}
fn cipher_hash(wire: &str) -> String {
    format!("{:x}", Sha256::digest(wire.as_bytes()))
}
fn save(state: &PrivateState, path: &std::path::Path, binding: &str) -> Result<(), String> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(state).map_err(|_| "Cannot encode protected encryption state.")?,
    );
    if bytes.len() > MAX_PRIVATE_BYTES {
        return Err("Protected message storage is full.".into());
    }
    let protected = protected::protect(&bytes, binding)?;
    let parent = path.parent().ok_or("Invalid protected storage path.")?;
    std::fs::create_dir_all(parent).map_err(|_| "Cannot create protected device storage.")?;
    let temporary = parent.join(format!("{}.pending", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| "Cannot create a protected device checkpoint.")?;
        file.write_all(&protected)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot save the device checkpoint; encryption is paused.")?;
        drop(file);
        protected::replace(&temporary, path)?;
        protected::set_head(binding, state.revision, &state.identity.device_id)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
mod protected {
    use super::*;
    pub fn protect(_: &[u8], _: &str) -> Result<Vec<u8>, String> {
        Err("Encryption device storage requires an OS-protected key store on this platform.".into())
    }
    pub fn unprotect(_: &[u8], _: &str) -> Result<Zeroizing<Vec<u8>>, String> {
        Err("Encryption device storage requires an OS-protected key store on this platform.".into())
    }
    pub fn head(_: &str) -> Result<Option<u64>, String> {
        Ok(None)
    }
    pub fn check_identity(_: &str, _: &str) -> Result<(), String> {
        Err("Protected device storage is unavailable.".into())
    }
    pub fn set_head(_: &str, _: u64, _: &str) -> Result<(), String> {
        Err("Protected device storage is unavailable.".into())
    }
    pub fn replace(_: &std::path::Path, _: &std::path::Path) -> Result<(), String> {
        Err("Protected device storage is unavailable.".into())
    }
}

#[cfg(windows)]
pub(crate) mod protected {
    use super::*;
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{
            Foundation::*,
            Security::{
                Credentials::*, Cryptography::*, GetLengthSid, GetTokenInformation, TokenUser,
                TOKEN_QUERY, TOKEN_USER,
            },
            Storage::FileSystem::*,
            System::Threading::{
                CreateMutexW, GetCurrentProcess, OpenProcessToken, ReleaseMutex,
                WaitForSingleObject,
            },
        },
    };
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    fn vault_target(binding: &str) -> Vec<u16> {
        wide(&format!(
            "Relay/crypto-revision/{:x}",
            Sha256::digest(binding.as_bytes())
        ))
    }
    struct VaultHandle {
        handle: HANDLE,
        owned: bool,
    }
    impl Drop for VaultHandle {
        fn drop(&mut self) {
            unsafe {
                if self.owned {
                    let _ = ReleaseMutex(self.handle);
                }
                let _ = CloseHandle(self.handle);
            }
        }
    }
    static LIVE_BINDINGS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    struct BindingReservation {
        name: String,
    }
    impl Drop for BindingReservation {
        fn drop(&mut self) {
            if let Ok(mut active) = LIVE_BINDINGS.lock() {
                active.remove(&self.name);
            }
        }
    }
    /// A device actor is owned by one thread/process independently of the path
    /// of its protected file. Copying a checkpoint cannot create a second live
    /// owner of the same MLS ratchets.
    pub(super) struct BindingLease {
        _handle: VaultHandle,
        _reservation: BindingReservation,
        _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
    }
    pub(super) fn lock_binding(binding: &str) -> Result<BindingLease, String> {
        let base = vault_mutex_name()?;
        let base = String::from_utf16(&base[..base.len() - 1])
            .map_err(|_| "Invalid credential account lock name.")?;
        let name = format!("{base}.Actor.{:x}", Sha256::digest(binding.as_bytes()));
        {
            let mut active = LIVE_BINDINGS
                .lock()
                .map_err(|_| "Encryption actor ownership was interrupted. Restart Relay.")?;
            // Kernel mutexes permit recursive acquisition on their owner thread;
            // this explicit reservation also rejects a same-thread copied actor.
            if !active.insert(name.clone()) {
                return Err("Another Relay actor is using this account's encryption keys. Close it before continuing.".into());
            }
        }
        let reservation = BindingReservation { name: name.clone() };
        let wide = wide(&name);
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }
            .map_err(|_| "Windows could not reserve this encryption device.".to_string())?;
        let mut guard = VaultHandle {
            handle,
            owned: false,
        };
        match unsafe{WaitForSingleObject(handle,0)} {
            WAIT_OBJECT_0|WAIT_ABANDONED=>guard.owned=true,
            WAIT_TIMEOUT=>return Err("Another Relay process is using this account's encryption keys. Close it before continuing.".into()),
            _=>return Err("Windows could not reserve this encryption device.".into()),
        }
        // Abandoned actor ownership alone never trusts an old state: open still
        // performs DPAPI identity/revision checks before this lease is installed.
        Ok(BindingLease {
            _handle: guard,
            _reservation: reservation,
            _thread_bound: std::marker::PhantomData,
        })
    }
    fn vault_mutex_name() -> Result<Vec<u16>, String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .map_err(|_| "Windows could not identify this credential account.")?;
            let token = VaultHandle {
                handle: token,
                owned: false,
            };
            let mut needed = 0;
            let _ = GetTokenInformation(token.handle, TokenUser, None, 0, &mut needed);
            if needed == 0 || needed > 64 * 1024 {
                return Err("Windows returned invalid credential account information.".into());
            }
            let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            GetTokenInformation(
                token.handle,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .map_err(|_| "Windows could not identify this credential account.")?;
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let length = GetLengthSid(user.User.Sid) as usize;
            if length == 0 || length > 68 {
                return Err("Windows returned an invalid credential account identifier.".into());
            }
            let sid = std::slice::from_raw_parts(user.User.Sid.0.cast::<u8>(), length);
            // The account SID separates users; Global coordinates the same user
            // across console/RDP sessions as well as multiple Relay processes.
            Ok(wide(&format!(
                "Global\\Relay.CredentialVault.v1.{:x}",
                Sha256::digest(sid)
            )))
        }
    }
    /// All Relay credential reads/writes/deletions share this boundary. The
    /// closure must not recursively call another locked credential operation.
    struct CredentialQueue {
        state: std::sync::Mutex<CredentialQueueState>,
        wake: std::sync::Condvar,
    }
    #[derive(Default)]
    struct CredentialQueueState {
        next: u64,
        tickets: std::collections::VecDeque<u64>,
        active: bool,
    }
    struct CredentialPermit<'a> {
        queue: &'a CredentialQueue,
        ticket: u64,
    }
    impl CredentialQueue {
        fn new() -> Self {
            Self {
                state: std::sync::Mutex::new(CredentialQueueState::default()),
                wake: std::sync::Condvar::new(),
            }
        }
        fn enter(&self, deadline: std::time::Instant) -> Result<CredentialPermit<'_>, String> {
            let poisoned = |_| {
                "Credential storage was interrupted; restart Relay before continuing.".to_string()
            };
            let mut state = self.state.lock().map_err(poisoned)?;
            let ticket = state.next;
            state.next = state
                .next
                .checked_add(1)
                .ok_or("Credential operation counter is exhausted; restart Relay.")?;
            state.tickets.push_back(ticket);
            loop {
                if !state.active && state.tickets.front() == Some(&ticket) {
                    state.active = true;
                    return Ok(CredentialPermit {
                        queue: self,
                        ticket,
                    });
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    state.tickets.retain(|value| *value != ticket);
                    self.wake.notify_all();
                    return Err("Credential storage is busy. Try again shortly.".into());
                }
                state = self
                    .wake
                    .wait_timeout(state, remaining)
                    .map_err(|_| {
                        "Credential storage was interrupted; restart Relay before continuing."
                            .to_string()
                    })?
                    .0;
            }
        }
    }
    impl Drop for CredentialPermit<'_> {
        fn drop(&mut self) {
            if let Ok(mut state) = self.queue.state.lock() {
                let released = state.tickets.pop_front();
                debug_assert_eq!(released, Some(self.ticket));
                state.active = false;
                self.queue.wake.notify_all();
            }
        }
    }
    pub(crate) fn with_credential_lock<T>(
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        static PROCESS_QUEUE: std::sync::OnceLock<CredentialQueue> = std::sync::OnceLock::new();
        static NAME: std::sync::OnceLock<Result<Vec<u16>, String>> = std::sync::OnceLock::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let _process = PROCESS_QUEUE
            .get_or_init(CredentialQueue::new)
            .enter(deadline)?;
        let name = NAME
            .get_or_init(vault_mutex_name)
            .as_ref()
            .map_err(Clone::clone)?;
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.map_err(|_| {
            "Windows could not coordinate protected credential storage.".to_string()
        })?;
        let mut guard = VaultHandle {
            handle,
            owned: false,
        };
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .min(u32::MAX as u128) as u32;
        match unsafe { WaitForSingleObject(handle, remaining) } {
            WAIT_OBJECT_0 => guard.owned = true,
            WAIT_ABANDONED => {
                guard.owned = true;
                return Err(
                    "Protected credential storage was interrupted. This operation was cancelled."
                        .into(),
                );
            }
            WAIT_TIMEOUT => return Err("Credential storage is busy. Try again shortly.".into()),
            _ => return Err("Windows could not coordinate protected credential storage.".into()),
        }
        operation()
    }
    const MARKER_MAGIC: &[u8; 8] = b"RLYREV01";
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct RevisionMarker {
        revision: u64,
        device_hash: Option<[u8; 32]>,
    }
    fn decode_marker(bytes: &[u8]) -> Result<RevisionMarker, String> {
        match bytes.len() {
            // The legacy counter is retained only as an explicit-reset floor.
            8 => Ok(RevisionMarker {
                revision: u64::from_le_bytes(bytes.try_into().unwrap()),
                device_hash: None,
            }),
            48 if &bytes[..8] == MARKER_MAGIC => Ok(RevisionMarker {
                revision: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
                device_hash: Some(bytes[16..].try_into().unwrap()),
            }),
            _ => Err("The device's protected checkpoint marker is invalid.".into()),
        }
    }
    fn identity_hash(device_id: &str) -> Result<[u8; 32], String> {
        if device_id.is_empty() || device_id.len() > 128 || device_id.chars().any(char::is_control)
        {
            return Err("The protected checkpoint device identity is invalid.".into());
        }
        Ok(Sha256::digest(device_id.as_bytes()).into())
    }
    pub fn head(binding: &str) -> Result<Option<u64>, String> {
        with_credential_lock(|| Ok(marker_unlocked(binding)?.map(|marker| marker.revision)))
    }
    pub fn check_identity(binding: &str, device_id: &str) -> Result<(), String> {
        let expected = identity_hash(device_id)?;
        with_credential_lock(|| {
            let marker = marker_unlocked(binding)?.ok_or(
                "This device's protected checkpoint marker is missing. Its keys were preserved; use explicit recovery before sending.",
            )?;
            match marker.device_hash {
                None => Err("This device has an older counter-only checkpoint marker. Its keys were preserved; use explicit recovery to create a generation-bound device.".into()),
                Some(actual) if actual == expected => Ok(()),
                Some(_) => Err("A retired encryption device snapshot was restored. Its keys were preserved; sending is blocked to prevent key reuse.".into()),
            }
        })
    }
    fn marker_unlocked(binding: &str) -> Result<Option<RevisionMarker>, String> {
        let target = vault_target(binding);
        let mut credential = std::ptr::null_mut();
        if let Err(error) = unsafe {
            CredReadW(
                PCWSTR(target.as_ptr()),
                CRED_TYPE_GENERIC,
                0,
                &mut credential,
            )
        } {
            if error.code().0 as u32 == 0x80070490 {
                return Ok(None);
            }
            return Err("Windows could not check this device's encryption checkpoint.".into());
        }
        let result = unsafe {
            let data = &*credential;
            let result =
                if matches!(data.CredentialBlobSize, 8 | 48) && !data.CredentialBlob.is_null() {
                    let bytes = std::slice::from_raw_parts(
                        data.CredentialBlob,
                        data.CredentialBlobSize as usize,
                    );
                    decode_marker(bytes).map(Some)
                } else {
                    Err("The device's protected checkpoint marker is invalid.".into())
                };
            CredFree(credential.cast());
            result
        };
        result
    }
    pub fn set_head(binding: &str, revision: u64, device_id: &str) -> Result<(), String> {
        let device_hash = identity_hash(device_id)?;
        with_credential_lock(|| {
            if marker_unlocked(binding)?.is_some_and(|previous| previous.revision >= revision) {
                return Err("The protected checkpoint counter must advance; another actor may have used these keys.".into());
            }
            let mut target = vault_target(binding);
            let mut bytes = [0u8; 48];
            bytes[..8].copy_from_slice(MARKER_MAGIC);
            bytes[8..16].copy_from_slice(&revision.to_le_bytes());
            bytes[16..].copy_from_slice(&device_hash);
            let credential = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target.as_mut_ptr()),
                CredentialBlobSize: bytes.len() as u32,
                CredentialBlob: bytes.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };
            unsafe { CredWriteW(&credential, 0) }.map_err(|_| {
                "Windows could not save the device checkpoint marker; encryption is paused."
                    .to_string()
            })?;
            if marker_unlocked(binding)?
                != Some(RevisionMarker {
                    revision,
                    device_hash: Some(device_hash),
                })
            {
                return Err(
                    "Windows did not confirm the device checkpoint marker; encryption is paused."
                        .into(),
                );
            }
            Ok(())
        })
    }
    #[cfg(test)]
    pub(super) fn legacy_head_for_test(binding: &str, revision: u64) -> Result<(), String> {
        with_credential_lock(|| {
            let mut target = vault_target(binding);
            let mut bytes = revision.to_le_bytes();
            let credential = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target.as_mut_ptr()),
                CredentialBlobSize: bytes.len() as u32,
                CredentialBlob: bytes.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };
            unsafe { CredWriteW(&credential, 0) }
                .map_err(|_| "Windows could not write the synthetic legacy marker.".into())
        })
    }
    fn transform(bytes: &[u8], binding: &str, decrypt: bool) -> Result<Zeroizing<Vec<u8>>, String> {
        if bytes.len() > MAX_PRIVATE_BYTES + 1024 * 1024 {
            return Err("Protected encryption state exceeds its limit.".into());
        }
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr().cast_mut(),
        };
        let entropy = Sha256::digest(binding.as_bytes());
        let entropy_blob = CRYPT_INTEGER_BLOB {
            cbData: entropy.len() as u32,
            pbData: entropy.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        let result = unsafe {
            if decrypt {
                CryptUnprotectData(
                    &input,
                    None,
                    Some(&entropy_blob),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptProtectData(
                    &input,
                    PCWSTR::null(),
                    Some(&entropy_blob),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        result.map_err(|_| {
            if decrypt {
                "Windows cannot unlock this device's encryption state. Its keys were not reset."
                    .to_string()
            } else {
                "Windows cannot protect this device's encryption state; encryption is paused."
                    .to_string()
            }
        })?;
        if output.pbData.is_null() {
            return Err("Windows returned an invalid protected checkpoint.".into());
        }
        let result = unsafe {
            let slice = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
            let copied = if slice.len() <= MAX_PRIVATE_BYTES + 1024 * 1024 {
                Ok(Zeroizing::new(slice.to_vec()))
            } else {
                Err("Protected encryption state exceeds its limit.".into())
            };
            slice.fill(0);
            let _ = LocalFree(HLOCAL(output.pbData.cast()));
            copied
        };
        result
    }
    pub fn protect(bytes: &[u8], binding: &str) -> Result<Vec<u8>, String> {
        transform(bytes, binding, false).map(|v| v.to_vec())
    }
    pub fn unprotect(bytes: &[u8], binding: &str) -> Result<Zeroizing<Vec<u8>>, String> {
        transform(bytes, binding, true)
    }
    pub fn replace(source: &std::path::Path, destination: &std::path::Path) -> Result<(), String> {
        use std::os::windows::ffi::OsStrExt;
        let native_path = |path: &std::path::Path| -> Result<Vec<u16>, String> {
            let name = path.file_name().ok_or("Invalid protected checkpoint path")?;
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            // Rust's Windows canonical parent uses extended-length syntax.
            // MoveFileExW otherwise rejects long paths even though std::fs
            // already created the temporary file successfully. This conversion
            // does not change the checkpoint's identity/vault binding.
            let parent = std::fs::canonicalize(parent)
                .map_err(|_| "Cannot resolve the protected checkpoint directory")?;
            Ok(parent.join(name).as_os_str().encode_wide().chain(Some(0)).collect())
        };
        let from = native_path(source)?;
        let to = native_path(destination)?;
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|_| {
            "Cannot commit this device's protected checkpoint; encryption is paused.".into()
        })
    }
    #[cfg(test)]
    pub fn remove_head(binding: &str) {
        let _ = with_credential_lock(|| {
            let target = vault_target(binding);
            let _ = unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) };
            Ok(())
        });
    }

    #[cfg(test)]
    mod queue_tests {
        use super::*;
        use std::{
            sync::{mpsc, Arc},
            time::{Duration, Instant},
        };

        #[test]
        fn checkpoint_replace_supports_long_paths_and_preserves_state_on_failure() {
            let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
                .map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
            let fixture = tempfile::tempdir_in(root).unwrap();
            let mut folder = fixture.path().to_path_buf();
            while folder.as_os_str().len() < 310 {
                folder.push("owned-checkpoint-path-segment");
            }
            std::fs::create_dir_all(&folder).unwrap();
            let pending = folder.join("checkpoint.pending");
            let committed = folder.join("checkpoint.protected");
            std::fs::write(&pending, b"new protected checkpoint").unwrap();
            std::fs::write(&committed, b"previous protected checkpoint").unwrap();
            replace(&pending, &committed).unwrap();
            assert!(!pending.exists());
            assert_eq!(std::fs::read(&committed).unwrap(), b"new protected checkpoint");
            assert!(replace(&pending, &committed).is_err());
            assert_eq!(std::fs::read(&committed).unwrap(), b"new protected checkpoint");
        }

        #[test]
        fn credential_queue_serves_waiters_in_ticket_order_without_barging() {
            let queue = Arc::new(CredentialQueue::new());
            let held = queue
                .enter(Instant::now() + Duration::from_secs(3))
                .unwrap();
            let (sender, receiver) = mpsc::channel();
            let mut workers = Vec::new();
            for _ in 0..8 {
                let queue = queue.clone();
                let sender = sender.clone();
                workers.push(std::thread::spawn(move || {
                    let permit = queue
                        .enter(Instant::now() + Duration::from_secs(3))
                        .unwrap();
                    sender.send(permit.ticket).unwrap();
                }));
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while queue.state.lock().unwrap().tickets.len() < 9 {
                assert!(
                    Instant::now() < deadline,
                    "all fixture threads must enqueue before release"
                );
                std::thread::yield_now();
            }
            drop(held);
            let order: Vec<_> = (0..8)
                .map(|_| receiver.recv_timeout(Duration::from_secs(3)).unwrap())
                .collect();
            assert_eq!(order, (1..=8).collect::<Vec<_>>());
            for worker in workers {
                worker.join().unwrap();
            }
            assert!(queue.state.lock().unwrap().tickets.is_empty());
        }

        #[test]
        fn timed_out_credential_waiter_does_not_block_the_next_operation() {
            let queue = CredentialQueue::new();
            let held = queue
                .enter(Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert!(queue
                .enter(Instant::now() + Duration::from_millis(2))
                .is_err());
            assert_eq!(queue.state.lock().unwrap().tickets.len(), 1);
            drop(held);
            let next = queue
                .enter(Instant::now() + Duration::from_secs(1))
                .unwrap();
            assert_eq!(next.ticket, 2);
            drop(next);
            assert!(queue.state.lock().unwrap().tickets.is_empty());
        }
    }
}
