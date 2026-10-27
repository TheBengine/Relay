//! Source runtime used by explicit protected-host setup. Ordinary opens remain
//! refused after protection is armed, until protected recovery is explicitly prepared.
//! The setup owner must supply verified P0/cohort, peer transport and promotion.
#![allow(dead_code)]
use crate::{
    lock,
    takeover_commit::{self, Approved, Replicate},
    takeover_inflight::Inflight,
    takeover_signer::HostKey,
    takeover_store::TakeoverStore,
    takeover_voter::Voter,
    takeover_wal_vfs::{CommitSink, Config, LocalCommit, Refusal, WalTransaction, WalVfs},
    AppState,
};
use rusqlite::{Connection, OpenFlags};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const BOOT_GUARD: &str = "takeover-runtime-required.v1";

/// A negative boot guard, never an authority/lease or a recovery certificate.
/// Ordinary startup checks it before database opening or migrations. Unknown
/// metadata or inaccessible storage fails closed instead of silently losing quorum.
pub(crate) fn require_ordinary_boot(data: &Path) -> Result<(), Refusal> {
    match std::fs::symlink_metadata(data.join(BOOT_GUARD)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(Refusal(
            "protected community requires takeover recovery before startup",
        )),
    }
}
/// True when this data folder holds a protected community that can be reopened
/// through its replicating database (its takeover state and feed keys exist).
pub(crate) fn protected_boot(data: &Path) -> bool {
    data.join(BOOT_GUARD).is_file()
        && data.join("takeover").join("feed-keys.json").is_file()
}
fn retain_boot_guard(data: &Path) -> Result<(), Refusal> {
    use std::io::Write;
    crate::handover_target::private_directory(data)
        .map_err(|_| Refusal("protected storage is unsafe"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(data.join(BOOT_GUARD))
        .map_err(|_| Refusal("protected boot guard is unavailable"))?;
    file.write_all(b"Relay protected database: explicit recovery required before opening.\n")
        .and_then(|_| file.sync_all())
        .map_err(|_| Refusal("protected boot guard could not be retained"))?;
    #[cfg(unix)]
    {
        std::fs::File::open(data)
            .and_then(|d| d.sync_all())
            .map_err(|_| Refusal("protected boot directory could not be retained"))?;
    }
    Ok(())
}

pub(crate) type DiskVoter = Voter<TakeoverStore, HostKey>;
struct State {
    voter: DiskVoter,
    recovery: Inflight,
    peers: Box<dyn Replicate + Send>,
    approved: Option<Approved>,
}
struct Control {
    state: Mutex<State>,
    blocked: AtomicBool,
    wake: Box<dyn Fn() + Send + Sync>,
}
impl Control {
    fn pause(&self) {
        if !self.blocked.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }
}
impl CommitSink for Control {
    fn approve(&self, tx: &WalTransaction) -> Result<(), Refusal> {
        if self.blocked.load(Ordering::Acquire) {
            return Err(Refusal("protected host is paused"));
        }
        let mut state = lock(&self.state);
        if !state.peers.ready() {
            // Refused before anything is proposed: the host stays usable and
            // resumes as soon as a co-host is back.
            return Err(Refusal("no co-host is confirming changes"));
        }
        if state.approved.is_some() {
            self.pause();
            return Err(Refusal("local receipt is pending"));
        }
        let State {
            voter,
            recovery,
            peers,
            ..
        } = &mut *state;
        match takeover_commit::admit(
            tx,
            voter,
            recovery,
            &mut **peers,
            Instant::now() + Duration::from_secs(20),
        ) {
            Ok(approved) => {
                state.approved = Some(approved);
                Ok(())
            }
            Err(_) => {
                self.pause();
                Err(Refusal("protected write requires recovery"))
            }
        }
    }
    fn local_committed(&self, receipt: &LocalCommit) -> Result<(), Refusal> {
        let mut state = lock(&self.state);
        let result = state
            .approved
            .take()
            .ok_or(Refusal("quorum ticket is missing"))
            .and_then(|ticket| {
                ticket
                    .acknowledge(&mut state.recovery, receipt)
                    .map_err(|_| Refusal("local receipt requires recovery"))
            });
        if result.is_err() {
            self.pause();
        }
        result
    }
}

pub(crate) struct Source {
    _vfs: WalVfs,
    control: Arc<Control>,
}
impl Source {
    fn open(
        path: &Path,
        voter: DiskVoter,
        recovery: Inflight,
        peers: Box<dyn Replicate + Send>,
        wake: Box<dyn Fn() + Send + Sync>,
    ) -> Result<(Arc<Self>, Connection), Refusal> {
        if voter.is_poisoned() || recovery.recovery_required() {
            return Err(Refusal("protected host requires recovery before opening"));
        }
        let control = Arc::new(Control {
            state: Mutex::new(State {
                voter,
                recovery,
                peers,
                approved: None,
            }),
            blocked: AtomicBool::new(false),
            wake,
        });
        let vfs = WalVfs::register(path, Config::DEFAULT, control.clone())?;
        let conn = vfs.open_connection(OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|_| Refusal("database timeout setup failed"))?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(|_| Refusal("database foreign key setup failed"))?;
        Ok((Arc::new(Self { _vfs: vfs, control }), conn))
    }
    pub(crate) fn applied_tip(&self) -> Result<relay_protocol::takeover::Tip, Refusal> {
        let state = lock(&self.control.state);
        if self.blocks_publication()
            || state.recovery.recovery_required()
            || state.approved.is_some()
        {
            return Err(Refusal("protected state is not fully applied"));
        }
        Ok(state.recovery.last_applied().clone())
    }
    pub(crate) fn blocks_publication(&self) -> bool {
        // This is a negative failure fence, not election/lease authority. Reads
        // here never wait for a slow peer or take the controller's state mutex.
        self.control.blocked.load(Ordering::Acquire) || self._vfs.is_poisoned()
    }
}

impl AppState {
    fn wake_readers(&self) -> Box<dyn Fn() + Send + Sync> {
        let weak = Arc::downgrade(&self.0);
        Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                let _ = inner.events.send(crate::Notice {
                    event: relay_protocol::Event {
                        kind: "server_paused".into(),
                        channel_id: None,
                        user_id: None,
                        recipient_id: None,
                        typing: None,
                        expires_at_ms: None,
                    },
                    audience: None,
                });
            }
        })
    }

    /// Reopens a protected community at startup, before anything is served:
    /// the same swap as arming, without writing a new boot guard.
    pub(crate) fn resume_takeover_source(
        &self,
        voter: DiskVoter,
        recovery: Inflight,
        peers: Box<dyn Replicate + Send>,
    ) -> Result<(), Refusal> {
        if voter.identity() != &self.0.handover.route_pin().host {
            return Err(Refusal("protected state belongs to another host"));
        }
        let mut db = lock(&self.0.db);
        let placeholder =
            Connection::open_in_memory().map_err(|_| Refusal("protected reopen failed"))?;
        drop(std::mem::replace(&mut *db, placeholder));
        let path = self.0.data.join("relay.sqlite3");
        let (source, conn) = Source::open(&path, voter, recovery, peers, self.wake_readers())?;
        *db = conn;
        *lock(&self.0.takeover) = Some(source);
        Ok(())
    }

    pub(crate) fn takeover_blocked(&self) -> bool {
        if self.0.takeover_failed.load(Ordering::Acquire) {
            return true;
        }
        lock(&self.0.takeover)
            .as_ref()
            .is_some_and(|source| source.blocks_publication())
    }
    pub(crate) fn writes_allowed(&self) -> bool {
        self.0.handover.writes_allowed()
            && !self.takeover_blocked()
            && !self.arming()
            && !self.takeover_waiting()
    }
    /// Protected, and no co-host is present to confirm a change right now.
    pub(crate) fn takeover_waiting(&self) -> bool {
        lock(&self.0.takeover).is_some()
            && lock(&self.0.feed)
                .as_ref()
                .is_some_and(|feed| !feed.present(std::time::Instant::now()))
    }
    /// Called only by explicit setup after P0/cohort admission. All ordinary
    /// requests are drained before replacing the connection. No HTTP/UI route
    /// activates this until boot recovery, transport and promotion are wired.
    pub(crate) async fn attach_takeover_source(
        &self,
        voter: DiskVoter,
        recovery: Inflight,
        peers: Box<dyn Replicate + Send>,
    ) -> Result<(), Refusal> {
        let _maintenance = self.0.maintenance.write().await;
        if self.0.config.people
            || !self.0.handover.writes_allowed()
            || self.takeover_blocked()
            || lock(&self.0.takeover).is_some()
            || voter.identity() != &self.0.handover.route_pin().host
        {
            return Err(Refusal("source setup does not match this community host"));
        }
        let mut db = lock(&self.0.db);
        if !db.is_autocommit() {
            return Err(Refusal("source transaction is still active"));
        }
        let checkpoint: (i64, i64, i64) = db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(|_| Refusal("source checkpoint failed"))?;
        if checkpoint.0 != 0 {
            return Err(Refusal("source checkpoint is busy"));
        }
        let placeholder =
            Connection::open_in_memory().map_err(|_| Refusal("source handoff failed"))?;
        // From here onward a restart must never open an ordinary writable DB.
        // Retain even a partially written marker; only explicit recovery owns it.
        if let Err(error) = retain_boot_guard(&self.0.data) {
            self.0.takeover_failed.store(true, Ordering::Release);
            self.0.shutdown.send_replace(true);
            return Err(error);
        }
        drop(std::mem::replace(&mut *db, placeholder));
        let weak = Arc::downgrade(&self.0);
        let wake = Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                // Wake existing event readers without using the paused notify
                // path. They observe the failure fence before sending data.
                let _ = inner.events.send(crate::Notice {
                    event: relay_protocol::Event {
                        kind: "server_paused".into(),
                        channel_id: None,
                        user_id: None,
                        recipient_id: None,
                        typing: None,
                        expires_at_ms: None,
                    },
                    audience: None,
                });
            }
        });
        let path = self.0.data.join("relay.sqlite3");
        match Source::open(&path, voter, recovery, peers, wake) {
            Ok((source, conn)) => {
                *db = conn;
                *lock(&self.0.takeover) = Some(source);
                Ok(())
            }
            Err(error) => {
                // No ordinary fallback after the durable boot guard. Existing
                // requests see shutdown; recovery retains all keys and databases.
                self.0.takeover_failed.store(true, Ordering::Release);
                self.0.shutdown.send_replace(true);
                Err(error)
            }
        }
    }
}

