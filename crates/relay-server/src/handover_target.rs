//! Local destination key for planned handover. It is never a replica token or
//! a community database key. Possession proves which instance accepted the
//! full-data offer; it does not prove reachability or grant writer authority.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::RngCore;
use relay_protocol::handover::{self as model, HostIdentity, HostProof};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

#[cfg(windows)]
#[path = "data_privacy.rs"]
mod privacy;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct TargetKey {
    signing: SigningKey,
    folder: PathBuf,
}

pub(crate) fn identity(key: &VerifyingKey) -> HostIdentity {
    let mut digest = Sha256::new();
    digest.update(b"relay-host-identity-v1\0");
    digest.update(key.as_bytes());
    let digest = digest.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    HostIdentity {
        host_id: uuid::Uuid::from_bytes(bytes).to_string(),
        public_key: URL_SAFE_NO_PAD.encode(key.as_bytes()),
    }
}

pub(crate) fn verify(proof: &HostProof, message: &[u8]) -> bool {
    if proof.identity.public_key.len() != 43 || proof.signature.len() != 86 {
        return false;
    }
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(&proof.identity.public_key) else {
        return false;
    };
    let Ok(bytes) = <[u8; 32]>::try_from(bytes) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&bytes) else {
        return false;
    };
    if identity(&key) != proof.identity {
        return false;
    }
    let Ok(signature) = URL_SAFE_NO_PAD.decode(&proof.signature) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    key.verify_strict(message, &signature).is_ok()
}

