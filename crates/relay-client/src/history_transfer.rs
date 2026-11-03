//! Explicit, short-lived device-to-device history transfer. No live MLS state.
use crate::{e2ee::CachedEnvelope, network::CommandOrigin};
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use zeroize::Zeroizing;

pub const CAPABILITY: &str = "history-transfer-v1";
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAGIC: &[u8; 8] = b"RLYXFER1";
const MAX_ROWS: usize = 100_000;
const INVALID: &str =
    "This history transfer is invalid or belongs to another account. Nothing was imported.";

pub struct Code(Zeroizing<[u8; 32]>);
impl Code {
    pub fn generate() -> Result<Self, String> {
        let mut key = Zeroizing::new([0; 32]);
        getrandom::getrandom(&mut *key).map_err(|_| "Cannot create a transfer code.")?;
        Ok(Self(key))
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > 128 {
            return Err("Invalid transfer code.".into());
        }
        let compact = Zeroizing::new(
            text.chars()
                .filter(|c| *c != '-' && !c.is_ascii_whitespace())
                .collect::<String>(),
        );
        if compact.len() != 64 || !compact.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Enter the complete transfer code shown on the new PC.".into());
        }
        let mut key = Zeroizing::new([0; 32]);
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&compact[index * 2..index * 2 + 2], 16)
                .map_err(|_| "Invalid transfer code.")?;
        }
        Ok(Self(key))
    }
    pub fn display(&self) -> Zeroizing<String> {
        use std::fmt::Write;
        let mut text = Zeroizing::new(String::new());
        for (i, b) in self.0.iter().enumerate() {
            if i > 0 && i % 4 == 0 {
                text.push('-');
            }
            write!(&mut *text, "{b:02X}").expect("String write");
        }
        text
    }
    pub fn id(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"Relay/history-transfer/id/v1\0");
        hash.update(self.0.as_slice());
        format!("{:x}", hash.finalize())
    }
    fn aad(&self, origin: &CommandOrigin) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&(
            "Relay/history-transfer/v1",
            self.id(),
            &origin.server_id,
            &origin.user_id,
        ))
        .map_err(|_| INVALID.into())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub server_id: String,
    pub user_id: String,
    pub source_device: String,
    pub entries: HashMap<String, CachedEnvelope>,
}
impl Snapshot {
    pub fn validate(&self, origin: &CommandOrigin) -> Result<(), String> {
        let id = |s: &str| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control);
        if self.version != 1
            || self.server_id != origin.server_id
            || self.user_id != origin.user_id
            || !id(&self.server_id)
            || !id(&self.user_id)
            || !id(&self.source_device)
            || self.entries.is_empty()
            || self.entries.len() > MAX_ROWS
        {
            return Err(INVALID.into());
        }
        let mut deliveries = std::collections::HashMap::new();
        for (hash, row) in &self.entries {
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || row.author.server_id != self.server_id
                || !id(&row.author.user_id)
                || !id(&row.author.device_id)
                || row.message_id.as_ref().is_none_or(|v| !id(v))
                || row.sequence == 0
            {
                return Err(INVALID.into());
            }
            crate::e2ee::validate_transferred_envelope(&row.envelope)?;
            let key = (&row.envelope.scope, row.message_id.as_ref().unwrap());
            let identity = (&row.author, &row.envelope.client_id, row.sequence);
            if deliveries
                .insert(key, identity)
                .is_some_and(|old| old != identity)
            {
                return Err(INVALID.into());
            }
        }
        Ok(())
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        for row in self.entries.values_mut() {
            row.envelope.content.zeroize();
        }
    }
}

