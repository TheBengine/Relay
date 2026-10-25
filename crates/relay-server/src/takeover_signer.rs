//! Reuse an already loaded host key for the takeover log. This adapter never
//! creates, reads, exports or replaces a key and never grants writer authority.
//! Arming, consent, snapshot admission and real-server fencing remain separate.
#![allow(dead_code)]
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer as _, SigningKey};
use relay_protocol::handover::HostIdentity;

pub(crate) struct HostKey {
    identity: HostIdentity,
    signing: SigningKey,
}
impl HostKey {
    pub(crate) fn from_loaded(signing: SigningKey) -> Self {
        Self {
            identity: crate::handover_target::identity(&signing.verifying_key()),
            signing,
        }
    }
}
impl std::fmt::Debug for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TakeoverHostKey")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}
impl crate::takeover_voter::HostSigner for HostKey {
    fn identity(&self) -> &HostIdentity {
        &self.identity
    }
    fn sign(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(message).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        takeover_inflight::{Error as InflightError, Inflight},
        takeover_store::{Quota, TakeoverStore},
        takeover_transaction::{Error as TransactionError, Parts, RetainedTransaction, PART_BYTES},
        takeover_voter::{AppendAnswer, Decision, HostSigner, VoteAnswer, Voter, Why},
        AppState,
    };
    use relay_protocol::takeover::{
        self as wire, AcceptBody, AuthoritySet, CandidateAccept, Payload, SetBody, Tip,
    };
    use sha2::{Digest, Sha256};

