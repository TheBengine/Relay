//! Real Open-call transitions; synthetic audio, virtual output, no native devices.
use super::*;

fn audio_pair(
    a: &(VoiceTicket, Arc<crate::e2ee::CallKeys>),
    b: &(VoiceTicket, Arc<crate::e2ee::CallKeys>),
) {
    assert_eq!(a.1.epoch, b.1.epoch);
    let mut first = crate::media::LivingVoiceProbe::start(&a.0, a.1.clone());
    let mut second = crate::media::LivingVoiceProbe::start(&b.0, b.1.clone());
    let until = Instant::now() + Duration::from_secs(4);
    while !first.played() || !second.played() {
        first.feed();
        second.feed();
        assert!(
            Instant::now() < until,
            "fresh Open membership must pass actual Opus both ways"
        );
        thread::sleep(Duration::from_millis(10));
    }
    first.assert_healthy();
    second.assert_healthy();
    first.finish();
    second.finish();
}

fn changed(actor: &mut CryptoWorker) -> (VoiceTicket, Arc<crate::e2ee::CallKeys>) {
    actor.worker.refresh_encryption();
    let updates = actor.drain();
    assert!(!updates
        .iter()
        .any(|u| matches!(u, Update::VoiceEncryptionPaused { .. })));
    let call =
        latest_voice_transport(updates).expect("membership change refreshes the admitted caller");
    let c = actor.worker.connection.as_ref().unwrap();
    assert!(
        !c.sharing && !c.camera,
        "capture remains stopped pending explicit user action"
    );
    assert!(c.crypto.is_none());
    call
}

fn stable(actor: &mut CryptoWorker, channel: &str, epoch: u64) {
    let cached = actor.worker.protected_call_keys(channel).unwrap();
    for _ in 0..6 {
        actor.worker.refresh_encryption();
        let updates = actor.drain();
        assert!(
            !updates.iter().any(|u| matches!(
                u,
                Update::VoiceEncryptionPaused { .. }
                    | Update::VoiceEncryptionChanged { .. }
                    | Update::VoiceJoined { .. }
            )),
            "unchanged participants must not restart voice or revoke RTC repeatedly"
        );
        let next = actor.worker.protected_call_keys(channel).unwrap();
        assert_eq!(next.epoch, epoch);
        assert!(
            Arc::ptr_eq(&cached, &next),
            "unchanged material reuses the validated context"
        );
        assert_eq!(
            actor.worker.connection.as_ref().unwrap().crypto_call_epoch,
            Some(epoch)
        );
    }
}

