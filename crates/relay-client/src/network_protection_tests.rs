//! R4.5 against the real server: the owner turns end-to-end encryption off for
//! the community, and each person's client still sends unprotected text only
//! for the revision that person accepted. Synthetic accounts, private localhost.
use super::*;
use relay_protocol::ProtectionLevel;

fn set(worker: &mut CryptoWorker, level: ProtectionLevel, revision: u64) -> Result<(), String> {
    let origin = worker.origin();
    let result = worker.worker.handle(Command::SetProtection {
        origin,
        level,
        expected_revision: revision,
    });
    worker.drain();
    result
}

fn consent(worker: &mut CryptoWorker, revision: Option<u64>) {
    let origin = worker.origin();
    worker.handle(Command::OpenConsent { origin, revision });
}

fn learn(worker: &mut CryptoWorker) {
    worker.worker.refresh();
    worker.drain();
}

fn last_raw(worker: &CryptoWorker, target: &Target) -> String {
    worker
        .raw_messages(target)
        .last()
        .expect("the server stored the message")
        .content
        .clone()
}

pub(super) fn run() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, channel) = workers(&server);
    alice.pin(&bob.own());
    bob.pin(&alice.own());

    // A file prepared while encryption is on keeps its key only inside a protected message.
    let prepared_path = server.root.join("prepared.bin");
    fs::write(&prepared_path, b"prepared while protected").unwrap();
    let prepared = bob.upload(&channel, &prepared_path);

    // Protected by default: the server only ever sees ciphertext.
    bob.send(&channel, "secret one", vec![], "protect-1", None);
    assert!(last_raw(&bob, &channel).starts_with(crate::e2ee::MESSAGE_PREFIX));

    // Only the owner can change the mode.
    assert!(set(&mut bob, ProtectionLevel::Open, 0).is_err());
    set(&mut alice, ProtectionLevel::Open, 0).unwrap();
    learn(&mut bob);
    let seen = bob.worker.get::<Bootstrap>("/api/bootstrap").unwrap();
    assert_eq!(seen.protection.level, ProtectionLevel::Open);
    assert_eq!(seen.protection.revision, 1);

    // Announced, but not accepted: Bob's client keeps encrypting.
    bob.send(&channel, "still secret", vec![], "protect-2", None);
    assert!(last_raw(&bob, &channel).starts_with(crate::e2ee::MESSAGE_PREFIX));

    // Accepting a different revision than the announced one changes nothing either.
    consent(&mut bob, Some(0));
    bob.send(&channel, "wrong revision", vec![], "protect-3", None);
    assert!(last_raw(&bob, &channel).starts_with(crate::e2ee::MESSAGE_PREFIX));

    // Accepting the announced revision sends text as it is, and others read it.
    consent(&mut bob, Some(1));
    bob.send(&channel, "plain two", vec![], "open-1", None);
    assert_eq!(last_raw(&bob, &channel), "plain two");
    let shown = alice.history(&channel);
    assert!(shown.iter().any(|m| m.content == "plain two"));
    assert!(shown.iter().any(|m| m.content == "secret one"));

    // A file prepared while encryption was on cannot be sent plainly: nobody could open it.
    let origin = bob.origin();
    let refused = bob.worker.handle(Command::SendRich {
        binding: None,
        origin,
        target: channel.clone(),
        content: "with old file".into(),
        attachment_ids: vec![prepared.id.clone()],
        client_id: "open-prepared".into(),
        reply_to_id: None,
        mentions: Vec::new(),
    });
    bob.drain();
    assert!(refused.unwrap_err().contains("prepared with encryption"));
    assert_eq!(last_raw(&bob, &channel), "plain two");

    // A file chosen after accepting is uploaded as it is and keeps its real name.
    let note_path = server.root.join("holiday-plan.txt");
    let note = b"synthetic plain attachment".to_vec();
    fs::write(&note_path, &note).unwrap();
    let plain_file = bob.upload(&channel, &note_path);
    bob.send(
        &channel,
        "with file",
        vec![plain_file.id.clone()],
        "open-file",
        None,
    );
    let stored = bob.raw_messages(&channel).pop().unwrap();
    assert_eq!(stored.content, "with file");
    assert_eq!(stored.attachments.len(), 1);
    assert_eq!(stored.attachments[0].name, "holiday-plan.txt");
    // Alice has no key for it. She can read it because the community is open.
    let saved = server.root.join("alice-copy.txt");
    alice.download(&stored.attachments[0], &saved);
    assert_eq!(fs::read(&saved).unwrap(), note);

    // Turning encryption back on ends unprotected sending at once, even for
    // someone whose agreement is still stored.
    set(&mut alice, ProtectionLevel::Protected, 1).unwrap();
    learn(&mut bob);
    bob.send(&channel, "secret three", vec![], "protect-4", None);
    assert!(last_raw(&bob, &channel).starts_with(crate::e2ee::MESSAGE_PREFIX));
    // Reading files without a key stops with the open mode: no plaintext fallback.
    let closed_copy = server.root.join("alice-closed.txt");
    let origin = alice.origin();
    let updates = alice.handle(Command::StartDownload {
        origin,
        attachment: stored.attachments[0].clone(),
        path: closed_copy.clone(),
        operation_id: "closed-download".into(),
    });
    assert!(updates.iter().any(|update| matches!(
        update,
        Update::DownloadFailed { error, .. } if error.contains("no verified encryption key")
    )));
    assert!(!closed_copy.exists());

    // Turning it off again is a new revision: the old agreement does not apply.
    set(&mut alice, ProtectionLevel::Open, 2).unwrap();
    learn(&mut bob);
    assert_eq!(
        bob.worker
            .get::<Bootstrap>("/api/bootstrap")
            .unwrap()
            .protection
            .revision,
        3
    );
    bob.send(&channel, "secret four", vec![], "protect-5", None);
    assert!(last_raw(&bob, &channel).starts_with(crate::e2ee::MESSAGE_PREFIX));
    consent(&mut bob, Some(3));
    bob.send(&channel, "plain five", vec![], "open-2", None);
    assert_eq!(last_raw(&bob, &channel), "plain five");

    // Withdrawing the agreement means encrypted again.
    consent(&mut bob, None);
    bob.send(&channel, "secret six", vec![], "protect-6", None);
    assert!(last_raw(&bob, &channel).starts_with(crate::e2ee::MESSAGE_PREFIX));

    // A stale change is refused and leaves the mode as it was.
    assert!(set(&mut alice, ProtectionLevel::Protected, 1).is_err());
    assert_eq!(
        alice
            .worker
            .get::<Bootstrap>("/api/bootstrap")
            .unwrap()
            .protection
            .level,
        ProtectionLevel::Open
    );
}

