//! Opt-in actual MLS actors and Windows checkpoints; no accounts or media devices.
use super::*;
use serde_json::{json, Value};
use std::{fs, time::Instant};

struct Fixture {
    root: PathBuf,
    server: String,
    users: Vec<String>,
    actors: Vec<Option<CryptoSession>>,
    samples: Vec<Value>,
}

impl Fixture {
    fn new(count: usize) -> Self {
        if cfg!(debug_assertions) {
            panic!("Run capacity measurements with --release");
        }
        let root = PathBuf::from(
            std::env::var_os("RELAY_TEST_WORK_ROOT")
                .expect("capacity tests require an explicitly owned RELAY_TEST_WORK_ROOT"),
        )
        .join(format!("mls-capacity-{}", uuid::Uuid::new_v4()));
        let mut fixture = Self {
            root,
            server: uuid::Uuid::new_v4().to_string(),
            users: Vec::new(),
            actors: Vec::new(),
            samples: Vec::new(),
        };
        fixture.sample("before_actors");
        for _ in 0..count {
            fixture.add_actor();
        }
        let public: Vec<_> = fixture
            .actors
            .iter()
            .map(|a| a.as_ref().unwrap().advertised())
            .collect();
        // Fixture setup explicitly approves the full known fingerprints in one
        // durable mutation per actor; this is not an onboarding/UI measurement.
        for index in 0..count {
            fixture
                .actor(index)
                .with_device(|device| {
                    for (peer, key) in public.iter().enumerate() {
                        if peer != index {
                            device.approve(key, &key.fingerprint)?;
                        }
                    }
                    Ok(())
                })
                .unwrap();
        }
        fixture.sample("verified_actors");
        fixture
    }
    fn add_actor(&mut self) -> usize {
        let index = self.users.len();
        let user = format!("capacity-device-{index}");
        self.users.push(user.clone());
        self.actors.push(Some(
            CryptoSession::open_for_test(self.root.clone(), &self.server, &user).unwrap(),
        ));
        index
    }
    fn actor(&mut self, index: usize) -> &mut CryptoSession {
        self.actors[index].as_mut().unwrap()
    }
    fn approve(&mut self, index: usize, peer: &PublicDevice) {
        self.actor(index)
            .with_device(|device| device.approve(peer, &peer.fingerprint))
            .unwrap();
    }
    fn reopen(&mut self, index: usize) {
        let identity = self.actor(index).device.identity().clone();
        drop(self.actors[index].take());
        let actor =
            CryptoSession::open_for_test(self.root.clone(), &self.server, &self.users[index])
                .unwrap();
        assert_eq!(actor.device.identity(), &identity);
        self.actors[index] = Some(actor);
    }
    fn sample(&mut self, phase: &str) {
        use windows::Win32::System::{
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            Threading::GetCurrentProcess,
        };
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: one correctly sized writable counters struct and the current
        // process pseudo-handle; no other application or process is inspected.
        assert!(unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                counters.cb,
            )
        }
        .as_bool());
        let checkpoints: Vec<_> = self
            .actors
            .iter()
            .flatten()
            .map(|a| {
                let usage = a.history_usage();
                json!({"checkpoint_bytes":fs::metadata(&a.path).unwrap().len(),
                "archive_bytes":usage.archived_bytes, "archive_segments":usage.segments,
                "history_records":usage.records, "groups":a.device.scopes().len()})
            })
            .collect();
        self.samples.push(json!({"phase":phase, "actual_actors":self.actors.iter().flatten().count(),
            "working_set_bytes":counters.WorkingSetSize, "private_bytes":counters.PrivateUsage,
            "peak_working_set_bytes_since_process_start":counters.PeakWorkingSetSize, "actors":checkpoints}));
    }
    fn report(&self, filename: &str, result: Value) {
        let path = self.root.join(filename);
        fs::write(&path, serde_json::to_vec_pretty(&json!({"result":result, "samples":self.samples,
            "process_id":std::process::id(), "scope":"Actual local MLS signing identities, group operations, encrypted applications and production Windows-protected checkpoints. Optimized isolated actor measurement, not HTTP/server stock, native UI, physical devices, WAN or whole-app media acceptance.",
            "memory_method":"Current process Windows memory counters at named phases; process peak includes test startup. Runner must launch each case in a fresh process.",
            "physical_devices_opened":false, "private_keys_exported":false})).unwrap()).unwrap();
        println!("Capacity evidence: {}", path.display());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for user in &self.users {
            protected::remove_head(&format!("Relay/crypto/v1/{}/{user}", self.server));
        }
    }
}

