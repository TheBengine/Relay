//! A short-lived code hands a LAN-found client a secure invitation, not an account.
//! Only the random 128-bit code shared by the host authenticates this exchange.
//! Discovery is never an identity source. Codes and invitation plaintext never appear
//! on the public HTTP request; normal HTTPS/iroh host proof still precedes login.
use crate::{handover::RoutePin, host_locator::Destination};
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "lan-pairing-v1";
pub const CREATE_PATH: &str = "/api/lan-pairing/create";
pub const REDEEM_PATH: &str = "/api/lan-pairing/redeem";
pub const REVOKE_PATH: &str = "/api/lan-pairing/revoke";
pub const PREFIX: &str = "LAN-";
pub const CODE_BYTES: usize = 16;
pub const CHALLENGE_BYTES: usize = 16;
pub const LIFETIME_MS: u64 = 120_000;
pub const MAX_ENTRIES: usize = 32;
pub const MAX_PER_OWNER: usize = 8;
pub const MAX_CREATE_BYTES: usize = 8192;
pub const MAX_REDEEM_BYTES: usize = 1024;
pub const MAX_ENVELOPE_BYTES: usize = 12_288;
pub const MAX_CIPHERTEXT_BYTES: usize = 6144;

// No Debug on code, payload, request or envelope types.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub server_id: String,
    pub code: String,
    pub authority: RoutePin,
    pub destination: Destination,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    /// Canonical base64url of exactly CODE_BYTES random bytes; no display prefix.
    pub code: String,
    pub invitation: Invitation,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Created {
    pub id: String,
    pub expires_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revoke {
    pub id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Redeem {
    pub server_id: String,
    pub id: String,
    /// Canonical base64url of CHALLENGE_BYTES random bytes, retained for retries.
    pub challenge: String,
    /// HMAC-SHA256(auth key, redeem_message), canonical base64url, 32 bytes.
    pub tag: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub server_id: String,
    pub id: String,
    pub expires_ms: u64,
    pub nonce: String,
    pub ciphertext: String,
}
#[derive(Clone, Copy)]
pub enum Purpose {
    Lookup,
    Auth,
    Seal,
}
/// SHA256 of these bytes yields the ID (lowercase hex), auth key or AEAD key.
/// The input is decoded random code bytes, never a human password or short PIN.
pub fn material(server_id: &str, code: &[u8; CODE_BYTES], purpose: Purpose) -> Vec<u8> {
    let domain: &[u8] = match purpose {
        Purpose::Lookup => b"relay-lan-pairing-id-v1\0",
        Purpose::Auth => b"relay-lan-pairing-auth-v1\0",
        Purpose::Seal => b"relay-lan-pairing-seal-v1\0",
    };
    let mut out = domain.to_vec();
    out.extend(serde_json::to_vec(&(server_id, code)).expect("fixed pairing material"));
    out
}
pub fn redeem_message(request: &Redeem) -> Vec<u8> {
    let mut out = b"relay-lan-pairing-redeem-v1\0".to_vec();
    out.extend(
        serde_json::to_vec(&(&request.server_id, &request.id, &request.challenge))
            .expect("fixed redemption"),
    );
    out
}
pub fn associated_data(envelope: &Envelope) -> Vec<u8> {
    let mut out = b"relay-lan-pairing-envelope-v1\0".to_vec();
    out.extend(
        serde_json::to_vec(&(
            &envelope.server_id,
            &envelope.id,
            envelope.expires_ms,
            &envelope.nonce,
        ))
        .expect("fixed envelope metadata"),
    );
    out
}
