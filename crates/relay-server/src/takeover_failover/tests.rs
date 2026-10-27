//! Three disposable hosts on one machine: the active host Anna and the approved
//! co-hosts Bo and Clara. No network; votes travel through `Direct`.
use super::*;
use crate::{
    takeover_runtime::DiskVoter,
    takeover_store::{Quota, TakeoverStore},
    takeover_voter::{AppendAnswer, Decision, HostSigner, Voter, Why},
    AppState,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::takeover::{
    self as wire, AcceptBody, CandidateAccept, Payload, SetBody, Tip,
};
use sha2::{Digest, Sha256};

struct Cohort {
    root: tempfile::TempDir,
    app: AppState,
    set: AuthoritySet,
    leader: DiskVoter,
    bo: Replica,
    clara: Replica,
}

const QUOTA: Quota = Quota {
    max_entries: 64,
    max_entry_bytes: 4 * 1024 * 1024,
};

fn cohort() -> Cohort {
    let root = tempfile::tempdir().unwrap();
    let app = AppState::open(&root.path().join("anna"), "Failover", 0).unwrap();
    app.initialize_admin("Anna", "river lantern sandal orchard 39")
        .unwrap();
    let pin = app.0.handover.route_pin();
    let leader_key = app.0.handover.takeover_signer();
    let mut hosts = ["bo", "clara"].map(|name| {
        let data = root.path().join(name);
        let key = crate::handover_target::TargetKey::open(&data).unwrap();
        (data, key)
    });
    hosts.sort_by_key(|(_, key)| key.identity().host_id);
    let body = SetBody {
        schema: wire::SCHEMA,
        pin: pin.clone(),
        epoch: 1,
        candidates: [hosts[0].1.identity(), hosts[1].1.identity()],
    };
    let set = AuthoritySet {
        signature: leader_key.sign(&body.message()),
        body,
    };
    let set_sha = format!("{:x}", Sha256::digest(set.body.message()));
    let base = Tip {
        term: pin.term,
        index: 0,
        digest: format!("{:x}", Sha256::digest(b"failover fixture snapshot")),
    };
    let leader = Voter::initialize(
        TakeoverStore::create_new(&root.path().join("anna-log"), QUOTA).unwrap(),
        leader_key,
        set.clone(),
        None,
        base.clone(),
    )
    .unwrap();
    let mut replicas = hosts.iter().map(|(data, key)| {
        let recipient =
            URL_SAFE_NO_PAD.encode(key.encryption_keys().unwrap().public_key().as_slice());
        let accept_body = AcceptBody {
            schema: wire::SCHEMA,
            server_id: pin.server_id.clone(),
            set_sha256: set_sha.clone(),
            epoch: 1,
            candidate: key.identity(),
            recipient_key: recipient,
            full_data: true,
            takeover: true,
        };
        let accept = CandidateAccept {
            signature: key.takeover_signer().sign(&accept_body.message()),
            body: accept_body,
        };
        let voter = Voter::initialize(
            TakeoverStore::create_new(&data.join("log"), QUOTA).unwrap(),
            key.takeover_signer(),
            set.clone(),
            Some(accept),
            base.clone(),
        )
        .unwrap();
        let folder = data.join("standby");
        crate::handover_target::private_directory(&folder).unwrap();
        app.backup_to(&folder.join(crate::takeover_standby::IMAGE_NAME))
            .unwrap();
        let standby = crate::takeover_standby::Standby::create_new(
            &folder,
            &set,
            voter.identity(),
            &base,
            crate::takeover_standby::Limits::DEFAULT,
        )
        .unwrap();
        Replica::new(voter, standby, &set).unwrap()
    });
    let bo = replicas.next().unwrap();
    let clara = replicas.next().unwrap();
    drop(replicas);
    Cohort {
        root,
        app,
        set,
        leader,
        bo,
        clara,
    }
}

/// Votes delivered in-process to the other co-hosts; `None` is an unreachable host.
struct Direct<'a> {
    peers: Vec<Option<&'a mut Replica>>,
}
impl VotePeers for Direct<'_> {
    fn ask(&mut self, request: &VoteRequest) -> Vec<Option<VoteAnswer>> {
        self.peers
            .iter_mut()
            .map(|peer| {
                peer.as_mut()
                    .and_then(|r| answer_vote(r, request, Instant::now()).ok())
            })
            .collect()
    }
}

fn silent(replica: &mut Replica) {
    replica.pretend_silent_since(Instant::now().checked_sub(Duration::from_secs(120)).unwrap());
}

