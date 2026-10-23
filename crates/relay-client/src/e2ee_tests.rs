//! Synthetic Windows key-store regressions; no personal account or media access.
use super::*;
#[path = "e2ee_history_cache_tests.rs"]
mod history_cache_tests;

#[path = "e2ee_history_measurement_tests.rs"]
mod history_measurement_tests;

fn synthetic_call_context(scope: &str, epoch: u64) -> CallKeys {
    CallKeys {
        group_id: "owned-call-context".into(),
        scope: scope.into(),
        epoch,
        local_user_id: "alice".into(),
        local_device_id: "owned-alice-device".into(),
        sender_keys: HashMap::from([(
            "alice".into(),
            SenderKeys {
                device_id: "owned-alice-device".into(),
                opus: Zeroizing::new([23; 32]),
                rtc: Zeroizing::new([24; 32]),
            },
        )]),
        voice_replay: Mutex::default(),
    }
}

#[test]
fn unchanged_call_context_retains_replay_and_rejects_same_epoch_changes() {
    let contexts = Mutex::default();
    let original =
        retain_call_context(&contexts, synthetic_call_context("owned-scope", 7)).unwrap();
    let again = retain_call_context(&contexts, synthetic_call_context("owned-scope", 7)).unwrap();
    assert!(Arc::ptr_eq(&original, &again));
    for field in [
        "user",
        "local_device",
        "sender_device",
        "opus",
        "rtc",
        "roster",
    ] {
        let mut changed = synthetic_call_context("owned-scope", 7);
        match field {
            "user" => changed.local_user_id.push_str("-other"),
            "local_device" => changed.local_device_id.push_str("-other"),
            "sender_device" => changed
                .sender_keys
                .get_mut("alice")
                .unwrap()
                .device_id
                .push_str("-other"),
            "opus" => changed.sender_keys.get_mut("alice").unwrap().opus[0] ^= 1,
            "rtc" => changed.sender_keys.get_mut("alice").unwrap().rtc[0] ^= 1,
            "roster" => changed.sender_keys.clear(),
            _ => unreachable!(),
        }
        assert!(
            retain_call_context(&contexts, changed).is_err(),
            "changed {field} reset replay"
        );
        assert!(Arc::ptr_eq(
            &original,
            &contexts.lock().unwrap()["owned-scope"]
        ));
    }
    let next = retain_call_context(&contexts, synthetic_call_context("owned-scope", 8)).unwrap();
    assert!(!Arc::ptr_eq(&original, &next));
    assert!(retain_call_context(&contexts, synthetic_call_context("owned-scope", 7)).is_err());
    assert!(Arc::ptr_eq(
        &next,
        &retain_call_context(&contexts, synthetic_call_context("owned-scope", 8)).unwrap()
    ));
    let other = retain_call_context(&contexts, synthetic_call_context("other-scope", 7)).unwrap();
    assert!(!Arc::ptr_eq(&original, &other));
}

#[test]
fn full_call_context_cache_keeps_existing_epoch_instead_of_evicting_replay() {
    let contexts = Mutex::default();
    let first = retain_call_context(&contexts, synthetic_call_context("scope-0", 7)).unwrap();
    for n in 1..128 {
        retain_call_context(&contexts, synthetic_call_context(&format!("scope-{n}"), 7)).unwrap();
    }
    assert!(retain_call_context(&contexts, synthetic_call_context("over-limit", 7)).is_err());
    assert_eq!(contexts.lock().unwrap().len(), 128);
    assert!(Arc::ptr_eq(
        &first,
        &retain_call_context(&contexts, synthetic_call_context("scope-0", 7)).unwrap()
    ));
    retain_call_context(&contexts, synthetic_call_context("scope-0", 8)).unwrap();
    assert_eq!(contexts.lock().unwrap().len(), 128);
}

struct Pair {
    root: PathBuf,
    server: String,
    scope: String,
    alice: Option<CryptoSession>,
    bob: Option<CryptoSession>,
}

fn call_refresh_target() -> relay_protocol::CryptoTarget {
    relay_protocol::CryptoTarget {
        channel_id: Some("general".into()),
        peer_id: None,
    }
}

#[test]
fn call_refresh_retries_after_actor_reopen_but_requires_new_local_rotation() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    let alice = pair.alice.as_mut().unwrap();
    let own = alice.device.identity().clone();
    let controller = alice.device.group_state(&scope).unwrap().controller.clone();
    let fingerprint = alice.device.fingerprint().unwrap();
    let old_key = alice.device.media_key(&scope, "opus", &own).unwrap();
    assert!(alice.needs_controller_call_refresh(&scope));
    let staged = alice
        .stage_call_refresh(&scope, call_refresh_target(), 3)
        .unwrap();
    let bytes = std::fs::read(&alice.path).unwrap();
    assert_eq!(
        alice
            .stage_call_refresh(&scope, call_refresh_target(), 3)
            .unwrap()
            .operation_id,
        staged.operation_id
    );
    assert_eq!(std::fs::read(&alice.path).unwrap(), bytes);
    assert!(alice.device.media_key(&scope, "opus", &own).is_err());
    assert!(alice
        .call_keys(&scope, &["alice".into(), "bob".into()])
        .is_err());
    drop(pair.alice.take());
    let mut reopened =
        CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    let retried = reopened
        .stage_call_refresh(&scope, call_refresh_target(), 3)
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&retried).unwrap(),
        serde_json::to_vec(&staged).unwrap()
    );
    assert!(reopened.committed_control(&scope, 5).is_err());
    assert_eq!(std::fs::read(&reopened.path).unwrap(), bytes);
    reopened.committed_control(&scope, 4).unwrap();
    assert!(
        reopened.needs_controller_call_refresh(&scope),
        "a previous actor's pending operation is not new entropy"
    );
    let fresh = reopened
        .stage_call_refresh(&scope, call_refresh_target(), 4)
        .unwrap();
    assert_ne!(fresh.operation_id, staged.operation_id);
    assert_ne!(fresh.events[0].body, staged.events[0].body);
    assert_eq!(fresh.epoch, staged.epoch + 1);
    reopened.committed_control(&scope, 5).unwrap();
    assert!(!reopened.needs_controller_call_refresh(&scope));
    let new_key = reopened.device.media_key(&scope, "opus", &own).unwrap();
    assert_ne!(&*old_key, &*new_key);
    assert_eq!(reopened.device.fingerprint().unwrap(), fingerprint);
    assert_eq!(
        reopened.device.group_state(&scope).unwrap().controller,
        controller
    );
    for (batch, sequence) in [(&staged, 4), (&fresh, 5)] {
        let event = relay_protocol::CryptoEvent {
            sequence,
            epoch: batch.epoch,
            group_id: batch.group_id.clone(),
            kind: "commit".into(),
            author_user_id: own.user_id.clone(),
            author_device_id: own.device_id.clone(),
            body: batch.events[0].body.clone(),
            ..Default::default()
        };
        pair.bob
            .as_mut()
            .unwrap()
            .apply_control(&scope, &event, &pair.server, None)
            .unwrap();
    }
    assert_eq!(
        &*pair
            .bob
            .as_ref()
            .unwrap()
            .device
            .media_key(&scope, "opus", &own)
            .unwrap(),
        &*new_key
    );
    pair.alice = Some(reopened);
    drop(pair.alice.take());
    let reopened = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    assert!(reopened.needs_controller_call_refresh(&scope));
    assert!(reopened.call_refresh_operations.is_empty());
    pair.alice = Some(reopened);
}

#[test]
fn call_refresh_rejects_wrong_scope_and_confirmation_without_losing_pending_operation() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    assert!(pair
        .bob
        .as_mut()
        .unwrap()
        .stage_call_refresh(&scope, call_refresh_target(), 3)
        .is_err());
    let alice = pair.alice.as_mut().unwrap();
    let mut target = call_refresh_target();
    target.channel_id = Some("another-channel".into());
    assert!(alice.stage_call_refresh(&scope, target, 3).is_err());
    assert!(alice
        .stage_call_refresh(&scope, call_refresh_target(), 2)
        .is_err());
    let staged = alice
        .stage_call_refresh(&scope, call_refresh_target(), 3)
        .unwrap();
    let saved = std::fs::read(&alice.path).unwrap();
    for mutation in ["body", "device", "group", "epoch", "recipient", "kind"] {
        let mut altered = staged.clone();
        match mutation {
            "body" => altered.events[0].body.push('A'),
            "device" => altered.device_id.push_str("-other"),
            "group" => altered.group_id.push_str("-other"),
            "epoch" => altered.epoch += 1,
            "recipient" => altered.events[0].recipient_device_id = Some("other".into()),
            "kind" => altered.events[0].kind = "welcome".into(),
            _ => unreachable!(),
        }
        alice.state.control_outbox.insert(scope.clone(), altered);
        assert!(alice.committed_control(&scope, 4).is_err(), "{mutation}");
        assert!(alice.needs_controller_call_refresh(&scope));
        assert_eq!(std::fs::read(&alice.path).unwrap(), saved);
    }
    alice.state.control_outbox.insert(scope.clone(), staged);
    alice.committed_control(&scope, 4).unwrap();
    assert!(!alice.needs_controller_call_refresh(&scope));
}

