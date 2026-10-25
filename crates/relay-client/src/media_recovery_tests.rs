// Included only in media::tests. All audio is generated; no device enumeration.
include!("media_gateway_acceptance.rs");
#[test]
fn actual_voice_worker_uses_a_proven_gateway_and_rebinds_before_fallback_audio() {
    use crate::local_voice_gateway::{Grant, Path, Worker};
    use zeroize::Zeroizing;
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(5)))
        .unwrap();
    let remote = server.local_addr().unwrap();
    let make_grant = || Grant {
        id: [13; 16],
        session: 7,
        key: Zeroizing::new([53; 32]),
        valid_for: Duration::from_secs(2),
    };
    let mut forwarder = Some(Worker::start("127.0.0.1:0".parse().unwrap(), remote).unwrap());
    let gateway_addr = forwarder.as_ref().unwrap().address;
    let began = Instant::now();
    assert!(forwarder.as_ref().unwrap().grant(make_grant(), began));
    let path = Path::new(gateway_addr, make_grant(), began, began).unwrap();
    let (mut worker, _, counters) = probe_fixture(remote);
    // The normal app uses an unconnected socket for server, direct and gateway
    // sources. This fixture compares against an explicitly simulated 100 ms RTT.
    worker.socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    worker.socket.set_nonblocking(true).unwrap();
    counters.udp_rtt_us.store(100_000, Ordering::Relaxed);
    worker
        .controls
        .gateway_enabled
        .store(true, Ordering::Release);
    worker
        .controls
        .transmission
        .store(TransmissionMode::Continuous as u32, Ordering::Release);
    // This is a transport fixture with a generated tone, not speech detection.
    worker.controls.suppression.store(false, Ordering::Relaxed);
    worker.controls.packet_samples.store(480, Ordering::Relaxed);
    worker.muted.store(false, Ordering::Release);
    worker.deafened.store(false, Ordering::Release);
    worker.gateway = Some(crate::gateway_control::Session::fixture(
        path,
        worker.controls.gateway_enabled.clone(),
    ));
    let cipher = worker.cipher.clone();
    let capture = worker.capture.clone();
    let errors = worker.errors.clone();
    let mut owner = OwnedVoiceWorker::start(worker);
    let pcm: Vec<_> = (0..960).map(|n| (n as f32 * 0.09).sin() * 0.2).collect();
    let mut active_binding = None;
    let mut server_sequence = 1;
    let mut inspector = CallCipher::new(test_call_keys("remote")).unwrap();
    let mut gateway_audio = false;
    let mut fallback_audio = false;
    let mut fed_gateway = false;
    let mut fed_fallback = false;
    let deadline = began + Duration::from_millis(1800);
    while Instant::now() < deadline && !fallback_audio {
        if counters.gateway_active.load(Ordering::Acquire) && !fed_gateway {
            capture.lock().unwrap().extend(pcm.iter().copied());
            fed_gateway = true;
        }
        if gateway_audio && forwarder.is_some() {
            forwarder.take();
        }
        if gateway_audio && !counters.gateway_active.load(Ordering::Acquire) && !fed_fallback {
            capture.lock().unwrap().extend(pcm.iter().copied());
            fed_fallback = true;
        }
        let Some((_, plain, from)) = receive_voice_fixture(&server, &cipher) else {
            continue;
        };
        if plain == [0] {
            active_binding = Some(from);
        }
        if plain.first() == Some(&relay_protocol::UDP_PATH_PROBE)
            || (plain.first() == Some(&UDP_PING) && from == gateway_addr)
        {
            let packet = seal_packet(&cipher, 7, server_sequence, 1, &plain).unwrap();
            server_sequence += 1;
            server.send_to(&packet, from).unwrap();
        }
        if plain.first() == Some(&AUDIO_PACKET_V3) {
            assert_eq!(active_binding, Some(from), "new route bound before audio");
            let frame = inspector.open(&plain).unwrap();
            assert_eq!(
                opus::packet::get_nb_samples(&frame.opus, 48_000).unwrap(),
                480
            );
            let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
            let mut decoded = [0.0; 480];
            assert_eq!(
                decoder
                    .decode_float(&frame.opus, &mut decoded, false)
                    .unwrap(),
                480
            );
            if from == gateway_addr {
                gateway_audio = true;
            } else if gateway_audio {
                fallback_audio = true;
            }
        }
    }
    owner.finish();
    assert!(
        gateway_audio,
        "generated 10 ms Opus went through the proven gateway; selected={fed_gateway}, stats={:?}, errors={:?}", counters.snapshot(), errors.lock().unwrap()
    );
    assert!(
        fallback_audio,
        "gateway loss returned the same call to its original UDP route"
    );
    assert!(!counters.snapshot().gateway_active);
    assert!(errors.lock().unwrap().is_none());
}