fn take_over(me: &mut Replica, set: &AuthoritySet, other: Option<&mut Replica>) -> (TakeoverStatus, Option<Mandate>) {
    // Anna is down: her vote never arrives.
    let mut peers = Direct {
        peers: vec![other, None],
    };
    attempt(me, set, &mut peers, Duration::ZERO, Instant::now())
}

#[test]
fn two_co_hosts_can_never_both_take_over_and_the_old_host_is_fenced() {
    let mut c = cohort();
    silent(&mut c.bo);
    silent(&mut c.clara);
    assert_eq!(copy_state(&c.bo), TakeoverState::CopyUpToDate);
    assert_eq!(
        TakeoverStatus {
            state: copy_state(&c.bo),
            ..Default::default()
        }
        .text(),
        "Data copy up to date"
    );

    // Both ask in the same term: each already voted for itself, so nobody wins.
    let (clara_request, _) = c.clara.start_election().unwrap();
    let (status, mandate) = take_over(&mut c.bo, &c.set, Some(&mut c.clara));
    assert_eq!(status.state, TakeoverState::NoMajority);
    assert!(mandate.is_none());
    assert!(status.is_problem());
    assert!(!matches!(
        answer_vote(&mut c.bo, &clara_request, Instant::now()),
        Ok(VoteAnswer::Granted(_))
    ));

    // The next term Bo asks first and wins with Clara's vote: 2 of 3.
    silent(&mut c.bo);
    silent(&mut c.clara);
    let (status, mandate) = take_over(&mut c.bo, &c.set, Some(&mut c.clara));
    assert_eq!(status.state, TakeoverState::TookOver);
    assert_eq!(status.votes, 2);
    let mandate = mandate.unwrap();
    assert!(c.bo.majority_for(&mandate.request, &mandate.votes));
    // A single vote, or the same vote twice, is never a majority.
    assert!(!c.bo.majority_for(&mandate.request, &mandate.votes[..1]));
    let doubled = vec![mandate.votes[0].clone(), mandate.votes[0].clone()];
    assert!(!c.bo.majority_for(&mandate.request, &doubled));

    // Clara voted, so she waits for Bo instead of asking at once.
    let (status, _) = take_over(&mut c.clara, &c.set, Some(&mut c.bo));
    assert_eq!(status.state, TakeoverState::CopyUpToDate);
    // Even after the wait she cannot win: Bo never votes again, Anna is down.
    silent(&mut c.clara);
    let (status, mandate) = take_over(&mut c.clara, &c.set, Some(&mut c.bo));
    assert_eq!(status.state, TakeoverState::NoMajority);
    assert!(mandate.is_none());

    // Anna returns with the old term. Neither co-host acknowledges her change,
    // so she can never commit it: she is fenced, not a second active host.
    let (entry, _) = c.leader.propose(Payload::new("AQID").unwrap()).unwrap();
    let mut nacks = Vec::new();
    for replica in [&mut c.bo, &mut c.clara] {
        match replica.append(&entry).unwrap() {
            AppendAnswer::Nack(nack) => nacks.push(nack),
            AppendAnswer::Ack(_) => panic!("a fenced host's change was acknowledged"),
        }
    }
    assert_eq!(
        c.leader.commit_gate(&entry, &[], &nacks),
        Decision::Rollback(Why::Fenced)
    );
}

#[test]
fn a_co_host_alone_or_while_the_host_is_reachable_cannot_take_over() {
    let mut c = cohort();
    // The host was heard recently: no attempt, and no vote for anyone.
    let (status, mandate) = take_over(&mut c.bo, &c.set, Some(&mut c.clara));
    assert_eq!(status.state, TakeoverState::CopyUpToDate);
    assert!(mandate.is_none());
    silent(&mut c.bo);
    let (status, mandate) = take_over(&mut c.bo, &c.set, Some(&mut c.clara));
    assert_eq!(status.state, TakeoverState::NoMajority, "Clara still hears Anna");
    assert!(mandate.is_none());
    // Clara and Anna both unreachable: one copy of three is never enough.
    silent(&mut c.bo);
    let (status, mandate) = take_over(&mut c.bo, &c.set, None);
    assert_eq!(status.state, TakeoverState::NoMajority);
    assert_eq!(status.votes, 1);
    assert_eq!(
        status.text(),
        "Can't take over: only 1 of 2 approved copies agreed"
    );
    assert!(mandate.is_none());
}