pub(super) fn call_consent() {
    let mut server = CryptoServer::start();
    let (mut alice, mut bob, _eve, text) = workers(&server);
    alice.pin(&bob.own());
    bob.pin(&alice.own());
    bob.send(
        &text,
        "protected history stays",
        vec![],
        "consent-history",
        None,
    );
    let bob_device = bob.own().identity.device_id.clone();
    let bootstrap: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
    let channel = bootstrap
        .channels
        .iter()
        .find(|c| c.kind == "voice")
        .unwrap()
        .id
        .clone();

    // A consent update is irrelevant to an already protected call.
    let joined = alice.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9100,
    });
    assert!(joined
        .iter()
        .any(|u| matches!(u, Update::VoiceJoined { .. })));
    let epoch = alice.worker.live.epoch.load(Ordering::SeqCst);
    let updates = alice.handle(Command::OpenConsent {
        origin: alice.origin(),
        revision: None,
    });
    assert!(!updates.iter().any(|u| matches!(u, Update::VoiceLeft)));
    assert_eq!(alice.worker.live.epoch.load(Ordering::SeqCst), epoch);
    assert_eq!(
        alice
            .worker
            .connection
            .as_ref()
            .unwrap()
            .voice_channel
            .as_deref(),
        Some(channel.as_str())
    );
    alice.handle(Command::LeaveVoice);

    set(&mut alice, ProtectionLevel::Open, 0).unwrap();
    learn(&mut bob);
    consent(&mut alice, Some(1));
    consent(&mut bob, Some(1));
    alice.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9101,
    });

    for (index, revision) in [None, Some(0)].into_iter().enumerate() {
        let joined = bob.handle(Command::JoinVoiceRequest {
            channel_id: channel.clone(),
            request_id: 9102 + index as u64,
        });
        assert!(joined
            .iter()
            .any(|u| matches!(u, Update::VoiceJoined { .. })));
        let epoch = bob.worker.live.epoch.load(Ordering::SeqCst);
        let c = bob.worker.connection.as_mut().unwrap();
        assert!(c.voice_admission.is_some());
        c.sharing = true;
        c.camera = true;
        c.frame_ids.insert("owned-fixture".into(), "frame".into());

        let updates = bob.handle(Command::OpenConsent {
            origin: bob.origin(),
            revision,
        });
        assert!(
            updates.iter().any(|u| matches!(u, Update::VoiceLeft)),
            "withdrawing or invalidating consent must end the Open call"
        );
        let c = bob.worker.connection.as_ref().unwrap();
        assert!(
            c.voice_channel.is_none() && c.voice_admission.is_none() && c.voice_recovery.is_none()
        );
        assert!(c.crypto_pending_join.is_none() && c.crypto_call_epoch.is_none());
        assert!(!c.sharing && !c.camera && c.frame_ids.is_empty());
        assert!(bob.worker.live.epoch.load(Ordering::SeqCst) > epoch);
        assert!(bob.worker.live.context.lock().unwrap().is_none());
        assert_eq!(bob.own().identity.device_id, bob_device);
        assert!(bob
            .history(&text)
            .iter()
            .any(|m| m.content == "protected history stays"));
        let state: Bootstrap = alice.worker.get("/api/bootstrap").unwrap();
        assert!(!state
            .voice_members
            .iter()
            .any(|m| m.user.id == bob.origin().user_id));
        assert!(state
            .voice_members
            .iter()
            .any(|m| m.user.id == alice.origin().user_id));

        consent(&mut bob, Some(1));
        bob.worker.refresh_encryption();
        assert!(!bob
            .drain()
            .iter()
            .any(|u| matches!(u, Update::VoiceJoined { .. })));
        assert!(
            bob.worker
                .connection
                .as_ref()
                .unwrap()
                .voice_channel
                .is_none(),
            "accepting again does not join a call"
        );
    }

    // Server failure cannot hold the local call open or later resume it.
    bob.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9104,
    });
    bob.worker.pause_voice_for_outage();
    bob.drain();
    server.stop();
    let updates = bob.handle(Command::OpenConsent {
        origin: bob.origin(),
        revision: None,
    });
    assert!(updates.iter().any(|u| matches!(u, Update::VoiceLeft)));
    let c = bob.worker.connection.as_ref().unwrap();
    assert!(c.voice_channel.is_none() && c.voice_admission.is_none() && c.voice_recovery.is_none());
    server.restart();
    bob.worker.refresh();
    assert!(!bob
        .drain()
        .iter()
        .any(|u| matches!(u, Update::VoiceJoined { .. })));
    assert!(bob
        .worker
        .connection
        .as_ref()
        .unwrap()
        .voice_channel
        .is_none());
    assert_eq!(bob.own().identity.device_id, bob_device);
    assert!(bob
        .history(&text)
        .iter()
        .any(|m| m.content == "protected history stays"));
}