#[test]
fn call_refresh_storage_failure_blocks_publication_and_freshness_release() {
    for during_confirmation in [false, true] {
        let mut pair = Pair::new();
        let alice = pair.alice.as_mut().unwrap();
        if during_confirmation {
            alice
                .stage_call_refresh(&pair.scope, call_refresh_target(), 3)
                .unwrap();
        }
        let old_revision = alice.state.revision;
        let saved = std::fs::read(&alice.path).unwrap();
        let path = alice.path.clone();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let result = if during_confirmation {
            alice.committed_control(&pair.scope, 4)
        } else {
            alice
                .stage_call_refresh(&pair.scope, call_refresh_target(), 3)
                .map(|_| ())
        };
        assert!(result.is_err());
        assert!(alice.device.is_blocked());
        assert!(alice.needs_controller_call_refresh(&pair.scope));
        assert_eq!(alice.state.revision, old_revision);
        assert!(!alice.fresh_call_scopes.contains(&pair.scope));
        assert!(alice
            .call_keys(&pair.scope, &["alice".into(), "bob".into()])
            .is_err());
        // Restore only this fixture's exact protected checkpoint for a real reopen.
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, saved).unwrap();
        drop(pair.alice.take());
        let reopened =
            CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
        assert!(reopened.needs_controller_call_refresh(&pair.scope));
        assert_eq!(
            reopened.pending_control(&pair.scope).is_some(),
            during_confirmation
        );
        pair.alice = Some(reopened);
    }
}

#[test]
fn call_refresh_authenticated_handover_retires_pending_outbox_without_releasing_removed_keys() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    let alice_id = pair
        .alice
        .as_ref()
        .unwrap()
        .device
        .identity()
        .device_id
        .clone();
    let staged = pair
        .alice
        .as_mut()
        .unwrap()
        .stage_call_refresh(&scope, call_refresh_target(), 3)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    let author = bob.device.identity().clone();
    let change = bob
        .with_device(|device| device.remove_members(&scope, &[alice_id]))
        .unwrap();
    let event = relay_protocol::CryptoEvent {
        sequence: 4,
        epoch: change.epoch,
        group_id: staged.group_id,
        kind: "commit".into(),
        author_user_id: author.user_id,
        author_device_id: author.device_id,
        body: STANDARD.encode(change.commit),
        ..Default::default()
    };
    let alice = pair.alice.as_mut().unwrap();
    let saved = std::fs::read(&alice.path).unwrap();
    let mut altered = event.clone();
    altered.epoch += 1;
    assert!(alice
        .apply_control(&scope, &altered, &pair.server, None)
        .is_err());
    assert_eq!(std::fs::read(&alice.path).unwrap(), saved);
    assert!(alice.pending_control(&scope).is_some());
    alice
        .apply_control(&scope, &event, &pair.server, None)
        .unwrap();
    assert!(alice.pending_control(&scope).is_none());
    assert!(!alice.fresh_call_scopes.contains(&scope));
    assert!(!alice.call_refresh_operations.contains_key(&scope));
    assert!(!alice.device.is_active(&scope).unwrap());
    assert!(alice
        .call_keys(&scope, &["alice".into(), "bob".into()])
        .is_err());
    drop(pair.alice.take());
    let reopened = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    assert!(reopened.pending_control(&scope).is_none());
    assert!(!reopened.device.is_active(&scope).unwrap());
    pair.alice = Some(reopened);
}
impl Pair {
    fn new() -> Self {
        let root = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"))
            .join(format!("private-actor-{}", uuid::Uuid::new_v4()));
        let server = uuid::Uuid::new_v4().to_string();
        let scope = format!("{server}/channel/general");
        let mut alice = CryptoSession::open_for_test(root.clone(), &server, "alice").unwrap();
        let mut bob = CryptoSession::open_for_test(root.clone(), &server, "bob").unwrap();
        let a = alice.advertised();
        let b = bob.advertised();
        alice
            .directory
            .insert(b.identity.device_id.clone(), b.clone());
        bob.directory
            .insert(a.identity.device_id.clone(), a.clone());
        alice
            .approve(&b.identity.device_id, &b.fingerprint)
            .unwrap();
        bob.approve(&a.identity.device_id, &a.fingerprint).unwrap();
        let batch = alice
            .stage_membership(
                &scope,
                relay_protocol::CryptoTarget {
                    channel_id: Some("general".into()),
                    peer_id: None,
                },
                "fixture-group".into(),
                0,
                &[b],
                &[],
                true,
            )
            .unwrap();
        let welcome = batch.events.iter().find(|e| e.kind == "welcome").unwrap();
        let event = relay_protocol::CryptoEvent {
            sequence: 3,
            epoch: 1,
            group_id: batch.group_id.clone(),
            kind: "welcome".into(),
            author_user_id: a.identity.user_id.clone(),
            author_device_id: a.identity.device_id.clone(),
            recipient_device_id: Some(bob.device.identity().device_id.clone()),
            body: welcome.body.clone(),
            ..Default::default()
        };
        bob.apply_control(&scope, &event, &server, None).unwrap();
        alice.committed_control(&scope, 3).unwrap();
        Self {
            root,
            server,
            scope,
            alice: Some(alice),
            bob: Some(bob),
        }
    }
    fn envelope(&self, id: &str, text: &str) -> Envelope {
        Envelope {
            version: 1,
            scope: self.scope.clone(),
            client_id: id.into(),
            content: text.into(),
            reply_to_id: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            action: None,
        }
    }
    fn reopen_bob(&mut self) {
        self.bob.take();
        self.bob =
            Some(CryptoSession::open_for_test(self.root.clone(), &self.server, "bob").unwrap());
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        // Each fixture has its own random server binding. Clean its markers even
        // when a failed-open regression temporarily has no actor to own them.
        for user in ["alice", "bob"] {
            protected::remove_head(&format!("Relay/crypto/v1/{}/{user}", self.server));
        }
    }
}

fn call_delivery(
    pair: &Pair,
    request: relay_protocol::CallKeyRequest,
) -> relay_protocol::CallKeyDelivery {
    relay_protocol::CallKeyDelivery {
        server_id: pair.server.clone(),
        target: call_refresh_target(),
        author_user_id: "bob".into(),
        request,
    }
}
fn call_device(session: &CryptoSession) -> relay_protocol::CryptoDevice {
    let public = session.advertised();
    relay_protocol::CryptoDevice {
        user_id: public.identity.user_id,
        device_id: public.identity.device_id,
        identity: public.fingerprint,
        key_package: STANDARD.encode(public.key_package),
        invite_attestation: None,
    }
}
fn deliver_call_rotation(pair: &mut Pair, sequence: u64) {
    let alice = pair.alice.as_mut().unwrap();
    let batch = alice
        .stage_call_refresh(&pair.scope, call_refresh_target(), sequence - 1)
        .unwrap();
    let own = alice.device.identity().clone();
    alice.committed_control(&pair.scope, sequence).unwrap();
    pair.bob
        .as_mut()
        .unwrap()
        .apply_control(
            &pair.scope,
            &relay_protocol::CryptoEvent {
                sequence,
                group_id: batch.group_id,
                epoch: batch.epoch,
                kind: "commit".into(),
                author_user_id: own.user_id,
                author_device_id: own.device_id,
                recipient_device_id: None,
                body: batch.events[0].body.clone(),
                operation_id: batch.operation_id,
            },
            &pair.server,
            None,
        )
        .unwrap();
}

#[test]
fn participant_reopen_retries_exact_old_request_but_only_its_live_nonce_releases_keys() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    let users = ["alice".into(), "bob".into()];
    let old_sender = pair
        .alice
        .as_ref()
        .unwrap()
        .call_keys(&scope, &users)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    let request = bob
        .stage_call_challenge(&scope, call_refresh_target())
        .unwrap();
    let old_nonce = bob.call_challenges[&scope];
    assert!(bob.fresh_call_keys(&scope, &users).is_err());
    let old_delivery = call_delivery(&pair, request.clone());
    let allowed = [call_device(pair.bob.as_ref().unwrap())];
    pair.alice
        .as_mut()
        .unwrap()
        .receive_call_challenge(&scope, &old_delivery, &allowed)
        .unwrap();
    let saved = std::fs::read(&pair.alice.as_ref().unwrap().path).unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .receive_call_challenge(&scope, &old_delivery, &allowed)
        .unwrap();
    assert_eq!(
        std::fs::read(&pair.alice.as_ref().unwrap().path).unwrap(),
        saved,
        "idempotent receipt does not decrypt or advance a ratchet again"
    );
    pair.reopen_bob();
    let bob = pair.bob.as_mut().unwrap();
    assert!(bob.call_challenges.is_empty());
    assert_eq!(
        bob.stage_call_challenge(&scope, call_refresh_target())
            .unwrap(),
        request
    );
    assert!(
        bob.call_challenges.is_empty(),
        "an old checkpoint cannot adopt its nonce"
    );
    deliver_call_rotation(&mut pair, 4);
    assert!(pair
        .bob
        .as_ref()
        .unwrap()
        .fresh_call_keys(&scope, &users)
        .is_err());
    pair.bob
        .as_mut()
        .unwrap()
        .committed_call_request(&scope, &old_delivery)
        .unwrap();
    let request = pair
        .bob
        .as_mut()
        .unwrap()
        .stage_call_challenge(&scope, call_refresh_target())
        .unwrap();
    assert_ne!(
        pair.bob.as_ref().unwrap().call_challenges[&scope],
        old_nonce
    );
    let delivery = call_delivery(&pair, request);
    pair.bob
        .as_mut()
        .unwrap()
        .committed_call_request(&scope, &delivery)
        .unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .receive_call_challenge(&scope, &delivery, &allowed)
        .unwrap();
    drop(pair.alice.take());
    pair.alice =
        Some(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap());
    assert!(pair.alice.as_ref().unwrap().has_call_challenges(&scope));
    deliver_call_rotation(&mut pair, 5);
    let fresh_sender = pair
        .alice
        .as_ref()
        .unwrap()
        .fresh_call_keys(&scope, &users)
        .unwrap();
    let fresh_receiver = pair
        .bob
        .as_ref()
        .unwrap()
        .fresh_call_keys(&scope, &users)
        .unwrap();
    crate::media::actor_reopen_voice_probe(old_sender, fresh_sender, fresh_receiver).unwrap();
    assert!(!pair.alice.as_ref().unwrap().has_call_challenges(&scope));
    pair.reopen_bob();
    assert!(pair.bob.as_ref().unwrap().needs_call_refresh(&scope));
    assert!(pair.bob.as_ref().unwrap().call_challenges.is_empty());
}