pub(super) fn run() {
    let server = CryptoServer::start_with_udp();
    let (mut host, mut one, mut two, _text) = workers(&server);
    host.handle(Command::SetProtection {
        origin: host.origin(),
        level: ProtectionLevel::Open,
        expected_revision: 0,
    });
    for actor in [&mut host, &mut one, &mut two] {
        actor.worker.refresh();
        actor.drain();
        actor.worker.connection.as_mut().unwrap().crypto = None;
        actor.handle(Command::OpenConsent {
            origin: actor.origin(),
            revision: Some(1),
        });
    }
    let boot: Bootstrap = host.worker.get("/api/bootstrap").unwrap();
    let channel = boot
        .channels
        .iter()
        .find(|c| c.kind == "voice")
        .unwrap()
        .id
        .clone();
    let initial = host.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9400,
    });
    let initial = latest_voice_transport(initial).unwrap();
    let first_join = one.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9401,
    });
    let first_join = latest_voice_transport(first_join).unwrap();
    let first_host = changed(&mut host);
    assert!(first_host.1.epoch > initial.1.epoch);
    assert_eq!(first_host.1.sender_keys.len(), 2);
    audio_pair(&first_host, &first_join);
    stable(&mut host, &channel, first_host.1.epoch);
    stable(&mut one, &channel, first_host.1.epoch);

    // Logical capture flags only: no camera, microphone or screen is opened.
    for actor in [&mut host, &mut one] {
        let c = actor.worker.connection.as_mut().unwrap();
        c.sharing = true;
        c.camera = true;
    }
    let third = two.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9402,
    });
    let third = latest_voice_transport(third).unwrap();
    let with_third_host = changed(&mut host);
    let with_third_one = changed(&mut one);
    assert!(with_third_host.1.epoch > first_host.1.epoch);
    assert_eq!(with_third_host.1.epoch, third.1.epoch);
    assert_eq!(with_third_host.1.sender_keys.len(), 3);
    assert_ne!(with_third_host.0.key, first_host.0.key);
    audio_pair(&with_third_host, &with_third_one);
    for actor in [&mut host, &mut one, &mut two] {
        stable(actor, &channel, third.1.epoch);
    }

    two.handle(Command::LeaveVoice);
    assert!(two.worker.protected_call_keys(&channel).is_err());
    let without_third_host = changed(&mut host);
    let without_third_one = changed(&mut one);
    assert!(without_third_host.1.epoch > third.1.epoch);
    assert_eq!(without_third_host.1.sender_keys.len(), 2);
    assert!(!without_third_host
        .1
        .sender_keys
        .contains_key(&two.origin().user_id));
    audio_pair(&without_third_host, &without_third_one);
    stable(&mut host, &channel, without_third_host.1.epoch);
    stable(&mut one, &channel, without_third_host.1.epoch);

    let rejoined = two.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9403,
    });
    let rejoined = latest_voice_transport(rejoined).unwrap();
    let rejoined_host = changed(&mut host);
    let rejoined_one = changed(&mut one);
    assert!(rejoined_host.1.epoch > without_third_host.1.epoch);
    assert_eq!(rejoined_host.1.epoch, rejoined.1.epoch);
    assert_eq!(rejoined_host.1.sender_keys.len(), 3);
    audio_pair(&rejoined_host, &rejoined_one);
    for actor in [&mut host, &mut one, &mut two] {
        stable(actor, &channel, rejoined.1.epoch);
    }
    for actor in [&mut host, &mut one, &mut two] {
        actor.handle(Command::LeaveVoice);
    }
    fs::write(server.root.join("open-membership-voice.json"), serde_json::to_vec_pretty(&json!({
        "actual_udp_opus_pairs":4,"packet_samples":480,"capture_flags_cleared_on_membership_change":true,
        "unchanged_refreshes_per_actor_per_phase":6,"rotation_loop_absent":true,
        "epochs":[initial.1.epoch,first_host.1.epoch,third.1.epoch,without_third_host.1.epoch,rejoined.1.epoch],
        "leaver_denied_future_keys":true,"local_mls_actor_not_recreated":true,
        "physical_devices_or_gui_opened":false,"video_frames_in_this_test":false,"wan_accepted":false
    })).unwrap()).unwrap();
    println!("Open membership voice evidence: {}", server.root.display());
}

