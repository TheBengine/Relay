use super::*;

fn send(origin: CommandOrigin, binding: SendBinding, target: Target) -> Command {
    Command::SendRich {
        origin,
        binding: Some(binding),
        target,
        content: "the original staged message".into(),
        attachment_ids: Vec::new(),
        client_id: "persisted-send-id".into(),
        reply_to_id: Some("original-reply".into()),
        mentions: vec!["original-mention".into()],
    }
}

fn open_binding(client: &mut TestWorker) -> SendBinding {
    let protection = CommunityProtection {
        level: ProtectionLevel::Open,
        revision: 7,
        changed_by: Some("owner".into()),
    };
    let chat = &mut client.worker.connection.as_mut().unwrap().chat;
    chat.update_bootstrap(&Bootstrap {
        protection: protection.clone(),
        ..Default::default()
    });
    chat.set_open_consent(Some(protection.revision));
    SendBinding {
        protection,
        plain: true,
        device_id: String::new(),
        fingerprint: String::new(),
    }
}

#[test]
fn restored_open_send_preserves_payload_and_acknowledges_its_original_account() {
    let server = MockHttp::with_request(rich_ack);
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let binding = open_binding(&mut client);
    let restored: SendBinding =
        serde_json::from_slice(&serde_json::to_vec(&binding).unwrap()).unwrap();
    assert_eq!(restored, binding);
    let origin = client.command_origin();
    let target = Target::Channel("room".into());
    let updates = client.handle(send(origin.clone(), restored, target.clone()));
    assert!(updates
        .iter()
        .any(|update| matches!(update, Update::SentRich {
        origin: sent_origin, target: sent_target, client_id,
    } if sent_origin == &origin && sent_target == &target && client_id == "persisted-send-id")));
    let requests = server.message_requests();
    assert_eq!(requests.len(), 1);
    let body: SendMessage = serde_json::from_slice(&requests[0]).unwrap();
    assert_eq!(body.content, "the original staged message");
    assert_eq!(body.reply_to_id.as_deref(), Some("original-reply"));
    assert_eq!(body.mentions, ["original-mention"]);
}