#[test]
fn transport_replacement_invalidates_only_live_freshness_and_keeps_durable_operations() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    let bob = pair.bob.as_mut().unwrap();
    let request = bob
        .stage_call_challenge(&scope, call_refresh_target())
        .unwrap();
    let before = std::fs::read(&bob.path).unwrap();
    bob.fresh_call_scopes.insert(scope.clone());
    bob.invalidate_call_freshness(&scope);
    assert!(bob.needs_call_refresh(&scope));
    assert!(!bob.call_challenges.contains_key(&scope));
    assert!(!bob.call_challenge_attempts.contains_key(&scope));
    assert_eq!(bob.pending_call_request(&scope), Some(request));
    assert_eq!(std::fs::read(&bob.path).unwrap(), before);
    assert!(bob
        .fresh_call_keys(&scope, &["alice".into(), "bob".into()])
        .is_err());
    let alice = pair.alice.as_mut().unwrap();
    let pending = alice
        .stage_call_refresh(&scope, call_refresh_target(), alice.cursor(&scope))
        .unwrap();
    let before = std::fs::read(&alice.path).unwrap();
    alice.invalidate_call_freshness(&scope);
    assert_eq!(
        serde_json::to_value(alice.pending_control(&scope)).unwrap(),
        serde_json::to_value(Some(pending)).unwrap()
    );
    assert!(!alice.call_refresh_operations.contains_key(&scope));
    assert_eq!(std::fs::read(&alice.path).unwrap(), before);
}

#[test]
fn participant_request_ack_and_authentication_reject_changed_delivery_without_advancing_state() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    let request = pair
        .bob
        .as_mut()
        .unwrap()
        .stage_call_challenge(&scope, call_refresh_target())
        .unwrap();
    let delivery = call_delivery(&pair, request);
    let before = std::fs::read(&pair.bob.as_ref().unwrap().path).unwrap();
    for field in [
        "server",
        "author",
        "target",
        "body",
        "device",
        "group",
        "epoch",
        "operation",
    ] {
        let mut changed = delivery.clone();
        match field {
            "server" => changed.server_id.push_str("-other"),
            "author" => changed.author_user_id.push_str("-other"),
            "target" => changed.target.channel_id = Some("other".into()),
            "body" => changed.request.body.push_str("AA=="),
            "device" => changed.request.device_id.push_str("-other"),
            "group" => changed.request.group_id.push_str("-other"),
            "epoch" => changed.request.epoch += 1,
            "operation" => changed.request.operation_id.push_str("-other"),
            _ => unreachable!(),
        }
        assert!(
            pair.bob
                .as_mut()
                .unwrap()
                .committed_call_request(&scope, &changed)
                .is_err(),
            "changed {field} must preserve the exact request"
        );
        assert_eq!(
            std::fs::read(&pair.bob.as_ref().unwrap().path).unwrap(),
            before
        );
    }
    let allowed = [call_device(pair.bob.as_ref().unwrap())];
    let before = std::fs::read(&pair.alice.as_ref().unwrap().path).unwrap();
    let mut changed = delivery.clone();
    changed.request.operation_id.push_str("-copied");
    assert!(pair
        .alice
        .as_mut()
        .unwrap()
        .receive_call_challenge(&scope, &changed, &allowed)
        .is_err());
    assert_eq!(
        std::fs::read(&pair.alice.as_ref().unwrap().path).unwrap(),
        before
    );
    pair.alice
        .as_mut()
        .unwrap()
        .receive_call_challenge(&scope, &delivery, &allowed)
        .unwrap();
    let before = std::fs::read(&pair.alice.as_ref().unwrap().path).unwrap();
    assert!(pair
        .alice
        .as_mut()
        .unwrap()
        .receive_call_challenge(&scope, &changed, &allowed)
        .is_err());
    assert_eq!(
        std::fs::read(&pair.alice.as_ref().unwrap().path).unwrap(),
        before
    );
}

#[test]
fn participant_request_and_proof_storage_failures_never_release_freshness() {
    for stage in [true, false] {
        let mut pair = Pair::new();
        let scope = pair.scope.clone();
        if !stage {
            let request = pair
                .bob
                .as_mut()
                .unwrap()
                .stage_call_challenge(&scope, call_refresh_target())
                .unwrap();
            let delivery = call_delivery(&pair, request);
            let allowed = [call_device(pair.bob.as_ref().unwrap())];
            pair.alice
                .as_mut()
                .unwrap()
                .receive_call_challenge(&scope, &delivery, &allowed)
                .unwrap();
        }
        let bob = pair.bob.as_ref().unwrap();
        let path = bob.path.clone();
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        if stage {
            let bob = pair.bob.as_mut().unwrap();
            assert!(bob
                .stage_call_challenge(&scope, call_refresh_target())
                .is_err());
            assert!(bob.call_challenges.is_empty());
        } else {
            let alice = pair.alice.as_mut().unwrap();
            let own = alice.device.identity().clone();
            let batch = alice
                .stage_call_refresh(&scope, call_refresh_target(), 3)
                .unwrap();
            alice.committed_control(&scope, 4).unwrap();
            assert!(pair
                .bob
                .as_mut()
                .unwrap()
                .apply_control(
                    &scope,
                    &relay_protocol::CryptoEvent {
                        sequence: 4,
                        group_id: batch.group_id,
                        epoch: batch.epoch,
                        kind: "commit".into(),
                        author_user_id: own.user_id,
                        author_device_id: own.device_id,
                        body: batch.events[0].body.clone(),
                        operation_id: batch.operation_id,
                        ..Default::default()
                    },
                    &pair.server,
                    None
                )
                .is_err());
        }
        assert!(pair.bob.as_ref().unwrap().device.is_blocked());
        assert!(pair.bob.as_ref().unwrap().needs_call_refresh(&scope));
        assert!(pair
            .bob
            .as_ref()
            .unwrap()
            .fresh_call_keys(&scope, &["alice".into(), "bob".into()])
            .is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, saved).unwrap();
        pair.reopen_bob();
        assert!(pair.bob.as_ref().unwrap().needs_call_refresh(&scope));
        assert!(pair.bob.as_ref().unwrap().call_challenges.is_empty());
    }
}

pub(super) fn fill_history(session: &mut CryptoSession, scope: &str, count: usize) {
    session
        .mutate(|_, state| {
            for n in 0..count {
                let wire = format!("synthetic-authenticated-history-{n}");
                state.cached.insert(
                    cipher_hash(&wire),
                    CachedEnvelope {
                        author: state.identity.clone(),
                        envelope: Envelope {
                            version: 1,
                            scope: scope.into(),
                            client_id: format!("historic-{n}"),
                            content: format!("Synthetic protected history text {n}"),
                            reply_to_id: None,
                            mentions: Vec::new(),
                            attachments: Vec::new(),
                            action: None,
                        },
                        message_id: Some(format!("historic-message-{n}")),
                        sequence: n as u64 + 1,
                        delivery_revision: n as u64 + 1,
                    },
                );
            }
            state.delivery_clock = count as u64;
            Ok(())
        })
        .unwrap();
}