/// A quiet call must outlive the server's60second idle limit without HTTP
/// presence writes. Only the production UDP workers maintain these two seats.
pub(super) fn silent_presence() {
    let server = CryptoServer::start_with_udp();
    let (mut host, mut one, mut two, _text) = workers(&server);
    host.handle(Command::SetProtection {
        origin: host.origin(),
        level: ProtectionLevel::Open,
        expected_revision: 0,
    });
    for actor in [&mut host, &mut one, &mut two] {
        actor.worker.refresh();
        actor.drain();
        actor.worker.connection.as_mut().unwrap().crypto = None;
        actor.handle(Command::OpenConsent {
            origin: actor.origin(),
            revision: Some(1),
        });
    }
    let boot: Bootstrap = host.worker.get("/api/bootstrap").unwrap();
    let channel = boot
        .channels
        .iter()
        .find(|c| c.kind == "voice")
        .unwrap()
        .id
        .clone();
    one.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9501,
    });
    let second = latest_voice_transport(two.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9502,
    }))
    .unwrap();
    let first = changed(&mut one);
    assert_eq!(first.1.epoch, second.1.epoch);
    let expected = [
        (&first.1.local_user_id, first.0.session_id),
        (&second.1.local_user_id, second.0.session_id),
    ];
    let check_presence = |boot: &Bootstrap| {
        for (user, session) in expected {
            assert!(
                boot.voice_members.iter().any(|m| m.user.id == *user
                    && m.channel_id == channel
                    && m.session_id == session),
                "silent participant lost the original admitted voice seat"
            );
        }
    };
    let mut a = crate::media::LivingVoiceProbe::start(&first.0, first.1.clone());
    let mut b = crate::media::LivingVoiceProbe::start(&second.0, second.1.clone());
    a.set_silent_gate(true, false);
    b.set_silent_gate(false, true);
    let started = Instant::now();
    let mut samples = Vec::new();
    let mut next_sample = Duration::ZERO;
    while started.elapsed() < Duration::from_secs(70) {
        // Synthetic microphone queues are nonempty, but mute/closedPTT must
        // withhold actual Opus while authenticated transport control continues.
        a.feed();
        b.feed();
        assert!(
            !a.played() && !b.played(),
            "privacy gates leaked generated speech"
        );
        a.assert_healthy();
        b.assert_healthy();
        if started.elapsed() >= next_sample {
            let boot: Bootstrap = host.worker.get("/api/bootstrap").unwrap();
            check_presence(&boot);
            samples.push(started.elapsed().as_millis());
            println!(
                "Silent-call presence: {}s, both original seats remain",
                started.elapsed().as_secs()
            );
            next_sample += Duration::from_secs(10);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let idle_ms = started.elapsed().as_millis();
    let boot: Bootstrap = host.worker.get("/api/bootstrap").unwrap();
    check_presence(&boot);
    assert!(samples.len() >= 7);
    let before = [a.stats(), b.stats()];
    assert!(before
        .iter()
        .all(|s| s.udp_reachable && s.received_packets >= 25));
    for actor in [&mut one, &mut two] {
        actor.worker.refresh_encryption();
        assert!(!actor.drain().iter().any(|u| matches!(
            u,
            Update::VoiceJoined { .. }
                | Update::VoiceEncryptionChanged { .. }
                | Update::VoiceEncryptionPaused { .. }
        )));
        assert_eq!(
            actor.worker.protected_call_keys(&channel).unwrap().epoch,
            first.1.epoch
        );
    }
    a.set_silent_gate(false, false);
    b.set_silent_gate(false, false);
    let resume = Instant::now();
    while !a.played() || !b.played() {
        a.feed();
        b.feed();
        assert!(
            resume.elapsed() < Duration::from_secs(4),
            "same admitted call did not resume actual Opus both ways"
        );
        thread::sleep(Duration::from_millis(10));
    }
    a.assert_healthy();
    b.assert_healthy();
    let resume_ms = resume.elapsed().as_millis();
    a.finish();
    b.finish();
    one.handle(Command::LeaveVoice);
    two.handle(Command::LeaveVoice);
    fs::write(server.root.join("open-silent-presence.json"),serde_json::to_vec_pretty(&json!({
        "silent_ms":idle_ms,"server_idle_limit_seconds":60,"presence_samples_ms":samples,
        "same_session_ids_preserved":true,"same_call_key_epoch":first.1.epoch,
        "muted_peer_and_closed_ptt_peer":true,"generated_input_withheld_during_idle":true,
        "http_voice_state_writes_during_idle":0,"only_unjoined_host_readonly_bootstrap_during_idle":true,
        "udp_reply_packets_before_resume":[before[0].received_packets,before[1].received_packets],
        "resumed_opus_both_directions":true,"resume_ms":resume_ms,"packet_samples":480,
        "worker_devices_or_gui_opened":false,"no_rejoin_or_key_rotation_after_idle":true,
        "test_server_shutdown":"fixture kills and waits own server; not graceful shutdown acceptance",
        "scope":"Real server/Open admission/production UDP voice workers and Opus; virtual input/output. Not normal app/UI/window minimization, iroh/public relay/WAN, hardware, VAD recognition or subjective sound quality."
    })).unwrap()).unwrap();
    println!("Silent-call evidence: {}", server.root.display());
}