#[test]
fn restored_send_rejects_protection_revision_and_consent_changes_before_http() {
    let server = MockHttp::new(|_| panic!("changed send binding reached HTTP"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let binding = open_binding(&mut client);
    let origin = client.command_origin();
    for field in ["level", "revision", "consent"] {
        let mut stale = binding.clone();
        match field {
            "level" => stale.protection.level = ProtectionLevel::Protected,
            "revision" => stale.protection.revision -= 1,
            _ => stale.plain = false,
        }
        let error = client
            .worker
            .handle(send(origin.clone(), stale, Target::Channel("room".into())))
            .unwrap_err();
        assert!(
            error.contains("protection settings changed"),
            "{field}: {error}"
        );
    }
    client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .chat
        .set_open_consent(None);
    let error = client
        .worker
        .handle(send(origin, binding, Target::Direct("friend".into())))
        .unwrap_err();
    assert!(error.contains("protection settings changed"));
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn restored_protected_send_with_missing_local_device_never_posts() {
    let server = MockHttp::new(|_| panic!("missing device reached HTTP"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let binding = SendBinding {
        protection: CommunityProtection::default(),
        plain: false,
        device_id: "previous-device".into(),
        fingerprint: "previous-fingerprint".into(),
    };
    let error = client
        .worker
        .handle(send(
            client.command_origin(),
            binding,
            Target::Channel("room".into()),
        ))
        .unwrap_err();
    assert!(error.contains("encryption device changed"));
    assert!(server.requests.lock().unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn plaintext_retry_keeps_its_wire_payload_after_background_crypto_device_reset() {
    let attempts = Arc::new(AtomicU64::new(0));
    let count = attempts.clone();
    let server = MockHttp::with_request(move |request| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply {
                status: 0,
                headers: Vec::new(),
                body: Vec::new(),
            }
        } else {
            rich_ack(request)
        }
    });
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let target = Target::Channel("room".into());
    server.enable_encrypted_target(&mut client, target.clone());
    let binding = open_binding(&mut client);
    let origin = client.command_origin();
    let crypto = client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap();
    let old_device = crypto.device.identity().device_id.clone();
    let before = crypto.device.checkpoint().unwrap().to_vec();
    client
        .worker
        .command(send(origin.clone(), binding.clone(), target.clone()));
    let updates = client.drain();
    assert!(updates.iter().any(|update| matches!(update,
        Update::SendFailed { origin: Some(failed_origin), .. } if failed_origin == &origin)));
    assert!(!updates
        .iter()
        .any(|update| matches!(update, Update::SentRich { .. })));
    assert_eq!(
        client
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .device
            .checkpoint()
            .unwrap()
            .to_vec(),
        before
    );

    let old_crypto = client
        .worker
        .connection
        .as_mut()
        .unwrap()
        .crypto
        .take()
        .unwrap();
    let keys = old_crypto.storage_root().unwrap();
    drop(old_crypto);
    let new_crypto =
        crate::e2ee::CryptoSession::reset_at(keys, &origin.server_id, &origin.user_id).unwrap();
    assert_ne!(new_crypto.device.identity().device_id, old_device);
    client.worker.connection.as_mut().unwrap().crypto = Some(new_crypto);
    client.handle(send(origin, binding, target));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let requests = server.message_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    let wire: SendMessage = serde_json::from_slice(&requests[0]).unwrap();
    assert_eq!(wire.content, "the original staged message");
    assert_eq!(wire.client_id.as_deref(), Some("persisted-send-id"));
    assert_eq!(wire.mentions, ["original-mention"]);
}

#[test]
fn rejected_stale_send_reports_the_submitted_account_not_the_new_connection() {
    let server = MockHttp::new(|_| panic!("stale send reached HTTP"));
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let binding = open_binding(&mut client);
    let origin = client.command_origin();
    let target = Target::Channel("room".into());
    client.worker.connection.as_mut().unwrap().session.user.id = "new-account".into();
    client
        .worker
        .command(send(origin.clone(), binding, target.clone()));
    assert!(client.drain().iter().any(|update| matches!(update, Update::SendFailed {
        origin: Some(failed_origin), target: failed_target, error,
    } if failed_origin == &origin && failed_target == &target && error.contains("another account or community"))));
    assert!(server.requests.lock().unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn changed_device_binding_does_not_advance_encryption_but_matching_binding_sends() {
    let server = MockHttp::with_request(rich_ack);
    let mut client = TestWorker::new();
    client.connection(server.url.clone());
    let target = Target::Channel("room".into());
    server.enable_encrypted_target(&mut client, target.clone());
    let origin = client.command_origin();
    let crypto = client
        .worker
        .connection
        .as_ref()
        .unwrap()
        .crypto
        .as_ref()
        .unwrap();
    let binding = SendBinding {
        protection: CommunityProtection::default(),
        plain: false,
        device_id: crypto.device.identity().device_id.clone(),
        fingerprint: crypto.device.fingerprint().unwrap(),
    };
    let before = crypto.device.checkpoint().unwrap().to_vec();
    for field in ["device", "fingerprint"] {
        let mut stale = binding.clone();
        if field == "device" {
            stale.device_id.push_str("-previous");
        } else {
            stale.fingerprint.push_str("-previous");
        }
        let error = client
            .worker
            .handle(send(origin.clone(), stale, target.clone()))
            .unwrap_err();
        assert!(error.contains("encryption device changed"));
    }
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(
        client
            .worker
            .connection
            .as_ref()
            .unwrap()
            .crypto
            .as_ref()
            .unwrap()
            .device
            .checkpoint()
            .unwrap()
            .to_vec(),
        before
    );
    let updates = client.handle(send(origin.clone(), binding, target.clone()));
    assert!(updates
        .iter()
        .any(|update| matches!(update, Update::SentRich {
        origin: sent_origin, target: sent_target, client_id,
    } if sent_origin == &origin && sent_target == &target && client_id == "persisted-send-id")));
    assert_eq!(server.message_requests().len(), 1);
    let wire: SendMessage = serde_json::from_slice(&server.message_requests()[0]).unwrap();
    assert!(wire.content.starts_with(crate::e2ee::MESSAGE_PREFIX));
}