struct OwnedVoiceWorker {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl OwnedVoiceWorker {
    fn start(worker: VoiceWorker) -> Self {
        Self {
            stop: worker.stop.clone(),
            handle: Some(thread::spawn(move || voice_worker(worker))),
        }
    }
    fn finished(&self) -> bool {
        self.handle.as_ref().unwrap().is_finished()
    }
    fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}
impl Drop for OwnedVoiceWorker {
    fn drop(&mut self) {
        self.finish();
    }
}

fn voice_recovery_evidence(name: &str, evidence: serde_json::Value) {
    let root = std::path::PathBuf::from(std::env::var("RELAY_TEST_WORK_ROOT").unwrap());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(name),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}

fn receive_voice_fixture(
    server: &UdpSocket,
    cipher: &ChaCha20Poly1305,
) -> Option<(u64, Vec<u8>, SocketAddr)> {
    let mut bytes = [0; UDP_MAX_PACKET];
    match server.recv_from(&mut bytes) {
        Ok((length, client)) => {
            let (sequence, plain) = open_packet(cipher, 7, 0, &bytes[..length]).unwrap();
            Some((sequence, plain, client))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            None
        }
        Err(error) => panic!("Owned UDP fixture receive failed: {error}"),
    }
}

#[test]
#[ignore = "Actual device-free UDP receive workers restarted under retained call keys; replayed inner audio stays rejected"]
fn actual_device_free_voice_receiver_restart_rejects_inner_replay() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let keys = test_call_keys("local");
    let sender = CallCipher::new(test_call_keys("remote")).unwrap();
    let successor = CallCipher::new(test_call_keys("remote")).unwrap();
    let opus = playout_packet();
    let first = sender.seal(99, 1, 0, true, &opus).unwrap();
    let second = sender.seal(99, 2, 960, false, &opus).unwrap();
    let retired_next = sender.seal(99, 3, 1920, false, &opus).unwrap();
    let fresh = successor.seal(100, 1, 0, true, &opus).unwrap();
    let fresh_next = successor.seal(100, 2, 960, false, &opus).unwrap();
    let phases = [
        (vec![first.clone()], 0),
        (vec![first.clone(), second.clone()], 1),
        (
            vec![first.clone(), second, fresh.clone(), retired_next.clone()],
            3,
        ),
        (vec![fresh, retired_next, fresh_next], 2),
    ];
    let mut rejected = Vec::new();
    for (index, (packets, expected_rejected)) in phases.into_iter().enumerate() {
        let (mut worker, _, counters) = probe_fixture(server.local_addr().unwrap());
        worker.call = CallCipher::new(keys.clone()).unwrap();
        // Every new outer transport has its own key/session/counter space. A
        // recording is wrapped anew by the relay, never by reusing its old nonce.
        worker.session_id = 700 + index as u64;
        worker.cipher = ChaCha20Poly1305::new_from_slice(&[70 + index as u8; 32]).unwrap();
        worker.deafened.store(false, Ordering::Release);
        worker.controls.jitter_ms.store(20, Ordering::Relaxed);
        let client = worker.socket.local_addr().unwrap();
        let session = worker.session_id;
        let cipher = worker.cipher.clone();
        let playback = worker.playback.clone();
        let errors = worker.errors.clone();
        let mut owner = OwnedVoiceWorker::start(worker);
        for (n, mut packet) in packets.iter().cloned().enumerate() {
            packet[1..9].copy_from_slice(&(900 + index as u64).to_le_bytes());
            server
                .send_to(
                    &seal_packet(&cipher, session, n as u64 + 1, 1, &packet).unwrap(),
                    client,
                )
                .unwrap();
        }
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let stats = counters.snapshot();
            let played = playback
                .lock()
                .unwrap()
                .iter()
                .any(|sample| sample.abs() > 0.001);
            if stats.received_packets == packets.len() as u64 && played {
                assert_eq!(stats.rejected_packets, expected_rejected);
                rejected.push(stats.rejected_packets);
                break;
            }
            assert!(
                Instant::now() < until,
                "Restarted receiver did not authenticate/decode fresh audio in phase {index}"
            );
            thread::sleep(Duration::from_millis(2));
        }
        owner.finish();
        assert!(errors.lock().unwrap().is_none());
    }
    voice_recovery_evidence(
        "voice-receiver-replay-evidence.json",
        serde_json::json!({
            "workers":4,"fresh_outer_transport_per_worker":true,"rejected_per_worker":rejected,
            "fresh_protected_opus_played_after_every_restart":true,"retired_sender_stays_retired":true,
            "receiver_context_retained":true,"physical_devices_opened":false,
            "scope":"Actual loopback UDP workers and generated protected Opus under one live verified call context; rejects recordings with new outer keys/counters/routing IDs. Not process-restart or persistent replay acceptance."
        }),
    );
}

