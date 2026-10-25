//! Durable source-side recovery checkpoint, separate from the community DB and
//! the retained log. A pending operation survives every intermediate outcome;
//! reopening never clears it or infers that SQLite committed. The server must
//! recover it before serving. This is not yet installed on real server writes.
#![allow(dead_code)]

use crate::{
    handover_target::verify,
    takeover_store::{OpenError, Quota, TakeoverStore},
    takeover_transaction::RetainedTransaction,
    takeover_voter::{Decision, HostSigner, LogStore, Voter},
};
use relay_protocol::{
    handover::{HostIdentity, HostProof},
    takeover::{Ack, AuthoritySet, LogEntry, Nack, Signed, Tip},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

const LIMIT: usize = 256 * 1024;
const QUOTA: Quota = Quota {
    max_entries: 1,
    max_entry_bytes: 1,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Invalid,
    Open(OpenError),
    Store,
    Poisoned,
    NotApproved,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    set: AuthoritySet,
    set_sha256: String,
    host: HostIdentity,
}
fn signed(value: &impl Signed) -> bool {
    verify(
        &HostProof {
            identity: value.signer().clone(),
            signature: value.signature().into(),
        },
        &value.signing_message(),
    )
}
impl Context {
    fn new(set: AuthoritySet, host: HostIdentity) -> Result<Self, Error> {
        if set.validate().is_err() || !signed(&set) || !set.body.is_member(&host) {
            return Err(Error::Invalid);
        }
        let set_sha256 = format!("{:x}", Sha256::digest(set.body.message()));
        Ok(Self {
            set,
            set_sha256,
            host,
        })
    }
    fn same(&self, other: &Self) -> bool {
        self.set == other.set && self.set_sha256 == other.set_sha256 && self.host == other.host
    }
    fn voter<J: LogStore, S: HostSigner>(&self, voter: &Voter<J, S>) -> bool {
        voter.identity() == &self.host
            && voter.set_sha256() == self.set_sha256
            && !voter.is_poisoned()
    }
    fn certificate(&self, end: &Tip, entry: &LogEntry, acks: &[Ack]) -> bool {
        let body = &entry.body;
        if entry.validate().is_err()
            || !signed(entry)
            || body.leader != self.host
            || body.server_id != self.set.body.pin.server_id
            || body.set_sha256 != self.set_sha256
            || body.epoch != self.set.body.epoch
            || body.index != end.index
            || body.term != end.term
            || format!("{:x}", Sha256::digest(body.digest_message())) != end.digest
            || acks.len() > 3
        {
            return false;
        }
        let mut voters = Vec::new();
        for ack in acks {
            let a = &ack.body;
            if ack.validate().is_err()
                || !signed(ack)
                || !self.set.body.is_member(&a.voter)
                || a.server_id != body.server_id
                || a.set_sha256 != self.set_sha256
                || a.epoch != body.epoch
                || a.term != end.term
                || a.index != end.index
                || a.digest != end.digest
                || voters.contains(&a.voter.host_id)
            {
                return false;
            }
            voters.push(a.voter.host_id.clone());
        }
        voters.len() >= 2 && voters.contains(&self.host.host_id)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Work {
    parent: Tip,
    expected_bytes: u64,
    decision: Phase,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum Phase {
    Staging,
    Sealed {
        end: Tip,
    },
    Approved {
        end: Tip,
        entry: LogEntry,
        acks: Vec<Ack>,
    },
}
impl Work {
    fn end(&self) -> Option<&Tip> {
        match &self.decision {
            Phase::Staging => None,
            Phase::Sealed { end } | Phase::Approved { end, .. } => Some(end),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    context: Context,
    applied: Tip,
    pending: Option<Work>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    state: State,
    sha256: String,
}

pub(crate) struct Inflight<J: LogStore = TakeoverStore> {
    store: J,
    state: State,
    poisoned: bool,
}
impl<J: LogStore> std::fmt::Debug for Inflight<J> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InflightRecovery")
            .field("pending", &self.state.pending.is_some())
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}
impl Inflight<TakeoverStore> {
    /// Only for explicit initial arming after a verified P0 snapshot. Never use
    /// this as a fallback when `open_existing` refuses a missing/corrupt record.
    pub(crate) fn create_new(
        folder: &Path,
        set: AuthoritySet,
        host: HostIdentity,
        applied: Tip,
    ) -> Result<Self, Error> {
        let state = State {
            schema: 1,
            context: Context::new(set, host)?,
            applied,
            pending: None,
        };
        validate(&state)?;
        let mut store = TakeoverStore::create_new(folder, QUOTA).map_err(Error::Open)?;
        let bytes = encode(&state)?;
        store.commit(None, &bytes).map_err(|_| Error::Store)?;
        Ok(Self {
            store,
            state,
            poisoned: false,
        })
    }
    pub(crate) fn open_existing(
        folder: &Path,
        set: AuthoritySet,
        host: HostIdentity,
    ) -> Result<Self, Error> {
        let context = Context::new(set, host)?;
        let mut store = TakeoverStore::open_existing(folder, QUOTA).map_err(Error::Open)?;
        let raw = store
            .load_state()
            .map_err(|_| Error::Store)?
            .ok_or(Error::Invalid)?;
        if raw.len() > LIMIT {
            return Err(Error::Invalid);
        }
        let value: Stored = serde_json::from_slice(&raw).map_err(|_| Error::Invalid)?;
        let expected = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&value.state).map_err(|_| Error::Invalid)?)
        );
        if value.sha256 != expected || !context.same(&value.state.context) {
            return Err(Error::Invalid);
        }
        validate(&value.state)?;
        Ok(Self {
            store,
            state: value.state,
            poisoned: false,
        })
    }
}
impl<J: LogStore> Inflight<J> {
    pub(crate) fn recovery_required(&self) -> bool {
        self.poisoned || self.state.pending.is_some()
    }
    pub(crate) fn last_applied(&self) -> &Tip {
        &self.state.applied
    }
    pub(crate) fn pending_end(&self) -> Option<&Tip> {
        self.state.pending.as_ref().and_then(Work::end)
    }
    fn live(&self) -> Result<(), Error> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }
    fn save(&mut self, next: State) -> Result<(), Error> {
        self.live()?;
        validate(&next)?;
        let bytes = encode(&next)?;
        if self.store.commit(None, &bytes).is_err() {
            self.poisoned = true; // The durable outcome may be uncertain; never clear/retry blindly.
            return Err(Error::Store);
        }
        self.state = next;
        Ok(())
    }
    /// Durably record BEFORE proposing the first transaction part. No second
    /// transaction can start until the first is explicitly recovered/applied.
    pub(crate) fn begin<L: LogStore, S: HostSigner>(
        &mut self,
        voter: &Voter<L, S>,
        expected_bytes: u64,
    ) -> Result<(), Error> {
        self.live()?;
        if self.state.pending.is_some() || !self.state.context.voter(voter) {
            return Err(Error::Invalid);
        }
        let mut next = self.state.clone();
        next.pending = Some(Work {
            parent: voter.tip().clone(),
            expected_bytes,
            decision: Phase::Staging,
        });
        self.save(next)
    }
    /// After local seal persistence, BEFORE sending that seal to peers. Re-read
    /// the entire retained transaction; a prefix or another transaction refuses.
    pub(crate) fn seal<L: LogStore, S: HostSigner>(
        &mut self,
        voter: &mut Voter<L, S>,
    ) -> Result<(), Error> {
        self.live()?;
        let work = self.state.pending.as_ref().ok_or(Error::Invalid)?;
        if !matches!(work.decision, Phase::Staging) || !self.state.context.voter(voter) {
            return Err(Error::Invalid);
        }
        let end = voter.tip().clone();
        let tx =
            RetainedTransaction::inspect(voter, &work.parent, &end).map_err(|_| Error::Invalid)?;
        if tx.bytes() != work.expected_bytes {
            return Err(Error::Invalid);
        }
        let mut next = self.state.clone();
        next.pending.as_mut().ok_or(Error::Invalid)?.decision = Phase::Sealed { end };
        self.save(next)
    }
    /// Persist the actual exact quorum evidence BEFORE returning an approval to
    /// the WAL writer. Loss of a peer reply leaves Sealed, never an inferred abort.
    pub(crate) fn approve<L: LogStore, S: HostSigner>(
        &mut self,
        voter: &mut Voter<L, S>,
        acks: &[Ack],
        nacks: &[Nack],
    ) -> Result<(), Error> {
        self.live()?;
        let work = self.state.pending.as_ref().ok_or(Error::Invalid)?;
        let Phase::Sealed { end } = &work.decision else {
            return Err(Error::Invalid);
        };
        if !self.state.context.voter(voter) {
            return Err(Error::Invalid);
        }
        let entry = voter.entry(end.index).map_err(|_| Error::Invalid)?;
        if !self.state.context.certificate(end, &entry, acks)
            || voter.commit_gate(&entry, acks, nacks) != Decision::Commit
        {
            return Err(Error::NotApproved);
        }
        let tx =
            RetainedTransaction::inspect(voter, &work.parent, end).map_err(|_| Error::Invalid)?;
        if tx.bytes() != work.expected_bytes {
            return Err(Error::Invalid);
        }
        let mut next = self.state.clone();
        next.pending.as_mut().ok_or(Error::Invalid)?.decision = Phase::Approved {
            end: end.clone(),
            entry,
            acks: acks.to_vec(),
        };
        self.save(next)
    }
    /// Caller contract: ONLY after SQLite has returned successful local COMMIT
    /// (or a verified atomic recovery apply), BEFORE HTTP/event publication. The
    /// journal cannot itself prove database application and never performs it.
    /// Merely reopening or receiving quorum MUST NOT call this operation.
    pub(crate) fn acknowledge_local_apply(&mut self, end: &Tip) -> Result<(), Error> {
        self.live()?;
        let Some(Work {
            decision: Phase::Approved { end: approved, .. },
            ..
        }) = &self.state.pending
        else {
            return Err(Error::NotApproved);
        };
        if end != approved {
            return Err(Error::Invalid);
        }
        let mut next = self.state.clone();
        next.applied = end.clone();
        next.pending = None;
        self.save(next)
    }
}
fn encode(state: &State) -> Result<Vec<u8>, Error> {
    let sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(state).map_err(|_| Error::Invalid)?)
    );
    let bytes = serde_json::to_vec(&Stored {
        state: state.clone(),
        sha256,
    })
    .map_err(|_| Error::Invalid)?;
    if bytes.len() > LIMIT {
        return Err(Error::Invalid);
    }
    Ok(bytes)
}
fn validate(state: &State) -> Result<(), Error> {
    if state.schema != 1
        || state.applied.validate().is_err()
        || state.applied.term < state.context.set.body.pin.term
    {
        return Err(Error::Invalid);
    }
    let checked = Context::new(state.context.set.clone(), state.context.host.clone())?;
    if !checked.same(&state.context) {
        return Err(Error::Invalid);
    }
    if let Some(work) = &state.pending {
        if work.parent.validate().is_err()
            || work.parent.index < state.applied.index
            || work.parent.term < state.applied.term
            || (work.parent.index == state.applied.index && work.parent != state.applied)
            || work.expected_bytes == 0
            || work.expected_bytes > 64 * 1024 * 1024 * 1024
        {
            return Err(Error::Invalid);
        }
        if let Some(end) = work.end() {
            if end.validate().is_err()
                || end.index <= work.parent.index
                || end.term < work.parent.term
            {
                return Err(Error::Invalid);
            }
        }
        if let Phase::Approved { end, entry, acks } = &work.decision {
            if !state.context.certificate(end, entry, acks) {
                return Err(Error::Invalid);
            }
        }
    }
    Ok(())
}