pub(crate) fn private_directory(folder: &Path) -> Result<()> {
    super::handover::direct(&folder)?;
    fs::create_dir_all(&folder)?;
    #[cfg(windows)]
    privacy::protect_data_directory(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn seed(folder: &Path, name: &str, allow_create: bool) -> Result<Zeroizing<[u8; 32]>> {
    let path = folder.join(name);
    super::handover::direct(&path)?;
    let mut seed = Zeroizing::new([0u8; 32]);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let opened = if allow_create {
        options.open(&path)
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
    };
    match opened {
        Ok(mut file) => {
            rand::rngs::OsRng.fill_bytes(&mut *seed);
            file.write_all(&*seed)?;
            file.sync_all()?;
            #[cfg(unix)]
            fs::File::open(&folder)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut file = fs::File::open(&path)?;
            let info = file.metadata()?;
            if !info.is_file() || info.len() != 32 {
                return Err("Invalid destination host key; retained unchanged".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if info.nlink() != 1 || info.mode() & 0o077 != 0 {
                    return Err("Destination host key must be private and have one link".into());
                }
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            file.read_exact(&mut *seed)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(seed)
}

/// Once the journal pins a source key, a missing key is a recovery error, never
/// an excuse to silently sign the next image under a newly generated identity.
pub(crate) fn source_key(data: &Path, expected: Option<&HostIdentity>) -> Result<SigningKey> {
    let folder = data.join("handover-source");
    private_directory(&folder)?;
    let seed = seed(&folder, "signing-key.v1", expected.is_none())?;
    let key = SigningKey::from_bytes(&seed);
    if expected.is_some_and(|p| p != &identity(&key.verifying_key())) {
        return Err("Source host key does not match its authority journal".into());
    }
    Ok(key)
}

impl TargetKey {
    /// Starting an already received host must never replace either private key,
    /// even if its public pin was also removed. New keys are setup work only.
    pub(crate) fn open_bound(data: &Path, expected: &HostIdentity) -> Result<Self> {
        let folder = data.join("handover-target");
        super::handover::direct(&folder)?;
        let signing = SigningKey::from_bytes(&*seed(&folder, "signing-key.v1", false)?);
        if &identity(&signing.verifying_key()) != expected {
            return Err("Destination host key does not match the accepted handover".into());
        }
        let _ = seed(&folder, "encryption-ikm.v1", false)?;
        Ok(Self { signing, folder })
    }

    /// Use a dedicated child folder, protecting only our own key storage. A
    /// malformed/partially written existing key is retained and refused.
    pub fn open(data: &Path) -> Result<Self> {
        let folder = data.join("handover-target");
        private_directory(&folder)?;
        let pinned = folder.join("consents").try_exists()?
            || folder.join("network-endpoint.v1").try_exists()? || data.join("handover-received").try_exists()?;
        let seed = seed(&folder, "signing-key.v1", !pinned)?;
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
            folder,
        })
    }

    pub(crate) fn copy_for_source(&self, data: &Path) -> Result<()> {
        let folder = data.join("handover-source");
        private_directory(&folder)?;
        let path = folder.join("signing-key.v1");
        super::handover::direct(&path)?;
        let bytes = Zeroizing::new(self.signing.to_bytes());
        if path.try_exists()? {
            let old = Zeroizing::new(super::handover_transfer::read_file(&path, 32)?);
            if old.as_slice() != bytes.as_slice() {
                return Err("Unrelated source signing key retained".into());
            }
        } else {
            super::handover_transfer::publish_new(&path, &*bytes)?;
        }
        Ok(())
    }

    pub(crate) fn sign(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(message).to_bytes())
    }

    /// Reuse this explicitly prepared destination's identity, never a database
    /// or replica token. No key is loaded, generated or copied to another host.
    pub(crate) fn takeover_signer(&self) -> crate::takeover_signer::HostKey {
        crate::takeover_signer::HostKey::from_loaded(self.signing.clone())
    }

    pub fn identity(&self) -> HostIdentity {
        identity(&self.signing.verifying_key())
    }

    pub fn network_identity(&self) -> Result<model::TargetNetworkIdentity> {
        let path = self.folder.join("network-endpoint.v1");
        super::handover::direct(&path)?;
        let endpoint_id = if path.try_exists()? {
            let text = String::from_utf8(super::handover_transfer::read_file(&path, 64)?)?;
            if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                return Err("Invalid destination network pin; retained unchanged".into());
            }
            Some(text)
        } else { None };
        Ok(model::TargetNetworkIdentity { target: self.identity(), endpoint_id })
    }

    pub fn plan(
        &self,
        source: model::RoutePin,
        user_id: String,
        target_url: String,
        private_route: Option<model::IrohDestination>,
    ) -> Result<model::DestinationCard> {
        if source.term == 0
            || !super::handover::target_url(&target_url)
            || !super::handover::destination_route(&target_url, private_route.as_ref())
            || !uuid::Uuid::parse_str(&source.server_id)
                .is_ok_and(|v| v.to_string() == source.server_id)
            || !uuid::Uuid::parse_str(&user_id).is_ok_and(|v| v.to_string() == user_id)
        {
            return Err("Invalid destination setup".into());
        }
        let bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&source.host.public_key)?
            .try_into()
            .map_err(|_| "Invalid source host key")?;
        if identity(&VerifyingKey::from_bytes(&bytes)?) != source.host
            || source.host == self.identity()
        {
            return Err("Choose another destination host".into());
        }
        if let Some(route) = private_route.as_ref().filter(|r| r.scope.is_some()) {
            if route.scope.as_deref() != Some(self.identity().host_id.as_str()) {
                return Err("The network identity belongs to another destination".into());
            }
            let existing = self.network_identity()?;
            if let Some(pin) = existing.endpoint_id {
                if pin != route.endpoint_id {
                    return Err("The established destination network key changed; retained unchanged".into());
                }
            } else {
                super::handover_transfer::publish_new(
                    &self.folder.join("network-endpoint.v1"), route.endpoint_id.as_bytes(),
                )?;
            }
        }
        let url = url::Url::parse(&target_url)?
            .as_str()
            .trim_end_matches('/')
            .to_string();
        let body = model::DestinationBody {
            source,
            user_id,
            target: self.identity(),
            target_url: url,
            private_route,
        };
        Ok(model::DestinationCard {
            signature: self.sign(&model::destination_message(&body)),
            body,
        })
    }

    pub(crate) fn encryption_keys(&self) -> Result<hpke_rs::HpkeKeyPair> {
        let pin = self.folder.join("encryption-public.v1");
        super::handover::direct(&pin)?;
        let pinned = pin.try_exists()?;
        let seed = seed(&self.folder, "encryption-ikm.v1", !pinned)?;
        let pair = crate::handover_transfer::hpke().derive_key_pair(&*seed)?;
        if pinned {
            if super::handover_transfer::read_file(&pin, 32)?.as_slice()
                != pair.public_key().as_slice()
            {
                return Err("Destination encryption key differs from its saved identity".into());
            }
        } else {
            super::handover_transfer::publish_new(&pin, pair.public_key().as_slice())?;
        }
        Ok(pair)
    }

    pub fn recipient(&self, status: &model::Status) -> Result<model::Recipient> {
        let p = status.preparation.as_ref().ok_or("No accepted handover")?;
        if !matches!(p.phase, model::Phase::Accepted | model::Phase::Frozen)
            || status.target_identity.as_ref() != Some(&self.identity())
            || p.target_host_id != self.identity().host_id
        {
            return Err("This destination has not accepted the handover".into());
        }
        self.check_consent(status)?;
        let keys = self.encryption_keys()?;
        let public_key = URL_SAFE_NO_PAD.encode(keys.public_key().as_slice());
        let message = model::recipient_message(status, &public_key).ok_or("No handover")?;
        Ok(model::Recipient {
            public_key,
            signature: URL_SAFE_NO_PAD.encode(self.signing.sign(&message).to_bytes()),
        })
    }

    /// Called only after the destination user explicitly accepts full server
    /// data in the app. Every immutable offer field and the source term is signed.
    pub fn accept(&self, status: &model::Status) -> Result<HostProof> {
        let p = status.preparation.as_ref().ok_or("No handover offer")?;
        if p.phase != model::Phase::Offered
            || p.target_host_id != self.identity().host_id
            || status.term == 0
            || !super::handover::target_url(&p.target_url)
            || [
                &status.server_id,
                &status.source_host_id,
                &p.id,
                &p.target_user_id,
                &p.offered_by,
            ]
            .iter()
            .any(|v| uuid::Uuid::parse_str(v).is_err())
        {
            return Err("The offer does not match this destination host".into());
        }
        let message = self.consent_message(status)?;
        let folder = self.folder.join("consents");
        private_directory(&folder)?;
        let path = folder.join(format!("{}.json", p.id));
        let expected = serde_json::to_vec(&URL_SAFE_NO_PAD.encode(Sha256::digest(&message)))?;
        super::handover::direct(&path)?;
        if path.try_exists()? {
            if fs::metadata(&path)?.len() > 128 || fs::read(&path)? != expected {
                return Err("Earlier consent differs; retained unchanged".into());
            }
        } else {
            if fs::read_dir(&folder)?.take(64).count() >= 64 {
                return Err(
                    "Too many retained handover consents; finish or cancel earlier preparations"
                        .into(),
                );
            }
            super::handover_transfer::publish_new(&path, &expected)?;
        }
        Ok(HostProof {
            identity: self.identity(),
            signature: URL_SAFE_NO_PAD.encode(self.signing.sign(&message).to_bytes()),
        })
    }

    fn consent_message(&self, status: &model::Status) -> Result<Vec<u8>> {
        let source = status
            .source_identity
            .as_ref()
            .ok_or("Missing source signing identity")?;
        // Validate the identity's public key encoding and derived host ID using
        // the same verifier as signed proofs, without trusting a caller's ID.
        let bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&source.public_key)?
            .try_into()
            .map_err(|_| "Invalid source key")?;
        if identity(&VerifyingKey::from_bytes(&bytes)?) != *source {
            return Err("Invalid source signing identity".into());
        }
        let p = status.preparation.as_ref().ok_or("No handover offer")?;
        Ok(model::acceptance_message(
            &status.server_id,
            &status.source_host_id,
            source,
            status.term,
            p,
        ))
    }

    pub(crate) fn check_consent(&self, status: &model::Status) -> Result<()> {
        let message = self.consent_message(status)?;
        let p = status.preparation.as_ref().ok_or("No handover offer")?;
        if uuid::Uuid::parse_str(&p.id).is_err() {
            return Err("Invalid handover offer".into());
        }
        let path = self.folder.join("consents").join(format!("{}.json", p.id));
        super::handover::direct(&path)?;
        if fs::metadata(&path)?.len() > 128
            || fs::read(&path)?
                != serde_json::to_vec(&URL_SAFE_NO_PAD.encode(Sha256::digest(&message)))?
        {
            return Err("Handover differs from this destination's saved consent".into());
        }
        Ok(())
    }
}

