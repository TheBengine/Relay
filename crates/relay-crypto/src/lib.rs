//! Client-only MLS. Identity approval and membership authorization are explicit.
//! The delivery service stores only public KeyPackages and opaque MLS messages.
//!
//! The caller must atomically persist `checkpoint()` before publishing any result
//! of a mutation. `transact` enforces that rule and rolls back rejected input.

use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::OpenMlsProvider;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use tls_codec::{Deserialize as TlsDeserialize, Serialize as TlsSerialize};
use zeroize::Zeroizing;

pub mod files;

pub const MAX_MEMBERS: usize = 64;
pub const MAX_GROUPS: usize = 128;
pub const MAX_WIRE_BYTES: usize = 512 * 1024;
pub const MAX_APPLICATION_BYTES: usize = 32 * 1024;
pub const MAX_STATE_BYTES: usize = 32 * 1024 * 1024;
/// Trusted device keys kept by one device (verified or trusted on first use).
pub const MAX_PINS: usize = 256;
const SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519;
const CALL_PROOF_DOMAIN: &[u8] = b"Relay/MLS/call-freshness/v1\0";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallFreshnessProof {
    pub identity: Identity,
    pub nonce: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallProofContext {
    scope: String,
    proofs: Vec<CallFreshnessProof>,
}

pub type Result<T> = std::result::Result<T, String>;
fn error(context: &str, _: impl std::fmt::Debug) -> String {
    // Do not propagate library diagnostics that might contain secret material.
    format!("Encryption: {context}.")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub server_id: String,
    pub user_id: String,
    pub device_id: String,
}
impl Identity {
    fn validate(&self) -> Result<()> {
        if [&self.server_id, &self.user_id, &self.device_id]
            .into_iter()
            .any(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
        {
            return Err("Encryption: invalid device identity.".into());
        }
        Ok(())
    }
    fn credential(&self) -> Result<Credential> {
        self.validate()?;
        Ok(BasicCredential::new(
            serde_json::to_vec(self).map_err(|e| error("prepare identity", e))?,
        )
        .into())
    }
    fn from_credential(credential: &Credential) -> Result<Self> {
        if credential.credential_type() != CredentialType::Basic {
            return Err("Encryption: unsupported identity credential.".into());
        }
        let basic = BasicCredential::try_from(credential.clone())
            .map_err(|e| error("invalid identity credential", e))?;
        if basic.identity().len() > 512 {
            return Err("Encryption: device identity is too large.".into());
        }
        let value: Self = serde_json::from_slice(basic.identity())
            .map_err(|e| error("invalid identity credential", e))?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicDevice {
    pub identity: Identity,
    pub fingerprint: String,
    pub key_package: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ApprovedDevice {
    identity: Identity,
    signature_key: Vec<u8>,
    fingerprint: String,
    /// Trusted automatically the first time it was seen, without a comparison
    /// of security codes. Missing in older snapshots, which only held
    /// compared (or invitation-proven) devices.
    #[serde(default, skip_serializing_if = "is_false")]
    first_use: bool,
}
fn is_false(value: &bool) -> bool {
    !*value
}

/// How this device came to trust another device's signing key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Security codes were compared, or an invitation proof checked out.
    Verified,
    /// Trusted on first use; nobody compared the code.
    FirstUse,
}

/// What trust on first use decided for one listed device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstUse {
    /// Already trusted with exactly this key (or this device itself).
    Known,
    /// Seen for the first time and now trusted.
    Trusted,
    /// An automatically trusted device presented a different key; the new key
    /// replaced the old one. The person should be told.
    KeyReplaced,
    /// The person was verified by hand (or this verified device changed its
    /// key). Nothing is trusted until the user accepts or verifies again.
    Held,
    /// Would be trusted, but the listing carries no KeyPackage to take the
    /// signing key from. It is trusted when it appears, with exactly the
    /// listed fingerprint, in an authenticated group update.
    Deferred,
}
fn fingerprint(identity: &Identity, signature_key: &[u8]) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"Relay/MLS/device-identity/v1\0");
    hash.update(serde_json::to_vec(identity).map_err(|e| error("prepare fingerprint", e))?);
    hash.update(signature_key);
    Ok(format!("{:x}", hash.finalize()))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroupState {
    pub group_id: String,
    pub controller: Identity,
    pub epoch: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRotation {
    commit: Vec<u8>,
    epoch: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    proofs: Vec<CallFreshnessProof>,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    identity: Identity,
    signer: SignatureKeyPair,
    values: Vec<(Vec<u8>, Vec<u8>)>,
    approved: BTreeMap<String, ApprovedDevice>,
    groups: BTreeMap<String, GroupState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_rotations: BTreeMap<String, PendingRotation>,
}

/// Preserve the saved schema while serializing the actor's already-owned data.
/// The storage read guard stays held until serialization finishes.
#[derive(Serialize)]
struct SnapshotView<'a> {
    version: u32,
    identity: &'a Identity,
    signer: &'a SignatureKeyPair,
    values: Vec<(&'a Vec<u8>, &'a Vec<u8>)>,
    approved: &'a BTreeMap<String, ApprovedDevice>,
    groups: &'a BTreeMap<String, GroupState>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pending_rotations: &'a BTreeMap<String, PendingRotation>,
}

/// Single-owner object. Never share it between UI, HTTP, and media threads.
pub struct Device {
    provider: OpenMlsRustCrypto,
    identity: Identity,
    signer: SignatureKeyPair,
    approved: BTreeMap<String, ApprovedDevice>,
    groups: BTreeMap<String, GroupState>,
    pending_rotations: BTreeMap<String, PendingRotation>,
    blocked: bool,
    /// Devices trust on first use accepted from the directory without a
    /// KeyPackage: (identity, fingerprint). Never saved; the next directory
    /// read rebuilds it. Only a leaf with a matching fingerprint is adopted.
    first_use_candidates: BTreeMap<String, (Identity, String)>,
}
#[derive(Clone, Debug)]
pub struct MembershipChange {
    pub commit: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
    pub epoch: u64,
}
#[derive(Debug)]
pub enum Received {
    Application { author: Identity, bytes: Vec<u8> },
    Membership { epoch: u64 },
}
/// Unauthenticated public epoch, used only to schedule attempts at authenticated decryption.
pub fn application_epoch(bytes: &[u8]) -> Result<u64> {
    if bytes.len() > MAX_WIRE_BYTES {
        return Err("Encryption: message exceeds its wire limit.".into());
    }
    let message = MlsMessageIn::tls_deserialize_exact(bytes)
        .map_err(|e| error("invalid MLS framing", e))?
        .try_into_protocol_message()
        .map_err(|e| error("invalid MLS message", e))?;
    if message.wire_format() != WireFormat::PrivateMessage
        || message.content_type() != ContentType::Application
    {
        return Err("Encryption: expected a private application message.".into());
    }
    Ok(message.epoch().as_u64())
}

impl Device {
    pub fn new(identity: Identity) -> Result<Self> {
        identity.validate()?;
        let signer = SignatureKeyPair::new(SUITE.signature_algorithm())
            .map_err(|e| error("create device signing key", e))?;
        let mut device = Self {
            provider: OpenMlsRustCrypto::default(),
            identity,
            signer,
            approved: BTreeMap::new(),
            groups: BTreeMap::new(),
            pending_rotations: BTreeMap::new(),
            blocked: false,
            first_use_candidates: BTreeMap::new(),
        };
        let own = ApprovedDevice {
            identity: device.identity.clone(),
            signature_key: device.signer.to_public_vec(),
            fingerprint: device.fingerprint()?,
            first_use: false,
        };
        device.approved.insert(own.identity.device_id.clone(), own);
        Ok(device)
    }
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    pub fn fingerprint(&self) -> Result<String> {
        fingerprint(&self.identity, &self.signer.to_public_vec())
    }
    pub fn is_approved(&self, device: &PublicDevice) -> bool {
        self.approved
            .get(&device.identity.device_id)
            .is_some_and(|pin| {
                pin.identity == device.identity && pin.fingerprint == device.fingerprint
            })
    }
    pub fn expected_fingerprint(&self, device_id: &str) -> Option<&str> {
        self.approved
            .get(device_id)
            .map(|pin| pin.fingerprint.as_str())
    }
    /// The trust of exactly this identity and key, if it is trusted at all.
    pub fn trust(&self, device: &PublicDevice) -> Option<Trust> {
        self.approved
            .get(&device.identity.device_id)
            .filter(|pin| pin.identity == device.identity && pin.fingerprint == device.fingerprint)
            .map(|pin| {
                if pin.first_use {
                    Trust::FirstUse
                } else {
                    Trust::Verified
                }
            })
    }
    /// The trust of whatever key is pinned for this device ID.
    pub fn pin_trust(&self, device_id: &str) -> Option<Trust> {
        self.approved.get(device_id).map(|pin| {
            if pin.first_use {
                Trust::FirstUse
            } else {
                Trust::Verified
            }
        })
    }
    /// Verified by comparing codes or by an invitation proof, with this key.
    pub fn is_verified(&self, device: &PublicDevice) -> bool {
        self.trust(device) == Some(Trust::Verified)
    }
    /// True when the user verified any device of this person. Such a person
    /// never gets a device or key trusted without the user's say. The own
    /// account is excluded: this device's own pin is not a choice.
    pub fn person_verified(&self, user_id: &str) -> bool {
        user_id != self.identity.user_id
            && self
                .approved
                .values()
                .any(|pin| pin.identity.user_id == user_id && !pin.first_use)
    }
    /// True when the listing's KeyPackage is signed by the key its
    /// fingerprint names, for this community.
    pub fn listing_is_valid(&self, device: &PublicDevice) -> bool {
        self.validate_package(device).is_ok()
    }
    /// The decision trust on first use would make, without changing anything.
    pub fn first_use_decision(&self, device: &PublicDevice) -> FirstUse {
        if device.identity == self.identity || self.trust(device).is_some() {
            return FirstUse::Known;
        }
        let pin = self.approved.get(&device.identity.device_id);
        let own_account = device.identity.user_id == self.identity.user_id;
        if !own_account
            && (self.person_verified(&device.identity.user_id)
                || pin.is_some_and(|p| !p.first_use))
        {
            return FirstUse::Held;
        }
        if device.key_package.is_empty() {
            return if pin.is_some() {
                // A changed key cannot be taken from a listing without a package.
                FirstUse::Held
            } else {
                FirstUse::Deferred
            };
        }
        if pin.is_some() {
            FirstUse::KeyReplaced
        } else {
            FirstUse::Trusted
        }
    }
    /// Trust on first use for one listed device. A device of a person the user
    /// verified by hand is never trusted here (`Held`). An invalid listing
    /// returns an error and changes nothing.
    pub fn trust_on_first_use(&mut self, device: &PublicDevice) -> Result<FirstUse> {
        self.guard()?;
        let decision = self.first_use_decision(device);
        match decision {
            FirstUse::Known | FirstUse::Held => {}
            FirstUse::Deferred => {
                if device.identity.server_id != self.identity.server_id
                    || device.fingerprint.len() != 64
                {
                    return Err("Encryption: invalid or foreign device listing.".into());
                }
                if self.first_use_candidates.len() >= MAX_PINS
                    && !self
                        .first_use_candidates
                        .contains_key(&device.identity.device_id)
                {
                    return Err("This device has reached its verification limit.".into());
                }
                self.first_use_candidates.insert(
                    device.identity.device_id.clone(),
                    (device.identity.clone(), device.fingerprint.clone()),
                );
            }
            FirstUse::Trusted | FirstUse::KeyReplaced => self.pin(device, true)?,
        }
        Ok(decision)
    }
    /// The user accepted a changed security code with one click (`None`), or
    /// compared the new code (`Some`). Replaces any earlier pin of the device.
    pub fn accept_new_key(
        &mut self,
        device: &PublicDevice,
        compared_fingerprint: Option<&str>,
    ) -> Result<()> {
        self.guard()?;
        if device.identity.device_id == self.identity.device_id {
            return Err("You cannot replace the key of this device.".into());
        }
        if compared_fingerprint.is_some_and(|code| code != device.fingerprint) {
            return Err("The device fingerprint does not match. No key was shared.".into());
        }
        self.pin(device, compared_fingerprint.is_none())
    }
    fn pin(&mut self, device: &PublicDevice, first_use: bool) -> Result<()> {
        let package = self.validate_package(device)?;
        if self.approved.len() >= MAX_PINS
            && !self.approved.contains_key(&device.identity.device_id)
        {
            return Err("This device has reached its verification limit.".into());
        }
        self.first_use_candidates.remove(&device.identity.device_id);
        self.approved.insert(
            device.identity.device_id.clone(),
            ApprovedDevice {
                identity: device.identity.clone(),
                fingerprint: device.fingerprint.clone(),
                signature_key: package.leaf_node().signature_key().as_slice().to_vec(),
                first_use,
            },
        );
        Ok(())
    }
    /// Removes the "verified" mark but keeps trusting the same key, as after
    /// trust on first use. The device stays in its conversations.
    pub fn unverify(&mut self, device_id: &str) -> Result<()> {
        self.guard()?;
        if device_id == self.identity.device_id {
            return Err("You cannot remove verification of this device.".into());
        }
        if let Some(pin) = self.approved.get_mut(device_id) {
            pin.first_use = true;
        }
        Ok(())
    }
    /// Number of trusted device keys, verified or not.
    pub fn pin_count(&self) -> usize {
        self.approved.len()
    }
    /// Forgets automatically trusted devices that are neither listed (`keep`)
    /// nor members of a conversation here, so the bounded pin store does not
    /// fill with retired devices. Verified pins are never forgotten.
    pub fn forget_unused_first_use(&mut self, keep: &HashSet<String>) -> Result<usize> {
        self.guard()?;
        let mut members = HashSet::new();
        for state in self.groups.values() {
            if let Some(group) = MlsGroup::load(
                self.provider.storage(),
                &GroupId::from_slice(state.group_id.as_bytes()),
            )
            .map_err(|e| error("load group members", e))?
            {
                for member in group.members() {
                    if let Ok(identity) = Identity::from_credential(&member.credential) {
                        members.insert(identity.device_id);
                    }
                }
            }
        }
        let before = self.approved.len();
        let own = self.identity.device_id.clone();
        self.approved.retain(|id, pin| {
            !pin.first_use || id == &own || keep.contains(id) || members.contains(id)
        });
        Ok(before - self.approved.len())
    }
    /// Members of a conversation that this device does not trust with their
    /// current key: no pin, or a pin for a different key. Only the key
    /// manager acts on this, by removing them.
    pub fn untrusted_members(&self, scope: &str) -> Result<Vec<String>> {
        let group = self.load_group_for_commit(scope)?;
        Ok(group
            .members()
            .filter_map(|member| {
                let identity = Identity::from_credential(&member.credential).ok()?;
                self.approved_member(&member.credential, &member.signature_key)
                    .is_err()
                    .then_some(identity.device_id)
            })
            .collect())
    }
    /// Trust on first use for a leaf of an authenticated group update.
    ///
    /// A device listed without a KeyPackage is adopted only with exactly its
    /// listed fingerprint. With `vouched` (a device the key manager added, or
    /// a member of a Welcome other than its sender), an unlisted device of a
    /// person nobody verified is adopted too: it may have been added and
    /// revoked while this device was offline, and refusing would stop the
    /// conversation for good. A verified person's device is never adopted.
    fn adopt_first_use(
        &mut self,
        credential: &Credential,
        signature_key: &[u8],
        vouched: bool,
    ) -> Result<()> {
        let identity = Identity::from_credential(credential)?;
        if self.approved.contains_key(&identity.device_id) {
            return Ok(());
        }
        let presented = fingerprint(&identity, signature_key)?;
        match self.first_use_candidates.get(&identity.device_id) {
            Some((listed, listed_fingerprint)) => {
                if *listed != identity || *listed_fingerprint != presented {
                    return Ok(());
                }
            }
            None => {
                if !vouched
                    || identity.server_id != self.identity.server_id
                    || self.person_verified(&identity.user_id)
                {
                    return Ok(());
                }
            }
        }
        if self.approved.len() >= MAX_PINS {
            return Err("This device has reached its verification limit.".into());
        }
        self.first_use_candidates.remove(&identity.device_id);
        self.approved.insert(
            identity.device_id.clone(),
            ApprovedDevice {
                fingerprint: presented,
                identity,
                signature_key: signature_key.to_vec(),
                first_use: true,
            },
        );
        Ok(())
    }
    pub fn group_state(&self, scope: &str) -> Option<&GroupState> {
        self.groups.get(scope)
    }
    pub fn scopes(&self) -> Vec<String> {
        self.groups.keys().cloned().collect()
    }
    pub fn is_blocked(&self) -> bool {
        self.blocked
    }
    fn guard(&self) -> Result<()> {
        if self.blocked {
            Err("Encryption paused: the device state could not be saved. Restart Relay before sending or joining a call.".into())
        } else {
            Ok(())
        }
    }
    fn guard_rotation_idle(&self, scope: &str) -> Result<()> {
        self.guard()?;
        if self.pending_rotations.contains_key(scope) {
            return Err("Encryption paused: the saved key rotation needs confirmation.".into());
        }
        Ok(())
    }
    /// Public identity advertisement. Its private init key stays in the snapshot.
    pub fn key_package(&mut self) -> Result<PublicDevice> {
        self.guard()?;
        let bundle = KeyPackage::builder()
            .build(SUITE, &self.provider, &self.signer, self.credential()?)
            .map_err(|e| error("create device KeyPackage", e))?;
        Ok(PublicDevice {
            identity: self.identity.clone(),
            fingerprint: self.fingerprint()?,
            key_package: bundle
                .key_package()
                .tls_serialize_detached()
                .map_err(|e| error("encode device KeyPackage", e))?,
        })
    }
    fn credential(&self) -> Result<CredentialWithKey> {
        Ok(CredentialWithKey {
            credential: self.identity.credential()?,
            signature_key: self.signer.to_public_vec().into(),
        })
    }
    fn validate_package(&self, device: &PublicDevice) -> Result<KeyPackage> {
        if device.identity.server_id != self.identity.server_id
            || device.key_package.len() > 16 * 1024
            || device.fingerprint.len() != 64
        {
            return Err("Encryption: invalid or foreign device KeyPackage.".into());
        }
        let package = KeyPackageIn::tls_deserialize_exact(&device.key_package)
            .map_err(|e| error("decode device KeyPackage", e))?
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(|e| error("verify device KeyPackage signature or lifetime", e))?;
        if package.ciphersuite() != SUITE
            || Identity::from_credential(package.leaf_node().credential())? != device.identity
            || fingerprint(
                &device.identity,
                package.leaf_node().signature_key().as_slice(),
            )? != device.fingerprint
        {
            return Err("Encryption: the device identity does not match its signed key.".into());
        }
        Ok(package)
    }
    /// Must be called only after the human compares this fingerprint externally.
    pub fn approve(&mut self, device: &PublicDevice, compared_fingerprint: &str) -> Result<()> {
        self.guard()?;
        if compared_fingerprint != device.fingerprint {
            return Err("The device fingerprint does not match. No key was shared.".into());
        }
        if let Some(pin) = self.approved.get_mut(&device.identity.device_id) {
            if pin.identity != device.identity || pin.fingerprint != device.fingerprint {
                return Err(
                    "This device's security code changed. Accept or compare the new code instead."
                        .into(),
                );
            }
            // The key was already validated when it was first trusted; an
            // existing member may be listed without an unused KeyPackage.
            pin.first_use = false;
            return Ok(());
        }
        self.pin(device, false)
    }
    pub fn unapprove(&mut self, device_id: &str) -> Result<()> {
        self.guard()?;
        if device_id == self.identity.device_id {
            return Err("You cannot remove verification of this device.".into());
        }
        self.approved.remove(device_id);
        Ok(())
    }
    fn approved_member(&self, credential: &Credential, signature_key: &[u8]) -> Result<Identity> {
        let identity = Identity::from_credential(credential)?;
        let pin = self
            .approved
            .get(&identity.device_id)
            .ok_or("Verify every group member's device before sharing or receiving keys.")?;
        if pin.identity != identity || pin.signature_key != signature_key {
            return Err("Encryption paused: a group member's device identity changed.".into());
        }
        Ok(identity)
    }
    fn validate_members(&self, group: &MlsGroup) -> Result<()> {
        if group.members().count() > MAX_MEMBERS {
            return Err("Encrypted groups support at most 64 devices.".into());
        }
        for member in group.members() {
            self.approved_member(&member.credential, &member.signature_key)?;
        }
        Ok(())
    }
    fn load_group(&self, scope: &str) -> Result<MlsGroup> {
        let group = self.load_group_for_commit(scope)?;
        self.validate_members(&group)?;
        Ok(group)
    }
    fn load_group_for_commit(&self, scope: &str) -> Result<MlsGroup> {
        self.guard()?;
        let state = self
            .groups
            .get(scope)
            .ok_or("Encryption keys are not ready for this conversation.")?;
        let group = MlsGroup::load(
            self.provider.storage(),
            &GroupId::from_slice(state.group_id.as_bytes()),
        )
        .map_err(|e| error("load encrypted group", e))?
        .ok_or("Encryption paused: group state is missing.")?;
        if !group.is_active() {
            return Err("This device is no longer a member of the encrypted conversation.".into());
        }
        Ok(group)
    }
    pub fn create_group(&mut self, scope: &str, group_id: &str) -> Result<()> {
        self.guard()?;
        if scope.is_empty()
            || scope.len() > 512
            || group_id.is_empty()
            || group_id.len() > 128
            || self.groups.contains_key(scope)
            || self.groups.values().any(|group| group.group_id == group_id)
        {
            return Err("Encryption: invalid or duplicate group.".into());
        }
        if self.groups.len() >= MAX_GROUPS {
            return Err(format!(
                "This device has reached its limit of {MAX_GROUPS} encrypted conversations."
            ));
        }
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(SUITE)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .use_ratchet_tree_extension(true)
            .max_past_epochs(1)
            .build();
        MlsGroup::new_with_group_id(
            &self.provider,
            &self.signer,
            &config,
            GroupId::from_slice(group_id.as_bytes()),
            self.credential()?,
        )
        .map_err(|e| error("create encrypted group", e))?;
        self.groups.insert(
            scope.into(),
            GroupState {
                group_id: group_id.into(),
                controller: self.identity.clone(),
                epoch: 0,
            },
        );
        Ok(())
    }
    pub fn members(&self, scope: &str) -> Result<Vec<Identity>> {
        self.load_group(scope)?
            .members()
            .map(|member| Identity::from_credential(&member.credential))
            .collect()
    }
    pub fn member_identities_for_update(&self, scope: &str) -> Result<Vec<Identity>> {
        self.load_group_for_commit(scope)?
            .members()
            .map(|member| Identity::from_credential(&member.credential))
            .collect()
    }
    pub fn rotate_group(&mut self, scope: &str) -> Result<MembershipChange> {
        self.guard_rotation_idle(scope)?;
        let mut group = self.load_group(scope)?;
        if self.groups[scope].controller != self.identity {
            return Err("Only the verified key manager can rotate this group's keys.".into());
        }
        let commit = group
            .self_update(&self.provider, &self.signer, LeafNodeParameters::default())
            .map_err(|e| error("rotate encrypted group", e))?
            .into_commit();
        group
            .merge_pending_commit(&self.provider)
            .map_err(|e| error("merge key rotation", e))?;
        self.groups.get_mut(scope).unwrap().epoch = group.epoch().as_u64();
        Ok(MembershipChange {
            commit: encode(&commit)?,
            welcome: None,
            epoch: group.epoch().as_u64(),
        })
    }
    fn validate_pending_rotation(&self, scope: &str, group: &MlsGroup) -> Result<()> {
        let rotation = self
            .pending_rotations
            .get(scope)
            .ok_or("Encryption: no saved key rotation exists.")?;
        let pending = group
            .pending_commit()
            .ok_or("Encryption: the saved key rotation is missing its private state.")?;
        let state = &self.groups[scope];
        Self::validate_call_proofs(&rotation.proofs, group)?;
        let message = parse(&rotation.commit)?
            .try_into_protocol_message()
            .map_err(|e| error("invalid saved key rotation", e))?;
        let leaf = pending
            .update_path_leaf_node()
            .ok_or("Encryption: the saved key rotation has no update path.")?;
        if state.controller != self.identity
            || self.groups.iter().any(|(other_scope, other)| {
                other_scope != scope && other.group_id == state.group_id
            })
            || group.epoch().as_u64() != state.epoch
            || state.epoch.checked_add(1) != Some(rotation.epoch)
            || pending.epoch().as_u64() != rotation.epoch
            || pending.queued_proposals().next().is_some()
            || Identity::from_credential(leaf.credential())? != self.identity
            || leaf.signature_key().as_slice() != self.signer.to_public_vec()
            || message.wire_format() != WireFormat::PrivateMessage
            || message.content_type() != ContentType::Commit
            || message.is_external()
            || message.group_id() != group.group_id()
            || message.epoch().as_u64() != state.epoch
        {
            return Err("Encryption: the saved key rotation does not match this group.".into());
        }
        Ok(())
    }
    /// Controller-only durable rotation. The caller owns actor freshness.
    /// Use `transact`: persist before publishing. Repeated calls return exactly
    /// the saved ciphertext, including after reopening the protected snapshot.
    /// No call keys or outgoing chat are released while this rotation is pending.
    pub fn pending_key_rotation(&self, scope: &str) -> Result<Option<MembershipChange>> {
        self.guard()?;
        if !self.pending_rotations.contains_key(scope) {
            return Ok(None);
        }
        let group = self.load_group_for_commit(scope)?;
        self.validate_pending_rotation(scope, &group)?;
        let rotation = &self.pending_rotations[scope];
        Ok(Some(MembershipChange {
            commit: rotation.commit.clone(),
            welcome: None,
            epoch: rotation.epoch,
        }))
    }

    /// Persist the returned commit before publishing; see `pending_key_rotation`.
    pub fn stage_key_rotation(&mut self, scope: &str) -> Result<MembershipChange> {
        self.stage_key_rotation_with_proofs(scope, &[])
    }
    fn validate_call_proofs(proofs: &[CallFreshnessProof], group: &MlsGroup) -> Result<()> {
        if proofs.len() > MAX_MEMBERS {
            return Err("Encryption: too many call challenges.".into());
        }
        let members = group
            .members()
            .map(|m| Identity::from_credential(&m.credential))
            .collect::<Result<Vec<_>>>()?;
        let mut seen = HashSet::new();
        for proof in proofs {
            proof.identity.validate()?;
            if !members.contains(&proof.identity) || !seen.insert(proof.identity.device_id.as_str())
            {
                return Err(
                    "Encryption: call challenge does not match verified membership.".into(),
                );
            }
        }
        Ok(())
    }
    pub fn pending_call_proofs(&self, scope: &str) -> Result<Vec<CallFreshnessProof>> {
        self.pending_key_rotation(scope)?;
        Ok(self
            .pending_rotations
            .get(scope)
            .map(|r| r.proofs.clone())
            .unwrap_or_default())
    }
    /// A controller-only self-update binds authenticated participant challenges.
    /// Proofs are additional authenticated context, never membership proposals.
    /// A saved operation is retried exactly; later challenges wait for the next one.
    pub fn stage_key_rotation_with_proofs(
        &mut self,
        scope: &str,
        proofs: &[CallFreshnessProof],
    ) -> Result<MembershipChange> {
        let mut group = self.load_group(scope)?;
        if self.groups[scope].controller != self.identity {
            return Err("Only the verified key manager can rotate this group's keys.".into());
        }
        if !self.pending_rotations.contains_key(scope) {
            Self::validate_call_proofs(proofs, &group)?;
            let aad = if proofs.is_empty() {
                scope.as_bytes().to_vec()
            } else {
                let mut aad = CALL_PROOF_DOMAIN.to_vec();
                aad.extend(
                    serde_json::to_vec(&CallProofContext {
                        scope: scope.into(),
                        proofs: proofs.to_vec(),
                    })
                    .map_err(|e| error("prepare call challenge context", e))?,
                );
                aad
            };
            group.set_aad(aad);
            let commit = group
                .self_update(&self.provider, &self.signer, LeafNodeParameters::default())
                .map_err(|e| error("stage encrypted key rotation", e))?
                .into_commit();
            let epoch = self.groups[scope]
                .epoch
                .checked_add(1)
                .ok_or("Encryption: group epoch limit reached.")?;
            self.pending_rotations.insert(
                scope.into(),
                PendingRotation {
                    commit: encode(&commit)?,
                    epoch,
                    proofs: proofs.to_vec(),
                },
            );
        }
        self.validate_pending_rotation(scope, &group)?;
        let rotation = &self.pending_rotations[scope];
        Ok(MembershipChange {
            commit: rotation.commit.clone(),
            welcome: None,
            epoch: rotation.epoch,
        })
    }
    /// The caller must validate the complete service operation/batch binding
    /// before passing its exact commit here, then persist through `transact`.
    /// This comparison is not proof of delivery to other members. There is no
    /// timeout cancellation: an authenticated authorized competing commit may
    /// supersede the pending commit through `receive`, without ratchet rollback.
    pub fn confirm_key_rotation(&mut self, scope: &str, confirmed_commit: &[u8]) -> Result<u64> {
        let mut group = self.load_group(scope)?;
        self.validate_pending_rotation(scope, &group)?;
        if self.pending_rotations[scope].commit != confirmed_commit {
            return Err(
                "Encryption: key rotation confirmation does not match the saved commit.".into(),
            );
        }
        let bindings: Vec<_> = group
            .members()
            .map(|m| (m.credential, m.signature_key))
            .collect();
        let expected_epoch = self.pending_rotations[scope].epoch;
        group
            .merge_pending_commit(&self.provider)
            .map_err(|e| error("confirm encrypted key rotation", e))?;
        let after: Vec<_> = group
            .members()
            .map(|m| (m.credential, m.signature_key))
            .collect();
        if after != bindings || group.epoch().as_u64() != expected_epoch {
            return Err("Encryption: key rotation changed verified membership.".into());
        }
        self.validate_members(&group)?;
        self.groups.get_mut(scope).unwrap().epoch = expected_epoch;
        self.pending_rotations.remove(scope);
        Ok(expected_epoch)
    }
    /// Used only after the service confirms that this initial claim lost its race.
    /// No application message may have been published from the discarded group.
    pub fn forget_initial_claim(&mut self, scope: &str) -> Result<()> {
        self.guard_rotation_idle(scope)?;
        let mut group = self.load_group_for_commit(scope)?;
        if self.groups[scope].controller != self.identity || group.epoch().as_u64() > 1 {
            return Err("A previously used encryption group cannot be reset automatically.".into());
        }
        group
            .delete(self.provider.storage())
            .map_err(|e| error("discard unpublished group claim", e))?;
        self.groups.remove(scope);
        Ok(())
    }
    pub fn add_members(
        &mut self,
        scope: &str,
        devices: &[PublicDevice],
    ) -> Result<MembershipChange> {
        self.guard_rotation_idle(scope)?;
        let mut group = self.load_group(scope)?;
        if self.groups[scope].controller != self.identity || devices.is_empty() {
            return Err(
                "Only the conversation's verified key manager can approve membership.".into(),
            );
        }
        if group.members().count() + devices.len() > MAX_MEMBERS {
            return Err(format!(
                "This encrypted conversation supports up to {MAX_MEMBERS} devices."
            ));
        }
        let mut packages = Vec::new();
        let mut seen = HashSet::new();
        let members = self.members(scope)?;
        for device in devices {
            if !self.is_approved(device)
                || members.contains(&device.identity)
                || !seen.insert(device.identity.device_id.clone())
            {
                return Err("Encryption: unverified or duplicate group device.".into());
            }
            packages.push(self.validate_package(device)?);
        }
        let (commit, welcome, _) = group
            .add_members(&self.provider, &self.signer, &packages)
            .map_err(|e| error("add encrypted group member", e))?;
        group
            .merge_pending_commit(&self.provider)
            .map_err(|e| error("merge member key change", e))?;
        self.groups.get_mut(scope).unwrap().epoch = group.epoch().as_u64();
        Ok(MembershipChange {
            commit: encode(&commit)?,
            welcome: Some(encode(&welcome)?),
            epoch: group.epoch().as_u64(),
        })
    }
    pub fn remove_members(
        &mut self,
        scope: &str,
        device_ids: &[String],
    ) -> Result<MembershipChange> {
        self.guard_rotation_idle(scope)?;
        // Previously trusted members may already have been locally revoked.
        // Their persisted membership can only be removed, never used for sending.
        let mut group = self.load_group_for_commit(scope)?;
        let remaining: Vec<_> = group
            .members()
            .filter_map(|member| Identity::from_credential(&member.credential).ok())
            .filter(|member| !device_ids.contains(&member.device_id))
            .collect();
        let handover = device_ids.contains(&self.groups[scope].controller.device_id)
            && remaining.first() == Some(&self.identity);
        if (self.groups[scope].controller != self.identity && !handover)
            || device_ids.is_empty()
            || device_ids.contains(&self.identity.device_id)
        {
            return Err("Only the conversation's verified key manager can remove members.".into());
        }
        let indices: Vec<_> = group
            .members()
            .filter_map(|member| {
                Identity::from_credential(&member.credential)
                    .ok()
                    .filter(|identity| device_ids.contains(&identity.device_id))
                    .map(|_| member.index)
            })
            .collect();
        if indices.len() != device_ids.len() {
            return Err("Encryption: member to remove was not found.".into());
        }
        let (commit, welcome, _) = group
            .remove_members(&self.provider, &self.signer, &indices)
            .map_err(|e| error("remove encrypted group member", e))?;
        group
            .merge_pending_commit(&self.provider)
            .map_err(|e| error("merge member removal", e))?;
        let state = self.groups.get_mut(scope).unwrap();
        state.epoch = group.epoch().as_u64();
        if handover {
            state.controller = self.identity.clone();
        }
        Ok(MembershipChange {
            commit: encode(&commit)?,
            welcome: welcome.map(|w| encode(&w)).transpose()?,
            epoch: group.epoch().as_u64(),
        })
    }
    pub fn join(
        &mut self,
        scope: &str,
        group_id: &str,
        controller: Identity,
        wire: &[u8],
    ) -> Result<()> {
        self.guard()?;
        let previous = self.groups.get(scope).cloned();
        if previous
            .as_ref()
            .is_some_and(|g| g.group_id != group_id || g.controller != controller)
            || (previous.is_some() && self.is_active(scope)?)
            || self
                .groups
                .iter()
                .any(|(other_scope, group)| other_scope != scope && group.group_id == group_id)
            || scope.is_empty()
            || scope.len() > 512
            || group_id.is_empty()
            || group_id.len() > 128
        {
            return Err("Encryption: invalid or duplicate group welcome.".into());
        }
        if previous.is_none() && self.groups.len() >= MAX_GROUPS {
            return Err(format!(
                "This device has reached its limit of {MAX_GROUPS} encrypted conversations."
            ));
        }
        if let Some(previous) = previous {
            let mut old = MlsGroup::load(
                self.provider.storage(),
                &GroupId::from_slice(previous.group_id.as_bytes()),
            )
            .map_err(|e| error("load retired group", e))?
            .ok_or("Encryption: retired group state is missing.")?;
            old.delete(self.provider.storage())
                .map_err(|e| error("retire previous group state", e))?;
        }
        let MlsMessageBodyIn::Welcome(welcome) = parse(wire)?.extract() else {
            return Err("Encryption: expected a group welcome.".into());
        };
        let config = MlsGroupJoinConfig::builder()
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .use_ratchet_tree_extension(true)
            .max_past_epochs(1)
            .build();
        let staged = StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None)
            .map_err(|e| error("verify group welcome", e))?;
        let sender = staged
            .welcome_sender()
            .map_err(|e| error("verify welcome signer", e))?;
        if Identity::from_credential(sender.credential())? != controller {
            return Err(
                "Encryption: the group welcome signer does not match its key manager.".into(),
            );
        }
        self.adopt_first_use(sender.credential(), sender.signature_key().as_slice(), false)?;
        self.approved_member(sender.credential(), sender.signature_key().as_slice())?;
        let group = staged
            .into_group(&self.provider)
            .map_err(|e| error("join encrypted group", e))?;
        if group.group_id().as_slice() != group_id.as_bytes() || group.ciphersuite() != SUITE {
            return Err("Encryption: group welcome belongs to another conversation.".into());
        }
        let leaves: Vec<_> = group
            .members()
            .map(|m| (m.credential, m.signature_key))
            .collect();
        for (credential, signature_key) in &leaves {
            self.adopt_first_use(credential, signature_key, true)?;
        }
        self.validate_members(&group)?;
        self.groups.insert(
            scope.into(),
            GroupState {
                group_id: group_id.into(),
                controller,
                epoch: group.epoch().as_u64(),
            },
        );
        Ok(())
    }
    pub fn is_active(&self, scope: &str) -> Result<bool> {
        self.guard()?;
        let Some(state) = self.groups.get(scope) else {
            return Ok(false);
        };
        let group = MlsGroup::load(
            self.provider.storage(),
            &GroupId::from_slice(state.group_id.as_bytes()),
        )
        .map_err(|e| error("load group status", e))?
        .ok_or("Encryption: group state is missing.")?;
        Ok(group.is_active())
    }
    pub fn encrypt(&mut self, scope: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.guard_rotation_idle(scope)?;
        if plaintext.len() > MAX_APPLICATION_BYTES {
            return Err("The encrypted message is too large.".into());
        }
        let mut group = self.load_group(scope)?;
        group.set_aad(scope.as_bytes().to_vec());
        let message = group
            .create_message(&self.provider, &self.signer, plaintext)
            .map_err(|e| error("encrypt message", e))?;
        encode(&message)
    }
    pub fn receive(&mut self, scope: &str, wire: &[u8]) -> Result<Received> {
        self.receive_with_call_proofs(scope, wire)
            .map(|(received, _)| received)
    }
    /// Returns proof context only after authentication, strict self-update
    /// validation and merge. Persist the result before using any proof.
    pub fn receive_with_call_proofs(
        &mut self,
        scope: &str,
        wire: &[u8],
    ) -> Result<(Received, Vec<CallFreshnessProof>)> {
        let message = parse(wire)?
            .try_into_protocol_message()
            .map_err(|e| error("invalid encrypted message", e))?;
        if message.wire_format() != WireFormat::PrivateMessage || message.is_external() {
            return Err(
                "Encryption: unencrypted messages and external membership changes are blocked."
                    .into(),
            );
        }
        let mut group = if message.content_type() == ContentType::Commit {
            self.load_group_for_commit(scope)?
        } else {
            self.load_group(scope)?
        };
        let processed = group
            .process_message(&self.provider, message)
            .map_err(|e| error("verify or decrypt message", e))?;
        let author = Identity::from_credential(processed.credential())?;
        let member = group
            .members()
            .find(|member| {
                Identity::from_credential(&member.credential).ok().as_ref() == Some(&author)
            })
            .ok_or("Encryption: message author is not a group member.")?;
        self.approved_member(&member.credential, &member.signature_key)?;
        let aad_matches = processed.aad() == scope.as_bytes();
        let call_proof = if let Some(bytes) = processed.aad().strip_prefix(CALL_PROOF_DOMAIN) {
            if bytes.len() > 64 * 1024 {
                return Err("Encryption: call proof context exceeds its limit.".into());
            }
            let context: CallProofContext = serde_json::from_slice(bytes)
                .map_err(|e| error("invalid call proof context", e))?;
            if context.scope != scope || context.proofs.is_empty() {
                return Err("Encryption: call proof has the wrong conversation context.".into());
            }
            Self::validate_call_proofs(&context.proofs, &group)?;
            Some(context.proofs)
        } else {
            None
        };
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(application) => {
                let bytes = application.into_bytes();
                if !aad_matches || bytes.len() > MAX_APPLICATION_BYTES {
                    return Err("Encryption: invalid conversation context or message size.".into());
                }
                Ok((Received::Application { author, bytes }, Vec::new()))
            }
            ProcessedMessageContent::StagedCommitMessage(commit) => {
                let bindings: Vec<_> = group
                    .members()
                    .map(|m| (m.credential, m.signature_key))
                    .collect();
                if call_proof.is_some() {
                    let leaf = commit
                        .update_path_leaf_node()
                        .ok_or("Encryption: call proof update is missing its update path.")?;
                    if author != self.groups[scope].controller
                        || commit.queued_proposals().next().is_some()
                        || Identity::from_credential(leaf.credential())? != author
                        || leaf.signature_key().as_slice() != member.signature_key
                    {
                        return Err(
                            "Encryption: call proof attempted to change membership or identity."
                                .into(),
                        );
                    }
                }
                let removed: Vec<_> = commit
                    .remove_proposals()
                    .map(|p| p.remove_proposal().removed())
                    .collect();
                let old_controller_removed = group.members().any(|member| {
                    removed.contains(&member.index)
                        && Identity::from_credential(&member.credential).ok().as_ref()
                            == Some(&self.groups[scope].controller)
                });
                let successor = group
                    .members()
                    .find(|member| !removed.contains(&member.index))
                    .map(|member| Identity::from_credential(&member.credential))
                    .transpose()?;
                let handover = old_controller_removed
                    && successor.as_ref() == Some(&author)
                    && commit.add_proposals().next().is_none();
                if author != self.groups[scope].controller && !handover {
                    return Err("Encryption: unauthorized membership change.".into());
                }
                // Validate each added device before merging or deriving the next epoch.
                let added: Vec<_> = commit
                    .add_proposals()
                    .map(|proposal| {
                        let leaf = proposal.add_proposal().key_package().leaf_node();
                        (
                            leaf.credential().clone(),
                            leaf.signature_key().as_slice().to_vec(),
                        )
                    })
                    .collect();
                for (credential, signature_key) in &added {
                    self.adopt_first_use(credential, signature_key, true)?;
                    self.approved_member(credential, signature_key)?;
                }
                group
                    .merge_staged_commit(&self.provider, *commit)
                    .map_err(|e| error("merge group key change", e))?;
                if group.is_active() {
                    self.validate_members(&group)?;
                }
                if call_proof.is_some()
                    && group
                        .members()
                        .map(|m| (m.credential, m.signature_key))
                        .collect::<Vec<_>>()
                        != bindings
                {
                    return Err("Encryption: call proof changed verified membership.".into());
                }
                let state = self.groups.get_mut(scope).unwrap();
                state.epoch = group.epoch().as_u64();
                if old_controller_removed {
                    state.controller =
                        successor.ok_or("Encryption: group key manager is missing.")?;
                }
                self.pending_rotations.remove(scope);
                Ok((
                    Received::Membership {
                        epoch: group.epoch().as_u64(),
                    },
                    call_proof.unwrap_or_default(),
                ))
            }
            _ => Err("Encryption: unsupported membership proposal.".into()),
        }
    }
    /// Separate keys for each purpose/sender. Removing a member changes the epoch.
    pub fn media_key(
        &self,
        scope: &str,
        purpose: &str,
        sender: &Identity,
    ) -> Result<Zeroizing<Vec<u8>>> {
        self.guard_rotation_idle(scope)?;
        let group = self.load_group(scope)?;
        if !self.members(scope)?.contains(sender) || !matches!(purpose, "opus" | "rtc") {
            return Err("Encryption: invalid media sender or purpose.".into());
        }
        let context = serde_json::to_vec(&(scope, purpose, sender))
            .map_err(|e| error("prepare media key context", e))?;
        Ok(Zeroizing::new(
            group
                .export_secret(self.provider.crypto(), "Relay/media/v1", &context, 32)
                .map_err(|e| error("derive media key", e))?,
        ))
    }
    /// Secret snapshot; the caller must protect it with the OS vault/DPAPI.
    pub fn checkpoint(&self) -> Result<Zeroizing<Vec<u8>>> {
        let values = self
            .provider
            .storage()
            .values
            .read()
            .map_err(|e| error("read device state", e))?;
        let snapshot = SnapshotView {
            // Older clients must reject a pending snapshot rather than ignore
            // its key-release barrier. Completed snapshots retain version 1.
            version: if self.pending_rotations.is_empty() {
                1
            } else if self
                .pending_rotations
                .values()
                .any(|r| !r.proofs.is_empty())
            {
                3
            } else {
                2
            },
            identity: &self.identity,
            signer: &self.signer,
            values: values.iter().collect(),
            approved: &self.approved,
            groups: &self.groups,
            pending_rotations: &self.pending_rotations,
        };
        let bytes = Zeroizing::new(
            serde_json::to_vec(&snapshot).map_err(|e| error("serialize device state", e))?,
        );
        if bytes.len() > MAX_STATE_BYTES {
            return Err("Encryption device storage is full.".into());
        }
        Ok(bytes)
    }
    pub fn restore(bytes: &[u8], expected: &Identity) -> Result<Self> {
        if bytes.len() > MAX_STATE_BYTES {
            return Err("Encryption: saved device state is too large.".into());
        }
        let state: Snapshot =
            serde_json::from_slice(bytes).map_err(|e| error("read saved device state", e))?;
        if !matches!(state.version, 1..=3)
            || (state.version == 1) != state.pending_rotations.is_empty()
            || (state.version == 3)
                != state
                    .pending_rotations
                    .values()
                    .any(|r| !r.proofs.is_empty())
            || state.identity != *expected
            || state.values.len() > 16384
            || state.approved.len() > MAX_PINS
            || state.groups.len() > MAX_GROUPS
            || state.pending_rotations.len() > MAX_GROUPS
            || state
                .values
                .iter()
                .any(|(k, v)| k.len() > 2048 || v.len() > MAX_STATE_BYTES / 2)
        {
            return Err(
                "Encryption: saved state has an invalid identity or exceeds its limits.".into(),
            );
        }
        expected.validate()?;
        let provider = OpenMlsRustCrypto::default();
        *provider
            .storage()
            .values
            .write()
            .map_err(|e| error("restore device storage", e))? =
            state.values.into_iter().collect::<HashMap<_, _>>();
        let device = Self {
            provider,
            identity: state.identity,
            signer: state.signer,
            approved: state.approved,
            groups: state.groups,
            pending_rotations: state.pending_rotations,
            blocked: false,
            first_use_candidates: BTreeMap::new(),
        };
        let pin = device
            .approved
            .get(&device.identity.device_id)
            .ok_or("Encryption: saved device verification is missing.")?;
        if pin.fingerprint != device.fingerprint()?
            || pin.identity != device.identity
            || pin.signature_key != device.signer.to_public_vec()
        {
            return Err("Encryption: saved device keys do not match.".into());
        }
        for scope in device.pending_rotations.keys() {
            // A locally revoked member must not make the actor impossible to
            // reopen for explicit verification or an authorized removal. The
            // sending/confirmation paths still validate every approved member.
            let group = device.load_group_for_commit(scope)?;
            device.validate_pending_rotation(scope, &group)?;
        }
        // Removing metadata or downgrading the format cannot disable a stored
        // OpenMLS pending commit's barrier.
        for (scope, state) in &device.groups {
            let group = MlsGroup::load(
                device.provider.storage(),
                &GroupId::from_slice(state.group_id.as_bytes()),
            )
            .map_err(|e| error("validate saved group", e))?
            .ok_or("Encryption: saved group state is missing.")?;
            if group.pending_commit().is_some() != device.pending_rotations.contains_key(scope) {
                return Err("Encryption: saved pending key rotation is inconsistent.".into());
            }
        }
        Ok(device)
    }
    /// Run a mutation in isolation. Ciphertext becomes publishable only after
    /// the new ratchet snapshot is durable. A storage failure stops this actor.
    pub fn transact<T>(
        &mut self,
        mutate: impl FnOnce(&mut Self) -> Result<T>,
        persist: impl FnOnce(&[u8]) -> Result<()>,
    ) -> Result<T> {
        self.guard()?;
        let before = self.checkpoint()?;
        let mut next = Self::restore(&before, &self.identity)?;
        next.first_use_candidates = self.first_use_candidates.clone();
        let result = mutate(&mut next)?;
        let after = match next.checkpoint() {
            Ok(snapshot) => snapshot,
            Err(reason) => {
                self.blocked = true;
                return Err(reason);
            }
        };
        if let Err(reason) = persist(&after) {
            self.blocked = true;
            return Err(reason);
        }
        *self = next;
        Ok(result)
    }
}
fn encode(message: &MlsMessageOut) -> Result<Vec<u8>> {
    let bytes = message
        .tls_serialize_detached()
        .map_err(|e| error("encode MLS message", e))?;
    if bytes.len() > MAX_WIRE_BYTES {
        return Err("Encryption: MLS message exceeds its limit.".into());
    }
    Ok(bytes)
}
fn parse(wire: &[u8]) -> Result<MlsMessageIn> {
    if wire.is_empty() || wire.len() > MAX_WIRE_BYTES {
        return Err("Encryption: invalid MLS message size.".into());
    }
    MlsMessageIn::tls_deserialize_exact(wire).map_err(|e| error("decode MLS message", e))
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;

#[cfg(test)]
#[path = "message_cost_tests.rs"]
mod message_cost_tests;

#[cfg(test)]
#[path = "trust_tests.rs"]
mod trust_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn device(name: &str) -> Device {
        Device::new(Identity {
            server_id: "synthetic-community".into(),
            user_id: name.into(),
            device_id: format!("{name}-fixture-device"),
        })
        .unwrap()
    }
    fn durable<T>(device: &mut Device, op: impl FnOnce(&mut Device) -> Result<T>) -> T {
        device.transact(op, |_| Ok(())).unwrap()
    }
    fn verify(device: &mut Device, public: &PublicDevice) {
        durable(device, |d| d.approve(public, &public.fingerprint));
    }
    fn pair() -> (Device, Device, PublicDevice, PublicDevice) {
        let mut alice = device("alice");
        let mut bob = device("bob");
        let a = durable(&mut alice, |d| d.key_package());
        let b = durable(&mut bob, |d| d.key_package());
        verify(&mut alice, &b);
        verify(&mut bob, &a);
        durable(&mut alice, |d| {
            d.create_group("channel:one", "fixture-group-one")
        });
        let change = durable(&mut alice, |d| {
            d.add_members("channel:one", std::slice::from_ref(&b))
        });
        durable(&mut bob, |d| {
            d.join(
                "channel:one",
                "fixture-group-one",
                a.identity.clone(),
                change.welcome.as_ref().unwrap(),
            )
        });
        (alice, bob, a, b)
    }
    #[test]
    fn verified_mls_chat_rejects_replays_tampering_and_wrong_conversations() {
        let (mut alice, mut bob, _, _) = pair();
        let wire = durable(&mut alice, |d| {
            d.encrypt("channel:one", b"a private synthetic reply")
        });
        assert!(!wire.windows(7).any(|w| w == b"private"));
        let mut tampered = wire.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(bob
            .transact(|d| d.receive("channel:one", &tampered), |_| Ok(()))
            .is_err());
        let Received::Application { author, bytes } =
            durable(&mut bob, |d| d.receive("channel:one", &wire))
        else {
            panic!()
        };
        assert_eq!(author.user_id, "alice");
        assert_eq!(bytes, b"a private synthetic reply");
        assert!(bob
            .transact(|d| d.receive("channel:one", &wire), |_| Ok(()))
            .is_err());
        assert!(bob
            .transact(|d| d.receive("channel:two", &wire), |_| Ok(()))
            .is_err());
        assert!(bob
            .transact(
                |d| d.receive("channel:one", &vec![0; MAX_WIRE_BYTES + 1]),
                |_| Ok(())
            )
            .is_err());
    }
    #[test]
    fn unverified_or_substituted_devices_cannot_join_or_receive_keys() {
        let mut alice = device("alice");
        let mut bob = device("bob");
        let public = durable(&mut bob, |d| d.key_package());
        durable(&mut alice, |d| d.create_group("dm:bob", "fixture-dm"));
        assert!(alice
            .transact(
                |d| d.add_members("dm:bob", std::slice::from_ref(&public)),
                |_| Ok(())
            )
            .is_err());
        assert!(alice
            .transact(|d| d.approve(&public, &"0".repeat(64)), |_| Ok(()))
            .is_err());
        verify(&mut alice, &public);
        let mut replacement = device("bob");
        let forged = durable(&mut replacement, |d| d.key_package());
        assert!(alice
            .transact(|d| d.approve(&forged, &forged.fingerprint), |_| Ok(()))
            .is_err());
        assert!(!alice.is_approved(&forged));
        let mut changed = public.clone();
        changed.identity.user_id = "another-user".into();
        assert!(alice
            .transact(|d| d.approve(&changed, &changed.fingerprint), |_| Ok(()))
            .is_err());
    }
    #[test]
    fn removal_rotates_media_keys_and_blocks_future_messages() {
        let (mut alice, mut bob, a, b) = pair();
        let old_key = alice.media_key("channel:one", "opus", &a.identity).unwrap();
        assert_eq!(
            *old_key,
            *bob.media_key("channel:one", "opus", &a.identity).unwrap()
        );
        assert_ne!(
            *old_key,
            *alice.media_key("channel:one", "rtc", &a.identity).unwrap()
        );
        let change = durable(&mut alice, |d| {
            d.remove_members("channel:one", std::slice::from_ref(&b.identity.device_id))
        });
        assert_ne!(
            *old_key,
            *alice.media_key("channel:one", "opus", &a.identity).unwrap()
        );
        let wire = durable(&mut alice, |d| d.encrypt("channel:one", b"after removal"));
        assert!(bob
            .transact(|d| d.receive("channel:one", &wire), |_| Ok(()))
            .is_err());
        durable(&mut bob, |d| d.receive("channel:one", &change.commit));
        assert!(bob.media_key("channel:one", "opus", &a.identity).is_err());
        assert!(bob
            .transact(|d| d.encrypt("channel:one", b"removed sender"), |_| Ok(()))
            .is_err());
    }
    #[test]
    fn persisted_ratchet_survives_restart_and_storage_failure_returns_no_ciphertext() {
        let (mut alice, mut bob, _, _) = pair();
        let first = durable(&mut alice, |d| d.encrypt("channel:one", b"first"));
        durable(&mut bob, |d| d.receive("channel:one", &first));
        let checkpoint = alice.checkpoint().unwrap();
        alice = Device::restore(&checkpoint, alice.identity()).unwrap();
        let second = durable(&mut alice, |d| d.encrypt("channel:one", b"second"));
        assert_ne!(first, second);
        durable(&mut bob, |d| d.receive("channel:one", &second));
        let result = alice.transact(
            |d| d.encrypt("channel:one", b"must never publish"),
            |_| Err("synthetic disk failure".into()),
        );
        assert!(result.is_err());
        assert!(alice.is_blocked());
        assert!(alice
            .media_key("channel:one", "opus", alice.identity())
            .is_err());
        assert!(alice
            .transact(
                |d| d.encrypt("channel:one", b"retry after failure"),
                |_| Ok(())
            )
            .is_err());
        let mut foreign = alice.identity().clone();
        foreign.server_id = "other-community".into();
        assert!(Device::restore(&checkpoint, &foreign).is_err());
    }
    #[test]
    fn non_controller_members_cannot_change_membership() {
        let (_, mut bob, _, _) = pair();
        let mut carol = device("carol");
        let public = durable(&mut carol, |d| d.key_package());
        verify(&mut bob, &public);
        assert!(bob
            .transact(|d| d.add_members("channel:one", &[public]), |_| Ok(()))
            .is_err());
    }
    #[test]
    fn staged_rotation_reopens_with_exact_retry_and_releases_only_confirmed_new_keys() {
        let (mut alice, mut bob, a, _) = pair();
        let old = alice.media_key("channel:one", "opus", &a.identity).unwrap();
        let controller = alice.group_state("channel:one").unwrap().controller.clone();
        let members = alice.members("channel:one").unwrap();
        let before_epoch = alice.group_state("channel:one").unwrap().epoch;
        let rotation = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        assert_eq!(rotation.epoch, before_epoch + 1);
        assert_eq!(
            alice.group_state("channel:one").unwrap().epoch,
            before_epoch
        );
        assert!(rotation.welcome.is_none());
        let checkpoint = alice.checkpoint().unwrap();
        let state: serde_json::Value = serde_json::from_slice(&checkpoint).unwrap();
        assert_eq!(state["version"], 2);
        alice = Device::restore(&checkpoint, &a.identity).unwrap();
        let retry = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        assert_eq!(rotation.commit, retry.commit);
        assert_eq!(rotation.epoch, retry.epoch);
        assert!(alice.media_key("channel:one", "opus", &a.identity).is_err());
        assert!(alice
            .transact(|d| d.encrypt("channel:one", b"wait"), |_| Ok(()))
            .is_err());
        durable(&mut bob, |d| d.receive("channel:one", &retry.commit));
        let epoch = durable(&mut alice, |d| {
            d.confirm_key_rotation("channel:one", &retry.commit)
        });
        assert_eq!(epoch, rotation.epoch);
        assert_eq!(alice.members("channel:one").unwrap(), members);
        assert_eq!(
            alice.group_state("channel:one").unwrap().controller,
            controller
        );
        let new = alice.media_key("channel:one", "opus", &a.identity).unwrap();
        assert_ne!(*old, *new);
        assert_eq!(
            *new,
            *bob.media_key("channel:one", "opus", &a.identity).unwrap()
        );
        let checkpoint = alice.checkpoint().unwrap();
        let state: serde_json::Value = serde_json::from_slice(&checkpoint).unwrap();
        assert_eq!(state["version"], 1);
        assert!(state.get("pending_rotations").is_none());
        alice = Device::restore(&checkpoint, &a.identity).unwrap();
        let wire = durable(&mut alice, |d| d.encrypt("channel:one", b"new epoch"));
        assert!(matches!(
            durable(&mut bob, |d| d.receive("channel:one", &wire)),
            Received::Application { .. }
        ));
    }
    #[test]
    fn staged_rotation_rejects_wrong_confirmation_membership_edits_and_non_controller() {
        let (mut alice, mut bob, _, b) = pair();
        let bob_before = bob.checkpoint().unwrap();
        assert!(bob
            .transact(|d| d.stage_key_rotation("channel:one"), |_| Ok(()))
            .is_err());
        assert_eq!(*bob.checkpoint().unwrap(), *bob_before);
        let rotation = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        let before = alice.checkpoint().unwrap();
        let mut wrong = rotation.commit.clone();
        *wrong.last_mut().unwrap() ^= 1;
        for (scope, commit) in [
            ("channel:one", wrong.as_slice()),
            ("channel:two", rotation.commit.as_slice()),
        ] {
            assert!(alice
                .transact(|d| d.confirm_key_rotation(scope, commit), |_| Ok(()))
                .is_err());
            assert_eq!(*alice.checkpoint().unwrap(), *before);
        }
        assert!(alice
            .transact(|d| d.rotate_group("channel:one"), |_| Ok(()))
            .is_err());
        assert!(alice
            .transact(|d| d.add_members("channel:one", &[]), |_| Ok(()))
            .is_err());
        assert!(alice
            .transact(
                |d| d.remove_members("channel:one", std::slice::from_ref(&b.identity.device_id)),
                |_| Ok(())
            )
            .is_err());
        assert!(alice
            .transact(|d| d.forget_initial_claim("channel:one"), |_| Ok(()))
            .is_err());
        assert_eq!(*alice.checkpoint().unwrap(), *before);
        assert_eq!(
            durable(&mut alice, |d| d.stage_key_rotation("channel:one")).commit,
            rotation.commit
        );
    }
    #[test]
    fn staged_rotation_storage_failures_release_no_result_or_media_key() {
        let (mut alice, _, a, _) = pair();
        let last_durable = alice.checkpoint().unwrap();
        assert!(alice
            .transact(
                |d| d.stage_key_rotation("channel:one"),
                |_| Err("synthetic disk failure".into())
            )
            .is_err());
        assert!(alice.is_blocked());
        assert!(alice.media_key("channel:one", "opus", &a.identity).is_err());
        alice = Device::restore(&last_durable, &a.identity).unwrap();
        let rotation = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        let staged = alice.checkpoint().unwrap();
        assert!(alice
            .transact(
                |d| d.confirm_key_rotation("channel:one", &rotation.commit),
                |_| Err("synthetic disk failure".into())
            )
            .is_err());
        assert!(alice.is_blocked());
        assert!(alice.media_key("channel:one", "opus", &a.identity).is_err());
        alice = Device::restore(&staged, &a.identity).unwrap();
        assert_eq!(
            durable(&mut alice, |d| d.stage_key_rotation("channel:one")).commit,
            rotation.commit
        );
        durable(&mut alice, |d| {
            d.confirm_key_rotation("channel:one", &rotation.commit)
        });
    }
    #[test]
    fn staged_rotation_preserves_incoming_history_and_its_ratchet_across_restart() {
        let (mut alice, mut bob, a, _) = pair();
        let history = durable(&mut bob, |d| {
            d.encrypt("channel:one", b"history during pending rotation")
        });
        let rotation = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        let Received::Application { bytes, .. } =
            durable(&mut alice, |d| d.receive("channel:one", &history))
        else {
            panic!()
        };
        assert_eq!(bytes, b"history during pending rotation");
        alice = Device::restore(&alice.checkpoint().unwrap(), &a.identity).unwrap();
        assert!(alice
            .transact(|d| d.receive("channel:one", &history), |_| Ok(()))
            .is_err());
        assert_eq!(
            durable(&mut alice, |d| d.stage_key_rotation("channel:one")).commit,
            rotation.commit
        );
        durable(&mut alice, |d| {
            d.confirm_key_rotation("channel:one", &rotation.commit)
        });
        durable(&mut bob, |d| d.receive("channel:one", &rotation.commit));
        let wire = durable(&mut bob, |d| {
            d.encrypt("channel:one", b"after confirmation")
        });
        assert!(matches!(
            durable(&mut alice, |d| d.receive("channel:one", &wire)),
            Received::Application { .. }
        ));
    }
    #[test]
    fn authenticated_handover_supersedes_pending_rotation_without_restoring_old_ratchets() {
        let (mut alice, mut bob, a, b) = pair();
        let rotation = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        let removal = durable(&mut bob, |d| {
            d.remove_members("channel:one", std::slice::from_ref(&a.identity.device_id))
        });
        let mut corrupted = removal.commit.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        assert!(alice
            .transact(|d| d.receive("channel:one", &corrupted), |_| Ok(()))
            .is_err());
        assert_eq!(
            durable(&mut alice, |d| d.stage_key_rotation("channel:one")).commit,
            rotation.commit
        );
        durable(&mut alice, |d| d.receive("channel:one", &removal.commit));
        assert!(!alice.is_active("channel:one").unwrap());
        assert_eq!(
            alice.group_state("channel:one").unwrap().controller,
            b.identity
        );
        assert!(alice.pending_rotations.is_empty());
        alice = Device::restore(&alice.checkpoint().unwrap(), &a.identity).unwrap();
        assert!(alice
            .transact(
                |d| d.confirm_key_rotation("channel:one", &rotation.commit),
                |_| Ok(())
            )
            .is_err());
        assert!(alice.media_key("channel:one", "opus", &a.identity).is_err());
        assert!(alice
            .transact(|d| d.receive("channel:one", &removal.commit), |_| Ok(()))
            .is_err());
    }
    #[test]
    fn pending_snapshot_cannot_be_downgraded_or_lose_its_release_barrier() {
        let (mut alice, _, a, b) = pair();
        let rotation = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        let original: serde_json::Value =
            serde_json::from_slice(&alice.checkpoint().unwrap()).unwrap();
        for variant in 0..4 {
            let mut state = original.clone();
            match variant {
                0 => state["version"] = 1.into(),
                1 => {
                    state.as_object_mut().unwrap().remove("pending_rotations");
                }
                2 => {
                    state["version"] = 1.into();
                    state.as_object_mut().unwrap().remove("pending_rotations");
                }
                _ => {
                    state["pending_rotations"]["channel:one"]["epoch"] = 9000.into();
                }
            }
            assert!(Device::restore(&serde_json::to_vec(&state).unwrap(), &a.identity).is_err());
        }
        durable(&mut alice, |d| d.unapprove("bob-fixture-device"));
        alice = Device::restore(&alice.checkpoint().unwrap(), &a.identity).unwrap();
        assert!(alice
            .transact(
                |d| d.confirm_key_rotation("channel:one", &rotation.commit),
                |_| Ok(())
            )
            .is_err());
        // A revoked member cannot turn the staged snapshot into usable call keys.
        assert!(alice.media_key("channel:one", "opus", &a.identity).is_err());
        verify(&mut alice, &b);
        assert_eq!(
            durable(&mut alice, |d| d.stage_key_rotation("channel:one")).commit,
            rotation.commit
        );
        durable(&mut alice, |d| {
            d.confirm_key_rotation("channel:one", &rotation.commit)
        });
    }
    #[test]
    fn authenticated_non_controller_commit_cannot_cancel_pending_rotation() {
        let (mut alice, mut bob, _, _) = pair();
        let rotation = durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        // A compromised verified participant can produce an authentic MLS
        // self-update. It still cannot bypass Relay's controller policy.
        let unauthorized = durable(&mut bob, |d| {
            let mut group = d.load_group("channel:one")?;
            group.set_aad(b"channel:one".to_vec());
            let commit = group
                .self_update(&d.provider, &d.signer, LeafNodeParameters::default())
                .map_err(|e| error("fixture self-update", e))?
                .into_commit();
            group
                .merge_pending_commit(&d.provider)
                .map_err(|e| error("fixture merge", e))?;
            d.groups.get_mut("channel:one").unwrap().epoch = group.epoch().as_u64();
            encode(&commit)
        });
        let before = alice.checkpoint().unwrap();
        let reason = alice
            .transact(|d| d.receive("channel:one", &unauthorized), |_| Ok(()))
            .unwrap_err();
        assert!(reason.contains("unauthorized membership change"));
        assert_eq!(*alice.checkpoint().unwrap(), *before);
        assert_eq!(
            durable(&mut alice, |d| d.stage_key_rotation("channel:one")).commit,
            rotation.commit
        );
        durable(&mut alice, |d| {
            d.confirm_key_rotation("channel:one", &rotation.commit)
        });
    }
    #[test]
    fn a_group_id_cannot_alias_another_conversations_key_barrier() {
        let (mut alice, mut bob, a, _) = pair();
        let before = alice.checkpoint().unwrap();
        assert!(alice
            .transact(
                |d| d.create_group("channel:two", "fixture-group-one"),
                |_| Ok(())
            )
            .is_err());
        assert_eq!(*alice.checkpoint().unwrap(), *before);
        let bob_before = bob.checkpoint().unwrap();
        assert!(bob
            .transact(
                |d| d.join(
                    "channel:two",
                    "fixture-group-one",
                    a.identity.clone(),
                    b"unused welcome"
                ),
                |_| Ok(())
            )
            .is_err());
        assert_eq!(*bob.checkpoint().unwrap(), *bob_before);
        durable(&mut alice, |d| {
            d.create_group("channel:two", "fixture-group-two")
        });
        durable(&mut alice, |d| d.stage_key_rotation("channel:one"));
        assert!(alice.media_key("channel:one", "opus", &a.identity).is_err());
        assert!(alice.media_key("channel:two", "opus", &a.identity).is_ok());
        assert!(alice
            .transact(
                |d| d.encrypt("channel:two", b"independent scope"),
                |_| Ok(())
            )
            .is_ok());
    }

    #[test]
    fn authenticated_call_proof_survives_exact_retry_and_preserves_membership() {
        let (mut alice, mut bob, a, b) = pair();
        let proof = CallFreshnessProof {
            identity: b.identity.clone(),
            nonce: [39; 32],
        };
        let before = alice.members("channel:one").unwrap();
        let change = durable(&mut alice, |d| {
            d.stage_key_rotation_with_proofs("channel:one", std::slice::from_ref(&proof))
        });
        let saved = alice.checkpoint().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&saved).unwrap()["version"],
            3
        );
        alice = Device::restore(&saved, &a.identity).unwrap();
        assert_eq!(
            alice.pending_call_proofs("channel:one").unwrap(),
            vec![proof.clone()]
        );
        let retry = durable(&mut alice, |d| {
            d.stage_key_rotation_with_proofs("channel:one", &[])
        });
        assert_eq!(retry.commit, change.commit);
        let (received, proofs) = durable(&mut bob, |d| {
            d.receive_with_call_proofs("channel:one", &retry.commit)
        });
        assert!(matches!(received, Received::Membership { epoch } if epoch == change.epoch));
        assert_eq!(proofs, vec![proof]);
        durable(&mut alice, |d| {
            d.confirm_key_rotation("channel:one", &retry.commit)
        });
        assert_eq!(alice.members("channel:one").unwrap(), before);
        assert_eq!(bob.members("channel:one").unwrap(), before);
        assert_eq!(
            *alice.media_key("channel:one", "opus", &b.identity).unwrap(),
            *bob.media_key("channel:one", "opus", &b.identity).unwrap()
        );
        assert!(bob
            .transact(
                |d| d.receive_with_call_proofs("channel:one", &retry.commit),
                |_| Ok(())
            )
            .is_err());
    }

    #[test]
    fn call_proof_rejects_unknown_duplicate_and_downgraded_challenges() {
        let (mut alice, _, a, b) = pair();
        let proof = CallFreshnessProof {
            identity: b.identity.clone(),
            nonce: [41; 32],
        };
        let mut unknown = proof.clone();
        unknown.identity.device_id.push_str("-unknown");
        for proofs in [
            vec![unknown],
            vec![proof.clone(), proof.clone()],
            vec![proof.clone(); MAX_MEMBERS + 1],
        ] {
            let before = alice.checkpoint().unwrap();
            assert!(alice
                .transact(
                    |d| d.stage_key_rotation_with_proofs("channel:one", &proofs),
                    |_| Ok(())
                )
                .is_err());
            assert_eq!(*alice.checkpoint().unwrap(), *before);
        }
        durable(&mut alice, |d| {
            d.stage_key_rotation_with_proofs("channel:one", &[proof])
        });
        let saved = alice.checkpoint().unwrap();
        for version in [1, 2] {
            let mut changed: serde_json::Value = serde_json::from_slice(&saved).unwrap();
            changed["version"] = version.into();
            assert!(Device::restore(&serde_json::to_vec(&changed).unwrap(), &a.identity).is_err());
        }
        let mut missing: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        missing["pending_rotations"]["channel:one"]
            .as_object_mut()
            .unwrap()
            .remove("proofs");
        assert!(Device::restore(&serde_json::to_vec(&missing).unwrap(), &a.identity).is_err());
    }

    #[test]
    fn authentic_call_proof_tag_cannot_authorize_a_membership_proposal() {
        let (mut alice, mut bob, a, b) = pair();
        let wire = durable(&mut alice, |d| {
            let mut group = d.load_group("channel:one")?;
            let mut aad = CALL_PROOF_DOMAIN.to_vec();
            aad.extend(
                serde_json::to_vec(&CallProofContext {
                    scope: "channel:one".into(),
                    proofs: vec![CallFreshnessProof {
                        identity: a.identity.clone(),
                        nonce: [43; 32],
                    }],
                })
                .unwrap(),
            );
            group.set_aad(aad);
            let index = group
                .members()
                .find(|m| Identity::from_credential(&m.credential).unwrap() == b.identity)
                .unwrap()
                .index;
            let (commit, _, _) = group
                .remove_members(&d.provider, &d.signer, &[index])
                .map_err(|e| error("fixture proof removal", e))?;
            group
                .merge_pending_commit(&d.provider)
                .map_err(|e| error("fixture merge", e))?;
            d.groups.get_mut("channel:one").unwrap().epoch = group.epoch().as_u64();
            encode(&commit)
        });
        let before = bob.checkpoint().unwrap();
        assert!(bob
            .transact(
                |d| d.receive_with_call_proofs("channel:one", &wire),
                |_| Ok(())
            )
            .unwrap_err()
            .contains("membership or identity"));
        assert_eq!(*bob.checkpoint().unwrap(), *before);
        assert!(bob.is_active("channel:one").unwrap());
    }

    #[test]
    fn authentic_call_proof_from_non_controller_or_wrong_scope_is_rejected() {
        for (wrong_scope, non_controller) in [(true, false), (false, true)] {
            let (mut alice, mut bob, a, _) = pair();
            let sender = if non_controller { &mut bob } else { &mut alice };
            let wire = durable(sender, |d| {
                let mut group = d.load_group("channel:one")?;
                let mut aad = CALL_PROOF_DOMAIN.to_vec();
                aad.extend(
                    serde_json::to_vec(&CallProofContext {
                        scope: if wrong_scope {
                            "channel:other"
                        } else {
                            "channel:one"
                        }
                        .into(),
                        proofs: vec![CallFreshnessProof {
                            identity: a.identity.clone(),
                            nonce: [47; 32],
                        }],
                    })
                    .unwrap(),
                );
                group.set_aad(aad);
                let commit = group
                    .self_update(&d.provider, &d.signer, LeafNodeParameters::default())
                    .map_err(|e| error("fixture proof update", e))?
                    .into_commit();
                group
                    .merge_pending_commit(&d.provider)
                    .map_err(|e| error("fixture merge", e))?;
                d.groups.get_mut("channel:one").unwrap().epoch = group.epoch().as_u64();
                encode(&commit)
            });
            let receiver = if non_controller { &mut alice } else { &mut bob };
            let before = receiver.checkpoint().unwrap();
            assert!(receiver
                .transact(
                    |d| d.receive_with_call_proofs("channel:one", &wire),
                    |_| Ok(())
                )
                .is_err());
            assert_eq!(*receiver.checkpoint().unwrap(), *before);
        }
    }
}