fn target(name: &str) -> relay_protocol::CryptoTarget {
    relay_protocol::CryptoTarget {
        channel_id: Some(name.into()),
        peer_id: None,
    }
}
fn envelope(scope: &str, id: &str) -> Envelope {
    Envelope {
        version: 1,
        scope: scope.into(),
        client_id: id.into(),
        content: format!("owned private capacity body {id}"),
        reply_to_id: None,
        mentions: Vec::new(),
        attachments: Vec::new(),
        action: None,
    }
}
fn send(actor: &mut CryptoSession, scope: &str, id: &str) -> String {
    let wire = actor.encrypted_content(envelope(scope, id)).unwrap();
    actor.committed(id).unwrap();
    wire
}
fn receive(actor: &mut CryptoSession, scope: &str, wire: &str, id: &str) {
    let decoded = actor.decrypt_content(scope, wire).unwrap();
    check_envelope(&decoded, scope, id);
}
fn check_envelope(decoded: &CachedEnvelope, scope: &str, id: &str) {
    assert_eq!(decoded.envelope.scope, scope);
    assert_eq!(decoded.envelope.client_id, id);
    assert_eq!(decoded.envelope.content, envelope(scope, id).content);
    assert!(decoded.envelope.mentions.is_empty() && decoded.envelope.attachments.is_empty());
    assert!(decoded.envelope.reply_to_id.is_none() && decoded.envelope.action.is_none());
}
fn delivered(
    batch: &relay_protocol::AppendCryptoEvents,
    event: &relay_protocol::CryptoEventBody,
    author: &Identity,
    sequence: u64,
) -> relay_protocol::CryptoEvent {
    relay_protocol::CryptoEvent {
        sequence,
        epoch: batch.epoch,
        group_id: batch.group_id.clone(),
        kind: event.kind.clone(),
        recipient_device_id: event.recipient_device_id.clone(),
        author_user_id: author.user_id.clone(),
        author_device_id: author.device_id.clone(),
        body: event.body.clone(),
        operation_id: batch.operation_id.clone(),
    }
}
fn admit(
    fixture: &mut Fixture,
    scope: &str,
    name: &str,
    packages: &[PublicDevice],
) -> Vec<relay_protocol::AppendCryptoEvents> {
    let author = fixture.actor(0).device.identity().clone();
    let server = fixture.server.clone();
    let mut batches = Vec::new();
    for round in 0..packages.len().max(1).div_ceil(16) {
        let start = round * 16;
        let end = (start + 16).min(packages.len());
        let cursor = fixture.actor(0).cursor(scope);
        let batch = fixture
            .actor(0)
            .stage_membership(
                scope,
                target(name),
                format!("capacity-{name}"),
                cursor,
                &packages[start..end],
                &[],
                round == 0,
            )
            .unwrap();
        let sequence = cursor + batch.events.len() as u64;
        for index in 1..fixture.actors.len() {
            if fixture.actor(index).device.is_active(scope).unwrap() {
                let (offset, commit) = batch
                    .events
                    .iter()
                    .enumerate()
                    .find(|(_, e)| e.kind == "commit")
                    .unwrap();
                fixture
                    .actor(index)
                    .apply_control(
                        scope,
                        &delivered(&batch, commit, &author, cursor + offset as u64 + 1),
                        &server,
                        None,
                    )
                    .unwrap();
                fixture.actor(index).set_cursor(scope, sequence).unwrap();
                continue;
            }
            let id = fixture.actor(index).device.identity().device_id.clone();
            if let Some((offset, welcome)) = batch
                .events
                .iter()
                .enumerate()
                .find(|(_, e)| e.recipient_device_id.as_ref() == Some(&id))
            {
                fixture
                    .actor(index)
                    .apply_control(
                        scope,
                        &delivered(&batch, welcome, &author, cursor + offset as u64 + 1),
                        &server,
                        None,
                    )
                    .unwrap();
                fixture.actor(index).set_cursor(scope, sequence).unwrap();
            }
        }
        fixture.actor(0).committed_control(scope, sequence).unwrap();
        batches.push(batch);
    }
    batches
}

