//! M4 takeover foundation, slice S1: bounded wire shapes and domain-separated signing
//! material for a three-host authority set (current leader plus two candidates, quorum
//! two). Nothing here is published: no capability, route, history edge or endpoint uses
//! these types yet, and this crate performs no cryptography and no I/O. Signatures are
//! made and checked by the server's pure voter module over the bytes built below.
//!
//! Every message binds the server, the set digest, the set epoch and the term, so an
//! acknowledgement or vote can never be replayed for another set, epoch, term or entry.
//! Log payloads are bounded OPAQUE complete-state bytes (unpadded base64url text); they
//! are not M2 filtered rows and never appear in `Debug` output.
use crate::handover::{HostIdentity, RoutePin};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const SCHEMA: u32 = 1;
pub const MEMBERS: usize = 3;
pub const QUORUM: usize = 2;
/// One entry carries at most 1 MiB of opaque bytes, i.e. 1_398_102 base64url characters.
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;
pub const MAX_PAYLOAD_TEXT: usize = 1_398_102;
/// Upper bound for parsing one serialized entry; oversize work is refused, never cut.
pub const MAX_ENTRY_JSON: usize = MAX_PAYLOAD_TEXT + 8 * 1024;
/// Upper bound for every other message (sets, consents, acks, nacks, votes).
pub const MAX_CONTROL_JSON: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid(pub &'static str);
impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid takeover message: {}", self.0)
    }
}
impl std::error::Error for Invalid {}
type Check = Result<(), Invalid>;
fn ensure(ok: bool, why: &'static str) -> Check {
    if ok {
        Ok(())
    } else {
        Err(Invalid(why))
    }
}

/// Bounded JSON parse: a larger input is refused explicitly, never truncated.
pub fn parse<T: DeserializeOwned>(bytes: &[u8], max: usize) -> Result<T, Invalid> {
    ensure(bytes.len() <= max, "oversized message")?;
    serde_json::from_slice(bytes).map_err(|_| Invalid("malformed message"))
}

// ---- canonical shapes ------------------------------------------------------------------

/// Lowercase hyphenated UUID text, the shape of `HostIdentity::host_id` and server ids.
pub fn canonical_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}
/// Lowercase hex SHA-256.
pub fn canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn b64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}
/// Unpadded base64url of exactly `bytes` bytes, with zero trailing bits.
fn canonical_b64(value: &str, bytes: usize) -> bool {
    let want = (bytes * 8).div_ceil(6);
    if value.len() != want || !value.bytes().all(|b| b64_value(b).is_some()) {
        return false;
    }
    let spare = want * 6 - bytes * 8;
    let last = value.as_bytes()[want - 1];
    b64_value(last).is_some_and(|v| v & ((1u8 << spare) - 1) == 0)
}
/// 32-byte public key text (host key or recipient key).
pub fn canonical_key(value: &str) -> bool {
    canonical_b64(value, 32)
}
/// 64-byte Ed25519 signature text.
pub fn canonical_signature(value: &str) -> bool {
    canonical_b64(value, 64)
}
pub fn canonical_identity(identity: &HostIdentity) -> bool {
    canonical_id(&identity.host_id) && canonical_key(&identity.public_key)
}
fn canonical_payload(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() > MAX_PAYLOAD_TEXT || !bytes.iter().all(|b| b64_value(*b).is_some()) {
        return false;
    }
    let last = bytes.last().and_then(|b| b64_value(*b)).unwrap_or(0);
    match bytes.len() % 4 {
        0 => true,
        1 => false,
        2 => last & 0x0f == 0,
        _ => last & 0x03 == 0,
    }
}

/// Domain-separated, length-prefixed signing/digest material, independent of JSON order.
struct Msg(Vec<u8>);
impl Msg {
    fn new(domain: &str) -> Self {
        let mut bytes = domain.as_bytes().to_vec();
        bytes.push(0);
        Msg(bytes)
    }
    fn bytes(mut self, value: &[u8]) -> Self {
        self.0.extend(
            u32::try_from(value.len())
                .expect("bounded takeover field")
                .to_be_bytes(),
        );
        self.0.extend(value);
        self
    }
    fn text(self, value: &str) -> Self {
        self.bytes(value.as_bytes())
    }
    fn num(mut self, value: u64) -> Self {
        self.0.extend(value.to_be_bytes());
        self
    }
    fn flag(self, value: bool) -> Self {
        self.num(u64::from(value))
    }
    fn id(self, identity: &HostIdentity) -> Self {
        self.text(&identity.host_id).text(&identity.public_key)
    }
    fn tip(self, tip: &Tip) -> Self {
        self.num(tip.term).num(tip.index).text(&tip.digest)
    }
}

