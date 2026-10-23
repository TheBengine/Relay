//! R3.2: invitation-based device verification, the pure part.
//!
//! The host's invitation link carries a one-time secret in its fragment (it
//! never reaches the server), the host's device ID and fingerprint, and the
//! server's invitation ID. A friend who joins with that link proves, with an
//! HMAC over their own device, that they hold the secret. The server stores
//! the proof but cannot make or check it; only the inviting host's client can.
//!
//! The trust is exactly that of sending a security code over the channel that
//! carries the link. See `docs/encryption.md`, "Invitation-based verification".
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

pub const SECRET_BYTES: usize = 32;
const DOMAIN: &[u8] = b"relay-invite-attest-v1\0";

fn lowercase_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn device_id_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'.'))
}

/// What a link carries to let the joining device and the host recognise each
/// other. Never printed (the secret is redacted from `Debug`).
#[derive(Clone, PartialEq, Eq)]
pub struct InviteProof {
    /// The server's identifier of this invitation (the server's `id`).
    pub invite_id: String,
    /// The host's device and fingerprint, pinned by the friend on joining.
    pub host_device_id: String,
    pub host_fingerprint: String,
    secret: Zeroizing<[u8; SECRET_BYTES]>,
}

impl std::fmt::Debug for InviteProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InviteProof")
            .field("invite_id", &self.invite_id)
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl InviteProof {
    pub fn new(
        invite_id: &str,
        host_device_id: &str,
        host_fingerprint: &str,
        secret: [u8; SECRET_BYTES],
    ) -> Result<Self, String> {
        if !lowercase_hex_64(invite_id)
            || !device_id_ok(host_device_id)
            || !lowercase_hex_64(host_fingerprint)
        {
            return Err("This invitation has invalid device verification details.".into());
        }
        Ok(Self {
            invite_id: invite_id.into(),
            host_device_id: host_device_id.into(),
            host_fingerprint: host_fingerprint.into(),
            secret: Zeroizing::new(secret),
        })
    }

    /// From the four link fields; the secret is unpadded URL-safe base64.
    pub fn from_link_fields(
        invite_id: &str,
        host_device_id: &str,
        host_fingerprint: &str,
        secret: &str,
    ) -> Result<Self, String> {
        let bytes =
            decode_secret(secret).ok_or("This invitation has an invalid verification secret.")?;
        Self::new(invite_id, host_device_id, host_fingerprint, *bytes)
    }

    pub fn secret_for_link(&self) -> Zeroizing<String> {
        Zeroizing::new(URL_SAFE_NO_PAD.encode(*self.secret))
    }

    pub fn secret(&self) -> &[u8; SECRET_BYTES] {
        &self.secret
    }
}

/// The secret text (unpadded URL-safe base64) back to its 32 bytes.
pub fn decode_secret(text: &str) -> Option<Zeroizing<[u8; SECRET_BYTES]>> {
    let bytes = Zeroizing::new(URL_SAFE_NO_PAD.decode(text).ok()?);
    <[u8; SECRET_BYTES]>::try_from(bytes.as_slice())
        .ok()
        .map(Zeroizing::new)
}

fn mac(
    secret: &[u8; SECRET_BYTES],
    server_id: &str,
    invite_id: &str,
    user_id: &str,
    device_id: &str,
    fingerprint: &str,
) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts any key size");
    mac.update(DOMAIN);
    for (index, part) in [server_id, invite_id, user_id, device_id, fingerprint]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            mac.update(&[0]);
        }
        mac.update(part.as_bytes());
    }
    mac
}