    #[test]
    fn existing_host_keys_and_disk_logs_preserve_a_quorum_and_fence_the_old_term_after_reopen() {
        // All identities, stores, recipient keys and snapshot-base bytes belong
        // to this disposable fixture. This is not a P0 transfer or GUI consent.
        let root = tempfile::tempdir().unwrap();
        let app = AppState::open(&root.path().join("source"), "Owned quorum", 0).unwrap();
        let pin = app.0.handover.route_pin();
        let leader_key = app.0.handover.takeover_signer();
        assert_eq!(leader_key.identity(), &pin.host);
        let mut targets = ["candidate-a", "candidate-b"].map(|name| {
            let data = root.path().join(name);
            let key = crate::handover_target::TargetKey::open(&data).unwrap();
            let recipient =
                URL_SAFE_NO_PAD.encode(key.encryption_keys().unwrap().public_key().as_slice());
            (data, key, recipient)
        });
        targets.sort_by_key(|(_, key, _)| key.identity().host_id);
        let body = SetBody {
            schema: wire::SCHEMA,
            pin: pin.clone(),
            epoch: 1,
            candidates: [targets[0].1.identity(), targets[1].1.identity()],
        };
        let set = AuthoritySet {
            signature: leader_key.sign(&body.message()),
            body,
        };
        let set_sha = format!("{:x}", Sha256::digest(set.body.message()));
        let accepts: Vec<_> = targets
            .iter()
            .map(|(_, key, recipient)| {
                let body = AcceptBody {
                    schema: wire::SCHEMA,
                    server_id: pin.server_id.clone(),
                    set_sha256: set_sha.clone(),
                    epoch: 1,
                    candidate: key.identity(),
                    recipient_key: recipient.clone(),
                    full_data: true,
                    takeover: true,
                };
                CandidateAccept {
                    signature: key.takeover_signer().sign(&body.message()),
                    body,
                }
            })
            .collect();
        let base = Tip {
            term: pin.term,
            index: 0,
            digest: format!("{:x}", Sha256::digest(b"owned snapshot-base fixture")),
        };
        let quota = Quota {
            max_entries: 128,
            max_entry_bytes: 16 * 1024 * 1024,
        };
        let source_store = root.path().join("source/log");
        let mut leader = Voter::initialize(
            TakeoverStore::create_new(&source_store, quota).unwrap(),
            leader_key,
            set.clone(),
            None,
            base.clone(),
        )
        .unwrap();
        let mut candidates: Vec<_> = targets
            .iter()
            .zip(&accepts)
            .map(|((data, key, _), accept)| {
                Voter::initialize(
                    TakeoverStore::create_new(&data.join("log"), quota).unwrap(),
                    key.takeover_signer(),
                    set.clone(),
                    Some(accept.clone()),
                    base.clone(),
                )
                .unwrap()
            })
            .collect();
        // A real transaction larger than one wire entry is retained in bounded
        // signed parts. Neither a truncated encoder nor a retained orphan prefix
        // may be presented as a complete transaction.
        let content: Vec<u8> = (0..1_100_000).map(|i| (i % 251) as u8).collect();
        let mut incomplete = Parts::new(content.len() as u64).unwrap();
        incomplete.push(&content[..100]).unwrap();
        assert_eq!(incomplete.seal(), Err(TransactionError::Invalid));
        let mut rejected = Parts::new(100).unwrap();
        assert_eq!(
            rejected.push(&content[..101]),
            Err(TransactionError::Invalid)
        );
        assert_eq!(
            rejected.push(&content[..100]),
            Err(TransactionError::Invalid)
        );
        assert_eq!(rejected.seal(), Err(TransactionError::Invalid));
        let recovery_folder = root.path().join("source/inflight");
        let mut recovery = Inflight::create_new(
            &recovery_folder,
            set.clone(),
            pin.host.clone(),
            base.clone(),
        )
        .unwrap();
        recovery.begin(&leader, content.len() as u64).unwrap();
        drop(recovery);
        let mut recovery =
            Inflight::open_existing(&recovery_folder, set.clone(), pin.host.clone()).unwrap();
        assert!(recovery.recovery_required());
        assert_eq!(recovery.last_applied(), &base);
        assert_eq!(recovery.pending_end(), None);
        let mut parts = Parts::new(content.len() as u64).unwrap();
        let mut first_entry = None;
        for part in content.chunks(PART_BYTES) {
            let (entry, own) = leader.propose(parts.push(part).unwrap()).unwrap();
            first_entry.get_or_insert_with(|| entry.clone());
            let mut acks = vec![own];
            for candidate in &mut candidates {
                let AppendAnswer::Ack(ack) = candidate.append(&entry).unwrap() else {
                    panic!("current candidate refused transaction part")
                };
                acks.push(ack);
            }
            assert_eq!(leader.commit_gate(&entry, &acks, &[]), Decision::Commit);
        }
        let orphan_end = leader.tip().clone();
        assert_eq!(
            RetainedTransaction::inspect(&mut candidates[0], &base, &orphan_end),
            Err(TransactionError::Invalid)
        );
        assert_eq!(recovery.seal(&mut leader), Err(InflightError::Invalid));
        let (seal, own) = leader.propose(parts.seal().unwrap()).unwrap();
        recovery.seal(&mut leader).unwrap(); // durable BEFORE the seal goes to peers
        assert_eq!(
            recovery.approve(&mut leader, &[own.clone()], &[]),
            Err(InflightError::NotApproved)
        );
        drop(recovery);
        let mut recovery =
            Inflight::open_existing(&recovery_folder, set.clone(), pin.host.clone()).unwrap();
        assert!(recovery.recovery_required()); // unknown peer outcome is never an inferred abort/apply
        assert_eq!(recovery.pending_end(), Some(leader.tip()));
        let mut acks = vec![own];
        for candidate in &mut candidates {
            let AppendAnswer::Ack(ack) = candidate.append(&seal).unwrap() else {
                panic!("current candidate refused transaction seal")
            };
            acks.push(ack);
        }
        assert_eq!(leader.commit_gate(&seal, &acks, &[]), Decision::Commit);
        let transaction_end = leader.tip().clone();
        let mut duplicate = acks.clone();
        duplicate[1] = duplicate[0].clone();
        assert_eq!(
            recovery.approve(&mut leader, &duplicate, &[]),
            Err(InflightError::NotApproved)
        );
        recovery.approve(&mut leader, &acks, &[]).unwrap();
        drop(recovery);
        let mut recovery =
            Inflight::open_existing(&recovery_folder, set.clone(), pin.host.clone()).unwrap();
        assert!(recovery.recovery_required()); // quorum does not prove local database application
        assert_eq!(recovery.last_applied(), &base);
        assert_eq!(recovery.begin(&leader, 100), Err(InflightError::Invalid));
        assert_eq!(
            recovery.acknowledge_local_apply(&base),
            Err(InflightError::Invalid)
        );
        let retained =
            RetainedTransaction::inspect(&mut candidates[0], &base, &transaction_end).unwrap();
        assert_eq!(retained.bytes(), content.len() as u64);
        let mut staging = Vec::new();
        retained
            .copy_to_staging(&mut candidates[0], &mut staging)
            .unwrap();
        assert_eq!(staging, content);
        // An explicit own opaque apply fixture exercises the caller receipt. This
        // is not a claim of real community SQL/publication integration.
        let applied_file = root.path().join("source/applied-fixture.bin");
        let mut applied = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&applied_file)
            .unwrap();
        std::io::Write::write_all(&mut applied, &staging).unwrap();
        applied.sync_all().unwrap();
        drop(applied);
        recovery.acknowledge_local_apply(&transaction_end).unwrap();
        drop(recovery);
        let recovery =
            Inflight::open_existing(&recovery_folder, set.clone(), pin.host.clone()).unwrap();
        assert!(!recovery.recovery_required());
        assert_eq!(recovery.last_applied(), &transaction_end);
        drop(recovery);
        assert!(matches!(
            Inflight::open_existing(&recovery_folder, set.clone(), targets[0].1.identity()),
            Err(InflightError::Invalid)
        ));
        assert!(matches!(
            Inflight::open_existing(
                &root.path().join("never-created"),
                set.clone(),
                pin.host.clone()
            ),
            Err(InflightError::Open(_))
        ));
        assert!(!root.path().join("never-created").exists());
        let mut bad_parent = base.clone();
        bad_parent.digest = format!("{:x}", Sha256::digest(b"another snapshot"));
        assert_eq!(
            RetainedTransaction::inspect(&mut candidates[0], &bad_parent, &transaction_end),
            Err(TransactionError::Invalid)
        );
        actual_capture_quorum(
            &app,
            root.path(),
            leader,
            candidates,
            &set,
            &pin.host,
            &transaction_end,
            &content,
        );

