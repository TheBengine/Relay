//! Bounded whole-file AEAD. Names, MIME types and file keys travel inside MLS.
use crate::Result;
use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

pub const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;
pub const FILE_OVERHEAD: usize = 24;
const MAGIC: &[u8; 8] = b"RLYFILE1";

/// True for bytes in Relay's encrypted-file format. A file served without a key
/// is only trusted as ordinary content if it is not one of these.
pub fn looks_encrypted(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

#[derive(Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct FileKey {
    key: [u8; 32],
    nonce: [u8; 24],
    pub size: u64,
    pub ciphertext_sha256: String,
    pub binding: String,
}
impl std::fmt::Debug for FileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKey([redacted])")
    }
}
pub fn encrypt(bytes: &[u8], binding: &str) -> Result<(Vec<u8>, FileKey)> {
    if bytes.len() > MAX_FILE_BYTES || binding.is_empty() || binding.len() > 512 {
        return Err("The encrypted file is too large or has invalid context.".into());
    }
    let mut key = FileKey {
        key: [0; 32],
        nonce: [0; 24],
        size: bytes.len() as u64,
        ciphertext_sha256: String::new(),
        binding: binding.into(),
    };
    OsRng.fill_bytes(&mut key.key);
    OsRng.fill_bytes(&mut key.nonce);
    let ciphertext = XChaCha20Poly1305::new_from_slice(&key.key)
        .unwrap()
        .encrypt(
            XNonce::from_slice(&key.nonce),
            Payload {
                msg: bytes,
                aad: binding.as_bytes(),
            },
        )
        .map_err(|_| "Cannot encrypt this file.")?;
    let mut wire = MAGIC.to_vec();
    wire.extend_from_slice(&ciphertext);
    key.ciphertext_sha256 = format!("{:x}", Sha256::digest(&wire));
    Ok((wire, key))
}
pub fn decrypt(wire: &[u8], key: &FileKey, expected_binding: &str) -> Result<Zeroizing<Vec<u8>>> {
    if key.binding != expected_binding
        || key.size > MAX_FILE_BYTES as u64
        || wire.len() != key.size as usize + MAGIC.len() + 16
        || !wire.starts_with(MAGIC)
        || format!("{:x}", Sha256::digest(wire)) != key.ciphertext_sha256
    {
        return Err("The encrypted file is incomplete or belongs to another message.".into());
    }
    XChaCha20Poly1305::new_from_slice(&key.key)
        .unwrap()
        .decrypt(
            XNonce::from_slice(&key.nonce),
            Payload {
                msg: &wire[MAGIC.len()..],
                aad: expected_binding.as_bytes(),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| "The encrypted file could not be authenticated.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_blobs_are_recognised_and_ordinary_files_are_not() {
        let (wire, _) = encrypt(b"synthetic contents", "room:one/upload:one").unwrap();
        assert!(looks_encrypted(&wire));
        for plain in [&b""[..], b"RLYFILE", b"holiday plan", b"PNG-like bytes"] {
            assert!(!looks_encrypted(plain));
        }
    }

    #[test]
    fn file_bytes_context_and_integrity_are_authenticated() {
        let (wire, key) = encrypt(b"synthetic GIF contents", "room:one/upload:one").unwrap();
        assert!(!wire.windows(8).any(|w| w == b"syntheti"));
        assert_eq!(
            &**decrypt(&wire, &key, &key.binding).unwrap(),
            b"synthetic GIF contents"
        );
        assert!(decrypt(&wire, &key, "room:two/upload:one").is_err());
        let mut changed = wire.clone();
        changed[10] ^= 1;
        assert!(decrypt(&changed, &key, &key.binding).is_err());
        assert!(decrypt(&wire[..wire.len() - 1], &key, &key.binding).is_err());
        let (_, other) = encrypt(b"synthetic GIF contents", &key.binding).unwrap();
        assert!(decrypt(&wire, &other, &key.binding).is_err());
        assert_eq!(format!("{key:?}"), "FileKey([redacted])");
    }
}
