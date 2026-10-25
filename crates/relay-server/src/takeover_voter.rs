//! M4 takeover foundation, slice S1: a pure, durable log/voter state machine.
//!
//! This is NOT a fence for real server writes. It is not wired to SQLite, handlers, HTTP,
//! voice, routes, history or any capability, and `Abilities` stay unchanged. It performs no
//! network or database work. The injected `LogStore` is the only I/O and the injected
//! `HostSigner` the only key use; this module never creates, reads or regenerates a key.
//!
//! Storage contract. The store holds two things: the small voter metadata value (promise,
//! vote, set, consent, base tip, the last 64 positions; at most 256 KiB, it never grows with
//! the log) and one record per log entry: the EXACT full signed `LogEntry` as bounded JSON
//! (opaque payload included, at most `MAX_ENTRY_JSON`), keyed by its index, never evicted.
//! One `commit(entry, state)` call must retain both or neither. A successful append or
//! propose therefore proves the complete entry is durable before an `Ack` exists; a digest
//! alone is never an acknowledgement. A vote changes only the metadata value.
//! Retrieval is part of the contract (`load_entry`): replay, `open`, `entry` and
//! `commit_gate` re-read the retained record and refuse to acknowledge a missing, corrupt,
//! conflicting or mismatched one. The seeding snapshot (`base`) is distinct P0 metadata,
//! not an invented entry.
//!
//! Invariants (each is checked in `tests`):
//! * Persist first: every state change is stored through the store BEFORE any signed ack,
//!   vote or proposed entry is returned. An uncertain store failure returns an error, leaves
//!   the in-memory state untouched and poisons the voter until reloaded; a definite
//!   `Full` writes nothing, adopts nothing and leaves the voter usable.
//! * A promised term only grows. A higher promise fences every older append, replay and ack.
//! * `append` needs the exact current term, `index == tip + 1` and `previous == tip digest`;
//!   an identical replay in the bounded window returns the identical ack only after the
//!   retained full entry is re-read and found identical; anything else at a known position
//!   is a refusal, and nothing here ever truncates or rewrites a chain.
//! * One vote per term; the same candidate may retry and receives the identical vote ONLY if
//!   its request carries the tip and consent persisted with that vote; otherwise it is
//!   refused as `ChangedRequest`. A granted `Vote` states the persisted tip and consent digest.
//!   A candidate behind this host's tip, or at the same position with another digest, is refused.
//! * `commit_gate` grants a commit only from a locally retained exact entry plus one correctly
//!   signed acknowledgement from a DIFFERENT current member, bound to the exact server, set,
//!   epoch, term, index and digest. Nothing else can grant it.
//! * Loading never initializes or repairs: a missing, corrupt, oversized or mismatched
//!   metadata value, or any missing, corrupt, conflicting or mismatched retained entry in the
//!   replay window, is refused.
//! * Payloads never appear in `Debug` or error output.
//!
//! Not here (later slices, after review): the complete online snapshot that seeds the log,
//! full-state row/blob capture, SQLite hooks, publication/ack gating of real writes, arming
//! consent, candidate liveness policy and promotion. Truncation of an uncommitted local suffix
//! is deliberately unsupported: a candidate must not be behind this host in index.
#![allow(dead_code)]

