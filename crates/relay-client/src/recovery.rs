//! Portable, read-only history. No API imports these rows into live MLS state.
use crate::{e2ee::CachedEnvelope, network::CommandOrigin};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use relay_crypto::Identity;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

const MAGIC: &[u8; 8] = b"RLYHIST1";
const HEADER: usize = 8 + 16 + 24;
const MAX_PLAIN: usize = 64 * 1024 * 1024;
const MAX_ROWS: usize = 100_000;
const INVALID: &str = "This history backup is invalid or exceeds its limits. Nothing was restored.";

pub enum BackupResult {
    Saved(usize),
    Opened(HistoryArchive),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryArchive {
    version: u8,
    pub source: Identity,
    pub fingerprint: String,
    pub created_at: String,
    pub entries: Vec<CachedEnvelope>,
    #[serde(skip)]
    estimated_bytes: usize,
}

impl Drop for HistoryArchive {
    fn drop(&mut self) {
        for row in &mut self.entries {
            row.envelope.content.zeroize();
            if let Some(reply) = &mut row.envelope.reply_to_id {
                reply.zeroize();
            }
            for mention in &mut row.envelope.mentions {
                mention.zeroize();
            }
            for file in &mut row.envelope.attachments {
                file.name.zeroize();
                file.mime.zeroize();
                // FileKey wipes its private material on drop.
            }
        }
    }
}

impl HistoryArchive {
    pub fn new(source: Identity, fingerprint: String) -> Self {
        Self {
            version: 1,
            source,
            fingerprint,
            created_at: chrono::Utc::now().to_rfc3339(),
            entries: Vec::new(),
            estimated_bytes: 2048,
        }
    }
    pub fn push(&mut self, entry: CachedEnvelope) -> Result<(), String> {
        let plain = Zeroizing::new(serde_json::to_vec(&entry).map_err(|_| INVALID)?);
        self.estimated_bytes = self
            .estimated_bytes
            .checked_add(plain.len() + 1)
            .ok_or(INVALID)?;
        if self.entries.len() >= MAX_ROWS || self.estimated_bytes > MAX_PLAIN {
            return Err("This history exceeds the 64 MiB / 100,000 record backup limit. No partial backup was created.".into());
        }
        self.entries.push(entry);
        Ok(())
    }
    pub fn validate(&self, expected: Option<&CommandOrigin>) -> Result<(), String> {
        let id = |s: &str| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control);
        if self.version != 1
            || !id(&self.source.server_id)
            || !id(&self.source.user_id)
            || !id(&self.source.device_id)
            || self.fingerprint.len() != 64
            || !self.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            || self.created_at.len() > 64
            || chrono::DateTime::parse_from_rfc3339(&self.created_at).is_err()
            || self.entries.len() > MAX_ROWS
        {
            return Err(INVALID.into());
        }
        if expected.is_some_and(|origin| {
            origin.server_id != self.source.server_id || origin.user_id != self.source.user_id
        }) {
            return Err("This backup belongs to another community or account. Sign out to view it separately; it cannot restore this account.".into());
        }
        for entry in &self.entries {
            let e = &entry.envelope;
            if entry.author.server_id != self.source.server_id
                || !id(&entry.author.user_id)
                || !id(&entry.author.device_id)
                || e.version != 1
                || e.scope.is_empty()
                || e.scope.len() > 512
                || !id(&e.client_id)
                || entry.message_id.as_ref().is_none_or(|s| !id(s))
                || e.content.len() > 10000
                || e.mentions.len() > 64
                || e.mentions.iter().any(|s| !id(s))
                || e.reply_to_id.as_ref().is_some_and(|s| !id(s))
                || e.attachments.len() > 8
                || e.attachments.iter().any(|a| {
                    !id(&a.id)
                        || a.name.len() > 512
                        || a.mime.len() > 128
                        || a.file.binding.len() > 512
                        || a.file.size > relay_crypto::files::MAX_FILE_BYTES as u64
                })
            {
                return Err(INVALID.into());
            }
        }
        Ok(())
    }
}

pub fn password_valid(password: &str) -> bool {
    (12..=1024).contains(&password.chars().count()) && password.len() <= 4096
}