#[test]
fn portable_history_from_real_mls_archive_never_restores_send_identity_or_ratchets() {
    let mut pair = Pair::new();
    let scope = pair.scope.clone();
    let wires = pair
        .alice
        .as_mut()
        .unwrap()
        .mutate(|device, _| {
            (0..1100)
                .map(|n| {
                    let envelope = Envelope {
                        version: 1,
                        scope: scope.clone(),
                        client_id: format!("backup-send-{n}"),
                        content: format!("real MLS backup text {n}"),
                        reply_to_id: None,
                        mentions: vec![],
                        attachments: vec![],
                        action: None,
                    };
                    let plain = Zeroizing::new(serde_json::to_vec(&envelope).unwrap());
                    Ok(format!(
                        "{MESSAGE_PREFIX}{}",
                        STANDARD.encode(device.encrypt(&scope, &plain)?)
                    ))
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .unwrap();
    let ids: Vec<_> = (0..1100).map(|n| format!("backup-message-{n}")).collect();
    let references: Vec<_> = wires.iter().map(String::as_str).collect();
    let bob = pair.bob.as_mut().unwrap();
    let deliveries: Vec<_> = wires
        .iter()
        .zip(&ids)
        .enumerate()
        .map(|(n, (wire, id))| (wire.as_str(), id.as_str(), n as u64 + 1))
        .collect();
    for (page, delivered) in references.chunks(200).zip(deliveries.chunks(200)) {
        assert!(bob
            .decrypt_contents(&scope, page)
            .into_iter()
            .all(|row| row.is_ok()));
        assert!(bob
            .bind_deliveries(delivered)
            .into_iter()
            .all(|row| row.is_ok()));
    }
    pair.reopen_bob();
    let bob = pair.bob.as_ref().unwrap();
    let before = std::fs::read(&bob.path).unwrap();
    let fingerprint = bob.device.fingerprint().unwrap();
    let archive = bob.history_backup().unwrap();
    assert_eq!(archive.entries.len(), 1100);
    assert!(bob.history_usage().segments > 0);
    assert_eq!(bob.history_usage().records, 1100);
    let path = pair.root.join("real-history.relay-history");
    let password = "owned real history passphrase";
    crate::recovery::save(&path, &archive, password).unwrap();
    assert_eq!(std::fs::read(&bob.path).unwrap(), before);
    assert_eq!(bob.device.fingerprint().unwrap(), fingerprint);
    let origin = crate::network::CommandOrigin {
        url: "unused".into(),
        server_id: pair.server.clone(),
        user_id: "bob".into(),
    };
    let imported = crate::recovery::open(&path, password, Some(&origin)).unwrap();
    assert_eq!(
        imported.entries.first().unwrap().envelope.content,
        "real MLS backup text 0"
    );
    assert_eq!(
        imported.entries.last().unwrap().envelope.content,
        "real MLS backup text 1099"
    );
    assert_eq!(std::fs::read(&bob.path).unwrap(), before);
    let segment = history::archive_paths(&bob.state, &bob.path)[0].clone();
    let bytes = std::fs::read(&segment).unwrap();
    let mut changed = bytes.clone();
    changed[0] ^= 1;
    std::fs::write(&segment, &changed).unwrap();
    assert!(bob.history_backup().is_err());
    assert_eq!(std::fs::read(&bob.path).unwrap(), before);
    assert_eq!(
        crate::recovery::open(&path, password, Some(&origin))
            .unwrap()
            .entries
            .len(),
        1100
    );
    std::fs::write(segment, bytes).unwrap();
    drop(imported);
    drop(archive);
    pair.bob.take();
    let new = CryptoSession::reset_at(pair.root.clone(), &pair.server, "bob").unwrap();
    assert_ne!(new.device.fingerprint().unwrap(), fingerprint);
    let checkpoint = std::fs::read(&new.path).unwrap();
    for _ in 0..2 {
        let opened = crate::recovery::open(&path, password, Some(&origin)).unwrap();
        assert_eq!(opened.source.user_id, new.device.identity().user_id);
        assert_ne!(opened.source.device_id, new.device.identity().device_id);
        assert_eq!(std::fs::read(&new.path).unwrap(), checkpoint);
        assert!(new.device.group_state(&scope).is_none());
        assert!(new.state.outbox.is_empty());
        assert!(new
            .fresh_call_keys(&scope, &["alice".into(), "bob".into()])
            .is_err());
    }
    assert!(!new.device.is_blocked());
    pair.bob = Some(new);
}

#[test]
fn protected_history_archives_past_old_limit_reopens_and_preserves_send_receipts() {
    let mut pair = Pair::new();
    let envelope = pair.envelope("kept-pending", "pending ciphertext stays exact");
    let pending = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope.clone())
        .unwrap();
    fill_history(pair.alice.as_mut().unwrap(), &pair.scope, 8300);
    fill_history(pair.bob.as_mut().unwrap(), &pair.scope, 8300);
    let alice = pair.alice.as_mut().unwrap();
    assert!(alice.state.cached.len() <= history::ACTIVE_LIMIT);
    assert!(!alice.state.history.is_empty());
    assert!(alice.state.cached.contains_key(&cipher_hash(&pending)));
    assert_eq!(alice.encrypted_content(envelope.clone()).unwrap(), pending);
    assert!(
        alice
            .encrypted_content(Envelope {
                client_id: "historic-0".into(),
                ..envelope.clone()
            })
            .is_err(),
        "archived sends outside the retry window must never be encrypted again"
    );
    assert!(alice.state.completed.is_empty());
    let first = "synthetic-authenticated-history-0";
    let cached = alice.decrypt_content(&pair.scope, first).unwrap();
    assert_eq!(
        cached.envelope.content,
        "Synthetic protected history text 0"
    );
    assert!(alice.bind_delivery(first, "forged-binding", 999).is_err());
    let outgoing = pair.envelope("after-old-limit", "actual MLS after archive rollover");
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(outgoing)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(
        bob.decrypt_content(&pair.scope, &wire)
            .unwrap()
            .envelope
            .content,
        "actual MLS after archive rollover"
    );
    bob.bind_delivery(&wire, "new-message", 9000).unwrap();
    let revision = bob
        .decrypt_content(&pair.scope, &wire)
        .unwrap()
        .delivery_revision;
    assert!(revision > 8300);
    pair.reopen_bob();
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(bob.decrypt_content(&pair.scope, first).unwrap().sequence, 1);
    assert_eq!(
        bob.decrypt_content(&pair.scope, &wire)
            .unwrap()
            .delivery_revision,
        revision
    );
    assert_eq!(
        bob.cached_envelopes(&pair.scope)
            .map(Result::unwrap)
            .count(),
        8301
    );
    for path in history::archive_paths(&bob.state, &bob.path) {
        assert!(!std::fs::read(path)
            .unwrap()
            .windows(20)
            .any(|bytes| bytes == b"Synthetic protected "));
    }
    pair.alice.take();
    let mut alice = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    assert_eq!(alice.encrypted_content(envelope).unwrap(), pending);
    assert!(alice
        .encrypted_content(pair.envelope("historic-0", "different old send"))
        .is_err());
}

#[test]
fn protected_history_tamper_missing_files_and_foreign_segments_fail_closed() {
    let mut pair = Pair::new();
    fill_history(pair.bob.as_mut().unwrap(), &pair.scope, 1100);
    let bob = pair.bob.as_mut().unwrap();
    let segment = history::archive_paths(&bob.state, &bob.path)[0].clone();
    let original = std::fs::read(&segment).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 1;
    std::fs::write(&segment, &changed).unwrap();
    assert!(bob
        .decrypt_content(&pair.scope, "synthetic-authenticated-history-0")
        .is_err());
    assert!(bob
        .cached_envelopes(&pair.scope)
        .any(|entry| entry.is_err()));
    pair.bob.take();
    assert!(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "bob").is_err());
    std::fs::write(&segment, &original).unwrap();
    pair.reopen_bob();
    fill_history(pair.alice.as_mut().unwrap(), &pair.scope, 1100);
    let alice = pair.alice.as_ref().unwrap();
    let foreign = std::fs::read(&history::archive_paths(&alice.state, &alice.path)[0]).unwrap();
    std::fs::write(&segment, foreign).unwrap();
    assert!(pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_content(&pair.scope, "synthetic-authenticated-history-0")
        .is_err());
    std::fs::remove_file(&segment).unwrap();
    pair.bob.take();
    assert!(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "bob").is_err());
    std::fs::write(&segment, original).unwrap();
    pair.reopen_bob();
    assert!(pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_content(&pair.scope, "synthetic-authenticated-history-0")
        .is_ok());
}

#[test]
fn protected_history_failed_checkpoint_ignores_uncommitted_archive_and_pauses_sender() {
    let mut pair = Pair::new();
    let alice = pair.alice.as_mut().unwrap();
    let original_path = alice.path.clone();
    let original_bytes = std::fs::read(&original_path).unwrap();
    let original_revision = alice.state.revision;
    // A directory in place of the checkpoint makes its final replacement fail,
    // after immutable segments have been written and read back.
    std::fs::remove_file(&original_path).unwrap();
    std::fs::create_dir(&original_path).unwrap();
    let result = alice.mutate(|_, state| {
        for n in 0..1100 {
            state.cached.insert(
                cipher_hash(&format!("orphan-{n}")),
                CachedEnvelope {
                    author: state.identity.clone(),
                    envelope: Envelope {
                        version: 1,
                        scope: "orphan-scope".into(),
                        client_id: format!("orphan-{n}"),
                        content: "not committed".into(),
                        reply_to_id: None,
                        mentions: Vec::new(),
                        attachments: Vec::new(),
                        action: None,
                    },
                    message_id: None,
                    sequence: 0,
                    delivery_revision: 0,
                },
            );
        }
        Ok(())
    });
    assert!(result.is_err());
    assert!(alice.device.is_blocked());
    assert_eq!(alice.state.revision, original_revision);
    assert!(alice.state.history.is_empty());
    assert_eq!(
        protected::head(&alice.binding()).unwrap(),
        Some(original_revision)
    );
    std::fs::remove_dir(&original_path).unwrap();
    std::fs::write(&original_path, original_bytes).unwrap();
    pair.alice.take();
    let alice = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    assert!(alice.state.history.is_empty());
    assert!(history::lookup(
        &alice.state,
        &alice.path,
        &alice.binding(),
        &cipher_hash("orphan-0")
    )
    .unwrap()
    .is_none());
}

#[test]
fn protected_history_keeps_files_reactions_edit_order_and_migrates_v1_storage() {
    let mut pair = Pair::new();
    let (_, key) =
        relay_crypto::files::encrypt(b"synthetic archived attachment", "history-file").unwrap();
    let file = EncryptedAttachment {
        id: "archived-file".into(),
        name: "private-archived-name.txt".into(),
        mime: "text/plain".into(),
        file: key,
    };
    let mut envelope = pair.envelope("archived-original", "original private message");
    envelope.attachments.push(file.clone());
    envelope.mentions.push("bob".into());
    let first = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope.clone())
        .unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .committed(&envelope.client_id)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &first).unwrap();
    bob.bind_delivery(&first, "original-id", 1).unwrap();
    let first_revision = bob
        .decrypt_content(&pair.scope, &first)
        .unwrap()
        .delivery_revision;
    envelope.content = "edited private message".into();
    envelope.mentions.clear();
    let edited = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_edit_content(envelope.clone())
        .unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .committed(&envelope.client_id)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &edited).unwrap();
    bob.bind_delivery(&edited, "original-id", 1).unwrap();
    let edit_revision = bob
        .decrypt_content(&pair.scope, &edited)
        .unwrap()
        .delivery_revision;
    let mut action = pair.envelope("archived-reaction", "");
    action.action = Some(MessageAction::Reaction {
        message_id: "original-id".into(),
        target_client_id: Some("archived-original".into()),
        emoji: "<3".into(),
        active: true,
    });
    let reaction = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(action)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &reaction).unwrap();
    bob.bind_delivery(&reaction, "reaction-id", 2).unwrap();
    // Reproduce a valid legacy v1 checkpoint before its first archive rollover.
    bob.state.version = 1;
    bob.state.revision += 1;
    save(&bob.state, &bob.path, &bob.binding()).unwrap();
    pair.reopen_bob();
    assert_eq!(pair.bob.as_ref().unwrap().state.version, 1);
    fill_history(pair.bob.as_mut().unwrap(), &pair.scope, 1300);
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(
        bob.state.version, 2,
        "old clients must refuse archive checkpoints"
    );
    assert!(!bob.state.cached.contains_key(&cipher_hash(&first)));
    assert!(!bob.state.cached.contains_key(&cipher_hash(&edited)));
    assert_eq!(bob.file("archived-file").unwrap().unwrap().name, file.name);
    assert_eq!(
        bob.decrypt_content(&pair.scope, &first)
            .unwrap()
            .delivery_revision,
        first_revision
    );
    assert_eq!(
        bob.decrypt_content(&pair.scope, &edited)
            .unwrap()
            .delivery_revision,
        edit_revision
    );
    assert!(edit_revision > first_revision);
    let archived_reaction = bob
        .cached_envelopes(&pair.scope)
        .map(Result::unwrap)
        .find(|entry| entry.envelope.client_id == "archived-reaction")
        .unwrap();
    assert_eq!(archived_reaction.author.user_id, "alice");
    assert!(matches!(
        archived_reaction.envelope.action,
        Some(MessageAction::Reaction { active: true, .. })
    ));
    pair.reopen_bob();
    assert!(pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_content(&pair.scope, &edited)
        .unwrap()
        .envelope
        .mentions
        .is_empty());
    assert_eq!(
        pair.bob
            .as_ref()
            .unwrap()
            .file("archived-file")
            .unwrap()
            .unwrap()
            .file
            .binding,
        file.file.binding
    );
}