pub fn seal(snapshot: &Snapshot, code: &Code, origin: &CommandOrigin) -> Result<Vec<u8>, String> {
    snapshot.validate(origin)?;
    let mut plain = Zeroizing::new(serde_json::to_vec(snapshot).map_err(|_| INVALID)?);
    if plain.len() > MAX_BYTES - 48 {
        return Err("This history exceeds the 32 MiB transfer limit. Nothing was sent.".into());
    }
    let mut nonce = [0; 24];
    getrandom::getrandom(&mut nonce).map_err(|_| "Cannot protect history.")?;
    XChaCha20Poly1305::new_from_slice(&*code.0)
        .map_err(|_| INVALID)?
        .encrypt_in_place(XNonce::from_slice(&nonce), &code.aad(origin)?, &mut *plain)
        .map_err(|_| INVALID)?;
    let mut wire = MAGIC.to_vec();
    wire.extend(nonce);
    wire.extend_from_slice(&plain);
    Ok(wire)
}
pub fn open(wire: &[u8], code: &Code, origin: &CommandOrigin) -> Result<Snapshot, String> {
    if !(48..=MAX_BYTES).contains(&wire.len()) || !wire.starts_with(MAGIC) {
        return Err(INVALID.into());
    }
    let mut plain = Zeroizing::new(wire[32..].to_vec());
    XChaCha20Poly1305::new_from_slice(&*code.0).map_err(|_| INVALID)?
        .decrypt_in_place(XNonce::from_slice(&wire[8..32]), &code.aad(origin)?, &mut *plain)
        .map_err(|_| "The code does not match this transfer, or its protected data changed. Nothing was imported.")?;
    let snapshot: Snapshot = serde_json::from_slice(&plain).map_err(|_| INVALID)?;
    snapshot.validate(origin)?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (CommandOrigin, Snapshot) {
        let origin = CommandOrigin {
            url: "https://owned.invalid".into(),
            server_id: "server".into(),
            user_id: "owner".into(),
        };
        let row = CachedEnvelope {
            author: relay_crypto::Identity {
                server_id: "server".into(),
                user_id: "friend".into(),
                device_id: "old".into(),
            },
            envelope: crate::e2ee::Envelope {
                version: 1,
                scope: "channel/owned".into(),
                client_id: "client-1".into(),
                content: "private transferred text".into(),
                reply_to_id: None,
                mentions: vec![],
                attachments: vec![],
                action: None,
            },
            message_id: Some("message-1".into()),
            sequence: 9,
            delivery_revision: 0,
        };
        (
            origin,
            Snapshot {
                version: 1,
                server_id: "server".into(),
                user_id: "owner".into(),
                source_device: "old-owner".into(),
                entries: HashMap::from([("a".repeat(64), row)]),
            },
        )
    }
    #[test]
    fn history_transfer_authenticates_code_owner_community_and_ciphertext() {
        let (origin, snapshot) = fixture();
        let code = Code::generate().unwrap();
        let wire = seal(&snapshot, &code, &origin).unwrap();
        assert!(!wire.windows(7).any(|b| b == b"private"));
        assert_ne!(wire, seal(&snapshot, &code, &origin).unwrap());
        assert_eq!(code.id(), Code::parse(&code.display()).unwrap().id());
        assert_eq!(open(&wire, &code, &origin).unwrap().entries.len(), 1);
        assert!(open(&wire, &Code::generate().unwrap(), &origin).is_err());
        for field in ["server", "owner"] {
            let mut wrong = origin.clone();
            if field == "server" {
                wrong.server_id.push('x');
            } else {
                wrong.user_id.push('x');
            }
            assert!(open(&wire, &code, &wrong).is_err());
        }
        for at in [0, 8, 32, wire.len() - 1] {
            let mut wrong = wire.clone();
            wrong[at] ^= 1;
            assert!(open(&wrong, &code, &origin).is_err());
        }
        for text in ["123456", "", "a-b", "é", &"0".repeat(65)] {
            assert!(Code::parse(text).is_err());
        }
    }
    #[test]
    fn history_transfer_rejects_unbound_records_and_invalid_actions() {
        let (origin, mut snapshot) = fixture();
        let hash = "a".repeat(64);
        snapshot.entries.get_mut(&hash).unwrap().sequence = 0;
        assert!(snapshot.validate(&origin).is_err());
        snapshot.entries.get_mut(&hash).unwrap().sequence = 9;
        snapshot.entries.get_mut(&hash).unwrap().envelope.action =
            Some(crate::e2ee::MessageAction::Reaction {
                message_id: "target".into(),
                target_client_id: None,
                emoji: "x".into(),
                active: true,
            });
        assert!(snapshot.validate(&origin).is_err());
        snapshot.entries.get_mut(&hash).unwrap().envelope.action = None;
        let mut conflict = snapshot.entries[&hash].clone();
        conflict.envelope.client_id = "different".into();
        snapshot.entries.insert("b".repeat(64), conflict);
        assert!(snapshot.validate(&origin).is_err());
    }
}