pub(super) fn reactions() {
    let server = CryptoServer::start();
    let (mut alice, mut bob, _eve, channel) = workers(&server);
    alice.pin(&bob.own());
    bob.pin(&alice.own());
    bob.send(
        &channel,
        "protected before mode change",
        vec![],
        "old-protected",
        None,
    );
    let original = bob.raw_messages(&channel).pop().unwrap();
    assert!(original.content.starts_with(MESSAGE_PREFIX));
    assert_eq!(
        alice.history(&channel)[0].content,
        "protected before mode change"
    );

    set(&mut alice, ProtectionLevel::Open, 0).unwrap();
    consent(&mut alice, Some(1));
    learn(&mut bob);
    consent(&mut bob, Some(1));
    assert!(bob.worker.connection.as_ref().unwrap().chat.plain_allowed());

    for active in [true, false, true] {
        let origin = bob.origin();
        bob.handle(Command::React {
            origin,
            target: channel.clone(),
            message_id: original.id.clone(),
            target_client_id: original.client_id.clone(),
            emoji: "ok".into(),
            active,
        });
        let raw = bob.raw_messages(&channel);
        assert!(
            raw.last().unwrap().content.starts_with(MESSAGE_PREFIX),
            "a reaction to a protected original must stay encrypted after Open consent"
        );
        let shown = alice.history(&channel);
        let shown = shown.iter().find(|m| m.id == original.id).unwrap();
        assert_eq!(
            shown
                .reactions
                .iter()
                .any(|r| r.emoji == "ok" && r.user_ids == vec![bob.own().identity.user_id.clone()]),
            active,
            "the other person must see each authenticated toggle on the old message"
        );
        assert!(
            raw.iter()
                .find(|m| m.id == original.id)
                .unwrap()
                .reactions
                .is_empty(),
            "protected reactions must not be exposed in the host-controlled reaction table"
        );
    }

    bob.send(&channel, "new open message", vec![], "new-open", None);
    let plain = bob.raw_messages(&channel).pop().unwrap();
    assert_eq!(plain.content, "new open message");
    for active in [true, false] {
        let origin = bob.origin();
        bob.handle(Command::React {
            origin,
            target: channel.clone(),
            message_id: plain.id.clone(),
            target_client_id: None,
            emoji: "ok".into(),
            active,
        });
        let shown = alice.history(&channel);
        let shown = shown.iter().find(|m| m.id == plain.id).unwrap();
        assert_eq!(
            shown
                .reactions
                .iter()
                .any(|r| r.emoji == "ok" && r.user_ids == vec![bob.own().identity.user_id.clone()]),
            active
        );
    }
}