use crate::handover_target::verify;
use relay_protocol::{
    handover::{HostIdentity, HostProof},
    takeover::{
        self as wire, Ack, AckBody, AuthoritySet, CandidateAccept, EntryBody, LogEntry, Nack,
        NackBody, NackReason, Payload, Signed, Tip, Vote, VoteBody, VoteRequest, VoteRequestBody,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How many most recent positions are kept to recognize an identical replay.
const RECENT: usize = 64;
const MAX_STATE_BYTES: usize = 256 * 1024;
/// More evidence than this is refused outright, never partly looked at.
const MAX_EVIDENCE: usize = 16;

/// Why a store operation did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreError {
    /// Storage is exhausted. NOTHING was written; retained entries are never evicted to
    /// make room.
    Full,
    /// A different record is already retained at that index. Nothing was written.
    Conflict,
    /// The outcome is unknown: the previous contents may or may not be replaced.
    Failed,
}

/// The durable log boundary (production: files with temp, fsync, rename and directory sync).
pub(crate) trait LogStore {
    /// The voter metadata value, if any.
    fn load_state(&mut self) -> Result<Option<Vec<u8>>, StoreError>;
    /// The exact bytes retained for the entry at `index`, if any.
    fn load_entry(&mut self, index: u64) -> Result<Option<Vec<u8>>, StoreError>;
    /// ONE atomic, durable operation: retain `entry` (`(index, exact bytes)`, at most
    /// `MAX_ENTRY_JSON`) when given AND replace the metadata value, or do neither. `Ok`
    /// means both are durable. Re-retaining identical bytes at an index is idempotent; other
    /// bytes at a retained index are `Conflict`. An implementation never deletes or replaces
    /// a retained entry.
    fn commit(&mut self, entry: Option<(u64, &[u8])>, state: &[u8]) -> Result<(), StoreError>;
}

/// This host's signing identity. The key lives with the caller; nothing is derived here.
pub(crate) trait HostSigner {
    fn identity(&self) -> &HostIdentity;
    /// URL-safe, unpadded Ed25519 signature text.
    fn sign(&self, message: &[u8]) -> String;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Invalid(&'static str),
    BadSignature(&'static str),
    Mismatch(&'static str),
    NotMember,
    NotCandidate,
    NotLeader,
    Missing,
    Corrupt(&'static str),
    AlreadyInitialized,
    Overflow,
    Persist,
    /// Storage is exhausted; nothing was written and nothing changed.
    Full,
    Poisoned,
}
fn invalid(e: wire::Invalid) -> Refused {
    Refused::Invalid(e.0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AppendAnswer {
    Ack(Ack),
    Nack(Nack),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VoteAnswer {
    Granted(Vote),
    Refused(Nack),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    Poisoned,
    Oversized,
    Mismatch,
    Fenced,
    NotPersisted,
    Insufficient,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Commit,
    Rollback(Why),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Voted {
    term: u64,
    candidate: HostIdentity,
    /// The candidate tip and consent digest this vote was granted for.
    tip: Tip,
    accept_sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    identity: HostIdentity,
    set: AuthoritySet,
    set_sha256: String,
    accept: Option<CandidateAccept>,
    promised_term: u64,
    /// The leader of `promised_term`: the set's leader at the start, later the candidate
    /// this host voted for.
    leader: HostIdentity,
    voted: Voted,
    /// The seeding snapshot position: P0 metadata, not an entry.
    base: Tip,
    tip: Tip,
    recent: Vec<Tip>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    state: State,
    check: String,
}
#[derive(Serialize)]
struct StoredRef<'a> {
    state: &'a State,
    check: String,
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}
fn sha256_hex(message: &[u8]) -> String {
    hex(&Sha256::digest(message))
}
fn accept_digest(accept: &CandidateAccept) -> String {
    let mut message = accept.body.message();
    message.extend(accept.signature.as_bytes());
    sha256_hex(&message)
}
fn set_digest(set: &AuthoritySet) -> String {
    sha256_hex(&set.body.message())
}
fn entry_digest(body: &EntryBody) -> String {
    sha256_hex(&body.digest_message())
}
fn state_check(state_json: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"relay-takeover-state-v1\0");
    hash.update(state_json);
    hex(&hash.finalize())
}
fn encode(state: &State) -> Result<Vec<u8>, Refused> {
    let json = serde_json::to_vec(state).map_err(|_| Refused::Corrupt("encode"))?;
    let bytes = serde_json::to_vec(&StoredRef {
        state,
        check: state_check(&json),
    })
    .map_err(|_| Refused::Corrupt("encode"))?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(Refused::Corrupt("state too large"));
    }
    Ok(bytes)
}
fn decode(bytes: &[u8]) -> Result<State, Refused> {
    if bytes.len() > MAX_STATE_BYTES {
        return Err(Refused::Corrupt("state too large"));
    }
    let stored: Stored =
        serde_json::from_slice(bytes).map_err(|_| Refused::Corrupt("unreadable state"))?;
    let json =
        serde_json::to_vec(&stored.state).map_err(|_| Refused::Corrupt("unreadable state"))?;
    if state_check(&json) != stored.check {
        return Err(Refused::Corrupt("state check"));
    }
    Ok(stored.state)
}

fn encode_entry(entry: &LogEntry) -> Result<Vec<u8>, Refused> {
    let bytes = serde_json::to_vec(entry).map_err(|_| Refused::Corrupt("encode entry"))?;
    if bytes.len() > wire::MAX_ENTRY_JSON {
        return Err(Refused::Invalid("entry too large"));
    }
    Ok(bytes)
}
/// A retained record must be the canonical encoding of a structurally valid entry.
fn decode_entry(bytes: &[u8]) -> Result<LogEntry, Refused> {
    let entry: LogEntry = wire::parse(bytes, wire::MAX_ENTRY_JSON)
        .map_err(|_| Refused::Corrupt("retained entry unreadable"))?;
    entry
        .validate()
        .map_err(|_| Refused::Corrupt("retained entry invalid"))?;
    if encode_entry(&entry)?.as_slice() != bytes {
        return Err(Refused::Corrupt("retained entry not canonical"));
    }
    Ok(entry)
}
/// Re-reads the full retained entry at `index` and checks it against the set and, when the
/// caller knows it, the expected position: binding, member signature, index and digest.
fn check_retained<L: LogStore>(
    store: &mut L,
    set: &AuthoritySet,
    set_sha256: &str,
    index: u64,
    expected: Option<&Tip>,
) -> Result<LogEntry, Refused> {
    let bytes = store
        .load_entry(index)
        .map_err(|_| Refused::Persist)?
        .ok_or(Refused::Corrupt("retained entry missing"))?;
    let entry = decode_entry(&bytes)?;
    let b = &entry.body;
    if b.server_id != set.body.pin.server_id
        || b.set_sha256 != set_sha256
        || b.epoch != set.body.epoch
    {
        return Err(Refused::Corrupt("retained entry binding"));
    }
    if !set.body.is_member(&b.leader) || !signed_by(&entry) {
        return Err(Refused::Corrupt("retained entry signature"));
    }
    let digest = entry_digest(b);
    if b.index != index
        || expected.is_some_and(|t| t.index != index || t.term != b.term || t.digest != digest)
    {
        return Err(Refused::Corrupt("retained entry mismatch"));
    }
    Ok(entry)
}

/// Signature of `signed` by its own named signer, which must be a canonical identity.
fn signed_by(signed: &impl Signed) -> bool {
    verify(
        &HostProof {
            identity: signed.signer().clone(),
            signature: signed.signature().to_owned(),
        },
        &signed.signing_message(),
    )
}

/// Everything a stored or supplied (set, consent, identity) triple must satisfy.
fn check_membership(
    set: &AuthoritySet,
    set_sha256: &str,
    identity: &HostIdentity,
    accept: Option<&CandidateAccept>,
) -> Result<(), Refused> {
    set.validate().map_err(invalid)?;
    if !signed_by(set) {
        return Err(Refused::BadSignature("authority set"));
    }
    if set_digest(set) != set_sha256 {
        return Err(Refused::Mismatch("set digest"));
    }
    if !set.body.is_member(identity) {
        return Err(Refused::NotMember);
    }
    match (set.body.is_candidate(identity), accept) {
        (true, Some(a)) => {
            check_accept(set, set_sha256, a)?;
            if &a.body.candidate != identity {
                return Err(Refused::Mismatch("consent candidate"));
            }
        }
        (true, None) => return Err(Refused::Mismatch("candidate consent missing")),
        (false, None) => {}
        (false, Some(_)) => return Err(Refused::Mismatch("the leader gives no candidate consent")),
    }
    Ok(())
}
fn check_accept(set: &AuthoritySet, set_sha256: &str, a: &CandidateAccept) -> Result<(), Refused> {
    a.validate().map_err(invalid)?;
    if a.body.server_id != set.body.pin.server_id
        || a.body.set_sha256 != set_sha256
        || a.body.epoch != set.body.epoch
    {
        return Err(Refused::Mismatch("consent binding"));
    }
    if !set.body.is_candidate(&a.body.candidate) {
        return Err(Refused::NotCandidate);
    }
    if !signed_by(a) {
        return Err(Refused::BadSignature("candidate consent"));
    }
    Ok(())
}
fn check_state(state: &State, signer: &impl HostSigner) -> Result<(), Refused> {
    if state.schema != wire::SCHEMA {
        return Err(Refused::Corrupt("schema"));
    }
    if &state.identity != signer.identity() {
        return Err(Refused::Mismatch("host identity"));
    }
    check_membership(
        &state.set,
        &state.set_sha256,
        &state.identity,
        state.accept.as_ref(),
    )?;
    let corrupt = Refused::Corrupt("inconsistent state");
    let members_ok =
        state.set.body.is_member(&state.leader) && state.set.body.is_member(&state.voted.candidate);
    if !members_ok
        || state.promised_term < state.set.body.pin.term
        || state.voted.term != state.promised_term
        || state.voted.candidate != state.leader
        || (state.promised_term == state.set.body.pin.term
            && state.leader != *state.set.body.leader())
    {
        return Err(corrupt);
    }
    state.base.validate().map_err(invalid)?;
    state.tip.validate().map_err(invalid)?;
    state.voted.tip.validate().map_err(invalid)?;
    if !wire::canonical_digest(&state.voted.accept_sha256) {
        return Err(corrupt);
    }
    if state.recent.is_empty()
        || state.recent.len() > RECENT
        || state.recent.last() != Some(&state.tip)
        || state.tip.term > state.promised_term
        || state.tip.index < state.base.index
        || state.base.term > state.set.body.pin.term
    {
        return Err(corrupt);
    }
    // The window is the last 64 positions, or everything back to the base position.
    let span = state.tip.index - state.base.index + 1;
    let first = &state.recent[0];
    let complete = span <= RECENT as u64;
    if (complete && (state.recent.len() as u64 != span || *first != state.base))
        || (!complete && state.recent.len() != RECENT)
    {
        return Err(corrupt);
    }
    for pair in state.recent.windows(2) {
        pair[0].validate().map_err(invalid)?;
        if pair[1].index != pair[0].index.wrapping_add(1) || pair[1].term < pair[0].term {
            return Err(corrupt);
        }
    }
    Ok(())
}
/// Every position in the window above the base must still have its exact full entry retained,
/// chained to its predecessor. Anything missing, corrupt, conflicting or mismatched refuses.
fn check_window<L: LogStore>(store: &mut L, state: &State) -> Result<(), Refused> {
    let mut previous: Option<&Tip> = None;
    for tip in &state.recent {
        if tip.index > state.base.index {
            let entry = check_retained(store, &state.set, &state.set_sha256, tip.index, Some(tip))?;
            if previous.is_some_and(|p| entry.body.previous != p.digest) {
                return Err(Refused::Corrupt("retained entry chain"));
            }
        }
        previous = Some(tip);
    }
    Ok(())
}

pub(crate) struct Voter<J: LogStore, S: HostSigner> {
    store: J,
    signer: S,
    state: State,
    poisoned: bool,
}

impl<J: LogStore, S: HostSigner> Voter<J, S> {
    /// First and only initialization: a verified leader-signed set, this host's membership,
    /// its own consent when it is a candidate (none for the leader) and the seeding tip.
    /// A store that already holds anything, valid or not, is refused and left as it is.
    pub(crate) fn initialize(
        mut store: J,
        signer: S,
        set: AuthoritySet,
        accept: Option<CandidateAccept>,
        base: Tip,
    ) -> Result<Self, Refused> {
        match store.load_state() {
            Ok(None) => {}
            Ok(Some(_)) => return Err(Refused::AlreadyInitialized),
            Err(_) => return Err(Refused::Persist),
        }
        base.validate().map_err(invalid)?;
        let first = base.index.checked_add(1).ok_or(Refused::Overflow)?;
        match store.load_entry(first) {
            Ok(None) => {}
            Ok(Some(_)) => return Err(Refused::AlreadyInitialized),
            Err(_) => return Err(Refused::Persist),
        }
        let set_sha256 = set_digest(&set);
        let identity = signer.identity().clone();
        check_membership(&set, &set_sha256, &identity, accept.as_ref())?;
        // The injected signer must really hold the key it claims.
        let probe = b"relay-takeover-selfcheck-v1";
        if !verify(
            &HostProof {
                identity: identity.clone(),
                signature: signer.sign(probe),
            },
            probe,
        ) {
            return Err(Refused::Mismatch("signer does not hold its identity key"));
        }
        if base.term > set.body.pin.term {
            return Err(Refused::Mismatch("seeding tip is from a later term"));
        }
        let leader = set.body.leader().clone();
        let state = State {
            schema: wire::SCHEMA,
            identity,
            set_sha256,
            accept,
            promised_term: set.body.pin.term,
            leader: leader.clone(),
            voted: Voted {
                term: set.body.pin.term,
                candidate: leader,
                tip: base.clone(),
                accept_sha256: sha256_hex(b"relay-takeover-initial-term"),
            },
            base: base.clone(),
            tip: base.clone(),
            recent: vec![base],
            set,
        };
        check_state(&state, &signer)?;
        let bytes = encode(&state)?;
        match store.commit(None, &bytes) {
            Ok(()) => {}
            Err(StoreError::Full) => return Err(Refused::Full),
            Err(_) => return Err(Refused::Persist),
        }
        Ok(Self {
            store,
            signer,
            state,
            poisoned: false,
        })
    }

    /// Reloads durable state. Nothing is created or repaired: a missing, unreadable,
    /// corrupt, oversized or mismatched metadata value, or a missing, corrupt, conflicting or
    /// mismatched retained entry anywhere in the replay window, is an error.
    pub(crate) fn open(mut store: J, signer: S) -> Result<Self, Refused> {
        let bytes = match store.load_state() {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err(Refused::Missing),
            Err(_) => return Err(Refused::Persist),
        };
        let state = decode(&bytes)?;
        check_state(&state, &signer)?;
        check_window(&mut store, &state)?;
        Ok(Self {
            store,
            signer,
            state,
            poisoned: false,
        })
    }

    pub(crate) fn identity(&self) -> &HostIdentity {
        &self.state.identity
    }

    pub(crate) fn promised_term(&self) -> u64 {
        self.state.promised_term
    }
    pub(crate) fn tip(&self) -> &Tip {
        &self.state.tip
    }
    pub(crate) fn set_sha256(&self) -> &str {
        &self.state.set_sha256
    }
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Persist `next` together with the exact full `entry` it adds (if any) in ONE store
    /// operation, then (and only then) adopt it. `Full` wrote nothing; anything uncertain
    /// poisons the voter until it is reloaded.
    fn persist(&mut self, next: State, entry: Option<(u64, &[u8])>) -> Result<(), Refused> {
        let bytes = encode(&next)?;
        match self.store.commit(entry, &bytes) {
            Ok(()) => {
                self.state = next;
                Ok(())
            }
            Err(StoreError::Full) => Err(Refused::Full),
            Err(StoreError::Conflict) => {
                self.poisoned = true;
                Err(Refused::Corrupt("retained entry conflict"))
            }
            Err(StoreError::Failed) => {
                self.poisoned = true;
                Err(Refused::Persist)
            }
        }
    }
    /// The retained full record at `tip` must exist, verify and equal `entry` exactly before
    /// any ack may rest on it. A fault in what is retained poisons the voter.
    fn confirm_retained(&mut self, entry: &LogEntry, tip: &Tip) -> Result<(), Refused> {
        let result = check_retained(
            &mut self.store,
            &self.state.set,
            &self.state.set_sha256,
            tip.index,
            Some(tip),
        );
        match result {
            Ok(stored) if stored == *entry => Ok(()),
            Ok(_) => {
                self.poisoned = true;
                Err(Refused::Corrupt("retained entry differs"))
            }
            Err(Refused::Persist) => Err(Refused::Persist),
            Err(other) => {
                self.poisoned = true;
                Err(other)
            }
        }
    }
    /// Retrieval of a retained entry by index (above the base, up to the tip), re-verified on
    /// every read. A retained fault poisons the voter.
    pub(crate) fn entry(&mut self, index: u64) -> Result<LogEntry, Refused> {
        self.live()?;
        if index <= self.state.base.index || index > self.state.tip.index {
            return Err(Refused::Missing);
        }
        let known = self.state.recent.iter().find(|t| t.index == index).cloned();
        match check_retained(
            &mut self.store,
            &self.state.set,
            &self.state.set_sha256,
            index,
            known.as_ref(),
        ) {
            Ok(entry) => Ok(entry),
            Err(Refused::Persist) => Err(Refused::Persist),
            Err(other) => {
                self.poisoned = true;
                Err(other)
            }
        }
    }
    fn live(&self) -> Result<(), Refused> {
        if self.poisoned {
            Err(Refused::Poisoned)
        } else {
            Ok(())
        }
    }
    fn sign(&self, message: Vec<u8>) -> String {
        self.signer.sign(&message)
    }
    fn is_member(&self, identity: &HostIdentity) -> bool {
        self.state.set.body.is_member(identity)
    }
    fn bound(&self, server_id: &str, set_sha256: &str, epoch: u64) -> bool {
        server_id == self.state.set.body.pin.server_id
            && set_sha256 == self.state.set_sha256
            && epoch == self.state.set.body.epoch
    }
    /// A member's signature over `signed`; a non-member is never trusted.
    fn member_signed(&self, signed: &impl Signed) -> bool {
        self.is_member(signed.signer()) && signed_by(signed)
    }

    fn ack_for(&self, term: u64, index: u64, digest: &str) -> Ack {
        let body = AckBody {
            schema: wire::SCHEMA,
            server_id: self.state.set.body.pin.server_id.clone(),
            set_sha256: self.state.set_sha256.clone(),
            epoch: self.state.set.body.epoch,
            term,
            index,
            digest: digest.to_owned(),
            voter: self.state.identity.clone(),
        };
        let signature = self.sign(body.message());
        Ack { body, signature }
    }
    fn nack(&self, reason: NackReason) -> Nack {
        let body = NackBody {
            schema: wire::SCHEMA,
            server_id: self.state.set.body.pin.server_id.clone(),
            set_sha256: self.state.set_sha256.clone(),
            epoch: self.state.set.body.epoch,
            voter: self.state.identity.clone(),
            reason,
            promised_term: self.state.promised_term,
            tip: self.state.tip.clone(),
        };
        let signature = self.sign(body.message());
        Nack { body, signature }
    }
    fn vote_for(&self, voted: &Voted) -> Vote {
        let body = VoteBody {
            schema: wire::SCHEMA,
            server_id: self.state.set.body.pin.server_id.clone(),
            set_sha256: self.state.set_sha256.clone(),
            epoch: self.state.set.body.epoch,
            term: voted.term,
            candidate: voted.candidate.clone(),
            candidate_tip: voted.tip.clone(),
            accept_sha256: voted.accept_sha256.clone(),
            voter: self.state.identity.clone(),
        };
        let signature = self.sign(body.message());
        Vote { body, signature }
    }

    /// Appends `entry` (whose position is `tip`): the exact full signed record and the new
    /// metadata are stored in one operation.
    fn advance(&mut self, tip: Tip, entry: &LogEntry) -> Result<(), Refused> {
        let record = encode_entry(entry)?;
        let mut next = self.state.clone();
        next.tip = tip.clone();
        next.recent.push(tip);
        if next.recent.len() > RECENT {
            next.recent.remove(0);
        }
        self.persist(next, Some((entry.body.index, record.as_slice())))
    }

    /// The leader of the promised term appends a new entry to its own chain. The complete
    /// signed entry is retained together with the new metadata before the entry and the
    /// leader's own acknowledgement are returned.
    pub(crate) fn propose(&mut self, payload: Payload) -> Result<(LogEntry, Ack), Refused> {
        self.live()?;
        payload.validate().map_err(invalid)?;
        if self.state.leader != self.state.identity {
            return Err(Refused::NotLeader);
        }
        let index = self
            .state
            .tip
            .index
            .checked_add(1)
            .ok_or(Refused::Overflow)?;
        let body = EntryBody {
            schema: wire::SCHEMA,
            server_id: self.state.set.body.pin.server_id.clone(),
            set_sha256: self.state.set_sha256.clone(),
            epoch: self.state.set.body.epoch,
            term: self.state.promised_term,
            index,
            previous: self.state.tip.digest.clone(),
            leader: self.state.identity.clone(),
            payload,
        };
        body.validate().map_err(invalid)?;
        let digest = entry_digest(&body);
        let mut entry = LogEntry {
            body,
            signature: String::new(),
        };
        entry.signature = self.sign(entry.signing_message());
        self.advance(
            Tip {
                term: entry.body.term,
                index,
                digest: digest.clone(),
            },
            &entry,
        )?;
        let ack = self.ack_for(entry.body.term, index, &digest);
        Ok((entry, ack))
    }

    /// Receives an entry from the term's leader. A signed nack reports a protocol refusal
    /// to the sender; `Err` is a malformed, unsigned or mis-bound input, or a storage fault.
    pub(crate) fn append(&mut self, entry: &LogEntry) -> Result<AppendAnswer, Refused> {
        self.live()?;
        entry.validate().map_err(invalid)?;
        let b = &entry.body;
        if !self.bound(&b.server_id, &b.set_sha256, b.epoch) {
            return Err(Refused::Mismatch("entry binding"));
        }
        if !self.is_member(&b.leader) || !signed_by(entry) {
            return Err(Refused::BadSignature("entry"));
        }
        // A higher promise fences everything older, replays included.
        if b.term < self.state.promised_term {
            return Ok(AppendAnswer::Nack(self.nack(NackReason::Fenced)));
        }
        if b.term > self.state.promised_term {
            return Ok(AppendAnswer::Nack(self.nack(NackReason::UnknownTerm)));
        }
        if b.leader != self.state.leader {
            return Err(Refused::Mismatch("not the leader of this term"));
        }
        let digest = entry_digest(b);
        if b.index <= self.state.tip.index {
            let known = self
                .state
                .recent
                .iter()
                .find(|t| t.index == b.index)
                .cloned();
            return match known {
                Some(known) if known.term == b.term && known.digest == digest => {
                    // The ack rests on the retained full entry, re-read now.
                    self.confirm_retained(entry, &known)?;
                    Ok(AppendAnswer::Ack(self.ack_for(b.term, b.index, &digest)))
                }
                Some(_) => Ok(AppendAnswer::Nack(self.nack(NackReason::Divergent))),
                None => Ok(AppendAnswer::Nack(self.nack(NackReason::Stale))),
            };
        }
        if b.index != self.state.tip.index + 1 {
            return Ok(AppendAnswer::Nack(self.nack(NackReason::Gap)));
        }
        if b.previous != self.state.tip.digest {
            return Ok(AppendAnswer::Nack(self.nack(NackReason::Divergent)));
        }
        self.advance(
            Tip {
                term: b.term,
                index: b.index,
                digest: digest.clone(),
            },
            entry,
        )?;
        Ok(AppendAnswer::Ack(self.ack_for(b.term, b.index, &digest)))
    }

    /// Considers a candidate's signed request. One vote per term; the same candidate may
    /// retry and gets the identical vote only if the request carries the tip and consent
    /// persisted with it (`ChangedRequest` otherwise). The promise is stored before the vote
    /// is returned.
    pub(crate) fn grant_vote(&mut self, request: &VoteRequest) -> Result<VoteAnswer, Refused> {
        self.live()?;
        request.validate().map_err(invalid)?;
        let b = &request.body;
        if !self.bound(&b.server_id, &b.set_sha256, b.epoch) {
            return Err(Refused::Mismatch("vote request binding"));
        }
        if !self.state.set.body.is_candidate(&b.candidate) {
            return Err(Refused::NotCandidate);
        }
        check_accept(&self.state.set, &self.state.set_sha256, &b.accept)?;
        if b.accept.body.candidate != b.candidate {
            return Err(Refused::Mismatch("consent candidate"));
        }
        if !signed_by(request) {
            return Err(Refused::BadSignature("vote request"));
        }
        if b.term < self.state.promised_term {
            return Ok(VoteAnswer::Refused(self.nack(NackReason::Fenced)));
        }
        let consent = accept_digest(&b.accept);
        if b.term == self.state.promised_term {
            return Ok(if self.state.voted.candidate != b.candidate {
                VoteAnswer::Refused(self.nack(NackReason::AlreadyVoted))
            } else if self.state.voted.tip != b.tip || self.state.voted.accept_sha256 != consent {
                VoteAnswer::Refused(self.nack(NackReason::ChangedRequest))
            } else {
                VoteAnswer::Granted(self.vote_for(&self.state.voted))
            });
        }
        // A later term. The candidate must not be behind this host: not an older
        // (term, index), not a lower index (S1 never truncates), and never the same position
        // with another digest.
        let mine = &self.state.tip;
        let theirs = &b.tip;
        if (theirs.term, theirs.index) < (mine.term, mine.index) || theirs.index < mine.index {
            return Ok(VoteAnswer::Refused(self.nack(NackReason::StaleTip)));
        }
        if theirs.index == mine.index && theirs.digest != mine.digest {
            return Ok(VoteAnswer::Refused(self.nack(NackReason::Divergent)));
        }
        let voted = Voted {
            term: b.term,
            candidate: b.candidate.clone(),
            tip: theirs.clone(),
            accept_sha256: consent,
        };
        let mut next = self.state.clone();
        next.promised_term = b.term;
        next.leader = b.candidate.clone();
        next.voted = voted.clone();
        self.persist(next, None)?;
        Ok(VoteAnswer::Granted(self.vote_for(&voted)))
    }

    /// This host, as a consenting candidate, asks for the next term and votes for itself.
    /// The request is signed and the self-vote stored before either is returned. A repeated
    /// call opens a still later term, so keep and resend the returned request on retries.
    pub(crate) fn start_election(&mut self) -> Result<(VoteRequest, Vote), Refused> {
        self.live()?;
        let accept = self.state.accept.clone().ok_or(Refused::NotCandidate)?;
        let term = self
            .state
            .promised_term
            .checked_add(1)
            .ok_or(Refused::Overflow)?;
        let body = VoteRequestBody {
            schema: wire::SCHEMA,
            server_id: self.state.set.body.pin.server_id.clone(),
            set_sha256: self.state.set_sha256.clone(),
            epoch: self.state.set.body.epoch,
            term,
            candidate: self.state.identity.clone(),
            tip: self.state.tip.clone(),
            accept,
        };
        let signature = self.sign(body.message());
        let request = VoteRequest { body, signature };
        match self.grant_vote(&request)? {
            VoteAnswer::Granted(vote) => Ok((request, vote)),
            VoteAnswer::Refused(_) => Err(Refused::Mismatch("own candidacy refused")),
        }
    }

    /// Authenticate historical commit evidence against this configured cohort and
    /// the exact retained entry. Unlike commit_gate this does not grant a new
    /// write, count an unsigned local vote, or reject an older committed term.
    pub(crate) fn committed_certificate(&mut self, entry: &LogEntry, acks: &[Ack]) -> bool {
        let b = &entry.body;
        if self.poisoned
            || acks.len() < wire::QUORUM
            || acks.len() > 3
            || entry.validate().is_err()
            || !self.bound(&b.server_id, &b.set_sha256, b.epoch)
            || !self.is_member(&b.leader)
            || !signed_by(entry)
        {
            return false;
        }
        let tip = Tip {
            term: b.term,
            index: b.index,
            digest: entry_digest(b),
        };
        if self.confirm_retained(entry, &tip).is_err() {
            return false;
        }
        let mut voters = Vec::new();
        for ack in acks {
            let a = &ack.body;
            if ack.validate().is_err()
                || !self.bound(&a.server_id, &a.set_sha256, a.epoch)
                || a.term != tip.term
                || a.index != tip.index
                || a.digest != tip.digest
                || !self.member_signed(ack)
                || voters.contains(&a.voter.host_id)
            {
                return false;
            }
            voters.push(a.voter.host_id.clone());
        }
        voters.len() >= wire::QUORUM && voters.contains(&b.leader.host_id)
    }

    /// Whether `entry` may be committed locally. Requires (1) the entry of the promised term
    /// whose exact full signed record this host retains (re-read and compared now), and (2) a
    /// correctly signed ack of that exact entry from a different current member. Evidence is only ever additive: a verified nack
    /// from a member that promised a later term withholds the commit; nothing else counts.
    pub(crate) fn commit_gate(
        &mut self,
        entry: &LogEntry,
        acks: &[Ack],
        nacks: &[Nack],
    ) -> Decision {
        if self.poisoned {
            return Decision::Rollback(Why::Poisoned);
        }
        if acks.len() + nacks.len() > MAX_EVIDENCE {
            return Decision::Rollback(Why::Oversized);
        }
        let b = &entry.body;
        if entry.validate().is_err()
            || !self.bound(&b.server_id, &b.set_sha256, b.epoch)
            || !self.is_member(&b.leader)
            || !signed_by(entry)
        {
            return Decision::Rollback(Why::Mismatch);
        }
        if b.term != self.state.promised_term {
            return Decision::Rollback(Why::Fenced);
        }
        if b.leader != self.state.leader {
            return Decision::Rollback(Why::Mismatch);
        }
        let digest = entry_digest(b);
        let known = self
            .state
            .recent
            .iter()
            .find(|t| t.term == b.term && t.index == b.index && t.digest == digest)
            .cloned();
        match known {
            Some(tip) if self.confirm_retained(entry, &tip).is_ok() => {}
            _ => return Decision::Rollback(Why::NotPersisted),
        }
        for nack in nacks {
            let n = &nack.body;
            if nack.validate().is_ok()
                && self.bound(&n.server_id, &n.set_sha256, n.epoch)
                && self.member_signed(nack)
                && n.promised_term > b.term
            {
                return Decision::Rollback(Why::Fenced);
            }
        }
        let mut others: Vec<&str> = Vec::new();
        for ack in acks {
            let a = &ack.body;
            let exact = ack.validate().is_ok()
                && self.bound(&a.server_id, &a.set_sha256, a.epoch)
                && a.term == b.term
                && a.index == b.index
                && a.digest == digest
                && a.voter != self.state.identity
                && self.member_signed(ack);
            if exact && !others.contains(&a.voter.host_id.as_str()) {
                others.push(&a.voter.host_id);
            }
        }
        // The local persisted append is one member; the quorum needs the rest.
        if others.len() + 1 >= wire::QUORUM {
            Decision::Commit
        } else {
            Decision::Rollback(Why::Insufficient)
        }
    }
}

#[cfg(test)]
mod tests;