        // Reopen real keys and actual SQLite logs. A higher-term election must
        // preserve the full old payload and prevent the old term getting quorum.
        let reopened: Vec<_> = targets
            .iter()
            .map(|(data, key, _)| {
                crate::handover_target::TargetKey::open_bound(data, &key.identity()).unwrap()
            })
            .collect();
        let mut a = Voter::open(
            TakeoverStore::open_existing(&targets[0].0.join("log"), quota).unwrap(),
            reopened[0].takeover_signer(),
        )
        .unwrap();
        let mut b = Voter::open(
            TakeoverStore::open_existing(&targets[1].0.join("log"), quota).unwrap(),
            reopened[1].takeover_signer(),
        )
        .unwrap();
        assert_eq!(a.entry(1).unwrap(), first_entry.unwrap());
        let reopened_tx = RetainedTransaction::inspect(&mut a, &base, &transaction_end).unwrap();
        assert_eq!(reopened_tx, retained);
        let mut staging = Vec::new();
        reopened_tx.copy_to_staging(&mut a, &mut staging).unwrap();
        assert_eq!(staging, content);
        let payload =
            || Payload::new(URL_SAFE_NO_PAD.encode(b"owned next-term fixture payload")).unwrap();
        let (request, _) = a.start_election().unwrap();
        assert!(matches!(
            b.grant_vote(&request).unwrap(),
            VoteAnswer::Granted(_)
        ));
        let mut old = Voter::open(
            TakeoverStore::open_existing(&source_store, quota).unwrap(),
            app.0.handover.takeover_signer(),
        )
        .unwrap();
        let (stale, stale_ack) = old.propose(payload()).unwrap();
        let nacks = [&mut a, &mut b]
            .into_iter()
            .map(|candidate| {
                let AppendAnswer::Nack(nack) = candidate.append(&stale).unwrap() else {
                    panic!("old term obtained another acknowledgement")
                };
                nack
            })
            .collect::<Vec<_>>();
        assert_eq!(
            old.commit_gate(&stale, &[stale_ack], &nacks),
            Decision::Rollback(Why::Fenced)
        );
        let (next, own) = a.propose(payload()).unwrap();
        let AppendAnswer::Ack(peer) = b.append(&next).unwrap() else {
            panic!("elected term refused")
        };
        assert_eq!(a.commit_gate(&next, &[own, peer], &[]), Decision::Commit);
        drop(a);
        drop(b);
        drop(old);

