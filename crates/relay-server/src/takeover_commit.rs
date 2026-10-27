//! Joins captured WAL pages, the signed retained log and the durable recovery
//! checkpoint. Explicitly constructed by a future owner; NOT a live server gate.
//! Peer transport and P0 authority are supplied, not discovered or enabled here.
#![allow(dead_code)]

use crate::{
    takeover_inflight::Inflight,
    takeover_transaction::{Parts, RetainedTransaction, PART_BYTES},
    takeover_voter::{AppendAnswer, Decision, HostSigner, LogStore, Voter},
    takeover_wal_vfs::{AssembledTx, LocalCommit, TxAssembler, WalGeneration, WalTransaction},
};
use relay_protocol::takeover::{Ack, LogEntry, Nack, Tip};
use std::{
    io::{self, Write},
    time::Instant,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    NotReady,
    Invalid,
    Deadline,
    NoQuorum,
    Storage,
}

/// At most two other members. Implementations MUST bound their own requests by
/// the supplied deadline; this synchronous interface cannot cancel a stuck call.
/// The source verifies signatures, exact entry binding and higher-term refusals.
pub(crate) trait Replicate {
    fn append(&mut self, entry: &LogEntry, deadline: Instant) -> [Option<AppendAnswer>; 2];
    /// Best-effort delivery after the durable source certificate. A failed
    /// delivery cannot turn a committed quorum into a rollback. Catch-up uses
    /// the retained log and the same certificate again after reconnect.
    fn committed(&mut self, notice: &CommitNotice, deadline: Instant);
    /// Whether a co-host can confirm a change right now. A write is refused
    /// before it starts when none can, so no half-confirmed change is left.
    fn ready(&self) -> bool {
        true
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitNotice {
    pub(crate) parent: Tip,
    pub(crate) end: Tip,
    pub(crate) seal: LogEntry,
    pub(crate) acks: Vec<Ack>,
}
impl CommitNotice {
    pub(crate) fn verify<L: LogStore, S: HostSigner>(&self, voter: &mut Voter<L, S>) -> bool {
        use sha2::{Digest, Sha256};
        self.parent.validate().is_ok()
            && self.end.validate().is_ok()
            && self.parent.index < self.end.index
            && self.parent.term <= self.end.term
            && self.seal.body.index == self.end.index
            && self.seal.body.term == self.end.term
            && format!("{:x}", Sha256::digest(self.seal.body.digest_message())) == self.end.digest
            && voter.committed_certificate(&self.seal, &self.acks)
    }
}

/// In-memory correlation for the later local SQLite commit receipt. Approval
/// alone does not imply local application or permission to publish a response.
/// The recovery checkpoint remains pending until that distinct receipt arrives.
pub(crate) struct Approved {
    pub(crate) sequence: u64,
    pub(crate) generation: WalGeneration,
    pub(crate) db_pages: u32,
    pub(crate) first_frame: u32,
    pub(crate) frames: u32,
    pub(crate) end: Tip,
}
impl std::fmt::Debug for Approved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovedCapture")
            .field("sequence", &self.sequence)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

impl Approved {
    /// Consume only the exact post-commit receipt from the owned SQLite adapter.
    /// This records local application; it does not grant public writer authority.
    pub(crate) fn acknowledge<J: LogStore>(
        self,
        recovery: &mut Inflight<J>,
        receipt: &LocalCommit,
    ) -> Result<(), Error> {
        if self.first_frame == 0 || self.frames == 0 {
            return Err(Error::Invalid);
        }
        let real_end = u64::from(self.first_frame) + u64::from(self.frames) - 1;
        let frame_size = 24 + u64::from(self.generation.page_size);
        if receipt.sequence != self.sequence
            || receipt.generation != self.generation
            || receipt.first_frame != self.first_frame
            || receipt.real_frames != self.frames
            || receipt.db_pages != self.db_pages
            || u64::from(receipt.wal_frames) < real_end
            || receipt.wal_end != 32 + u64::from(receipt.wal_frames) * frame_size
            || recovery.pending_end() != Some(&self.end)
        {
            return Err(Error::Invalid);
        }
        recovery
            .acknowledge_local_apply(&self.end)
            .map_err(|_| Error::Storage)
    }
}

/// Persist begin -> every complete part -> seal -> exact quorum certificate.
/// The seal is recorded locally BEFORE it is delivered to peers. Any error after
/// begin leaves recovery pending; a missing reply is never an automatic abort.
pub(crate) fn admit<L: LogStore, S: HostSigner, J: LogStore>(
    tx: &WalTransaction,
    voter: &mut Voter<L, S>,
    recovery: &mut Inflight<J>,
    peers: &mut (impl Replicate + ?Sized),
    deadline: Instant,
) -> Result<Approved, Error> {
    if recovery.recovery_required() || voter.is_poisoned() {
        return Err(Error::NotReady);
    }
    check_deadline(deadline)?;
    let chunks = tx.chunks(PART_BYTES - 4).map_err(|_| Error::Invalid)?;
    // WAL chunk format v1: 64 header bytes and (u32 page number + page) records.
    // Each opaque part adds a u32 length, preserving the WAL chunk boundary. The
    // Parts seal independently verifies this exact declared byte count at finish.
    let bytes = u64::from(tx.frame_count())
        .checked_mul(4 + u64::from(tx.page_size()))
        .and_then(|n| n.checked_add(u64::from(chunks.chunk_count()) * (64 + 4)))
        .ok_or(Error::Invalid)?;
    let mut parts = Parts::new(bytes).map_err(|_| Error::Invalid)?;
    let parent = voter.tip().clone();
    recovery.begin(voter, bytes).map_err(|_| Error::NotReady)?;
    for chunk in chunks {
        check_deadline(deadline)?;
        if chunk.len() > PART_BYTES - 4 {
            return Err(Error::Invalid);
        }
        let mut body = Vec::with_capacity(4 + chunk.len());
        body.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
        body.extend_from_slice(&chunk);
        let payload = parts.push(&body).map_err(|_| Error::Invalid)?;
        let (entry, own) = voter.propose(payload).map_err(|_| Error::Storage)?;
        quorum(voter, peers, &entry, own, deadline)?;
    }
    check_deadline(deadline)?;
    let (seal, own) = voter
        .propose(parts.seal().map_err(|_| Error::Invalid)?)
        .map_err(|_| Error::Storage)?;
    recovery.seal(voter).map_err(|_| Error::Storage)?;
    let (acks, nacks) = quorum(voter, peers, &seal, own, deadline)?;
    recovery
        .approve(voter, &acks, &nacks)
        .map_err(|_| Error::NoQuorum)?;
    peers.committed(
        &CommitNotice {
            parent,
            end: voter.tip().clone(),
            seal,
            acks,
        },
        deadline,
    );
    Ok(Approved {
        sequence: tx.sequence(),
        generation: tx.generation().clone(),
        db_pages: tx.db_pages(),
        first_frame: tx.first_frame(),
        frames: tx.frame_count(),
        end: voter.tip().clone(),
    })
}
fn check_deadline(deadline: Instant) -> Result<(), Error> {
    if Instant::now() >= deadline {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
fn quorum<L: LogStore, S: HostSigner>(
    voter: &mut Voter<L, S>,
    peers: &mut (impl Replicate + ?Sized),
    entry: &LogEntry,
    own: Ack,
    deadline: Instant,
) -> Result<(Vec<Ack>, Vec<Nack>), Error> {
    check_deadline(deadline)?;
    let answers = peers.append(entry, deadline);
    check_deadline(deadline)?;
    let nacks: Vec<_> = answers
        .iter()
        .filter_map(|a| match a {
            Some(AppendAnswer::Nack(n)) => Some(n.clone()),
            _ => None,
        })
        .collect();
    for answer in answers {
        if let Some(AppendAnswer::Ack(peer)) = answer {
            let acks = vec![own.clone(), peer];
            // Select a proven distinct member, retaining only the valid pair.
            // A signed higher promise from either peer still fences this commit.
            if voter.commit_gate(entry, &acks, &nacks) == Decision::Commit {
                return Ok((acks, nacks));
            }
        }
    }
    Err(Error::NoQuorum)
}

/// Validate the entire retained seal/chain before assembling any WAL data; then
/// re-read it into private memory. Nothing is written to an actual database.
/// The caller still needs committed-prefix authority and an atomic offline apply.
pub(crate) fn retained_wal<L: LogStore, S: HostSigner>(
    voter: &mut Voter<L, S>,
    parent: &Tip,
    end: &Tip,
    max_bytes: usize,
) -> Result<AssembledTx, Error> {
    let retained = RetainedTransaction::inspect(voter, parent, end).map_err(|_| Error::Invalid)?;
    let mut staged = WalStaging {
        assembler: TxAssembler::new(max_bytes),
        complete: None,
    };
    retained
        .copy_to_staging(voter, &mut staged)
        .map_err(|_| Error::Invalid)?;
    staged.complete.ok_or(Error::Invalid)
}
struct WalStaging {
    assembler: TxAssembler,
    complete: Option<AssembledTx>,
}
impl Write for WalStaging {
    fn write(&mut self, part: &[u8]) -> io::Result<usize> {
        // copy_to_staging calls write_all with ONE bounded opaque part at a
        // time; this writer consumes it in full or refuses without applying it.
        let invalid = || io::Error::from(io::ErrorKind::InvalidData);
        if self.complete.is_some() || !(5..=PART_BYTES).contains(&part.len()) {
            return Err(invalid());
        }
        let declared = u32::from_be_bytes(part[..4].try_into().map_err(|_| invalid())?) as usize;
        if declared != part.len() - 4 {
            return Err(invalid());
        }
        self.complete = self.assembler.add(&part[4..]).map_err(|_| invalid())?;
        Ok(part.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