#[test]
fn a_copy_that_is_out_of_date_cannot_take_over() {
    let mut c = cohort();
    // Clara acknowledged one more change than Bo before Anna disappeared.
    let (entry, _) = c.leader.propose(Payload::new("AQID").unwrap()).unwrap();
    assert!(matches!(c.clara.append(&entry).unwrap(), AppendAnswer::Ack(_)));
    // Clara holds it but has not applied it yet.
    assert_eq!(copy_state(&c.clara), TakeoverState::CopyOutOfDate);
    silent(&mut c.clara);
    let (status, mandate) = take_over(&mut c.clara, &c.set, Some(&mut c.bo));
    assert_eq!(status.text(), "Can't take over: copy is out of date");
    assert!(mandate.is_none());
    // Bo has applied everything he holds, but Clara refuses him: he lacks a change.
    silent(&mut c.bo);
    silent(&mut c.clara);
    let (status, mandate) = take_over(&mut c.bo, &c.set, Some(&mut c.clara));
    assert_eq!(status.state, TakeoverState::CopyOutOfDate);
    assert_eq!(status.text(), "Can't take over: copy is out of date");
    assert!(mandate.is_none());
}

#[test]
fn the_winner_prepares_a_database_without_login_secrets_and_keeps_its_mandate() {
    let mut c = cohort();
    silent(&mut c.bo);
    silent(&mut c.clara);
    let (_, mandate) = take_over(&mut c.bo, &c.set, Some(&mut c.clara));
    let mandate = mandate.unwrap();
    let data = c.root.path().join("bo");
    let database = promote(&mut c.bo, &mandate, &data).unwrap();
    let db = rusqlite::Connection::open(&database).unwrap();
    assert!(crate::host_copy_secrets::is_stripped(&db).unwrap());
    let anna: String = db
        .query_row("SELECT username FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(anna, "Anna");
    let server: String = db
        .query_row("SELECT value FROM metadata WHERE key='server_id'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(server, c.app.server_id().unwrap());
    drop(db);
    assert_eq!(saved_mandate(&data).as_ref(), Some(&mandate));
    // A second promotion never overwrites the first.
    assert!(promote(&mut c.bo, &mandate, &data).is_err());
    // Another co-host's evidence is refused.
    let mut forged = mandate.clone();
    forged.votes.truncate(1);
    assert!(promote(&mut c.clara, &forged, &c.root.path().join("clara")).is_err());
}

#[test]
fn votes_cross_the_private_transport_only_with_the_scoped_access_key() {
    use crate::takeover_transport::{router, HttpVotePeers, Target};
    use std::sync::{Arc, Mutex};
    let c = cohort();
    let Cohort {
        root: _root,
        set,
        mut bo,
        mut clara,
        ..
    } = c;
    silent(&mut bo);
    silent(&mut clara);
    let access = "c3".repeat(32); // Own isolated test endpoint only.
    let clara = Arc::new(Mutex::new(clara));
    let service = router(clara.clone(), access.clone()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, service)
                .with_graceful_shutdown(async move {
                    let _ = stopped.wait_for(|stop| *stop).await;
                })
                .await
                .unwrap();
        });
    });
    // A wrong key is refused before the request is read: no vote, no majority.
    let mut wrong = HttpVotePeers::new(vec![Target::owned_forward(address, "d4".repeat(32)).unwrap()]);
    let (status, mandate) = attempt(&mut bo, &set, &mut wrong, Duration::ZERO, Instant::now());
    assert_eq!(status.state, TakeoverState::NoMajority);
    assert!(mandate.is_none());
    // With Clara's scoped key her signed vote arrives; Anna's endpoint is gone.
    silent(&mut bo);
    let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut right = HttpVotePeers::new(vec![
        Target::owned_forward(address, access).unwrap(),
        Target::owned_forward(gone, "e5".repeat(32)).unwrap(),
    ]);
    let (status, mandate) = attempt(&mut bo, &set, &mut right, Duration::ZERO, Instant::now());
    assert_eq!(status.state, TakeoverState::TookOver);
    let mandate = mandate.unwrap();
    assert!(bo.majority_for(&mandate.request, &mandate.votes));
    stop.send_replace(true);
    server.join().unwrap();
}

/// Clara's copy, reached in-process by the running app's automatic check.
struct Shared(std::sync::Arc<std::sync::Mutex<Replica>>);
impl VotePeers for Shared {
    fn ask(&mut self, request: &VoteRequest) -> Vec<Option<VoteAnswer>> {
        let mut clara = self.0.lock().unwrap();
        vec![answer_vote(&mut clara, request, Instant::now()).ok(), None]
    }
}