#[test]
fn protected_history_can_bind_a_previously_unbound_archived_ciphertext() {
    let mut pair = Pair::new();
    let envelope = pair.envelope(
        "unbound-archived",
        "authenticated before HTTP routing was available",
    );
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &wire).unwrap();
    bob.mutate(|_, state| {
        let template = state.cached[&cipher_hash(&wire)].clone();
        for n in 0..1200 {
            let mut other = template.clone();
            other.envelope.client_id = format!("unbound-fixture-{n}");
            other.delivery_revision = n + 1;
            state
                .cached
                .insert(cipher_hash(&format!("unbound-fixture-{n}")), other);
        }
        Ok(())
    })
    .unwrap();
    assert!(!bob.state.cached.contains_key(&cipher_hash(&wire)));
    assert!(bob
        .decrypt_content(&pair.scope, &wire)
        .unwrap()
        .message_id
        .is_none());
    bob.bind_delivery(&wire, "later-authenticated-delivery", 1300)
        .unwrap();
    let bound = bob.decrypt_content(&pair.scope, &wire).unwrap();
    assert_eq!(
        bound.message_id.as_deref(),
        Some("later-authenticated-delivery")
    );
    assert!(bound.delivery_revision > 1200);
    assert!(bob
        .bind_delivery(&wire, "different-delivery", 1301)
        .is_err());
    assert_eq!(
        bob.cached_envelopes(&pair.scope)
            .map(Result::unwrap)
            .count(),
        1
    );
    pair.reopen_bob();
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &wire)
            .unwrap()
            .sequence,
        1300
    );
}

#[test]
#[ignore = "actual MLS rollover fixture, explicitly run without personal hardware"]
fn actual_mls_history_crosses_8192_messages_and_reloads_archived_ciphertext() {
    let mut pair = Pair::new();
    let mut first = String::new();
    for base in (0..8400).step_by(200) {
        let scope = pair.scope.clone();
        let wires = pair
            .alice
            .as_mut()
            .unwrap()
            .mutate(|device, state| {
                let mut wires = Vec::new();
                for n in base..base + 200 {
                    let envelope = Envelope {
                        version: 1,
                        scope: scope.clone(),
                        client_id: format!("actual-history-{n}"),
                        content: format!("actual private MLS text {n}"),
                        reply_to_id: None,
                        mentions: Vec::new(),
                        attachments: Vec::new(),
                        action: None,
                    };
                    let body = serde_json::to_vec(&envelope).unwrap();
                    let wire = format!(
                        "{MESSAGE_PREFIX}{}",
                        STANDARD.encode(device.encrypt(&scope, &body)?)
                    );
                    state.cached.insert(
                        cipher_hash(&wire),
                        CachedEnvelope {
                            author: state.identity.clone(),
                            envelope,
                            message_id: Some(format!("actual-message-{n}")),
                            sequence: n as u64 + 1,
                            delivery_revision: n as u64 + 1,
                        },
                    );
                    state.delivery_clock = n as u64 + 1;
                    wires.push(wire);
                }
                Ok(wires)
            })
            .unwrap();
        if base == 0 {
            first = wires[0].clone();
        }
        let ids: Vec<_> = (base..base + 200)
            .map(|n| format!("actual-message-{n}"))
            .collect();
        let references: Vec<_> = wires.iter().map(String::as_str).collect();
        let bob = pair.bob.as_mut().unwrap();
        assert!(bob
            .decrypt_contents(&scope, &references)
            .into_iter()
            .all(|entry| entry.is_ok()));
        let deliveries: Vec<_> = wires
            .iter()
            .zip(&ids)
            .enumerate()
            .map(|(i, (wire, id))| (wire.as_str(), id.as_str(), (base + i) as u64 + 1))
            .collect();
        assert!(bob
            .bind_deliveries(&deliveries)
            .into_iter()
            .all(|entry| entry.is_ok()));
    }
    pair.reopen_bob();
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(
        bob.decrypt_content(&pair.scope, &first)
            .unwrap()
            .envelope
            .content,
        "actual private MLS text 0"
    );
    assert!(bob.state.cached.len() <= history::ACTIVE_LIMIT);
    assert_eq!(
        bob.cached_envelopes(&pair.scope)
            .map(Result::unwrap)
            .count(),
        8400
    );
    let final_envelope = pair.envelope("after-8400", "ordinary send after 8400 actual messages");
    let final_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(final_envelope)
        .unwrap();
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &final_wire)
            .unwrap()
            .envelope
            .content,
        "ordinary send after 8400 actual messages"
    );
}

#[test]
fn welcome_cursor_and_ratchets_restart_together_and_rejected_events_roll_back() {
    let mut pair = Pair::new();
    pair.reopen_bob();
    assert_eq!(pair.bob.as_ref().unwrap().cursor(&pair.scope), 3);
    assert_eq!(
        pair.bob
            .as_ref()
            .unwrap()
            .device
            .group_state(&pair.scope)
            .unwrap()
            .epoch,
        1
    );
    let envelope = pair.envelope("first", "synthetic private body");
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope)
        .unwrap();
    let event = relay_protocol::CryptoEvent {
        sequence: 4,
        epoch: 2,
        kind: "commit".into(),
        body: wire.strip_prefix(MESSAGE_PREFIX).unwrap().into(),
        ..Default::default()
    };
    let bob = pair.bob.as_mut().unwrap();
    assert!(bob
        .apply_control(&pair.scope, &event, &pair.server, None)
        .is_err());
    assert_eq!(bob.cursor(&pair.scope), 3);
    assert_eq!(
        bob.decrypt_content(&pair.scope, &wire)
            .unwrap()
            .envelope
            .content,
        "synthetic private body"
    );
    pair.reopen_bob();
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &wire)
            .unwrap()
            .envelope
            .content,
        "synthetic private body"
    );
}

#[test]
fn rejected_private_mutation_and_disk_failure_never_publish_or_keep_partial_state() {
    let mut pair = Pair::new();
    let alice = pair.alice.as_mut().unwrap();
    assert!(alice
        .mutate::<()>(|_, state| {
            state.cursors.insert("rejected".into(), 99);
            Err("synthetic rejection".into())
        })
        .is_err());
    assert_eq!(alice.cursor("rejected"), 0);
    assert!(
        CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").is_err(),
        "exclusive actor lock"
    );
    let blocked = pair.root.join("synthetic-blocked-destination");
    std::fs::create_dir(&blocked).unwrap();
    alice.path = blocked;
    let revision = alice.state.revision;
    let envelope = Envelope {
        version: 1,
        scope: pair.scope.clone(),
        client_id: "disk-failure".into(),
        content: "never publish this".into(),
        reply_to_id: None,
        mentions: Vec::new(),
        attachments: Vec::new(),
        action: None,
    };
    assert!(alice.encrypted_content(envelope.clone()).is_err());
    assert!(alice.device.is_blocked());
    assert_eq!(alice.state.revision, revision);
    assert!(!alice.state.outbox.contains_key("disk-failure"));
    assert!(alice.encrypted_content(envelope).is_err());
}

