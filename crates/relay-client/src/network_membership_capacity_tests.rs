//! Actual bounded HTTP admission. All accounts and known fingerprints are fixtures.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};

fn context(actor: &CryptoWorker, target: &Target, after: u64) -> CryptoContext {
    parse(context_request(actor, target, after).send().unwrap()).unwrap()
}
fn context_request(
    actor: &CryptoWorker,
    target: &Target,
    after: u64,
) -> reqwest::blocking::RequestBuilder {
    let Target::Channel(channel) = target else {
        panic!("owned channel expected")
    };
    actor
        .worker
        .request(reqwest::Method::GET, "/api/e2ee/context")
        .unwrap()
        .query(&[
            ("device_id", actor.own().identity.device_id),
            ("channel_id", channel.clone()),
            ("after", after.to_string()),
        ])
}
fn assert_message(actor: &mut CryptoWorker, target: &Target, wanted: &str) {
    let messages = actor.history(target);
    assert!(
        messages.iter().any(|m| m.content == wanted),
        "all approved devices must decrypt the fixture message"
    );
    assert!(
        messages
            .iter()
            .all(|m| m.content != "Encrypted message unavailable"),
        "capacity catch-up must not discard an epoch"
    );
}
pub(super) fn run() {
    let started = Instant::now();
    let server = CryptoServer::start();
    let mut registration_window = Instant::now();
    let mut actors = vec![CryptoWorker::new(
        &server,
        register(&server, "capacity_http_00", ""),
    )];
    for index in 1..64 {
        // All synthetic clients share localhost's actual 15 sign-ins/minute
        // budget. Pace new registrations; never disable limits or retry 429s.
        if index % 15 == 0 {
            println!("Owned capacity setup: {index} accounts; respecting sign-in cooldown");
            if let Some(delay) = Duration::from_secs(61).checked_sub(registration_window.elapsed())
            {
                thread::sleep(delay);
            }
            registration_window = Instant::now();
        }
        let invite: serde_json::Value = actors[0].worker.post("/api/invites", &json!({})).unwrap();
        actors.push(CryptoWorker::new(
            &server,
            register(
                &server,
                &format!("capacity_http_{index:02}"),
                invite["code"].as_str().unwrap(),
            ),
        ));
    }
    let bootstrap: Bootstrap = actors[0].worker.get("/api/bootstrap").unwrap();
    let channel = bootstrap
        .channels
        .iter()
        .find(|c| c.kind == "text")
        .unwrap()
        .id
        .clone();
    let target = Target::Channel(channel.clone());
    let crypto_target = CryptoTarget {
        channel_id: Some(channel),
        peer_id: None,
    };
    let scope = actors[0].worker.crypto_scope(&target).unwrap();
    let public: Vec<_> = actors.iter().map(CryptoWorker::own).collect();
    // Full known fingerprints approved in one durable mutation per actor. This
    // deliberately does not claim to exercise user onboarding or trust exchange.
    for actor in &mut actors {
        actor
            .worker
            .crypto()
            .unwrap()
            .with_device(|device| {
                for peer in &public {
                    device.approve(peer, &peer.fingerprint)?;
                }
                Ok(())
            })
            .unwrap();
    }
    let setup_ms = started.elapsed().as_millis();
    let initial = context(&actors[0], &target, 0);
    assert_eq!(initial.devices.len(), 64);
    let own_id = actors[0].own().identity.device_id;
    let packages: Vec<_> = initial
        .devices
        .iter()
        .filter(|p| p.device_id != own_id)
        .map(|p| relay_crypto::PublicDevice {
            identity: relay_crypto::Identity {
                server_id: initial.server_id.clone(),
                user_id: p.user_id.clone(),
                device_id: p.device_id.clone(),
            },
            fingerprint: p.identity.clone(),
            key_package: STANDARD.decode(&p.key_package).unwrap(),
        })
        .collect();
    let actor = actors[0].worker.crypto().unwrap();
    let binding = format!(
        "Relay/crypto/v1/{}/{}",
        actor.device.identity().server_id,
        actor.device.identity().user_id
    );
    let checkpoint = actor
        .storage_root()
        .unwrap()
        .join(format!("{:x}.device", Sha256::digest(binding.as_bytes())));
    let saved = fs::read(&checkpoint).unwrap();
    let original_identity = actor.device.identity().clone();
    let revision = actor.checkpoint_revision_for_test();
    // Reproduce the old unbounded actual MLS operation, now rejected inside the
    // isolated transaction before persisting a group or an undeliverable outbox.
    let error = actor
        .stage_membership(
            &scope,
            crypto_target.clone(),
            "owned-oversized-attempt".into(),
            0,
            &packages,
            &[],
            true,
        )
        .unwrap_err();
    assert!(error.contains("delivery limit"), "{error}");
    assert_eq!(fs::read(&checkpoint).unwrap(), saved);
    assert_eq!(actor.checkpoint_revision_for_test(), revision);
    assert_eq!(actor.device.identity(), &original_identity);
    assert!(actor.device.group_state(&scope).is_none());
    assert!(actor.pending_control(&scope).is_none());
    assert!(!actor.device.is_blocked());
    assert_eq!(context(&actors[0], &target, 0).sequence, 0);

    let _ = encrypted::take_control_delivery_sizes();
    let admission_at = Instant::now();
    // Acknowledge the first production-sized portion and pause the controller.
    // A welcomed participant must not send while its other verified recipients
    // are still absent. The controller then resumes from this durable prefix.
    actors[0]
        .worker
        .crypto()
        .unwrap()
        .stage_membership(
            &scope,
            crypto_target.clone(),
            uuid::Uuid::new_v4().to_string(),
            0,
            &packages[..16],
            &[],
            true,
        )
        .unwrap();
    actors[0].worker.flush_crypto_control(&scope).unwrap();
    let welcomed = actors
        .iter()
        .position(|a| a.own().identity.device_id == packages[0].identity.device_id)
        .unwrap();
    let partial_error = actors[welcomed]
        .worker
        .ensure_crypto_conversation(&target)
        .unwrap_err();
    assert!(
        partial_error.contains("finish adding your verified devices"),
        "{partial_error}"
    );
    assert_eq!(
        actors[welcomed]
            .worker
            .crypto()
            .unwrap()
            .device
            .members(&scope)
            .unwrap()
            .len(),
        17
    );
    let origin = actors[welcomed].origin();
    assert!(actors[welcomed]
        .worker
        .handle(Command::SendRich {
            binding: None,
            origin,
            target: target.clone(),
            content: "must not escape partial admission".into(),
            attachment_ids: Vec::new(),
            client_id: "capacity-http-partial-blocked".into(),
            reply_to_id: None,
            mentions: Vec::new(),
        })
        .is_err());
    assert!(actors[welcomed].raw_messages(&target).is_empty());
    let joined = actors[0]
        .worker
        .ensure_crypto_conversation(&target)
        .unwrap();
    let admission_ms = admission_at.elapsed().as_millis();
    let deliveries = encrypted::take_control_delivery_sizes();
    assert_eq!(deliveries.len(), 4);
    assert_eq!(
        deliveries
            .iter()
            .map(|(_, _, welcomes)| welcomes)
            .sum::<usize>(),
        63
    );
    for (json_bytes, decoded_bytes, welcomes) in &deliveries {
        assert!(*json_bytes <= relay_protocol::CRYPTO_MAX_OPERATION_JSON_BYTES);
        assert!(*decoded_bytes <= relay_protocol::CRYPTO_MAX_OPERATION_DECODED_BYTES);
        assert!(*welcomes <= 16);
    }
    assert_eq!(joined.sequence, 68);
    assert_eq!(
        actors[0]
            .worker
            .crypto()
            .unwrap()
            .device
            .members(&scope)
            .unwrap()
            .len(),
        64
    );
    assert!(actors[0]
        .worker
        .crypto()
        .unwrap()
        .pending_control(&scope)
        .is_none());
    let first = "owned HTTP 64-device first encrypted message";
    actors[0].send(&target, first, Vec::new(), "capacity-http-first", None);
    let catchup_at = Instant::now();
    for actor in &mut actors {
        assert_message(actor, &target, first);
        assert_eq!(
            actor
                .worker
                .crypto()
                .unwrap()
                .device
                .members(&scope)
                .unwrap()
                .len(),
            64
        );
    }
    let catchup_ms = catchup_at.elapsed().as_millis();
    let reopen_at = Instant::now();
    for actor in &mut actors {
        let identity = actor.own().identity;
        actor.reopen_keys();
        assert_eq!(actor.own().identity, identity);
        assert_eq!(
            actor
                .worker
                .crypto()
                .unwrap()
                .device
                .members(&scope)
                .unwrap()
                .len(),
            64
        );
    }
    let second = "owned HTTP 64-device after all actors reopen";
    actors[0].send(&target, second, Vec::new(), "capacity-http-second", None);
    for actor in &mut actors {
        assert_message(actor, &target, second);
    }
    let reopen_ms = reopen_at.elapsed().as_millis();

    // One additional real signing device on an existing account exercises the
    // device quota without exceeding the independent 64-account roster limit.
    let mut extra = relay_crypto::Device::new(relay_crypto::Identity {
        server_id: bootstrap.server_id.clone(),
        user_id: actors[1].own().identity.user_id,
        device_id: uuid::Uuid::new_v4().to_string(),
    })
    .unwrap();
    let extra_public = extra.key_package().unwrap();
    let _: CryptoDevice = parse(
        actors[1]
            .worker
            .request(reqwest::Method::PUT, "/api/e2ee/device")
            .unwrap()
            .json(&RegisterCryptoDevice {
                device_id: extra_public.identity.device_id.clone(),
                identity: extra_public.fingerprint.clone(),
                key_package: STANDARD.encode(&extra_public.key_package),
            })
            .send()
            .unwrap(),
    )
    .unwrap();
    actors[0]
        .worker
        .crypto()
        .unwrap()
        .with_device(|d| d.approve(&extra_public, &extra_public.fingerprint))
        .unwrap();
    let before_limit = serde_json::to_vec(
        actors[0]
            .worker
            .crypto()
            .unwrap()
            .device
            .group_state(&scope)
            .unwrap(),
    )
    .unwrap();
    let limit_error = actors[0]
        .worker
        .ensure_crypto_conversation(&target)
        .unwrap_err();
    assert!(limit_error.contains("64 devices"), "{limit_error}");
    assert_eq!(
        context(&actors[0], &target, joined.sequence).sequence,
        joined.sequence
    );
    assert_eq!(
        serde_json::to_vec(
            actors[0]
                .worker
                .crypto()
                .unwrap()
                .device
                .group_state(&scope)
                .unwrap()
        )
        .unwrap(),
        before_limit
    );
    assert!(actors[0]
        .worker
        .crypto()
        .unwrap()
        .pending_control(&scope)
        .is_none());
    assert_eq!(
        actors[0]
            .worker
            .crypto()
            .unwrap()
            .device
            .members(&scope)
            .unwrap()
            .len(),
        64
    );
    checked(
        actors[1]
            .worker
            .request(
                reqwest::Method::DELETE,
                &format!("/api/e2ee/device/{}", extra_public.identity.device_id),
            )
            .unwrap()
            .send()
            .unwrap(),
    )
    .unwrap();

    // The removed account revokes its own fixture device through the actual API.
    // Keep the controller's acknowledged server commit pending locally to model
    // a lost response, then reopen and confirm the exact idempotent operation.
    let removed_id = actors[63].own().identity.device_id;
    checked(
        actors[63]
            .worker
            .request(
                reqwest::Method::DELETE,
                &format!("/api/e2ee/device/{removed_id}"),
            )
            .unwrap()
            .send()
            .unwrap(),
    )
    .unwrap();
    let cursor = actors[0].worker.crypto().unwrap().cursor(&scope);
    let current = context(&actors[0], &target, cursor);
    let batch = actors[0]
        .worker
        .crypto()
        .unwrap()
        .stage_membership(
            &scope,
            crypto_target,
            current.group_id.unwrap(),
            current.sequence,
            &[],
            &[removed_id],
            false,
        )
        .unwrap();
    let acknowledged: CryptoContext = actors[0].worker.post("/api/e2ee/events", &batch).unwrap();
    let ack_sequence = acknowledged.sequence;
    let pending_saved = serde_json::to_vec(&batch).unwrap();
    actors[0].reopen_keys();
    assert_eq!(
        serde_json::to_vec(
            &actors[0]
                .worker
                .crypto()
                .unwrap()
                .pending_control(&scope)
                .unwrap()
        )
        .unwrap(),
        pending_saved
    );
    assert!(actors[0]
        .worker
        .crypto()
        .unwrap()
        .encrypted_content(crate::e2ee::Envelope {
            version: 1,
            scope: scope.clone(),
            client_id: "capacity-http-blocked".into(),
            content: "never released before acknowledgment".into(),
            reply_to_id: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            action: None,
        })
        .is_err());
    actors[0]
        .worker
        .ensure_crypto_conversation(&target)
        .unwrap();
    assert!(actors[0]
        .worker
        .crypto()
        .unwrap()
        .pending_control(&scope)
        .is_none());
    assert_eq!(
        context(&actors[0], &target, ack_sequence).sequence,
        ack_sequence
    );
    assert_eq!(
        actors[0]
            .worker
            .crypto()
            .unwrap()
            .device
            .members(&scope)
            .unwrap()
            .len(),
        63
    );
    let third = "owned HTTP 63-device after revoked member and lost acknowledgment";
    actors[0].send(&target, third, Vec::new(), "capacity-http-third", None);
    for actor in &mut actors[..63] {
        assert_message(actor, &target, third);
    }
    assert_eq!(
        context_request(&actors[63], &target, 0)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let record = json!({
        "accounts":64,"actual_mls_devices":64,"batches":deliveries.iter().map(|(j,d,w)|json!({"json_bytes":j,"decoded_bytes":d,"welcomes":w})).collect::<Vec<_>>(),
        "setup_ms":setup_ms,"admission_ms":admission_ms,"first_message_all_devices_catchup_ms":catchup_ms,"all_reopen_and_next_message_ms":reopen_ms,
        "total_ms":started.elapsed().as_millis(),"unbounded_operation_rejected_before_save":true,"first_message_all_64":true,"reopened_all_64":true,
        "lost_ack_exact_operation_resumed":true,"revoked_device_denied_context":true,"after_removal_all_63":true,
        "sixty_fifth_signing_device_rejected_before_partial_admission":true,
        "welcomed_participant_send_blocked_between_batches":true,"controller_resumed_durable_admission_prefix":true,
        "scope":"Actual localhost HTTP and 64 independent accounts/MLS actors, real Windows-protected checkpoints. Known fingerprint fixture setup, not onboarding UI, native responsiveness, physical devices, WAN, whole-app performance or concurrent key-stock acceptance.",
        "private_keys_exported":false,"physical_media_devices_opened":false
    });
    let path = server.root.join("http-membership-capacity.json");
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    println!("HTTP membership evidence: {}", path.display());
}
