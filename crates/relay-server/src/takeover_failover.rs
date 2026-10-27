//! Continuing a community after an unexpected outage of its active host.
//!
//! Who may take over: only an approved co-host whose full copy is a member of the
//! leader-signed authority set and who signed its own consent. Nobody approves
//! anything at the moment of the outage; the rules below decide.
//!
//! When: the host has been silent for `HOST_SILENCE` (plus a per-machine offset
//! so two co-hosts rarely ask at the same moment), and this copy has applied
//! every change it holds ("Data copy up to date").
//!
//! Authority: a majority of the approved full copies (2 of 3) must sign a vote
//! for this candidate in a new term. Each copy votes once per term, refuses a
//! candidate whose copy is behind its own, refuses while it still hears from the
//! host, and a copy that won never votes again. Two copies can therefore never
//! both win, and the old host, whose writes need a second copy's acknowledgement,
//! is fenced by the higher term as soon as it comes back.
//!
//! Promotion only prepares the new host's database from the verified copy, with
//! login secrets removed, next to the signed mandate. Starting a server from it
//! and moving members' clients to it are separate, later steps.
// Setup (arming) between real machines is not wired yet; only tests attach.
#![allow(dead_code)]
use crate::{
    takeover_runtime::Replica,
    takeover_voter::VoteAnswer,
    takeover_wal_vfs::Refusal,
};
use relay_protocol::{
    hosting::{TakeoverState, TakeoverStatus, TAKEOVER_SILENCE_SECONDS},
    takeover::{AuthoritySet, NackReason, VoteRequest},
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) const HOST_SILENCE: Duration = Duration::from_secs(TAKEOVER_SILENCE_SECONDS);
/// Folder (inside the co-host's data folder) holding the prepared database and mandate.
pub(crate) const PROMOTED: &str = "takeover-promoted";
const MANDATE: &str = "mandate.json";
const DATABASE: &str = "relay.sqlite3";

/// Asks every other member that serves votes. `None` is a member that did not
/// answer in time; it is never counted.
pub(crate) trait VotePeers: Send {
    fn ask(&mut self, request: &VoteRequest) -> Vec<Option<VoteAnswer>>;
}

/// The signed evidence that a majority chose this machine (shared with the app).
pub(crate) use relay_protocol::takeover::Mandate;

/// The route a mandate authorizes, checked with this server's own crypto.
pub(crate) fn mandate_route(mandate: &Mandate) -> Option<relay_protocol::handover::RoutePin> {
    use sha2::{Digest, Sha256};
    mandate.route(
        |bytes| format!("{:x}", Sha256::digest(bytes)),
        |identity, signature, message| {
            crate::handover_target::verify(
                &relay_protocol::handover::HostProof {
                    identity: identity.clone(),
                    signature: signature.to_owned(),
                },
                message,
            )
        },
    )
}

/// Turns this co-host's prepared takeover into a host data folder that an
/// ordinary `relay-server --data <host_data>` serves: the stripped database
/// bound to the new term, the candidate's own key, and the mandate in the
/// authority journal. Safe to repeat after an interruption; never overwrites
/// another community.
pub(crate) fn install(
    standby_data: &Path,
    host_data: &Path,
) -> Result<relay_protocol::handover::RoutePin, Box<dyn std::error::Error>> {
    let mandate = saved_mandate(standby_data).ok_or("No takeover was prepared on this machine")?;
    let pin = mandate_route(&mandate).ok_or("The takeover mandate does not verify")?;
    let key = crate::handover_target::TargetKey::open_bound(standby_data, &pin.host)?;
    crate::handover_target::private_directory(host_data)?;
    if host_data.join("host-authority.json").try_exists()? {
        return Err("This folder already belongs to a host".into());
    }
    key.copy_for_source(host_data)?;
    let database = host_data.join(DATABASE);
    let source_host_id = if database.try_exists()? {
        // An earlier attempt placed the database; finish with its own marker.
        let db = rusqlite::Connection::open(&database)?;
        if crate::community::server_id(&db)? != pin.server_id || !crate::host_copy_secrets::is_stripped(&db)? {
            return Err("This folder holds another database".into());
        }
        crate::handover::database_marker(&db)?.ok_or("The placed database has no host marker")?
    } else {
        let pending = host_data.join("relay.sqlite3.takeover-pending");
        let _ = std::fs::remove_file(&pending);
        std::fs::copy(standby_data.join(PROMOTED).join(DATABASE), &pending)?;
        let db = rusqlite::Connection::open(&pending)?;
        if crate::community::server_id(&db)? != pin.server_id || !crate::host_copy_secrets::is_stripped(&db)? {
            drop(db);
            let _ = std::fs::remove_file(&pending);
            return Err("The prepared database does not match the mandate".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        crate::handover::mark_database(&db, &id, pin.term)?;
        drop(db);
        std::fs::rename(&pending, &database)?;
        id
    };
    crate::handover::install_takeover_journal(host_data, &source_host_id, &mandate, &pin)?;
    Ok(pin)
}

/// Extra silence before this machine asks, 0..15 s from its host id, so two
/// co-hosts do not keep splitting one term between them.
pub(crate) fn stagger(host_id: &str) -> Duration {
    let sum = host_id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b.into()));
    Duration::from_secs(u64::from(sum % 16))
}