#[tokio::test]
async fn the_running_app_shows_the_copy_state_and_takes_over_without_anyone_approving() {
    let Cohort {
        root,
        set,
        mut bo,
        mut clara,
        ..
    } = cohort();
    let data = root.path().join("bo-server");
    let app = AppState::open(&data, "Bo", 0).unwrap();
    assert_eq!(app.takeover_status().state, TakeoverState::NotSetUp);
    silent(&mut bo);
    silent(&mut clara);
    let clara = std::sync::Arc::new(std::sync::Mutex::new(clara));
    app.attach_takeover_replica(bo, set.clone(), Box::new(Shared(clara)))
        .unwrap();
    assert_eq!(app.takeover_status().text(), "Data copy up to date");
    app.failover_tick().await;
    let status = app.takeover_status();
    assert_eq!(status.state, TakeoverState::TookOver);
    assert_eq!(status.votes, 2);
    assert!(status.text().starts_with("Took over: 2 of 3 approved copies"));
    let prepared = data.join(PROMOTED);
    assert!(prepared.join(DATABASE).is_file());
    assert!(saved_mandate(&data).is_some());
}

#[tokio::test]
async fn the_chosen_co_host_starts_as_the_community_host_and_proves_it_with_the_mandate() {
    use crate::tests::request;
    use serde_json::json;
    let Cohort {
        root,
        app: anna,
        set,
        mut bo,
        mut clara,
        ..
    } = cohort();
    silent(&mut bo);
    silent(&mut clara);
    let (_, mandate) = take_over(&mut bo, &set, Some(&mut clara));
    let mandate = mandate.unwrap();
    // Hosts are sorted by their random keys: find the folder holding Bo's key.
    let standby = ["bo", "clara"]
        .map(|name| root.path().join(name))
        .into_iter()
        .find(|dir| crate::handover_target::TargetKey::open_bound(dir, bo.identity()).is_ok())
        .unwrap();
    promote(&mut bo, &mandate, &standby).unwrap();
    let host = root.path().join("bo-host");
    let pin = install(&standby, &host).unwrap();
    assert_eq!(pin.server_id, anna.server_id().unwrap());
    assert_eq!(&pin.host, bo.identity());
    assert!(pin.term > anna.0.handover.route_pin().term);
    assert!(install(&standby, &host).is_err(), "never installs over a host");

    let new_host = AppState::open(&host, "Bo continues", 0).unwrap();
    assert_eq!(new_host.0.handover.route_pin(), pin);
    assert!(new_host.writes_allowed());
    let users: i64 = crate::lock(&new_host.0.db)
        .query_row("SELECT COUNT(*) FROM users WHERE username='Anna'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(users, 1, "same members");
    let app = crate::router(new_host.clone());
    let challenge = URL_SAFE_NO_PAD.encode([7u8; 32]);
    let (code, body) = request(
        &app,
        "POST",
        "/api/hosting/takeover/route",
        None,
        json!({ "challenge": challenge }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{body}");
    let proof: relay_protocol::handover::RouteProof =
        serde_json::from_value(body["proof"].clone()).unwrap();
    assert_eq!(proof.state.pin, pin);
    assert_eq!(proof.state.challenge, challenge);
    assert!(crate::handover_target::verify(
        &relay_protocol::handover::HostProof {
            identity: pin.host.clone(),
            signature: proof.signature.clone(),
        },
        &relay_protocol::handover::route_message(&proof.state),
    ));
    let carried: Mandate = serde_json::from_value(body["mandate"].clone()).unwrap();
    assert_eq!(mandate_route(&carried), Some(pin.clone()));
    // The old host has no mandate to show.
    let (code, _) = request(
        &crate::router(anna.clone()),
        "POST",
        "/api/hosting/takeover/route",
        None,
        json!({ "challenge": challenge }),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::NOT_FOUND);
    // One vote short, or another candidate's name, never verifies.
    let mut short = mandate.clone();
    short.votes.truncate(1);
    assert!(mandate_route(&short).is_none());
    let mut renamed = mandate.clone();
    renamed.request.body.candidate = clara.identity().clone();
    assert!(mandate_route(&renamed).is_none());
    // A restart keeps the new authority.
    drop(app);
    drop(new_host);
    assert_eq!(AppState::open(&host, "again", 0).unwrap().0.handover.route_pin(), pin);
}