#[test]
#[ignore = "Actual device-free UDP outage/recovery with protected generated Opus and live nonce/control checks"]
fn actual_device_free_voice_recovery_preserves_nonce_counters_and_controls() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(80)))
        .unwrap();
    let (worker, _, counters) = probe_fixture(server.local_addr().unwrap());
    let cipher = worker.cipher.clone();
    let capture = worker.capture.clone();
    let muted = worker.muted.clone();
    let deafened = worker.deafened.clone();
    let errors = worker.errors.clone();
    let input: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
        .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
        .collect();
    muted.store(false, Ordering::Release);
    deafened.store(false, Ordering::Release);
    capture.lock().unwrap().extend(input.iter().copied());
    let mut owner = OwnedVoiceWorker::start(worker);
    let mut relay_sequence = 1;
    let mut outer_sequences = std::collections::HashSet::new();
    let mut first_media = None;
    let mut latest_probe = None;
    let until = Instant::now() + Duration::from_secs(3);
    while first_media.is_none() || !counters.snapshot().udp_reachable {
        assert!(
            Instant::now() < until,
            "Initial authenticated voice did not connect/send"
        );
        if let Some((sequence, plain, client)) = receive_voice_fixture(&server, &cipher) {
            assert!(
                outer_sequences.insert(sequence),
                "Outer nonce sequence repeated"
            );
            if plain.first() == Some(&UDP_PING) {
                server
                    .send_to(
                        &seal_packet(&cipher, 7, relay_sequence, 1, &plain).unwrap(),
                        client,
                    )
                    .unwrap();
                relay_sequence += 1;
                latest_probe = Some((plain, client));
            } else if plain.first() == Some(&AUDIO_PACKET_V3) {
                first_media = Some((sequence, plain, client));
            }
        }
    }
    muted.store(true, Ordering::Release);
    deafened.store(true, Ordering::Release);
    let disconnected = Instant::now();
    let until = disconnected + Duration::from_secs(7);
    let mut wrong_probes = 0;
    while counters.snapshot().udp_recovery_remaining_ms.is_none() {
        assert!(
            Instant::now() < until,
            "Wrong probe IDs concealed a transport outage"
        );
        if let Some((sequence, plain, client)) = receive_voice_fixture(&server, &cipher) {
            assert!(
                outer_sequences.insert(sequence),
                "Outer nonce sequence repeated during outage"
            );
            assert_ne!(
                plain.first(),
                Some(&AUDIO_PACKET_V3),
                "Muted/deafened worker sent new speech"
            );
            if plain.first() == Some(&UDP_PING) {
                let mut wrong = plain.clone();
                let id = u64::from_le_bytes(wrong[1..9].try_into().unwrap());
                wrong[1..9].copy_from_slice(&(id + 999).to_le_bytes());
                server
                    .send_to(
                        &seal_packet(&cipher, 7, relay_sequence, 1, &wrong).unwrap(),
                        client,
                    )
                    .unwrap();
                relay_sequence += 1;
                wrong_probes += 1;
                latest_probe = Some((plain, client));
            }
        }
    }
    assert!(wrong_probes >= 2);
    assert!(!counters.snapshot().udp_reachable);
    let (probe, client) = latest_probe.unwrap();
    server
        .send_to(
            &seal_packet(&cipher, 7, relay_sequence, 1, &probe).unwrap(),
            client,
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(1);
    while !counters.snapshot().udp_reachable {
        assert!(
            Instant::now() < until,
            "Current authenticated probe did not recover the same worker"
        );
        thread::sleep(Duration::from_millis(2));
    }
    assert!(counters.snapshot().udp_recovery_remaining_ms.is_none());
    assert!(muted.load(Ordering::Acquire) && deafened.load(Ordering::Acquire));
    assert!(errors.lock().unwrap().is_none());
    muted.store(false, Ordering::Release);
    deafened.store(false, Ordering::Release);
    capture.lock().unwrap().extend(input.iter().copied());
    let until = Instant::now() + Duration::from_secs(2);
    let (new_outer, new_media, resumed_client) = loop {
        assert!(
            Instant::now() < until,
            "Explicit unmute after recovery did not send protected Opus"
        );
        if let Some((sequence, plain, client)) = receive_voice_fixture(&server, &cipher) {
            assert!(
                outer_sequences.insert(sequence),
                "Outer nonce sequence restarted after recovery"
            );
            if plain.first() == Some(&AUDIO_PACKET_V3) {
                break (sequence, plain, client);
            }
        }
    };
    let (first_outer, first_media, first_client) = first_media.unwrap();
    let first_sequence = u64::from_le_bytes(first_media[9..17].try_into().unwrap());
    let resumed_sequence = u64::from_le_bytes(new_media[9..17].try_into().unwrap());
    assert_eq!(
        resumed_client, first_client,
        "Recovery reopened the routing socket"
    );
    assert!(new_outer > first_outer && resumed_sequence > first_sequence);
    assert_eq!(
        first_media[36..52],
        new_media[36..52],
        "Recovery replaced the sender nonce prefix"
    );
    assert_ne!(
        first_media[36..60],
        new_media[36..60],
        "Protected nonce repeated after recovery"
    );
    let mut listener = CallCipher::new(test_call_keys("remote")).unwrap();
    let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
    for media in [&first_media, &new_media] {
        let frame = listener
            .open(media)
            .expect("Recovered Opus failed end-to-end authentication");
        let mut samples = [0.0; AUDIO_FRAME_SAMPLES];
        decoder
            .decode_float(&frame.opus, &mut samples, false)
            .unwrap();
        assert!(samples.iter().any(|value| value.abs() > 0.001));
    }
    owner.finish();
    assert!(counters.snapshot().udp_recovery_remaining_ms.is_none());
    assert!(errors.lock().unwrap().is_none());
    voice_recovery_evidence(
        "voice-udp-recovery-evidence.json",
        serde_json::json!({
            "wrong_authenticated_probe_ids_rejected":wrong_probes,"same_routing_socket":true,
            "same_sender_nonce_prefix":true,"first_inner_sequence":first_sequence,"resumed_inner_sequence":resumed_sequence,
            "outer_sequences_unique":true,"inner_nonces_distinct":true,"authenticated_opus_before_and_after":true,
            "mute_and_deafen_preserved":true,"physical_devices_opened":false,
            "scope":"Actual loopback UDP worker, generated Opus, wrong authenticated probe IDs, five-second outage detection and same-worker recovery. Not device/WAN or native-video IV acceptance."
        }),
    );
}