#[test]
fn received_file_manifests_do_not_overflow_pending_upload_storage_on_restart() {
    let mut pair = Pair::new();
    let (_, key) = relay_crypto::files::encrypt(b"owned synthetic file", "fixture-file").unwrap();
    let file = EncryptedAttachment {
        id: "incoming".into(),
        name: "private filename.txt".into(),
        mime: "text/plain".into(),
        file: key,
    };
    let bob = pair.bob.as_mut().unwrap();
    bob.mutate(|_, state| {
        for n in 0..1024 {
            let mut f = file.clone();
            f.id = format!("pending-{n}");
            state.files.insert(f.id.clone(), f);
        }
        Ok(())
    })
    .unwrap();
    let mut envelope = pair.envelope("file-message", "private file caption");
    envelope.attachments.push(file.clone());
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &wire).unwrap();
    assert_eq!(bob.state.files.len(), 1024);
    assert_eq!(bob.file("incoming").unwrap().unwrap().name, file.name);
    pair.reopen_bob();
    assert_eq!(
        pair.bob
            .as_ref()
            .unwrap()
            .file("incoming")
            .unwrap()
            .unwrap()
            .name,
        file.name
    );
    let store = std::fs::read(&pair.bob.as_ref().unwrap().path).unwrap();
    assert!(!store
        .windows(file.name.len())
        .any(|bytes| bytes == file.name.as_bytes()));
}

#[test]
fn acknowledged_ciphertext_retry_survives_restart_and_edits_use_a_new_generation() {
    let mut pair = Pair::new();
    let envelope = pair.envelope("retry", "original message");
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(envelope.clone())
        .unwrap();
    pair.alice.as_mut().unwrap().committed("retry").unwrap();
    pair.alice.take();
    pair.alice =
        Some(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap());
    assert_eq!(
        pair.alice
            .as_mut()
            .unwrap()
            .encrypted_content(envelope.clone())
            .unwrap(),
        wire
    );
    let mut edited = envelope;
    edited.content = "edited private message".into();
    assert!(pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(edited.clone())
        .is_err());
    let updated = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_edit_content(edited.clone())
        .unwrap();
    assert_ne!(wire, updated);
    assert_eq!(
        pair.alice
            .as_mut()
            .unwrap()
            .encrypted_edit_content(edited)
            .unwrap(),
        updated
    );
    pair.bob
        .as_mut()
        .unwrap()
        .decrypt_content(&pair.scope, &wire)
        .unwrap();
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &updated)
            .unwrap()
            .envelope
            .content,
        "edited private message"
    );
}

#[test]
fn changed_pending_edit_supersedes_ciphertext_durably_without_reusing_old_generations() {
    let mut pair = Pair::new();
    let original = pair.envelope("editable-original", "original private message");
    let original_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(original.clone())
        .unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .bind_delivery(&original_wire, "editable-message", 7)
        .unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .committed(&original.client_id)
        .unwrap();
    let mut first_edit = original.clone();
    first_edit.content = "pending first edit".into();
    let first_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_edit_content(first_edit.clone())
        .unwrap();
    assert_eq!(
        pair.alice
            .as_mut()
            .unwrap()
            .encrypted_edit_content(first_edit)
            .unwrap(),
        first_wire
    );
    let mut later_edit = original.clone();
    later_edit.content = "new explicit edit after a failed response".into();
    assert!(
        pair.alice
            .as_mut()
            .unwrap()
            .encrypted_content(later_edit.clone())
            .is_err(),
        "an ordinary send cannot replace a pending ID's content"
    );
    let mut wrong_scope = later_edit.clone();
    wrong_scope.scope = "another-conversation".into();
    assert!(
        pair.alice
            .as_mut()
            .unwrap()
            .encrypted_edit_content(wrong_scope)
            .is_err(),
        "an edit cannot move a pending intent to another conversation"
    );
    let later_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_edit_content(later_edit.clone())
        .unwrap();
    assert_ne!(first_wire, later_wire);
    assert_ne!(original_wire, later_wire);
    pair.alice.take();
    pair.alice =
        Some(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap());
    assert_eq!(
        pair.alice
            .as_mut()
            .unwrap()
            .encrypted_edit_content(later_edit)
            .unwrap(),
        later_wire,
        "restart preserves the latest pending edit's exact ciphertext"
    );
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(
        bob.decrypt_content(&pair.scope, &original_wire)
            .unwrap()
            .envelope
            .content,
        original.content
    );
    assert_eq!(
        bob.decrypt_content(&pair.scope, &first_wire)
            .unwrap()
            .envelope
            .content,
        "pending first edit"
    );
    assert_eq!(
        bob.decrypt_content(&pair.scope, &later_wire)
            .unwrap()
            .envelope
            .content,
        "new explicit edit after a failed response"
    );
    assert_eq!(
        pair.alice
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &first_wire)
            .unwrap()
            .envelope
            .content,
        "pending first edit",
        "superseding an intent retains an already published generation"
    );
    let reverted_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_edit_content(original.clone())
        .unwrap();
    assert_ne!(
        reverted_wire, original_wire,
        "reverting a pending edit cannot replay an older completed generation"
    );
    assert_ne!(reverted_wire, later_wire);
    assert_eq!(
        pair.alice
            .as_mut()
            .unwrap()
            .encrypted_edit_content(original)
            .unwrap(),
        reverted_wire
    );
}

#[test]
fn first_authenticated_delivery_orders_edit_versions_and_cached_reloads_do_not_promote_old_mentions(
) {
    let mut pair = Pair::new();
    let mut original = pair.envelope("mentioned-original", "original private mention");
    original.mentions.push("bob".into());
    let original_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(original.clone())
        .unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .committed(&original.client_id)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &original_wire).unwrap();
    bob.bind_delivery(&original_wire, "mentioned-message", 9)
        .unwrap();
    let original_revision = bob
        .decrypt_content(&pair.scope, &original_wire)
        .unwrap()
        .delivery_revision;
    let mut edit = original;
    edit.content = "edited private message without the mention".into();
    edit.mentions.clear();
    let edit_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_edit_content(edit)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    bob.decrypt_content(&pair.scope, &edit_wire).unwrap();
    bob.bind_delivery(&edit_wire, "mentioned-message", 9)
        .unwrap();
    let edit_revision = bob
        .decrypt_content(&pair.scope, &edit_wire)
        .unwrap()
        .delivery_revision;
    assert!(edit_revision > original_revision);
    bob.decrypt_content(&pair.scope, &original_wire).unwrap();
    bob.bind_delivery(&original_wire, "mentioned-message", 9)
        .unwrap();
    assert_eq!(
        bob.decrypt_content(&pair.scope, &original_wire)
            .unwrap()
            .delivery_revision,
        original_revision
    );
    pair.reopen_bob();
    let current = pair
        .bob
        .as_ref()
        .unwrap()
        .cached_envelopes(&pair.scope)
        .map(Result::unwrap)
        .filter(|cached| cached.message_id.as_deref() == Some("mentioned-message"))
        .max_by_key(|cached| cached.delivery_revision)
        .unwrap();
    assert_eq!(current.delivery_revision, edit_revision);
    assert!(
        current.envelope.mentions.is_empty(),
        "reopened observed latest edit excludes the old mention"
    );
}

#[test]
fn one_history_page_assigns_distinct_observed_revisions_to_original_and_edit() {
    let mut pair = Pair::new();
    let mut original = pair.envelope("same-page-original", "private mention");
    original.mentions.push("bob".into());
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(original.clone())
        .unwrap();
    pair.alice
        .as_mut()
        .unwrap()
        .committed(&original.client_id)
        .unwrap();
    original.content = "updated private message".into();
    original.mentions.clear();
    let edited = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_edit_content(original)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    assert!(bob
        .decrypt_contents(&pair.scope, &[&wire, &edited])
        .iter()
        .all(Result::is_ok));
    let deliveries = [
        (wire.as_str(), "same-page-message", 12),
        (edited.as_str(), "same-page-message", 12),
    ];
    assert!(bob.bind_deliveries(&deliveries).iter().all(Result::is_ok));
    let first = bob
        .decrypt_content(&pair.scope, &wire)
        .unwrap()
        .delivery_revision;
    let latest = bob
        .decrypt_content(&pair.scope, &edited)
        .unwrap()
        .delivery_revision;
    assert!(
        latest > first,
        "two versions bound in one checkpoint must not have an arbitrary tie"
    );
    assert!(bob
        .bind_deliveries(&[deliveries[1], deliveries[0]])
        .iter()
        .all(Result::is_ok));
    assert_eq!(
        bob.decrypt_content(&pair.scope, &wire)
            .unwrap()
            .delivery_revision,
        first
    );
    assert_eq!(
        bob.decrypt_content(&pair.scope, &edited)
            .unwrap()
            .delivery_revision,
        latest
    );
    assert!(bob
        .cached_envelopes(&pair.scope)
        .map(Result::unwrap)
        .max_by_key(|cached| cached.delivery_revision)
        .unwrap()
        .envelope
        .mentions
        .is_empty());
}

fn retired_files(root: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "retired")
        })
        .collect()
}