/// What the copy itself can prove right now.
pub(crate) fn copy_state(replica: &Replica) -> TakeoverState {
    if replica.needs_repair() {
        TakeoverState::CopyNeedsRepair
    } else if replica.applied() != replica.tip() {
        // Changes are retained but not yet confirmed and applied.
        TakeoverState::CopyOutOfDate
    } else if replica.has_won() {
        TakeoverState::TookOver
    } else {
        TakeoverState::CopyUpToDate
    }
}

fn status(state: TakeoverState, replica: &Replica, votes: u32) -> TakeoverStatus {
    TakeoverStatus {
        state,
        term: replica.term(),
        votes,
    }
}

/// Answers another candidate's signed request on behalf of this copy.
pub(crate) fn answer_vote(
    replica: &mut Replica,
    request: &VoteRequest,
    now: Instant,
) -> Result<VoteAnswer, Refusal> {
    if replica.silent_for(now) < HOST_SILENCE {
        return Err(Refusal("the active host is still reachable from this copy"));
    }
    replica.grant_vote(request, now)
}

/// One automatic attempt. Returns the status to show and, only on a verified
/// majority, the mandate.
pub(crate) fn attempt(
    replica: &mut Replica,
    set: &AuthoritySet,
    peers: &mut dyn VotePeers,
    extra_silence: Duration,
    now: Instant,
) -> (TakeoverStatus, Option<Mandate>) {
    let state = copy_state(replica);
    if state != TakeoverState::CopyUpToDate {
        return (status(state, replica, 0), None);
    }
    if replica.silent_for(now) < HOST_SILENCE + extra_silence {
        return (status(state, replica, 0), None);
    }
    let Ok((request, own)) = replica.start_election() else {
        return (status(TakeoverState::CopyNeedsRepair, replica, 0), None);
    };
    let mut votes = vec![own];
    let mut behind = false;
    for answer in peers.ask(&request).into_iter().flatten() {
        match answer {
            VoteAnswer::Granted(vote) => votes.push(vote),
            VoteAnswer::Refused(nack) => {
                behind |= matches!(
                    nack.body.reason,
                    NackReason::StaleTip | NackReason::Divergent
                )
            }
        }
    }
    if replica.majority_for(&request, &votes) {
        replica.mark_won();
        let count = votes.len() as u32;
        let mandate = Mandate {
            set: set.clone(),
            request,
            votes,
        };
        return (status(TakeoverState::TookOver, replica, count), Some(mandate));
    }
    if behind {
        // Another approved copy holds changes this one lacks: it must not lead.
        return (status(TakeoverState::CopyOutOfDate, replica, 1), None);
    }
    (status(TakeoverState::NoMajority, replica, 1), None)
}

/// Prepares the new host's database: a verified copy of the image at the
/// elected tip, with every login secret removed, plus the mandate. Never opens
/// or moves the owned standby image itself.
pub(crate) fn promote(
    replica: &mut Replica,
    mandate: &Mandate,
    data: &Path,
) -> Result<PathBuf, Refusal> {
    if !replica.majority_for(&mandate.request, &mandate.votes)
        || mandate.request.body.tip != *replica.applied()
    {
        return Err(Refusal("mandate does not match this copy"));
    }
    let tip = replica.applied().clone();
    replica
        .verify(&tip)
        .map_err(|_| Refusal("copy failed its final check"))?;
    let folder = data.join(PROMOTED);
    if folder.exists() {
        return Err(Refusal("a prepared takeover already exists"));
    }
    crate::handover_target::private_directory(&folder)
        .map_err(|_| Refusal("takeover folder is unsafe"))?;
    let database = folder.join(DATABASE);
    std::fs::copy(replica.image(), &database).map_err(|_| Refusal("copy could not be read"))?;
    let db = rusqlite::Connection::open(&database).map_err(|_| Refusal("copy could not open"))?;
    let ok: String = db
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|_| Refusal("copy check failed"))?;
    if ok != "ok" {
        return Err(Refusal("copy check failed"));
    }
    crate::host_copy_secrets::strip(&db).map_err(|_| Refusal("secrets could not be removed"))?;
    drop(db);
    let raw = serde_json::to_vec(mandate).map_err(|_| Refusal("mandate could not be saved"))?;
    crate::handover_transfer::publish_new(&folder.join(MANDATE), &raw)
        .map_err(|_| Refusal("mandate could not be saved"))?;
    Ok(database)
}