fn key(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>, String> {
    if !password_valid(password) {
        return Err("Use a backup passphrase with 12 to 1,024 characters.".into());
    }
    // RFC 9106 second recommended profile. Fixed versioned parameters prevent
    // attacker-controlled KDF work; explicitly wipe all memory blocks.
    let params = Params::new(65536, 3, 4, Some(32)).map_err(|_| INVALID)?;
    let mut blocks = Zeroizing::new(vec![Block::default(); params.block_count()]);
    let mut key = Zeroizing::new([0; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password.as_bytes(), salt, &mut *key, &mut *blocks)
        .map_err(|_| "Cannot protect this history backup.")?;
    Ok(key)
}

struct LimitedPlain(Zeroizing<Vec<u8>>);
impl Write for LimitedPlain {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_PLAIN {
            return Err(std::io::Error::other("History backup limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn seal(archive: &HistoryArchive, password: &str) -> Result<Vec<u8>, String> {
    archive.validate(None)?;
    let mut header = [0; HEADER];
    header[..8].copy_from_slice(MAGIC);
    getrandom::getrandom(&mut header[8..]).map_err(|_| "Cannot generate backup protection.")?;
    let key = key(password, &header[8..24])?;
    let mut plain = LimitedPlain(Zeroizing::new(Vec::new()));
    serde_json::to_writer(&mut plain, archive).map_err(|_| INVALID)?;
    XChaCha20Poly1305::new_from_slice(&*key)
        .map_err(|_| INVALID)?
        .encrypt_in_place(XNonce::from_slice(&header[24..]), &header, &mut *plain.0)
        .map_err(|_| "Cannot protect this history backup.")?;
    let mut wire = header.to_vec();
    wire.extend_from_slice(&plain.0);
    Ok(wire)
}

fn unlock(
    wire: &[u8],
    password: &str,
    expected: Option<&CommandOrigin>,
) -> Result<HistoryArchive, String> {
    if wire.len() < HEADER + 16 || wire.len() > MAX_PLAIN + HEADER + 16 || !wire.starts_with(MAGIC)
    {
        return Err(INVALID.into());
    }
    let key = key(password, &wire[8..24])?;
    let mut plain = Zeroizing::new(wire[HEADER..].to_vec());
    XChaCha20Poly1305::new_from_slice(&*key)
        .map_err(|_| INVALID)?
        .decrypt_in_place(
            XNonce::from_slice(&wire[24..HEADER]),
            &wire[..HEADER],
            &mut *plain,
        )
        .map_err(|_| {
            "The backup passphrase is wrong, or the file has changed. Nothing was restored."
        })?;
    let mut archive: HistoryArchive = serde_json::from_slice(&plain).map_err(|_| INVALID)?;
    archive.validate(expected)?;
    archive.entries.sort_by(|a, b| {
        (&a.envelope.scope, a.sequence, a.delivery_revision).cmp(&(
            &b.envelope.scope,
            b.sequence,
            b.delivery_revision,
        ))
    });
    Ok(archive)
}

pub fn save(path: &Path, archive: &HistoryArchive, password: &str) -> Result<usize, String> {
    let wire = seal(archive, password)?;
    // Never replace an existing file, including a device file or previous backup.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "Choose a new backup filename. Existing files are preserved.")?;
    if file.write_all(&wire).and_then(|_| file.sync_all()).is_err() {
        drop(file);
        let _ = std::fs::remove_file(path); // Only the file just created by this operation.
        return Err("The history backup could not be saved. Try another location.".into());
    }
    Ok(archive.entries.len())
}

pub fn open(
    path: &Path,
    password: &str,
    expected: Option<&CommandOrigin>,
) -> Result<HistoryArchive, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1).custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options
        .open(path)
        .map_err(|_| "Cannot open the history backup.")?;
    let meta = file.metadata().map_err(|_| INVALID)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(INVALID.into());
        }
    }
    if !meta.is_file() || meta.len() > (MAX_PLAIN + HEADER + 16) as u64 {
        return Err(INVALID.into());
    }
    let mut wire = Vec::new();
    file.take((MAX_PLAIN + HEADER + 17) as u64)
        .read_to_end(&mut wire)
        .map_err(|_| INVALID)?;
    unlock(&wire, password, expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> HistoryArchive {
        let identity = Identity {
            server_id: "community".into(),
            user_id: "alice".into(),
            device_id: "retired".into(),
        };
        let mut archive = HistoryArchive::new(identity.clone(), "a".repeat(64));
        archive
            .push(CachedEnvelope {
                author: identity,
                envelope: crate::e2ee::Envelope {
                    version: 1,
                    scope: "channel/one".into(),
                    client_id: "send-one".into(),
                    content: "synthetic secret history".into(),
                    reply_to_id: None,
                    mentions: vec![],
                    attachments: vec![],
                    action: None,
                },
                message_id: Some("message-one".into()),
                sequence: 1,
                delivery_revision: 1,
            })
            .unwrap();
        archive
    }
    const PASSWORD: &str = "synthetic long passphrase";
    #[test]
    fn portable_history_authenticates_password_header_content_and_account() {
        let archive = fixture();
        let wire = seal(&archive, PASSWORD).unwrap();
        let other = seal(&archive, PASSWORD).unwrap();
        assert_ne!(wire, other);
        assert!(!wire.windows(9).any(|b| b == b"synthetic"));
        let opened = unlock(&wire, PASSWORD, None).unwrap();
        assert_eq!(
            opened.entries[0].envelope.content,
            "synthetic secret history"
        );
        assert!(unlock(&wire, "wrong long passphrase", None).is_err());
        for at in [8, 24, HEADER, wire.len() - 1] {
            let mut changed = wire.clone();
            changed[at] ^= 1;
            assert!(unlock(&changed, PASSWORD, None).is_err());
        }
        assert!(unlock(&wire[..wire.len() - 1], PASSWORD, None).is_err());
        let mut origin = CommandOrigin {
            url: "unused".into(),
            server_id: "community".into(),
            user_id: "alice".into(),
        };
        unlock(&wire, PASSWORD, Some(&origin)).unwrap();
        origin.user_id = "bob".into();
        assert!(unlock(&wire, PASSWORD, Some(&origin)).is_err());
        origin.user_id = "alice".into();
        origin.server_id = "other".into();
        assert!(unlock(&wire, PASSWORD, Some(&origin)).is_err());
        let plain = Zeroizing::new(serde_json::to_vec(&archive).unwrap());
        let json: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["created_at", "entries", "fingerprint", "source", "version"]
        );
        // No device checkpoint, approvals, ratchets, outbox or live media keys.
        assert_eq!(json["source"]["device_id"], "retired");
    }
    #[test]
    fn existing_files_invalid_archives_and_short_passwords_are_preserved() {
        let root =
            std::env::temp_dir().join(format!("relay-history-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("history.relay-history");
        let archive = fixture();
        save(&path, &archive, PASSWORD).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(save(&path, &archive, PASSWORD).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(open(&path, PASSWORD, None).unwrap().entries.len(), 1);
        assert!(seal(&archive, "short").is_err());
        let mut invalid = fixture();
        invalid.entries[0].author.server_id = "other".into();
        assert!(seal(&invalid, PASSWORD).is_err());
        let mut invalid = fixture();
        invalid.version = 2;
        assert!(seal(&invalid, PASSWORD).is_err());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn oversized_file_and_serialization_are_rejected_without_partial_restore() {
        let path =
            std::env::temp_dir().join(format!("relay-history-quota-{}", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len((MAX_PLAIN + HEADER + 17) as u64).unwrap();
        drop(file);
        assert!(open(&path, PASSWORD, None).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (MAX_PLAIN + HEADER + 17) as u64
        );
        std::fs::remove_file(path).unwrap();
        let mut output = LimitedPlain(Zeroizing::new(vec![0; MAX_PLAIN]));
        assert!(output.write_all(b"x").is_err());
        assert_eq!(output.0.len(), MAX_PLAIN);
        let mut archive = fixture();
        archive.estimated_bytes = MAX_PLAIN;
        assert!(archive.push(archive.entries[0].clone()).is_err());
        assert_eq!(archive.entries.len(), 1);
    }
}