#[test]
fn explicit_device_reset_archives_corruption_advances_revision_and_rejects_old_snapshots() {
    let mut pair = Pair::new();
    let original = pair.alice.as_ref().unwrap();
    let path = original.path.clone();
    let original_public = original.advertised();
    let original_revision = original.state.revision;
    let binding = original.binding();
    assert!(
        CryptoSession::reset_at(pair.root.clone(), &pair.server, "alice").is_err(),
        "a live actor must retain its exclusive reset lock"
    );
    assert!(retired_files(&pair.root).is_empty());
    drop(pair.alice.take());

    let corrupted = b"owned synthetic corrupt DPAPI checkpoint";
    std::fs::write(&path, corrupted).unwrap();
    assert!(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), corrupted);
    assert_eq!(protected::head(&binding).unwrap(), Some(original_revision));
    assert!(
        retired_files(&pair.root).is_empty(),
        "a failed unlock must not replace keys"
    );

    let replacement = CryptoSession::reset_at(pair.root.clone(), &pair.server, "alice").unwrap();
    let first_public = replacement.advertised();
    assert_ne!(
        first_public.identity.device_id,
        original_public.identity.device_id
    );
    assert_ne!(first_public.fingerprint, original_public.fingerprint);
    assert_eq!(replacement.storage_root().unwrap(), pair.root);
    assert!(replacement.state.revision > original_revision);
    assert_eq!(
        protected::head(&binding).unwrap(),
        Some(replacement.state.revision)
    );
    let archives = retired_files(&pair.root);
    assert_eq!(archives.len(), 1);
    assert_eq!(std::fs::read(&archives[0]).unwrap(), corrupted);
    let first_valid_blob = std::fs::read(&path).unwrap();
    let first_revision = replacement.state.revision;
    drop(replacement);

    // A later explicit reset must preserve a valid old DPAPI blob too, and may
    // not restart the anti-rollback counter at zero with its fresh identity.
    let replacement = CryptoSession::reset_at(pair.root.clone(), &pair.server, "alice").unwrap();
    let final_public = replacement.advertised();
    assert_ne!(
        final_public.identity.device_id,
        first_public.identity.device_id
    );
    assert_ne!(final_public.fingerprint, first_public.fingerprint);
    assert!(replacement.state.revision > first_revision);
    let archives = retired_files(&pair.root);
    assert_eq!(archives.len(), 2);
    assert!(archives
        .iter()
        .any(|archive| std::fs::read(archive).unwrap() == first_valid_blob));
    let latest_blob = std::fs::read(&path).unwrap();
    let latest_revision = replacement.state.revision;
    drop(replacement);
    std::fs::write(&path, &first_valid_blob).unwrap();
    assert!(
        CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").is_err(),
        "reset must not permit restoring the retired identity's older counter"
    );
    assert_eq!(protected::head(&binding).unwrap(), Some(latest_revision));
    assert_eq!(std::fs::read(&path).unwrap(), first_valid_blob);
    std::fs::write(&path, &latest_blob).unwrap();
    let reopened = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    assert_eq!(reopened.advertised().identity, final_public.identity);
    assert_eq!(reopened.advertised().fingerprint, final_public.fingerprint);
    pair.alice = Some(reopened);
}

#[test]
fn lost_revision_marker_preserves_existing_keys_until_explicit_reset_and_blocks_retired_copies() {
    let mut pair = Pair::new();
    let original = pair.alice.as_ref().unwrap();
    let path = original.path.clone();
    let binding = original.binding();
    let original_public = original.advertised();
    let original_blob = std::fs::read(&path).unwrap();
    assert!(protected::head(&binding).unwrap().is_some());
    drop(pair.alice.take());

    // Remove only this fixture's random server/account marker. A valid DPAPI
    // file alone must never silently disable the account's rollback boundary.
    protected::remove_head(&binding);
    assert_eq!(protected::head(&binding).unwrap(), None);
    assert!(
        CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").is_err(),
        "an existing checkpoint with a missing marker requires explicit recovery"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original_blob);
    assert_eq!(protected::head(&binding).unwrap(), None);
    assert!(retired_files(&pair.root).is_empty());

    let copy_root = pair.root.join("owned-historical-copy");
    std::fs::create_dir_all(&copy_root).unwrap();
    let copied_path = copy_root.join(path.file_name().unwrap());
    std::fs::write(&copied_path, &original_blob).unwrap();
    assert!(
        CryptoSession::open_for_test(copy_root.clone(), &pair.server, "alice").is_err(),
        "a copied historical checkpoint cannot bypass a missing account marker"
    );
    assert_eq!(std::fs::read(&copied_path).unwrap(), original_blob);
    assert!(retired_files(&copy_root).is_empty());

    // This is the storage operation used after the UI/Worker validates the
    // typed, account-scoped reset. It preserves the old bytes and creates keys.
    let replacement = CryptoSession::reset_at(pair.root.clone(), &pair.server, "alice").unwrap();
    let replacement_public = replacement.advertised();
    assert_ne!(
        replacement_public.identity.device_id,
        original_public.identity.device_id
    );
    assert_ne!(replacement_public.fingerprint, original_public.fingerprint);
    let replacement_revision = replacement.state.revision;
    assert_eq!(
        protected::head(&binding).unwrap(),
        Some(replacement_revision)
    );
    let archives = retired_files(&pair.root);
    assert_eq!(archives.len(), 1);
    assert_eq!(std::fs::read(&archives[0]).unwrap(), original_blob);
    let replacement_blob = std::fs::read(&path).unwrap();
    drop(replacement);

    assert!(
        CryptoSession::open_for_test(copy_root, &pair.server, "alice").is_err(),
        "a reset must not make a retired device copy acceptable under its new marker"
    );
    assert_eq!(std::fs::read(&copied_path).unwrap(), original_blob);
    assert_eq!(
        protected::head(&binding).unwrap(),
        Some(replacement_revision)
    );
    let reopened = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    assert_eq!(reopened.advertised().identity, replacement_public.identity);
    assert_eq!(
        reopened.advertised().fingerprint,
        replacement_public.fingerprint
    );
    assert_eq!(std::fs::read(&path).unwrap(), replacement_blob);
    pair.alice = Some(reopened);
}

#[test]
fn legacy_counter_marker_is_only_a_reset_floor_and_cannot_authorize_existing_keys() {
    let mut pair = Pair::new();
    let original = pair.alice.as_ref().unwrap();
    let binding = original.binding();
    let revision = original.state.revision;
    let original_public = original.advertised();
    let path = original.path.clone();
    let blob = std::fs::read(&path).unwrap();
    drop(pair.alice.take());
    protected::legacy_head_for_test(&binding, revision).unwrap();
    assert_eq!(protected::head(&binding).unwrap(), Some(revision));
    assert!(protected::check_identity(&binding, &original_public.identity.device_id).is_err());
    assert!(CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), blob);
    assert!(retired_files(&pair.root).is_empty());

    let replacement = CryptoSession::reset_at(pair.root.clone(), &pair.server, "alice").unwrap();
    assert!(replacement.state.revision > revision);
    let replacement_public = replacement.advertised();
    assert_ne!(replacement_public.identity, original_public.identity);
    assert!(protected::check_identity(&binding, &replacement_public.identity.device_id).is_ok());
    assert!(protected::check_identity(&binding, &original_public.identity.device_id).is_err());
    let archives = retired_files(&pair.root);
    assert_eq!(archives.len(), 1);
    assert_eq!(std::fs::read(&archives[0]).unwrap(), blob);
    drop(replacement);
    let reopened = CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").unwrap();
    assert_eq!(reopened.advertised().identity, replacement_public.identity);
    pair.alice = Some(reopened);
}