/// A mandate this machine saved earlier. Its presence means the copy won and
/// must never vote for anyone else in this set, even after a restart.
pub(crate) fn saved_mandate(data: &Path) -> Option<Mandate> {
    let raw = crate::handover_transfer::read_file(&data.join(PROMOTED).join(MANDATE), 64 * 1024)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

/// A co-host's running takeover candidate: its copy, the other members that
/// answer votes, and what the app shows.
pub(crate) struct Failover {
    replica: std::sync::Arc<std::sync::Mutex<Replica>>,
    set: AuthoritySet,
    peers: std::sync::Mutex<Box<dyn VotePeers>>,
    status: std::sync::Mutex<TakeoverStatus>,
    stagger: Duration,
}

impl crate::AppState {
    /// Called only by explicit co-host setup, after the copy was seeded from a
    /// verified complete snapshot and the set and consent were signed. The
    /// returned replica is what the private takeover transport serves.
    pub(crate) fn attach_takeover_replica(
        &self,
        mut replica: Replica,
        set: AuthoritySet,
        peers: Box<dyn VotePeers>,
    ) -> Result<std::sync::Arc<std::sync::Mutex<Replica>>, Refusal> {
        let mut slot = crate::lock(&self.0.failover);
        if slot.is_some() || crate::lock(&self.0.takeover).is_some() {
            return Err(Refusal("this machine already has a takeover role"));
        }
        if let Some(mandate) = saved_mandate(&self.0.data) {
            if !replica.majority_for(&mandate.request, &mandate.votes) {
                return Err(Refusal("saved takeover mandate does not match this copy"));
            }
            replica.mark_won();
        }
        let shown = status(copy_state(&replica), &replica, 0);
        let stagger = stagger(&replica.identity().host_id);
        let replica = std::sync::Arc::new(std::sync::Mutex::new(replica));
        *slot = Some(std::sync::Arc::new(Failover {
            replica: replica.clone(),
            set,
            peers: std::sync::Mutex::new(peers),
            status: std::sync::Mutex::new(shown),
            stagger,
        }));
        Ok(replica)
    }

    /// Set by takeover setup on the protected host: where members find the
    /// approved co-hosts if this host disappears.
    pub(crate) fn announce_standby(&self, urls: Vec<String>) {
        *crate::lock(&self.0.standby_urls) = urls.into_iter().take(2).collect();
    }

    /// What this machine can say about continuing the community.
    pub(crate) fn takeover_status(&self) -> TakeoverStatus {
        let waiting = self.takeover_waiting();
        if let Some(source) = crate::lock(&self.0.takeover).as_ref() {
            return TakeoverStatus {
                state: if source.blocks_publication() {
                    TakeoverState::Fenced
                } else if waiting {
                    TakeoverState::Paused
                } else {
                    TakeoverState::ProtectedHost
                },
                term: 0,
                votes: 0,
            };
        }
        if self.0.takeover_failed.load(std::sync::atomic::Ordering::Acquire) {
            return TakeoverStatus {
                state: TakeoverState::Fenced,
                ..Default::default()
            };
        }
        crate::lock(&self.0.failover)
            .as_ref()
            .map(|f| crate::lock(&f.status).clone())
            .unwrap_or_default()
    }

    /// One automatic check from the 10-second maintenance loop. Nobody has to
    /// approve anything: the silence, copy and majority rules decide.
    pub(crate) async fn failover_tick(&self) {
        let Some(failover) = crate::lock(&self.0.failover).clone() else {
            return;
        };
        let data = self.0.data.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let mut replica = crate::lock(&failover.replica);
            let (mut shown, mandate) = {
                let mut peers = crate::lock(&failover.peers);
                attempt(
                    &mut replica,
                    &failover.set,
                    &mut **peers,
                    failover.stagger,
                    Instant::now(),
                )
            };
            if let Some(mandate) = mandate {
                if promote(&mut replica, &mandate, &data).is_err() {
                    shown.state = TakeoverState::CopyNeedsRepair;
                }
            }
            *crate::lock(&failover.status) = shown;
        })
        .await;
    }
}

#[cfg(test)]
mod tests;