fn device_case(count: usize) {
    let began = Instant::now();
    let mut fixture = Fixture::new(count);
    let name = format!("devices-{count}");
    let scope = format!("{}/channel/{name}", fixture.server);
    let pool_at = Instant::now();
    let mut packages = Vec::new();
    let mut package_hashes = HashSet::new();
    for index in 1..count {
        let package = fixture.actor(index).key_packages(1).unwrap().remove(0);
        assert!(package_hashes.insert(format!("{:x}", Sha256::digest(&package.key_package))));
        fixture.actor(index).pool_committed().unwrap();
        packages.push(package);
    }
    let pool_ms = pool_at.elapsed().as_secs_f64() * 1000.0;
    let group_at = Instant::now();
    let batches = admit(&mut fixture, &scope, &name, &packages);
    let batch = batches.last().unwrap();
    for index in 0..count {
        assert_eq!(
            fixture.actor(index).device.members(&scope).unwrap().len(),
            count
        );
    }
    let group_ms = group_at.elapsed().as_secs_f64() * 1000.0;
    fixture.sample("group_joined");
    let send_at = Instant::now();
    let wire = send(fixture.actor(0), &scope, "initial");
    let send_ms = send_at.elapsed().as_secs_f64() * 1000.0;
    let receive_at = Instant::now();
    for index in 0..count {
        receive(fixture.actor(index), &scope, &wire, "initial");
    }
    let all_receive_ms = receive_at.elapsed().as_secs_f64() * 1000.0;
    let mut peers = Vec::new();
    for index in 1..count {
        peers.push(send(fixture.actor(index), &scope, &format!("peer-{index}")));
    }
    let refs: Vec<_> = peers.iter().map(String::as_str).collect();
    let replies = fixture.actor(0).decrypt_contents(&scope, &refs);
    assert_eq!(replies.len(), count - 1);
    for (index, reply) in replies.into_iter().enumerate() {
        let reply = reply.unwrap();
        check_envelope(&reply, &scope, &format!("peer-{}", index + 1));
        assert_eq!(
            reply.author.user_id,
            format!("capacity-device-{}", index + 1)
        );
    }
    let mut backlog = Vec::new();
    for index in 0..32 {
        backlog.push(send(fixture.actor(0), &scope, &format!("backlog-{index}")));
    }
    let receiver = usize::from(count > 1);
    let refs: Vec<_> = backlog.iter().map(String::as_str).collect();
    let revision = fixture.actor(receiver).state.revision;
    let catch_at = Instant::now();
    let decoded = fixture.actor(receiver).decrypt_contents(&scope, &refs);
    for (index, message) in decoded.into_iter().enumerate() {
        check_envelope(&message.unwrap(), &scope, &format!("backlog-{index}"));
    }
    let ids: Vec<_> = (0..32).map(|i| format!("message-{i}")).collect();
    let bindings: Vec<_> = backlog
        .iter()
        .zip(&ids)
        .enumerate()
        .map(|(i, (w, id))| (w.as_str(), id.as_str(), i as u64 + 2))
        .collect();
    assert!(fixture
        .actor(receiver)
        .bind_deliveries(&bindings)
        .iter()
        .all(Result::is_ok));
    let catch_ms = catch_at.elapsed().as_secs_f64() * 1000.0;
    let checkpoints = fixture.actor(receiver).state.revision - revision;
    assert!(checkpoints <= 2);
    fixture.sample("applications_and_catch_up");
    fixture.reopen(receiver);
    let saved = fs::read(&fixture.actor(receiver).path).unwrap();
    let cached_at = Instant::now();
    assert!(fixture
        .actor(receiver)
        .decrypt_contents(&scope, &refs)
        .iter()
        .all(Result::is_ok));
    assert!(fixture
        .actor(receiver)
        .bind_deliveries(&bindings)
        .iter()
        .all(Result::is_ok));
    let cached_ms = cached_at.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(fs::read(&fixture.actor(receiver).path).unwrap(), saved);
    let mut limit = Value::Null;
    let mut clear_error = true;
    if count == 64 {
        let extra = fixture.add_actor();
        let public = fixture.actor(extra).advertised();
        let existing: Vec<_> = fixture
            .actors
            .iter()
            .take(64)
            .flatten()
            .map(|a| a.advertised())
            .collect();
        fixture
            .actor(extra)
            .with_device(|device| {
                for peer in &existing {
                    device.approve(peer, &peer.fingerprint)?;
                }
                Ok(())
            })
            .unwrap();
        for index in 0..64 {
            fixture.approve(index, &public);
        }
        let before = fs::read(&fixture.actor(0).path).unwrap();
        let head = protected::head(&fixture.actor(0).binding()).unwrap();
        let attempted_cursor = fixture.actor(0).cursor(&scope);
        let error = fixture
            .actor(0)
            .stage_membership(
                &scope,
                target(&name),
                batch.group_id.clone(),
                attempted_cursor,
                std::slice::from_ref(&public),
                &[],
                false,
            )
            .unwrap_err();
        assert_eq!(fs::read(&fixture.actor(0).path).unwrap(), before);
        assert_eq!(protected::head(&fixture.actor(0).binding()).unwrap(), head);
        assert_eq!(fixture.actor(0).device.members(&scope).unwrap().len(), 64);
        clear_error = error.contains("64");
        let removed = fixture.actor(63).device.identity().device_id.clone();
        let author = fixture.actor(0).device.identity().clone();
        let cursor = fixture.actor(0).cursor(&scope);
        let removal = fixture
            .actor(0)
            .stage_membership(
                &scope,
                target(&name),
                batch.group_id.clone(),
                cursor,
                &[],
                &[removed],
                false,
            )
            .unwrap();
        let server = fixture.server.clone();
        for index in 1..64 {
            fixture
                .actor(index)
                .apply_control(
                    &scope,
                    &delivered(&removal, &removal.events[0], &author, cursor + 1),
                    &server,
                    None,
                )
                .unwrap();
        }
        fixture
            .actor(0)
            .committed_control(&scope, cursor + 1)
            .unwrap();
        assert!(!fixture.actor(63).device.is_active(&scope).unwrap());
        // Reuse only the package rejected before admission: its private init key
        // has never been consumed. Removed participant's old package is not reused.
        let addition = fixture
            .actor(0)
            .stage_membership(
                &scope,
                target(&name),
                batch.group_id.clone(),
                cursor + 1,
                &[public],
                &[],
                false,
            )
            .unwrap();
        for index in 1..63 {
            fixture
                .actor(index)
                .apply_control(
                    &scope,
                    &delivered(&addition, &addition.events[0], &author, cursor + 2),
                    &server,
                    None,
                )
                .unwrap();
        }
        fixture
            .actor(extra)
            .apply_control(
                &scope,
                &delivered(&addition, &addition.events[1], &author, cursor + 3),
                &server,
                None,
            )
            .unwrap();
        fixture
            .actor(0)
            .committed_control(&scope, cursor + 3)
            .unwrap();
        fixture.reopen(0);
        fixture.reopen(extra);
        let future = send(fixture.actor(0), &scope, "after-limit-removal-restart");
        for index in (1..63).chain(std::iter::once(extra)) {
            receive(
                fixture.actor(index),
                &scope,
                &future,
                "after-limit-removal-restart",
            );
        }
        assert!(fixture.actor(63).decrypt_content(&scope, &future).is_err());
        assert_eq!(fixture.actor(0).device.members(&scope).unwrap().len(), 64);
        limit = json!({"rejected_device":65,"error":error,"checkpoint_and_head_unchanged":true,
            "removal_addition_and_actor_reopen":true,"removed_device_future_application_rejected":true,
            "previously_rejected_package_still_usable":true});
        fixture.sample("limit_rejection_removal_and_reopen");
    }
    fixture.report("devices.json", json!({"devices_in_primary_measurement":count,"fixture_setup_batched_known_approvals":true,
        "group_join_ms":group_ms,"key_package_pool_ms":pool_ms,"fresh_join_packages":package_hashes.len(),
        "encrypted_payload_bytes":wire.len(),"membership_batch_bytes":batches.iter().map(|b|serde_json::to_vec(b).unwrap().len()).max().unwrap(),
        "membership_batch_count":batches.len(), "membership_total_json_bytes":batches.iter().map(|b|serde_json::to_vec(b).unwrap().len()).sum::<usize>(),
        "send_with_receipt_ms":send_ms,"all_initial_receivers_ms":all_receive_ms,"peer_applications_verified":peers.len(),
        "catch_up_messages":32,"fresh_catch_up_recipient":(count>1).then_some(receiver),"catch_up_ms":catch_ms,
        "catch_up_checkpoint_count":checkpoints,"cached_reload_ms":cached_ms,"cached_reload_checkpoint_count":0,
        "elapsed_ms":began.elapsed().as_secs_f64()*1000.0,"limit":limit,"clear_quota_error":clear_error}));
    assert!(
        clear_error,
        "the full conversation must explain the 64-device limit"
    );
}