#[test]
fn authenticated_cached_history_reloads_without_checkpoint_writes() {
    let mut pair = Pair::new();
    let mut wires = Vec::new();
    for n in 0..24 {
        let envelope = pair.envelope(
            &format!("batch-{n}"),
            &format!("synthetic batch private message {n}"),
        );
        wires.push(
            pair.alice
                .as_mut()
                .unwrap()
                .encrypted_content(envelope)
                .unwrap(),
        );
    }
    let wire_refs: Vec<_> = wires.iter().map(String::as_str).collect();
    let message_ids: Vec<_> = (0..wires.len()).map(|n| format!("message-{n}")).collect();
    let deliveries: Vec<_> = wires
        .iter()
        .zip(&message_ids)
        .enumerate()
        .map(|(n, (wire, id))| (wire.as_str(), id.as_str(), n as u64 + 1))
        .collect();
    let before = pair.bob.as_ref().unwrap().state.revision;
    let first_start = std::time::Instant::now();
    for (n, received) in pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_contents(&pair.scope, &wire_refs)
        .into_iter()
        .enumerate()
    {
        assert_eq!(received.unwrap().envelope.client_id, format!("batch-{n}"));
    }
    assert!(pair
        .bob
        .as_mut()
        .unwrap()
        .bind_deliveries(&deliveries)
        .iter()
        .all(Result::is_ok));
    let first_elapsed = first_start.elapsed();
    let bob = pair.bob.as_ref().unwrap();
    let checkpoints = bob.state.revision - before;
    assert!((1..=2).contains(&checkpoints), "one bounded page must preserve ratchets and delivery binding without per-message disk writes");
    let saved = std::fs::read(&bob.path).unwrap();
    let revision = bob.state.revision;
    pair.reopen_bob();
    let cached_start = std::time::Instant::now();
    for (n, received) in pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_contents(&pair.scope, &wire_refs)
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            received.unwrap().envelope.content,
            format!("synthetic batch private message {n}")
        );
    }
    assert!(pair
        .bob
        .as_mut()
        .unwrap()
        .bind_deliveries(&deliveries)
        .iter()
        .all(Result::is_ok));
    let cached_elapsed = cached_start.elapsed();
    let bob = pair.bob.as_ref().unwrap();
    assert_eq!(bob.state.revision, revision);
    assert_eq!(protected::head(&bob.binding()).unwrap(), Some(revision));
    assert_eq!(std::fs::read(&bob.path).unwrap(), saved);
    assert_eq!(bob.cached_envelopes(&pair.scope).count(), wires.len());
    std::fs::write(
        pair.root.join("cached-history-evidence.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "messages":wires.len(), "first_delivery_checkpoint_count":checkpoints,
            "first_delivery_ms":first_elapsed.as_secs_f64()*1000.0,
            "cached_reload_ms":cached_elapsed.as_secs_f64()*1000.0,
            "cached_reload_checkpoint_count":0, "checkpoint_bytes":saved.len(),
            "method":"one bounded decrypt_contents/bind_deliveries page",
            "scope":"synthetic debug actor fixture, not a release performance benchmark"
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn batched_receive_isolates_invalid_wire_and_rejects_conflicting_same_page_delivery_ids() {
    let mut pair = Pair::new();
    let first = pair.envelope("batch-first", "owned first batch plaintext");
    let next = pair.envelope("batch-next", "owned next batch plaintext");
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(first)
        .unwrap();
    let next_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(next)
        .unwrap();
    let invalid = format!("{MESSAGE_PREFIX}AAAA");
    let revision = pair.bob.as_ref().unwrap().state.revision;
    let results = pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_contents(&pair.scope, &[&wire, &invalid, &next_wire, &wire]);
    assert_eq!(results.len(), 4);
    assert_eq!(
        results[0].as_ref().unwrap().envelope.content,
        "owned first batch plaintext"
    );
    assert!(results[1].is_err());
    assert_eq!(
        results[2].as_ref().unwrap().envelope.content,
        "owned next batch plaintext"
    );
    assert_eq!(
        results[3].as_ref().unwrap().envelope.client_id,
        "batch-first"
    );
    assert_eq!(pair.bob.as_ref().unwrap().state.revision, revision + 1);
    let results = pair.bob.as_mut().unwrap().bind_deliveries(&[
        (&wire, "first-id", 1),
        (&wire, "attacker-rebinding", 999),
        (&next_wire, "next-id", 2),
        (&wire, "first-id", 1),
    ]);
    assert!(results[0].is_ok());
    assert!(results[1].is_err());
    assert!(results[2].is_ok());
    assert!(results[3].is_ok());
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &wire)
            .unwrap()
            .message_id
            .as_deref(),
        Some("first-id")
    );
    pair.reopen_bob();
    let first = pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_content(&pair.scope, &wire)
        .unwrap();
    assert_eq!(first.message_id.as_deref(), Some("first-id"));
    assert_eq!(first.sequence, 1);
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &next_wire)
            .unwrap()
            .message_id
            .as_deref(),
        Some("next-id")
    );
    let later = pair.envelope("batch-later", "owned later batch plaintext");
    let later_wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(later)
        .unwrap();
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &later_wire)
            .unwrap()
            .envelope
            .content,
        "owned later batch plaintext"
    );
}

#[test]
fn batched_receive_disk_failure_returns_no_plaintext_and_preserves_durable_ratchets() {
    let mut pair = Pair::new();
    let one = pair.envelope("batch-disk-one", "not published first plaintext");
    let two = pair.envelope("batch-disk-two", "not published second plaintext");
    let wire = pair.alice.as_mut().unwrap().encrypted_content(one).unwrap();
    let second = pair.alice.as_mut().unwrap().encrypted_content(two).unwrap();
    let bob = pair.bob.as_mut().unwrap();
    let real_path = bob.path.clone();
    let saved = std::fs::read(&real_path).unwrap();
    let revision = bob.state.revision;
    let blocked = pair.root.join("batch-disk-directory");
    std::fs::create_dir(&blocked).unwrap();
    bob.path = blocked;
    let results = bob.decrypt_contents(&pair.scope, &[&wire, &second]);
    assert!(
        results.iter().all(Result::is_err),
        "no decrypted result may escape a failed page checkpoint"
    );
    assert!(bob.device.is_blocked());
    assert_eq!(bob.state.revision, revision);
    assert!(bob.state.cached.is_empty());
    assert_eq!(std::fs::read(&real_path).unwrap(), saved);
    assert_eq!(protected::head(&bob.binding()).unwrap(), Some(revision));
    pair.reopen_bob();
    let results = pair
        .bob
        .as_mut()
        .unwrap()
        .decrypt_contents(&pair.scope, &[&wire, &second]);
    assert_eq!(
        results[0].as_ref().unwrap().envelope.content,
        "not published first plaintext"
    );
    assert_eq!(
        results[1].as_ref().unwrap().envelope.content,
        "not published second plaintext"
    );
}

#[test]
fn concurrent_distinct_revision_markers_do_not_observe_older_writes() {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
    let mut workers = Vec::new();
    for _ in 0..6 {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let binding = format!(
                "Relay/crypto/v1/{}/marker-only-fixture",
                uuid::Uuid::new_v4()
            );
            let url = format!("https://{}.relay-test.invalid", uuid::Uuid::new_v4());
            let session = relay_protocol::Session {
                token: "owned-concurrent-vault-fixture".into(),
                user: relay_protocol::User {
                    id: "owned-concurrent-user".into(),
                    ..Default::default()
                },
            };
            let mut failures = Vec::new();
            for revision in 1..=16 {
                barrier.wait();
                if let Err(error) = protected::set_head(&binding, revision, "marker-fixture-device")
                {
                    failures.push(format!("write {revision}: {error}"));
                }
                if let Err(error) = crate::credential_vault::save(
                    &url,
                    "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b",
                    &session,
                ) {
                    failures.push(format!("session save: {error}"));
                }
                match protected::head(&binding) {
                    Ok(Some(actual)) if actual == revision => {}
                    other => failures.push(format!("after writing {revision}, got {other:?}")),
                }
                if let Err(error) = crate::credential_vault::forget(&url) {
                    failures.push(format!("session forget: {error}"));
                }
            }
            protected::remove_head(&binding);
            failures
        }));
    }
    let failures: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    assert!(
        failures.is_empty(),
        "distinct-account anti-rollback markers lost writes: {failures:?}"
    );
}

#[test]
fn revision_marker_rejects_equal_or_older_writes() {
    let binding = format!(
        "Relay/crypto/v1/{}/marker-counter-fixture",
        uuid::Uuid::new_v4()
    );
    protected::set_head(&binding, 7, "marker-fixture-device").unwrap();
    assert!(protected::check_identity(&binding, "marker-fixture-device").is_ok());
    assert!(protected::check_identity(&binding, "retired-fixture-device").is_err());
    let equal = protected::set_head(&binding, 7, "marker-fixture-device");
    let older = protected::set_head(&binding, 6, "marker-fixture-device");
    let observed = protected::head(&binding);
    protected::set_head(&binding, 8, "replacement-fixture-device").unwrap();
    assert!(protected::check_identity(&binding, "replacement-fixture-device").is_ok());
    assert!(protected::check_identity(&binding, "marker-fixture-device").is_err());
    protected::remove_head(&binding);
    assert!(
        equal.is_err(),
        "copied actors may not both publish the same checkpoint generation"
    );
    assert!(older.is_err());
    assert_eq!(observed.unwrap(), Some(7));
}

#[test]
fn copied_current_checkpoint_cannot_open_a_second_actor_in_another_root() {
    if let (Some(root), Ok(server)) = (
        std::env::var_os("RELAY_ACTOR_LEASE_CHILD_ROOT"),
        std::env::var("RELAY_ACTOR_LEASE_CHILD_SERVER"),
    ) {
        assert!(
            CryptoSession::open_for_test(PathBuf::from(root), &server, "alice").is_err(),
            "a separate process may not fork the live actor through a copied checkpoint"
        );
        return;
    }
    let mut pair = Pair::new();
    let actor = pair.alice.as_ref().unwrap();
    let copied_root = pair.root.join("copied-current-actor");
    std::fs::create_dir(&copied_root).unwrap();
    let copied_path = copied_root.join(actor.path.file_name().unwrap());
    let original = std::fs::read(&actor.path).unwrap();
    std::fs::write(&copied_path, &original).unwrap();
    assert!(
        CryptoSession::open_for_test(copied_root.clone(), &pair.server, "alice").is_err(),
        "path changes cannot fork a live actor's nonce/ratchet ownership"
    );
    assert!(
        CryptoSession::reset_at(copied_root.clone(), &pair.server, "alice").is_err(),
        "a copied path cannot reset behind the live owner's lifetime lease"
    );
    assert_eq!(std::fs::read(&copied_path).unwrap(), original);
    assert!(retired_files(&copied_root).is_empty());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "e2ee::tests::copied_current_checkpoint_cannot_open_a_second_actor_in_another_root",
            "--exact",
            "--nocapture",
        ])
        .env("RELAY_ACTOR_LEASE_CHILD_ROOT", &copied_root)
        .env("RELAY_ACTOR_LEASE_CHILD_SERVER", &pair.server);
    {
        use std::os::windows::process::CommandExt;
        child.creation_flags(0x0800_0000);
    }
    child
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = child.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("separate actor ownership check timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let child = child.wait_with_output().unwrap();
    assert!(
        child.status.success(),
        "separate actor ownership regression failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    drop(pair.alice.take());
    // Once the real owner exits, a current valid copy is a legitimate restore.
    // Its global marker advances, making the old original snapshot unusable.
    let mut restored = CryptoSession::open_for_test(copied_root, &pair.server, "alice").unwrap();
    restored.set_cursor("copied-fixture", 1).unwrap();
    drop(restored);
    assert!(
        CryptoSession::open_for_test(pair.root.clone(), &pair.server, "alice").is_err(),
        "the previous path must not revive its stale snapshot after a restore advances"
    );
}