/// The friend's proof for their own device, as 64 lowercase hexadecimal characters.
pub fn attest(
    secret: &[u8; SECRET_BYTES],
    server_id: &str,
    invite_id: &str,
    user_id: &str,
    device_id: &str,
    fingerprint: &str,
) -> String {
    mac(
        secret,
        server_id,
        invite_id,
        user_id,
        device_id,
        fingerprint,
    )
    .finalize()
    .into_bytes()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// Checks a proof in constant time. Any malformed proof is simply wrong.
pub fn verify(
    secret: &[u8; SECRET_BYTES],
    server_id: &str,
    invite_id: &str,
    user_id: &str,
    device_id: &str,
    fingerprint: &str,
    proof: &str,
) -> bool {
    if !lowercase_hex_64(proof) {
        return false;
    }
    let bytes: Vec<u8> = (0..32)
        .map(|index| u8::from_str_radix(&proof[index * 2..index * 2 + 2], 16).unwrap_or(0))
        .collect();
    mac(
        secret,
        server_id,
        invite_id,
        user_id,
        device_id,
        fingerprint,
    )
    .verify_slice(&bytes)
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";

    fn secret() -> [u8; SECRET_BYTES] {
        std::array::from_fn(|index| index as u8)
    }
    fn iid() -> String {
        "ab".repeat(32)
    }
    fn fingerprint() -> String {
        "cd".repeat(32)
    }

    #[test]
    fn matches_an_independent_reference_implementation() {
        // Computed separately with Python's hmac/hashlib for the same message:
        // "relay-invite-attest-v1\0" + server_id + "\0" + iid + "\0" + user_id
        // + "\0" + device_id + "\0" + fingerprint.
        assert_eq!(
            attest(
                &secret(),
                SERVER,
                &iid(),
                "friend-user",
                "friend-device",
                &fingerprint()
            ),
            "96318ca5b390082862398535072ced019e79f6bf1be2282a91c802985baebbf9"
        );
        let proof = InviteProof::new(&iid(), "host-device", &fingerprint(), secret()).unwrap();
        assert_eq!(
            proof.secret_for_link().as_str(),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
        );
    }

    #[test]
    fn only_the_exact_secret_server_invitation_account_device_and_fingerprint_verify() {
        let good = attest(&secret(), SERVER, &iid(), "u", "d", &fingerprint());
        assert!(verify(
            &secret(),
            SERVER,
            &iid(),
            "u",
            "d",
            &fingerprint(),
            &good
        ));
        let mut other_secret = secret();
        other_secret[0] ^= 1;
        let other_server = "5be44aef-8c98-4db6-a9d5-81c2eb311330";
        let other_invite = "ac".repeat(32);
        let other_fingerprint = "ce".repeat(32);
        for (name, ok) in [
            (
                "secret",
                verify(
                    &other_secret,
                    SERVER,
                    &iid(),
                    "u",
                    "d",
                    &fingerprint(),
                    &good,
                ),
            ),
            (
                "server",
                verify(
                    &secret(),
                    other_server,
                    &iid(),
                    "u",
                    "d",
                    &fingerprint(),
                    &good,
                ),
            ),
            (
                "invitation",
                verify(
                    &secret(),
                    SERVER,
                    &other_invite,
                    "u",
                    "d",
                    &fingerprint(),
                    &good,
                ),
            ),
            (
                "account",
                verify(&secret(), SERVER, &iid(), "v", "d", &fingerprint(), &good),
            ),
            (
                "device",
                verify(&secret(), SERVER, &iid(), "u", "e", &fingerprint(), &good),
            ),
            (
                "fingerprint",
                verify(
                    &secret(),
                    SERVER,
                    &iid(),
                    "u",
                    "d",
                    &other_fingerprint,
                    &good,
                ),
            ),
        ] {
            assert!(!ok, "a proof for another {name} must not verify");
        }
        // Moving a character between two fields cannot forge a proof.
        let shifted = attest(&secret(), SERVER, &iid(), "ud", "", &fingerprint());
        assert!(!verify(
            &secret(),
            SERVER,
            &iid(),
            "u",
            "d",
            &fingerprint(),
            &shifted
        ));
    }

    #[test]
    fn malformed_proofs_never_verify() {
        let good = attest(&secret(), SERVER, &iid(), "u", "d", &fingerprint());
        let mut flipped = good.clone();
        flipped.replace_range(..1, if good.starts_with('0') { "1" } else { "0" });
        for bad in [
            "",
            &good[..62],
            &format!("{good}00"),
            &good.to_uppercase(),
            &"z".repeat(64),
            &flipped,
        ] {
            assert!(
                !verify(&secret(), SERVER, &iid(), "u", "d", &fingerprint(), bad),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn link_fields_are_validated_and_the_secret_is_never_printed() {
        let secret_text = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        let proof =
            InviteProof::from_link_fields(&iid(), "host-device:1", &fingerprint(), secret_text)
                .unwrap();
        assert_eq!(proof.secret(), &secret());
        let printed = format!("{proof:?}");
        assert!(
            !printed.contains(secret_text) && printed.contains("redacted"),
            "{printed}"
        );
        for (invite, device, print, secret_value) in [
            ("short", "host-device", &fingerprint(), secret_text),
            (
                &iid().to_uppercase(),
                "host-device",
                &fingerprint(),
                secret_text,
            ),
            (&iid(), "", &fingerprint(), secret_text),
            (&iid(), "has space", &fingerprint(), secret_text),
            (&iid(), &"d".repeat(129), &fingerprint(), secret_text),
            (&iid(), "host-device", &"cd".repeat(31), secret_text),
            (&iid(), "host-device", &"CD".repeat(32), secret_text),
            (&iid(), "host-device", &fingerprint(), ""),
            (&iid(), "host-device", &fingerprint(), "AAECAwQ"),
            (
                &iid(),
                "host-device",
                &fingerprint(),
                &format!("{secret_text}AA"),
            ),
            (
                &iid(),
                "host-device",
                &fingerprint(),
                "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!",
            ),
        ] {
            assert!(
                InviteProof::from_link_fields(invite, device, print, secret_value).is_err(),
                "{invite} {device} {secret_value}"
            );
        }
    }
}