/// Anything a member signs: who signs, what exactly, and the signature text.
pub trait Signed {
    fn signer(&self) -> &HostIdentity;
    fn signing_message(&self) -> Vec<u8>;
    fn signature(&self) -> &str;
}

// ---- authority set and consent --------------------------------------------------------

/// The leader's current route pin (server id, host key, term), the set epoch and the two
/// candidates in strictly ascending `host_id` order. The leader signs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetBody {
    pub schema: u32,
    pub pin: RoutePin,
    pub epoch: u64,
    pub candidates: [HostIdentity; 2],
}
impl SetBody {
    pub fn leader(&self) -> &HostIdentity {
        &self.pin.host
    }
    pub fn members(&self) -> [&HostIdentity; MEMBERS] {
        [&self.pin.host, &self.candidates[0], &self.candidates[1]]
    }
    pub fn is_member(&self, identity: &HostIdentity) -> bool {
        self.members().iter().any(|m| *m == identity)
    }
    pub fn is_candidate(&self, identity: &HostIdentity) -> bool {
        self.candidates.iter().any(|c| c == identity)
    }
    pub fn validate(&self) -> Check {
        ensure(self.schema == SCHEMA, "set schema")?;
        ensure(canonical_id(&self.pin.server_id), "set server id")?;
        ensure(self.pin.term > 0 && self.epoch > 0, "set term and epoch")?;
        let members = self.members();
        ensure(members.iter().all(|m| canonical_identity(m)), "set member")?;
        for (i, a) in members.iter().enumerate() {
            for b in &members[i + 1..] {
                ensure(
                    a.host_id != b.host_id && a.public_key != b.public_key,
                    "duplicate set member",
                )?;
            }
        }
        ensure(
            self.candidates[0].host_id < self.candidates[1].host_id,
            "candidates must be in ascending host id order",
        )
    }
    pub fn message(&self) -> Vec<u8> {
        Msg::new("relay-takeover-set-v1")
            .num(u64::from(self.schema))
            .text(&self.pin.server_id)
            .num(self.pin.term)
            .id(&self.pin.host)
            .num(self.epoch)
            .id(&self.candidates[0])
            .id(&self.candidates[1])
            .0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoritySet {
    pub body: SetBody,
    pub signature: String,
}
impl AuthoritySet {
    pub fn validate(&self) -> Check {
        self.body.validate()?;
        ensure(canonical_signature(&self.signature), "set signature")
    }
}
impl Signed for AuthoritySet {
    fn signer(&self) -> &HostIdentity {
        self.body.leader()
    }
    fn signing_message(&self) -> Vec<u8> {
        self.body.message()
    }
    fn signature(&self) -> &str {
        &self.signature
    }
}

/// One candidate's own, recipient-specific consent: this exact set, its full-data
/// recipient key and an explicit takeover agreement. Distinct from M2 replica consent
/// and from planned-handover consent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptBody {
    pub schema: u32,
    pub server_id: String,
    pub set_sha256: String,
    pub epoch: u64,
    pub candidate: HostIdentity,
    pub recipient_key: String,
    pub full_data: bool,
    pub takeover: bool,
}
impl AcceptBody {
    pub fn validate(&self) -> Check {
        ensure(self.schema == SCHEMA, "consent schema")?;
        ensure(canonical_id(&self.server_id), "consent server id")?;
        ensure(
            canonical_digest(&self.set_sha256) && self.epoch > 0,
            "consent set",
        )?;
        ensure(canonical_identity(&self.candidate), "consent candidate")?;
        ensure(canonical_key(&self.recipient_key), "consent recipient key")?;
        ensure(
            self.full_data && self.takeover,
            "both consents must be given",
        )
    }
    pub fn message(&self) -> Vec<u8> {
        Msg::new("relay-takeover-accept-v1")
            .num(u64::from(self.schema))
            .text(&self.server_id)
            .text(&self.set_sha256)
            .num(self.epoch)
            .id(&self.candidate)
            .text(&self.recipient_key)
            .flag(self.full_data)
            .flag(self.takeover)
            .0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAccept {
    pub body: AcceptBody,
    pub signature: String,
}
impl CandidateAccept {
    pub fn validate(&self) -> Check {
        self.body.validate()?;
        ensure(canonical_signature(&self.signature), "consent signature")
    }
}
impl Signed for CandidateAccept {
    fn signer(&self) -> &HostIdentity {
        &self.body.candidate
    }
    fn signing_message(&self) -> Vec<u8> {
        self.body.message()
    }
    fn signature(&self) -> &str {
        &self.signature
    }
}

// ---- log position, entries, acknowledgements ------------------------------------------

/// A log position and the digest of the entry (or of the seeding snapshot) found there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tip {
    pub term: u64,
    pub index: u64,
    pub digest: String,
}
impl Tip {
    pub fn validate(&self) -> Check {
        ensure(self.term > 0, "tip term")?;
        ensure(canonical_digest(&self.digest), "tip digest")
    }
}

/// Bounded opaque complete-state bytes as unpadded base64url text. Never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(String);
impl Payload {
    pub fn new(text: impl Into<String>) -> Result<Self, Invalid> {
        let text = text.into();
        ensure(canonical_payload(&text), "payload")?;
        Ok(Self(text))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn validate(&self) -> Check {
        ensure(canonical_payload(&self.0), "payload")
    }
    pub fn decoded_len(&self) -> usize {
        self.0.len() / 4 * 3 + [0, 0, 1, 2][self.0.len() % 4]
    }
}
impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Payload(<{} bytes redacted>)", self.decoded_len())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryBody {
    pub schema: u32,
    pub server_id: String,
    pub set_sha256: String,
    pub epoch: u64,
    pub term: u64,
    pub index: u64,
    /// Digest of the previous entry, or of the seeding snapshot for the first entry.
    pub previous: String,
    pub leader: HostIdentity,
    pub payload: Payload,
}
impl EntryBody {
    pub fn validate(&self) -> Check {
        ensure(self.schema == SCHEMA, "entry schema")?;
        ensure(canonical_id(&self.server_id), "entry server id")?;
        ensure(
            canonical_digest(&self.set_sha256) && self.epoch > 0,
            "entry set",
        )?;
        ensure(self.term > 0 && self.index > 0, "entry position")?;
        ensure(canonical_digest(&self.previous), "entry previous digest")?;
        ensure(canonical_identity(&self.leader), "entry leader")?;
        self.payload.validate()
    }
    fn fields(&self, domain: &str) -> Msg {
        Msg::new(domain)
            .num(u64::from(self.schema))
            .text(&self.server_id)
            .text(&self.set_sha256)
            .num(self.epoch)
            .num(self.term)
            .num(self.index)
            .text(&self.previous)
            .id(&self.leader)
            .text(self.payload.as_str())
    }
    /// The bytes whose SHA-256 (lowercase hex) is the entry's digest.
    pub fn digest_message(&self) -> Vec<u8> {
        self.fields("relay-takeover-entry-digest-v1").0
    }
}
/// Signed by the term's leader (`body.leader`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
    pub body: EntryBody,
    pub signature: String,
}
impl LogEntry {
    pub fn validate(&self) -> Check {
        self.body.validate()?;
        ensure(canonical_signature(&self.signature), "entry signature")
    }
}
impl Signed for LogEntry {
    fn signer(&self) -> &HostIdentity {
        &self.body.leader
    }
    fn signing_message(&self) -> Vec<u8> {
        self.body.fields("relay-takeover-entry-v1").0
    }
    fn signature(&self) -> &str {
        &self.signature
    }
}

/// "I durably appended exactly this entry." Signed by `voter`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckBody {
    pub schema: u32,
    pub server_id: String,
    pub set_sha256: String,
    pub epoch: u64,
    pub term: u64,
    pub index: u64,
    pub digest: String,
    pub voter: HostIdentity,
}
impl AckBody {
    pub fn validate(&self) -> Check {
        ensure(self.schema == SCHEMA, "ack schema")?;
        ensure(canonical_id(&self.server_id), "ack server id")?;
        ensure(
            canonical_digest(&self.set_sha256) && self.epoch > 0,
            "ack set",
        )?;
        ensure(self.term > 0 && self.index > 0, "ack position")?;
        ensure(canonical_digest(&self.digest), "ack digest")?;
        ensure(canonical_identity(&self.voter), "ack voter")
    }
    pub fn message(&self) -> Vec<u8> {
        Msg::new("relay-takeover-ack-v1")
            .num(u64::from(self.schema))
            .text(&self.server_id)
            .text(&self.set_sha256)
            .num(self.epoch)
            .num(self.term)
            .num(self.index)
            .text(&self.digest)
            .id(&self.voter)
            .0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub body: AckBody,
    pub signature: String,
}
impl Ack {
    pub fn validate(&self) -> Check {
        self.body.validate()?;
        ensure(canonical_signature(&self.signature), "ack signature")
    }
}
impl Signed for Ack {
    fn signer(&self) -> &HostIdentity {
        &self.body.voter
    }
    fn signing_message(&self) -> Vec<u8> {
        self.body.message()
    }
    fn signature(&self) -> &str {
        &self.signature
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NackReason {
    /// The refusing host has promised a higher term.
    Fenced,
    /// The entry is ahead of the refusing host's tip.
    Gap,
    /// A different entry (or snapshot lineage) holds that position.
    Divergent,
    /// The position is older than the bounded replay window.
    Stale,
    /// The entry's term is one this host has not promised.
    UnknownTerm,
    /// This host already voted for another candidate in that term.
    AlreadyVoted,
    /// The candidate's tip is behind this host's.
    StaleTip,
    /// Same term and candidate, but the request's tip or consent is not the binding this
    /// host persisted with its vote. It is never answered as an identical retry.
    ChangedRequest,
}
impl NackReason {
    pub fn as_str(self) -> &'static str {
        match self {
            NackReason::Fenced => "fenced",
            NackReason::Gap => "gap",
            NackReason::Divergent => "divergent",
            NackReason::Stale => "stale",
            NackReason::UnknownTerm => "unknown_term",
            NackReason::AlreadyVoted => "already_voted",
            NackReason::StaleTip => "stale_tip",
            NackReason::ChangedRequest => "changed_request",
        }
    }
}
/// A signed refusal that reports the refuser's promise and tip. Only a VERIFIED nack from
/// a current member is ever acted on, and it can only withhold a commit, never grant one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NackBody {
    pub schema: u32,
    pub server_id: String,
    pub set_sha256: String,
    pub epoch: u64,
    pub voter: HostIdentity,
    pub reason: NackReason,
    pub promised_term: u64,
    pub tip: Tip,
}
impl NackBody {
    pub fn validate(&self) -> Check {
        ensure(self.schema == SCHEMA, "nack schema")?;
        ensure(canonical_id(&self.server_id), "nack server id")?;
        ensure(
            canonical_digest(&self.set_sha256) && self.epoch > 0,
            "nack set",
        )?;
        ensure(canonical_identity(&self.voter), "nack voter")?;
        ensure(self.promised_term > 0, "nack term")?;
        self.tip.validate()
    }
    pub fn message(&self) -> Vec<u8> {
        Msg::new("relay-takeover-nack-v1")
            .num(u64::from(self.schema))
            .text(&self.server_id)
            .text(&self.set_sha256)
            .num(self.epoch)
            .id(&self.voter)
            .text(self.reason.as_str())
            .num(self.promised_term)
            .tip(&self.tip)
            .0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nack {
    pub body: NackBody,
    pub signature: String,
}
impl Nack {
    pub fn validate(&self) -> Check {
        self.body.validate()?;
        ensure(canonical_signature(&self.signature), "nack signature")
    }
}
impl Signed for Nack {
    fn signer(&self) -> &HostIdentity {
        &self.body.voter
    }
    fn signing_message(&self) -> Vec<u8> {
        self.body.message()
    }
    fn signature(&self) -> &str {
        &self.signature
    }
}

// ---- votes ------------------------------------------------------------------------------

/// A candidate's signed claim: this term, this tip, and its own consent. A voter that
/// grants it signs a `Vote`; an election result alone never enables promotion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteRequestBody {
    pub schema: u32,
    pub server_id: String,
    pub set_sha256: String,
    pub epoch: u64,
    pub term: u64,
    pub candidate: HostIdentity,
    pub tip: Tip,
    pub accept: CandidateAccept,
}
impl VoteRequestBody {
    pub fn validate(&self) -> Check {
        ensure(self.schema == SCHEMA, "vote request schema")?;
        ensure(canonical_id(&self.server_id), "vote request server id")?;
        ensure(
            canonical_digest(&self.set_sha256) && self.epoch > 0,
            "vote request set",
        )?;
        ensure(
            canonical_identity(&self.candidate),
            "vote request candidate",
        )?;
        self.tip.validate()?;
        ensure(
            self.tip.term < self.term,
            "a candidate tip must be from an earlier term",
        )?;
        self.accept.validate()
    }
    pub fn message(&self) -> Vec<u8> {
        Msg::new("relay-takeover-vote-request-v1")
            .num(u64::from(self.schema))
            .text(&self.server_id)
            .text(&self.set_sha256)
            .num(self.epoch)
            .num(self.term)
            .id(&self.candidate)
            .tip(&self.tip)
            .bytes(&self.accept.body.message())
            .text(&self.accept.signature)
            .0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteRequest {
    pub body: VoteRequestBody,
    pub signature: String,
}
impl VoteRequest {
    pub fn validate(&self) -> Check {
        self.body.validate()?;
        ensure(
            canonical_signature(&self.signature),
            "vote request signature",
        )
    }
}
impl Signed for VoteRequest {
    fn signer(&self) -> &HostIdentity {
        &self.body.candidate
    }
    fn signing_message(&self) -> Vec<u8> {
        self.body.message()
    }
    fn signature(&self) -> &str {
        &self.signature
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteBody {
    pub schema: u32,
    pub server_id: String,
    pub set_sha256: String,
    pub epoch: u64,
    pub term: u64,
    pub candidate: HostIdentity,
    /// The candidate tip this voter persisted with the vote.
    pub candidate_tip: Tip,
    /// Digest of the candidate's consent (body message plus signature) that this voter
    /// persisted with the vote.
    pub accept_sha256: String,
    pub voter: HostIdentity,
}
impl VoteBody {
    pub fn validate(&self) -> Check {
        ensure(self.schema == SCHEMA, "vote schema")?;
        ensure(canonical_id(&self.server_id), "vote server id")?;
        ensure(
            canonical_digest(&self.set_sha256) && self.epoch > 0,
            "vote set",
        )?;
        ensure(self.term > 0, "vote term")?;
        ensure(canonical_identity(&self.candidate), "vote candidate")?;
        ensure(canonical_identity(&self.voter), "vote voter")?;
        ensure(canonical_digest(&self.accept_sha256), "vote consent digest")?;
        self.candidate_tip.validate()
    }
    pub fn message(&self) -> Vec<u8> {
        Msg::new("relay-takeover-vote-v1")
            .num(u64::from(self.schema))
            .text(&self.server_id)
            .text(&self.set_sha256)
            .num(self.epoch)
            .num(self.term)
            .id(&self.candidate)
            .tip(&self.candidate_tip)
            .text(&self.accept_sha256)
            .id(&self.voter)
            .0
    }
}
/// One voter's durable grant for one term. Signed by `voter`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vote {
    pub body: VoteBody,
    pub signature: String,
}
impl Vote {
    pub fn validate(&self) -> Check {
        self.body.validate()?;
        ensure(canonical_signature(&self.signature), "vote signature")
    }
}
impl Signed for Vote {
    fn signer(&self) -> &HostIdentity {
        &self.body.voter
    }
    fn signing_message(&self) -> Vec<u8> {
        self.body.message()
    }
    fn signature(&self) -> &str {
        &self.signature
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_shapes_are_exact_and_payloads_are_bounded_and_never_printed() {
        let key = "A".repeat(43);
        assert!(canonical_key(&key));
        // The 43rd character carries only two data bits; a non-zero low bit is not canonical.
        assert!(!canonical_key(&format!("{}B", "A".repeat(42))));
        assert!(!canonical_key(&"A".repeat(42)) && !canonical_key(&"A".repeat(44)));
        assert!(!canonical_key(&format!("{}+", "A".repeat(42))));
        assert!(canonical_signature(&format!("{}Q", "A".repeat(85))));
        assert!(!canonical_signature(&format!("{}R", "A".repeat(85))));
        assert!(canonical_digest(&"0".repeat(64)) && !canonical_digest(&"A".repeat(64)));
        assert!(canonical_id("0a1b2c3d-0000-8000-8000-000000000001"));
        assert!(!canonical_id("0A1B2C3D-0000-8000-8000-000000000001"));

        let full = "A".repeat(MAX_PAYLOAD_TEXT);
        assert!(Payload::new(full.clone()).is_ok());
        assert_eq!(Payload::new(full).unwrap().decoded_len(), MAX_PAYLOAD_BYTES);
        assert!(
            Payload::new("A".repeat(MAX_PAYLOAD_TEXT + 4)).is_err(),
            "refused, never truncated"
        );
        assert!(Payload::new("A").is_err() && Payload::new("AB==").is_err());
        let secret = Payload::new("c2VjcmV0LWJ5dGVz").unwrap();
        assert!(!format!("{secret:?}").contains("c2Vj"));
        assert!(
            Payload::new("").is_ok(),
            "an empty payload is a valid barrier entry"
        );
    }

    #[test]
    fn unknown_fields_and_oversized_messages_are_refused() {
        let tip = r#"{"term":1,"index":0,"digest":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        assert!(parse::<Tip>(tip.as_bytes(), MAX_CONTROL_JSON).is_ok());
        let extra = tip.replace("}", r#","extra":1}"#);
        assert!(parse::<Tip>(extra.as_bytes(), MAX_CONTROL_JSON).is_err());
        assert!(parse::<Tip>(tip.as_bytes(), 10).is_err());
    }
}