/// Owns one offline candidate: retained signed log plus recoverable page image.
/// An append only retains bytes. A separate signed quorum certificate is needed
/// before any of them are applied; neither operation makes this host writable.
pub(crate) struct Replica {
    voter: DiskVoter,
    standby: crate::takeover_standby::Standby,
    /// Last accepted append or commit from the host, or last vote this copy granted.
    /// Starts at construction, so a restart never looks like a long outage.
    heard: Instant,
    /// This copy won a majority. It never votes for anyone again in this set.
    won: bool,
}
impl Replica {
    pub(crate) fn new(
        voter: DiskVoter,
        standby: crate::takeover_standby::Standby,
        set: &relay_protocol::takeover::AuthoritySet,
    ) -> Result<Self, Refusal> {
        use sha2::{Digest, Sha256};
        if voter.is_poisoned()
            || standby.is_poisoned()
            || voter.set_sha256() != format!("{:x}", Sha256::digest(set.body.message()))
            || !standby.bound_to(set, voter.identity())
            || standby.applied().index > voter.tip().index
        {
            return Err(Refusal("replica context differs"));
        }
        Ok(Self {
            voter,
            standby,
            heard: Instant::now(),
            won: false,
        })
    }
    /// How long nothing has been heard from the host (or from a candidate this
    /// copy voted for).
    pub(crate) fn silent_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.heard)
    }
    #[cfg(test)]
    pub(crate) fn pretend_silent_since(&mut self, when: Instant) {
        self.heard = when;
    }
    pub(crate) fn identity(&self) -> &relay_protocol::handover::HostIdentity {
        self.voter.identity()
    }
    pub(crate) fn tip(&self) -> &relay_protocol::takeover::Tip {
        self.voter.tip()
    }
    pub(crate) fn term(&self) -> u64 {
        self.voter.promised_term()
    }
    pub(crate) fn needs_repair(&self) -> bool {
        self.voter.is_poisoned()
            || self.standby.is_poisoned()
            || self.standby.recovery_needed().is_some()
    }
    pub(crate) fn has_won(&self) -> bool {
        self.won
    }
    pub(crate) fn mark_won(&mut self) {
        self.won = true;
    }
    pub(crate) fn image(&self) -> &Path {
        self.standby.image_path()
    }
    pub(crate) fn start_election(
        &mut self,
    ) -> Result<
        (
            relay_protocol::takeover::VoteRequest,
            relay_protocol::takeover::Vote,
        ),
        Refusal,
    > {
        self.voter
            .start_election()
            .map_err(|_| Refusal("this copy may not ask for a takeover"))
    }
    /// A vote for another candidate. A copy that already won never votes again,
    /// and a granted vote restarts the silence timer, so a fresh winner gets
    /// time to take over before anyone else asks.
    pub(crate) fn grant_vote(
        &mut self,
        request: &relay_protocol::takeover::VoteRequest,
        now: Instant,
    ) -> Result<crate::takeover_voter::VoteAnswer, Refusal> {
        if self.won {
            return Err(Refusal("this copy is already the chosen host"));
        }
        let answer = self
            .voter
            .grant_vote(request)
            .map_err(|_| Refusal("vote request refused"))?;
        if matches!(answer, crate::takeover_voter::VoteAnswer::Granted(_)) {
            self.heard = now;
        }
        Ok(answer)
    }
    pub(crate) fn majority_for(
        &self,
        request: &relay_protocol::takeover::VoteRequest,
        votes: &[relay_protocol::takeover::Vote],
    ) -> bool {
        self.voter.majority_for(request, votes)
    }
    pub(crate) fn append(
        &mut self,
        entry: &relay_protocol::takeover::LogEntry,
    ) -> Result<crate::takeover_voter::AppendAnswer, Refusal> {
        let answer = self
            .voter
            .append(entry)
            .map_err(|_| Refusal("replica append refused"))?;
        if matches!(answer, crate::takeover_voter::AppendAnswer::Ack(_)) {
            self.heard = Instant::now();
        }
        Ok(answer)
    }
    pub(crate) fn committed(
        &mut self,
        notice: &takeover_commit::CommitNotice,
    ) -> Result<(), Refusal> {
        if !notice.verify(&mut self.voter) {
            return Err(Refusal("replica commit proof refused"));
        }
        if self.standby.is_poisoned() {
            return Err(Refusal("replica storage requires reopen"));
        }
        self.heard = Instant::now();
        if self.standby.recovery_needed().is_none() && self.standby.applied() == &notice.end {
            return Ok(()); // Exact authenticated retransmission, no repeated page writes.
        }
        if self.standby.applied() != &notice.parent {
            return Err(Refusal("replica needs the preceding committed transaction"));
        }
        let tx = takeover_commit::retained_wal(
            &mut self.voter,
            &notice.parent,
            &notice.end,
            Config::DEFAULT.max_staged_bytes,
        )
        .map_err(|_| Refusal("replica transaction is incomplete"))?;
        if self.standby.recovery_needed().is_some() {
            self.standby.recover(&notice.parent, &notice.end, &tx)
        } else {
            self.standby.apply(&notice.parent, &notice.end, &tx)
        }
        .map_err(|_| Refusal("replica apply requires recovery"))
    }
    pub(crate) fn applied(&self) -> &relay_protocol::takeover::Tip {
        self.standby.applied()
    }
    pub(crate) fn verify(
        &mut self,
        expected: &relay_protocol::takeover::Tip,
    ) -> Result<crate::takeover_standby::Verified, Refusal> {
        self.standby
            .verify(expected)
            .map_err(|_| Refusal("replica is not ready"))
    }
}