        // A missing pinned key on restart remains missing; never manufacture a
        // replacement identity to make a retained quorum log appear usable.
        let key_file = targets[0].0.join("handover-target/signing-key.v1");
        std::fs::remove_file(&key_file).unwrap();
        assert!(crate::handover_target::TargetKey::open_bound(
            &targets[0].0,
            &reopened[0].identity()
        )
        .is_err());
        assert!(!key_file.exists());
    }

    type DiskVoter = Voter<TakeoverStore, HostKey>;
    struct LocalPeers {
        http: crate::takeover_transport::HttpPeers,
        last_notice: std::sync::Arc<std::sync::Mutex<Option<crate::takeover_commit::CommitNotice>>>,
    }
    impl crate::takeover_commit::Replicate for LocalPeers {
        fn append(
            &mut self,
            entry: &wire::LogEntry,
            deadline: std::time::Instant,
        ) -> [Option<AppendAnswer>; 2] {
            self.http.append(entry, deadline)
        }
        fn committed(
            &mut self,
            notice: &crate::takeover_commit::CommitNotice,
            deadline: std::time::Instant,
        ) {
            *self.last_notice.lock().unwrap() = Some(notice.clone());
            self.http.committed(notice, deadline);
        }
    }
    struct ReplicaNetwork {
        stop: tokio::sync::watch::Sender<bool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for ReplicaNetwork {
        fn drop(&mut self) {
            self.stop.send_replace(true);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    fn loopback_replicas(
        replicas: &[std::sync::Arc<std::sync::Mutex<crate::takeover_runtime::Replica>>],
    ) -> (
        crate::takeover_transport::HttpPeers,
        ReplicaNetwork,
        [std::net::SocketAddr; 2],
    ) {
        let listeners = [
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
        ];
        let addresses = std::array::from_fn(|i| listeners[i].local_addr().unwrap());
        for listener in &listeners {
            listener.set_nonblocking(true).unwrap();
        }
        let keys = ["a1".repeat(32), "b2".repeat(32)]; // Own isolated test endpoints only.
        let routers: [_; 2] = std::array::from_fn(|i| {
            crate::takeover_transport::router(replicas[i].clone(), keys[i].clone()).unwrap()
        });
        let targets = std::array::from_fn(|i| {
            crate::takeover_transport::Target::owned_forward(addresses[i], keys[i].clone()).unwrap()
        });
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut tasks = Vec::new();
                for (listener, router) in listeners.into_iter().zip(routers) {
                    let mut stopped = stopped.clone();
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    tasks.push(tokio::spawn(async move {
                        axum::serve(listener, router)
                            .with_graceful_shutdown(async move {
                                while !*stopped.borrow_and_update() {
                                    if stopped.changed().await.is_err() {
                                        break;
                                    }
                                }
                            })
                            .await
                            .unwrap();
                    }));
                }
                for task in tasks {
                    task.await.unwrap();
                }
            });
        });
        (
            crate::takeover_transport::HttpPeers::new(targets).unwrap(),
            ReplicaNetwork {
                stop,
                thread: Some(thread),
            },
            addresses,
        )
    }
    fn actual_capture_quorum(
        app: &AppState,
        root: &std::path::Path,
        leader: DiskVoter,
        candidates: Vec<DiskVoter>,
        set: &AuthoritySet,
        host: &HostIdentity,
        parent: &Tip,
        content: &[u8],
    ) {
        use rusqlite::Connection;
        use std::sync::{Arc, Mutex};
        use tower::ServiceExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // The actual Relay AppState owns this database and the HTTP/notices gates.
        // The snapshot/cohort anchor remains an explicit own P0 acceptance fixture.
        {
            let db = crate::lock(&app.0.db);
            db.execute_batch("CREATE TABLE capture_payload(id TEXT PRIMARY KEY, data BLOB); INSERT INTO capture_payload VALUES ('seed', x'010203');").unwrap();
        }
        let mut image_paths = Vec::new();
        let candidates = candidates
            .into_iter()
            .enumerate()
            .map(|(i, voter)| {
                let folder = root.join(format!("standby-{i}"));
                crate::handover_target::private_directory(&folder).unwrap();
                let image = folder.join(crate::takeover_standby::IMAGE_NAME);
                app.backup_to(&image).unwrap();
                image_paths.push(image);
                let standby = crate::takeover_standby::Standby::create_new(
                    &folder,
                    set,
                    voter.identity(),
                    parent,
                    crate::takeover_standby::Limits::DEFAULT,
                )
                .unwrap();
                Arc::new(Mutex::new(
                    crate::takeover_runtime::Replica::new(voter, standby, set).unwrap(),
                ))
            })
            .collect::<Vec<_>>();
        let recovery_folder = root.join("source/inflight");
        let recovery =
            Inflight::open_existing(&recovery_folder, set.clone(), host.clone()).unwrap();
        let voters = candidates;
        let (http, network, addresses) = loopback_replicas(&voters);
        runtime.block_on(async {
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            for path in ["append", "committed"] {
                let response = http
                    .post(format!("http://{}/internal/takeover/{path}", addresses[0]))
                    .body("invalid body is not inspected before authentication")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
            }
        });
        let last_notice = Arc::new(Mutex::new(None));
        runtime
            .block_on(app.attach_takeover_source(
                leader,
                recovery,
                Box::new(LocalPeers {
                    http,
                    last_notice: last_notice.clone(),
                }),
            ))
            .unwrap();
        let mut notices = app.0.events.subscribe();
        crate::lock(&app.0.db)
            .execute(
                "INSERT INTO capture_payload VALUES ('large', ?1)",
                rusqlite::params![content],
            )
            .unwrap();
        let committed_end = crate::lock(&app.0.takeover)
            .as_ref()
            .unwrap()
            .applied_tip()
            .unwrap();
        assert!(!app.takeover_blocked());
        // Commit delivery is asynchronous and does not hold the writer waiting
        // for two image fsyncs. Wait only in this owned final integration fixture.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while !voters
            .iter()
            .all(|p| p.lock().unwrap().applied() == &committed_end)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "certified changes did not reach both replicas"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        for peer in &voters {
            peer.lock().unwrap().verify(&committed_end).unwrap();
        }
        // Read disposable copies, never open the owned standby itself in SQLite.
        for (i, image) in image_paths.iter().enumerate() {
            let copy = root.join(format!("compare-{i}.sqlite3"));
            std::fs::copy(image, &copy).unwrap();
            let target = Connection::open(&copy).unwrap();
            assert_eq!(
                target
                    .query_row(
                        "SELECT data FROM capture_payload WHERE id='large'",
                        [],
                        |r| r.get::<_, Vec<u8>>(0)
                    )
                    .unwrap(),
                content
            );
            assert_eq!(
                target
                    .query_row("SELECT count(*) FROM capture_payload", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                2
            );
        }
        app.notify("refresh", None, None, None);
        assert_eq!(notices.try_recv().unwrap().event.kind, "refresh");
        let notice = last_notice.lock().unwrap().clone().unwrap();
        runtime.block_on(async {
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let url = format!("http://{}/internal/takeover/committed", addresses[0]);
            let mut forged = notice.clone();
            forged.acks[1] = forged.acks[0].clone();
            assert_eq!(
                http.post(&url)
                    .bearer_auth("a1".repeat(32))
                    .json(&forged)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::CONFLICT
            );
            assert_eq!(
                http.post(&url)
                    .bearer_auth("a1".repeat(32))
                    .json(&notice)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::NO_CONTENT
            );
        });
        drop(network); // Real listeners stop; subsequent source requests cannot reach either peer.
        assert!(crate::lock(&app.0.db)
            .execute("INSERT INTO capture_payload VALUES ('refused', x'04')", [])
            .is_err());
        assert!(app.takeover_blocked());
        assert_eq!(
            crate::lock(&app.0.db)
                .query_row("SELECT count(*) FROM capture_payload", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(notices.try_recv().unwrap().event.kind, "server_paused");
        app.notify("unconfirmed", None, None, None);
        assert!(
            notices.try_recv().is_err(),
            "unconfirmed work must not be published"
        );
        let response = runtime
            .block_on(
                crate::router(app.clone()).oneshot(
                    axum::http::Request::builder()
                        .uri("/api/bootstrap")
                        .header("host", "127.0.0.1")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(crate::lock(&app.0.takeover)
            .as_ref()
            .unwrap()
            .applied_tip()
            .is_err());
        // Stop this own fixture and release its file locks before testing restart/election.
        drop(std::mem::replace(
            &mut *crate::lock(&app.0.db),
            Connection::open_in_memory().unwrap(),
        ));
        drop(crate::lock(&app.0.takeover).take());
        assert!(AppState::open(&app.0.data, "must not reopen unprotected", 0).is_err());
        drop(voters);
        let recovery =
            Inflight::open_existing(&recovery_folder, set.clone(), host.clone()).unwrap();
        assert!(recovery.recovery_required());
        assert_eq!(recovery.last_applied(), &committed_end);
    }
}