#[test]
#[ignore = "owned Windows checkpoint capacity measurement; run each case in a fresh optimized test process"]
fn actual_capacity_one_device() {
    device_case(1);
}
#[test]
#[ignore = "owned Windows checkpoint capacity measurement; run each case in a fresh optimized test process"]
fn actual_capacity_sixteen_devices() {
    device_case(16);
}
#[test]
#[ignore = "owned Windows checkpoint capacity measurement; run each case in a fresh optimized test process"]
fn actual_capacity_thirty_two_devices() {
    device_case(32);
}
#[test]
#[ignore = "owned Windows checkpoint capacity measurement; run each case in a fresh optimized test process"]
fn actual_capacity_sixty_four_devices() {
    device_case(64);
}

#[test]
#[ignore = "128 actual protected MLS groups; isolated optimized process, not synthetic counters"]
fn actual_capacity_one_hundred_twenty_eight_groups() {
    let began = Instant::now();
    let mut fixture = Fixture::new(2);
    let mut packages = Vec::new();
    let mut hashes = HashSet::new();
    let mut pool_ms = 0.0;
    let mut stages = Vec::new();
    for index in 0..128 {
        if packages.is_empty() {
            let at = Instant::now();
            packages = fixture.actor(1).key_packages(16).unwrap();
            fixture.actor(1).pool_committed().unwrap();
            pool_ms += at.elapsed().as_secs_f64() * 1000.0;
        }
        let package = packages.pop().unwrap();
        assert!(hashes.insert(format!("{:x}", Sha256::digest(&package.key_package))));
        let name = format!("group-{index}");
        let scope = format!("{}/channel/{name}", fixture.server);
        let at = Instant::now();
        admit(&mut fixture, &scope, &name, &[package]);
        let wire = send(fixture.actor(0), &scope, &format!("group-body-{index}"));
        receive(
            fixture.actor(1),
            &scope,
            &wire,
            &format!("group-body-{index}"),
        );
        let delivery_id = format!("group-message-{index}");
        for actor in 0..2 {
            assert!(fixture
                .actor(actor)
                .bind_deliveries(&[(&wire, &delivery_id, index as u64 + 1,)])
                .iter()
                .all(Result::is_ok));
        }
        stages.push(json!({"group":index+1,"join_send_receive_ms":at.elapsed().as_secs_f64()*1000.0,"encrypted_payload_bytes":wire.len()}));
        if [1, 16, 32, 64, 128].contains(&(index + 1)) {
            fixture.sample(&format!("groups-{}", index + 1));
        }
    }
    assert_eq!(hashes.len(), 128);
    for index in 0..2 {
        assert_eq!(fixture.actor(index).device.scopes().len(), 128);
    }
    let stock = fixture.actor(1).key_packages(1).unwrap().remove(0);
    fixture.actor(1).pool_committed().unwrap();
    let scope = format!("{}/channel/group-129", fixture.server);
    let before = fs::read(&fixture.actor(0).path).unwrap();
    let head = protected::head(&fixture.actor(0).binding()).unwrap();
    let create_error = fixture
        .actor(0)
        .stage_membership(
            &scope,
            target("group-129"),
            "capacity-group-129".into(),
            0,
            std::slice::from_ref(&stock),
            &[],
            true,
        )
        .unwrap_err();
    assert_eq!(fs::read(&fixture.actor(0).path).unwrap(), before);
    assert_eq!(protected::head(&fixture.actor(0).binding()).unwrap(), head);
    // A separately authenticated Welcome also cannot bypass the receiver limit.
    let inviter = fixture.add_actor();
    let invited_public = fixture.actor(inviter).advertised();
    fixture.approve(inviter, &stock);
    fixture.approve(1, &invited_public);
    let author = invited_public.identity;
    let batch = fixture
        .actor(inviter)
        .stage_membership(
            &scope,
            target("group-129"),
            "capacity-group-129".into(),
            0,
            &[stock],
            &[],
            true,
        )
        .unwrap();
    fixture.actor(inviter).committed_control(&scope, 3).unwrap();
    let before = fs::read(&fixture.actor(1).path).unwrap();
    let head = protected::head(&fixture.actor(1).binding()).unwrap();
    let server = fixture.server.clone();
    let join_error = fixture
        .actor(1)
        .apply_control(
            &scope,
            &delivered(&batch, &batch.events[2], &author, 3),
            &server,
            None,
        )
        .unwrap_err();
    assert_eq!(fs::read(&fixture.actor(1).path).unwrap(), before);
    assert_eq!(protected::head(&fixture.actor(1).binding()).unwrap(), head);
    assert!(fixture.actor(1).device.group_state(&scope).is_none());
    for index in 0..2 {
        fixture.reopen(index);
        assert_eq!(fixture.actor(index).device.scopes().len(), 128);
    }
    for index in 0..128 {
        let old_scope = format!("{}/channel/group-{index}", fixture.server);
        for actor in 0..2 {
            assert!(fixture
                .actor(actor)
                .cached_envelopes(&old_scope)
                .any(|cached| cached.unwrap().envelope.client_id == format!("group-body-{index}")));
        }
        let wire = send(
            fixture.actor(0),
            &old_scope,
            &format!("after-full-reopen-{index}"),
        );
        receive(
            fixture.actor(1),
            &old_scope,
            &wire,
            &format!("after-full-reopen-{index}"),
        );
        let delivery_id = format!("after-reopen-message-{index}");
        for actor in 0..2 {
            assert!(fixture
                .actor(actor)
                .bind_deliveries(&[(&wire, &delivery_id, index as u64 + 129,)])
                .iter()
                .all(Result::is_ok));
        }
    }
    fixture.sample("group_limits_and_all_reopened_applications");
    let clear = create_error.contains("128") && join_error.contains("128");
    fixture.report("groups.json", json!({"actual_groups_per_primary_actor":128,"actual_primary_actors":2,"additional_welcome_inviter":1,
        "fresh_consumed_join_packages":hashes.len(),"pool_batches":8,"pool_batch_size":16,"pool_generation_ms":pool_ms,
        "per_group_measurements":stages,"rejected_group":129,"create_error":create_error,"join_error":join_error,
        "both_checkpoint_heads_unchanged_after_rejection":true,"all_group_history_and_fresh_applications_after_reopen":true,
        "clear_quota_error":clear,"elapsed_ms":began.elapsed().as_secs_f64()*1000.0}));
    assert!(
        clear,
        "both creator and receiver must explain the 128-conversation limit"
    );
}
