//! An independent directory carries signed opaque route hints. Its response is
//! never a trust root: readers need their invitation pin and a fresh host proof.
use crate::handover::{ActivationGrant, IrohDestination, RoutePin};
use serde::{Deserialize, Serialize};

pub const LOOKUP_PATH: &str = "/api/host-locator/lookup";
pub const PUBLISH_PATH: &str = "/api/host-locator/publish";
pub const ISSUE_PATH: &str = "/api/hosting/handover/locator";
pub const LOCAL_PAGE_PATH: &str = "/api/local/handover/locator-page";
pub const LOCAL_SESSION_PAGE_PATH: &str = "/api/local/handover/session-locator-page";
pub const PAGE_RECORDS: usize = 4;
pub const MAX_PAGE_BYTES: usize = PAGE_RECORDS * MAX_RECORD_BYTES + 4096;
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_CIPHERTEXT_BYTES: usize = 6144;
pub const MAX_LIFETIME_MS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lookup {
    /// A 256-bit capability, sent in a POST body, never in a URL or access log.
    pub id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub anchor: RoutePin,
    pub current: RoutePin,
    pub history: Vec<ActivationGrant>,
    /// Source clock, also the strictly increasing revision within one term.
    pub issued_ms: u64,
    pub expires_ms: u64,
    /// Base64url 12-byte ChaCha20-Poly1305 nonce and ciphertext (including tag).
    pub nonce: String,
    pub ciphertext: String,
    pub signature: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub url: String,
    /// The successor's own admission capability; only inside ciphertext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh: Option<IrohDestination>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    /// Existing invitation verifier, not the plaintext invitation bearer.
    pub invite_id: String,
    pub anchor: RoutePin,
    pub destination: Destination,
}

/// An operator-private position, never a directory request or a public link.
/// It contains an invitation or session verifier and must stay inside the owned
/// host profile. The historical `invite_id` field name is kept on this private API.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub invite_id: String,
    pub anchor_term: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    pub current: RoutePin,
    pub destination: Destination,
    #[serde(default)]
    pub after: Option<Cursor>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub current: RoutePin,
    pub records: Vec<Record>,
    pub next: Option<Cursor>,
}

/// The caller hashes these domain-separated bytes with SHA-256. The input is
/// the existing 32-byte invite verifier. Knowing a directory ID must not expose
/// either the encryption key or the original invitation.
pub fn capability_material(anchor: &RoutePin, invite_hash: &[u8; 32], encryption: bool) -> Vec<u8> {
    let mut out = if encryption {
        b"relay-host-locator-key-v1\0".to_vec()
    } else {
        b"relay-host-locator-id-v1\0".to_vec()
    };
    out.extend(serde_json::to_vec(anchor).expect("fixed route pin"));
    out.extend(invite_hash);
    out
}

/// A remembered, still-valid session can find its host without retaining an
/// invitation. Session tokens/verifiers never go to the directory. The purpose
/// has a separate domain from invitation lookups, even for equal verifier bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Invitation,
    Session,
}
pub fn session_material(anchor: &RoutePin, session_hash: &[u8; 32], encryption: bool) -> Vec<u8> {
    let mut out = if encryption {
        b"relay-session-host-locator-key-v1\0".to_vec()
    } else {
        b"relay-session-host-locator-id-v1\0".to_vec()
    };
    out.extend(serde_json::to_vec(anchor).expect("fixed route pin"));
    out.extend(session_hash);
    out
}

/// Associated data binds the opaque destination to its independently pinned
/// anchor and exact current authority. Signatures bind all fields, including
/// history, freshness, nonce and ciphertext, without signing the signature.
pub fn associated_data(record: &Record) -> Vec<u8> {
    let mut out = b"relay-host-locator-aead-v1\0".to_vec();
    out.extend(serde_json::to_vec(&(&record.id, &record.anchor, &record.current)).expect("fixed locator fields"));
    out
}
pub fn record_message(record: &Record) -> Vec<u8> {
    let mut out = b"relay-host-locator-record-v1\0".to_vec();
    out.extend(serde_json::to_vec(&(
        &record.id, &record.anchor, &record.current, &record.history,
        record.issued_ms, record.expires_ms, &record.nonce, &record.ciphertext,
    )).expect("fixed locator fields"));
    out
}