use model::TargetCommand as Command;

/// Owned stdin/stdout companion: identity, consent, receive, receipt and signed
/// activation. It never starts a server, opens a device or loads a default
/// profile. Never put a seed or proof request on the command line.
pub fn run(data: &Path) -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(model::CONTROL_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > model::CONTROL_BYTES {
        return Err("Destination host request is too large".into());
    }
    let command: Command = serde_json::from_slice(&input)?;
    let key = TargetKey::open(data)?;
    match command {
        Command::Plan {
            source,
            user_id,
            target_url,
            private_route,
        } => println!(
            "{}",
            serde_json::to_string(&key.plan(source, user_id, target_url, private_route)?)?
        ),
        Command::Activate { status, grant } => println!(
            "{}",
            serde_json::to_string(&super::handover::activation::activate(
                data, &status, &grant, &key
            )?)?
        ),
        Command::Receipt { status } => println!(
            "{}",
            serde_json::to_string(&super::handover::activation::attest(data, &status, &key)?)?
        ),
        Command::Recipient { status } => {
            println!("{}", serde_json::to_string(&key.recipient(&status)?)?)
        }
        Command::Receive { status, cache } => println!(
            "{}",
            serde_json::to_string(&crate::handover_transfer::receive(
                data, &cache, &status, &key
            )?)?
        ),
        Command::Identity => println!("{}", serde_json::to_string(&key.identity())?),
        Command::NetworkIdentity => println!("{}", serde_json::to_string(&key.network_identity()?)?),
        Command::Accept { status } => println!("{}", serde_json::to_string(&key.accept(&status)?)?),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bound_restart_does_not_generate_missing_private_keys_even_without_public_pins() {
        let data = tempfile::tempdir().unwrap();
        let key = TargetKey::open(data.path()).unwrap();
        key.encryption_keys().unwrap();
        let identity = key.identity();
        TargetKey::open_bound(data.path(), &identity).unwrap();
        let signing = data.path().join("handover-target/signing-key.v1");
        let signing_bytes = fs::read(&signing).unwrap();
        fs::remove_file(&signing).unwrap();
        assert!(TargetKey::open_bound(data.path(), &identity).is_err());
        assert!(!signing.exists());
        fs::write(&signing, &signing_bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&signing, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let seed = data.path().join("handover-target/encryption-ikm.v1");
        fs::remove_file(data.path().join("handover-target/encryption-public.v1")).unwrap();
        fs::remove_file(&seed).unwrap();
        assert!(TargetKey::open_bound(data.path(), &identity).is_err());
        assert!(!seed.exists());
        assert_eq!(fs::read(signing).unwrap(), signing_bytes);
    }

    #[test]
    fn setup_card_pins_scoped_transport_and_refuses_key_loss() {
        let data=tempfile::tempdir().unwrap();
        let key=TargetKey::open(data.path()).unwrap();
        let identity=key.identity();
        assert!(key.network_identity().unwrap().endpoint_id.is_none());
        let source=model::RoutePin{server_id:uuid::Uuid::new_v4().to_string(),
            host:super::identity(&SigningKey::from_bytes(&[27;32]).verifying_key()),term:2};
        let user=uuid::Uuid::new_v4().to_string();
        let route=model::IrohDestination{endpoint_id:"a".repeat(64),relay:None,
            token:"N".repeat(43),scope:Some(identity.host_id.clone())};
        let address=format!("http://{}.iroh:4580",route.endpoint_id);
        let card=key.plan(source.clone(),user.clone(),address.clone(),Some(route.clone())).unwrap();
        assert_eq!(key.network_identity().unwrap().endpoint_id,Some(route.endpoint_id.clone()));
        assert_eq!(key.network_identity().unwrap().target,identity);
        assert!(crate::handover::destination_matches(&card,&source,&user,&identity.host_id,&address));
        let mut foreign=route.clone();foreign.scope=Some(uuid::Uuid::new_v4().to_string());
        assert!(key.plan(source.clone(),user.clone(),address.clone(),Some(foreign.clone())).is_err());
        let mut forged=card.clone();forged.body.private_route=Some(foreign);
        forged.signature=key.sign(&model::destination_message(&forged.body));
        assert!(!crate::handover::destination_matches(&forged,&source,&user,&identity.host_id,&address));
        let mut replaced=route.clone();replaced.endpoint_id="b".repeat(64);
        assert!(key.plan(source.clone(),user.clone(),format!("http://{}.iroh:4580",replaced.endpoint_id),Some(replaced)).is_err());
        assert_eq!(key.network_identity().unwrap().endpoint_id,Some(route.endpoint_id.clone()));
        let reopened=TargetKey::open(data.path()).unwrap();
        assert_eq!(reopened.network_identity().unwrap().endpoint_id,Some(route.endpoint_id));
        let path=data.path().join("handover-target/signing-key.v1");
        fs::remove_file(&path).unwrap();
        assert!(TargetKey::open(data.path()).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn setup_card_is_signed_without_accepting_full_data_or_activating() {
        let data = tempfile::tempdir().unwrap();
        let key = TargetKey::open(data.path()).unwrap();
        let source_key = SigningKey::from_bytes(&[24; 32]);
        let source = model::RoutePin {
            server_id: uuid::Uuid::new_v4().to_string(),
            host: identity(&source_key.verifying_key()),
            term: 2,
        };
        let card = key
            .plan(
                source.clone(),
                uuid::Uuid::new_v4().to_string(),
                "https://new.example/".into(),
                None,
            )
            .unwrap();
        assert_eq!(card.body.source, source);
        assert_eq!(card.body.target, key.identity());
        assert!(verify(
            &HostProof {
                identity: key.identity(),
                signature: card.signature.clone()
            },
            &model::destination_message(&card.body)
        ));
        let mut changed = card.body;
        changed.user_id = uuid::Uuid::new_v4().to_string();
        assert!(!verify(
            &HostProof {
                identity: key.identity(),
                signature: card.signature
            },
            &model::destination_message(&changed)
        ));
        assert!(!data.path().join("handover-target/consents").exists());
        assert!(!data.path().join("relay.sqlite3").exists());
        assert!(key
            .plan(
                source.clone(),
                uuid::Uuid::new_v4().to_string(),
                "http://new.example/".into(), None
            )
            .is_err());
        assert!(key
            .plan(
                model::RoutePin {
                    host: key.identity(),
                    ..source
                },
                uuid::Uuid::new_v4().to_string(),
                "https://new.example".into(), None
            )
            .is_err());
    }

    #[test]
    fn destination_keeps_its_key_and_refuses_corrupt_key_without_replacement() {
        let data = tempfile::tempdir().unwrap();
        let key = TargetKey::open(data.path()).unwrap();
        let original = key.identity();
        assert_eq!(TargetKey::open(data.path()).unwrap().identity(), original);
        let other = tempfile::tempdir().unwrap();
        assert_ne!(TargetKey::open(other.path()).unwrap().identity(), original);
        let encryption = key.encryption_keys().unwrap();
        let encrypted_seed = data.path().join("handover-target/encryption-ikm.v1");
        let original_seed = fs::read(&encrypted_seed).unwrap();
        fs::remove_file(&encrypted_seed).unwrap();
        assert!(key.encryption_keys().is_err());
        assert!(!encrypted_seed.exists());
        fs::write(&encrypted_seed, original_seed).unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&encrypted_seed, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            key.encryption_keys().unwrap().public_key(),
            encryption.public_key()
        );
        let path = data.path().join("handover-target/signing-key.v1");
        fs::write(&path, b"incomplete").unwrap();
        assert!(TargetKey::open(data.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"incomplete");
    }

    #[test]
    fn private_destination_key_signs_only_an_offer_for_this_instance() {
        let data = tempfile::tempdir().unwrap();
        let key = TargetKey::open(data.path()).unwrap();
        let id = || uuid::Uuid::new_v4().to_string();
        let mut status = model::Status {
            server_id: id(),
            source_host_id: id(),
            term: 1,
            writes_allowed: true,
            activation_available: false,
            history: Vec::new(),
            destination: None,
            target_identity: None,
            receipt_challenge: None,
            received: None,
            retired: None,
            source_identity: Some(identity(&SigningKey::from_bytes(&[19; 32]).verifying_key())),
            preparation: Some(model::Preparation {
                id: id(),
                target_user_id: id(),
                target_host_id: key.identity().host_id,
                target_url: "https://new.example/".into(),
                offered_by: id(),
                phase: model::Phase::Offered,
                updated_ms: 0,
            }),
        };
        let proof = key.accept(&status).unwrap();
        let p = status.preparation.as_ref().unwrap();
        let message = model::acceptance_message(
            &status.server_id,
            &status.source_host_id,
            status.source_identity.as_ref().unwrap(),
            status.term,
            p,
        );
        assert!(verify(&proof, &message));
        let mut forged = proof.clone();
        forged.identity.host_id = id();
        assert!(!verify(&forged, &message));
        let mut forged = proof;
        forged.signature.replace_range(..1, "!");
        assert!(!verify(&forged, &message));
        status.preparation.as_mut().unwrap().target_host_id = id();
        assert!(key.accept(&status).is_err());
    }
}

pub fn active_runtime(data: &Path) -> Result<Option<model::HostRuntime>> {
    super::handover::activation::active_runtime(data)
}