#[test]
#[ignore = "Actual device-free 35-second UDP blackhole bound plus Cancel during recovery"]
fn actual_device_free_voice_blackhole_deadline_and_cancel() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (worker, stop, counters) = probe_fixture(server.local_addr().unwrap());
    let capture = worker.capture.clone();
    let playback = worker.playback.clone();
    let errors = worker.errors.clone();
    let first_prefix = worker.call.nonce_prefix;
    playback.lock().unwrap().extend([0.2; AUDIO_FRAME_SAMPLES]);
    let mut owner = OwnedVoiceWorker::start(worker);
    // A delayed capture callback must not prevent the worker's terminal path.
    let capture_guard = capture.lock().unwrap();
    let start = Instant::now();
    let until = start + Duration::from_secs(38);
    while !owner.finished() {
        assert!(
            Instant::now() < until,
            "UDP blackhole left an unbounded worker"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let elapsed = start.elapsed().as_millis();
    assert!(elapsed >= 34_500 && stop.load(Ordering::Acquire));
    assert!(errors
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|error| error.contains("within 30 seconds")
            && error.contains("join the voice room again")));
    assert!(playback.lock().unwrap().is_empty());
    assert!(counters.snapshot().udp_recovery_remaining_ms.is_none());
    drop(capture_guard);
    owner.finish();
    let (worker, stop, counters) = probe_fixture(server.local_addr().unwrap());
    assert_ne!(
        first_prefix, worker.call.nonce_prefix,
        "Fresh join reused a sender nonce prefix"
    );
    let errors = worker.errors.clone();
    let mut cancelled = OwnedVoiceWorker::start(worker);
    let until = Instant::now() + Duration::from_secs(7);
    while counters.snapshot().udp_recovery_remaining_ms.is_none() {
        assert!(
            Instant::now() < until,
            "Fresh worker never entered recovery"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let start_cancel = Instant::now();
    stop.store(true, Ordering::Release);
    while !cancelled.finished() {
        assert!(
            start_cancel.elapsed() < Duration::from_secs(1),
            "Cancel waited for the recovery deadline"
        );
        thread::sleep(Duration::from_millis(2));
    }
    let cancel_ms = start_cancel.elapsed().as_millis();
    cancelled.finish();
    assert!(
        errors.lock().unwrap().is_none() && counters.snapshot().udp_recovery_remaining_ms.is_none()
    );
    voice_recovery_evidence(
        "voice-udp-deadline-evidence.json",
        serde_json::json!({
            "initial_detection_ms":5000,"recovery_limit_ms":30000,"total_blackhole_ms":elapsed,
            "cancel_during_recovery_ms":cancel_ms,"cancel_reported_error":false,
            "delayed_capture_did_not_block_deadline":true,"playback_cleared":true,"fresh_join_nonce_prefix":true,
            "physical_devices_opened":false,
            "scope":"Actual loopback UDP blackhole and production worker deadline; no physical streams. Cancel in a fresh recovery is quiet and prompt; fresh worker has a new sender nonce prefix."
        }),
    );
}
