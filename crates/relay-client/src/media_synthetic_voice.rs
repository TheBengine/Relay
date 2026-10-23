//! Opt-in microphone-path integration using owned generated PCM and local UDP.
//! No device backend, GUI, account service, physical capture or playback.
use super::*;

struct Running {
    stop: Arc<AtomicBool>,
    worker_stops: Vec<Arc<AtomicBool>>,
    threads: Vec<JoinHandle<()>>,
}
impl Running {
    fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
        for stop in &self.worker_stops {
            stop.store(true, Ordering::Release);
        }
        for thread in self.threads.drain(..) {
            assert!(thread.join().is_ok(), "owned synthetic voice thread");
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for stop in &self.worker_stops {
            stop.store(true, Ordering::Release);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn feed(capture: &SampleQueue, samples: &[f32]) {
    // This replaces an input callback, not the actual processor/encoder/worker.
    // A fresh monotonic schedule per phase prevents burst feeding after pauses.
    let began = Instant::now();
    for (index, frame) in samples.chunks_exact(480).enumerate() {
        let at = began + Duration::from_millis(index as u64 * 10);
        if let Some(wait) = at.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        let mut queue = capture.lock().unwrap();
        assert!(
            queue.len() < MAX_CAPTURE_SAMPLES,
            "owned feeder overran capture"
        );
        queue.extend(frame.iter().copied());
    }
    thread::sleep(Duration::from_millis(180));
}

fn rendered(output: &Mutex<Vec<f32>>) -> (usize, f64, f32) {
    let mut output = output.lock().unwrap();
    let length = output.len();
    let mut energy = 0.0;
    let mut peak = 0.0_f32;
    for sample in output.drain(..) {
        assert!(sample.is_finite());
        energy += f64::from(sample * sample);
        peak = peak.max(sample.abs());
    }
    (length, (energy / length.max(1) as f64).sqrt(), peak)
}

#[test]
#[ignore = "owned generated speech through native RNNoise/VAD/AGC, encrypted UDP workers and decoded queue; no devices"]
fn quiet_microphone_pipeline_crosses_encrypted_udp_and_preserves_privacy_controls() {
    let speech = tests::owned_synthetic_speech();
    let source_peak = speech
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    let quiet: Vec<_> = speech
        .iter()
        .take(6 * 48_000)
        .map(|s| s * 0.005 / source_peak)
        .collect();
    assert!(quiet.len() >= 3 * 48_000);
    let first = UdpSocket::bind("127.0.0.1:0").unwrap();
    let second = UdpSocket::bind("127.0.0.1:0").unwrap();
    first.set_nonblocking(true).unwrap();
    second.set_nonblocking(true).unwrap();
    let (mut sender, sender_stop, _) = tests::probe_fixture(first.local_addr().unwrap());
    let (mut receiver, receiver_stop, _) = tests::probe_fixture(second.local_addr().unwrap());
    let destinations = [
        sender.socket.local_addr().unwrap(),
        receiver.socket.local_addr().unwrap(),
    ];
    let ciphers = [
        sender.cipher.clone(),
        ChaCha20Poly1305::new_from_slice(&[24; 32]).unwrap(),
    ];
    receiver.session_id = 8;
    receiver.cipher = ciphers[1].clone();
    receiver.call = CallCipher::new(tests::test_call_keys("remote")).unwrap();
    let settings = AudioSettings::default();
    assert!(
        settings.noise_suppression
            && settings.automatic_microphone_volume
            && settings.automatic_sensitivity()
    );
    assert_eq!((settings.packet_ms, settings.jitter_ms), (10, 10));
    sender.controls = Arc::new(AudioControls::new(&settings));
    receiver.controls = Arc::new(AudioControls::new(&settings));
    for worker in [&sender, &receiver] {
        worker.muted.store(false, Ordering::Release);
        worker.deafened.store(false, Ordering::Release);
    }
    let capture = sender.capture.clone();
    let playback = receiver.playback.clone();
    let muted = sender.muted.clone();
    let deafened = receiver.deafened.clone();
    let controls = sender.controls.clone();
    let errors = [sender.errors.clone(), receiver.errors.clone()];
    let forwarded = Arc::new(AtomicUsize::new(0));
    let output = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut running = Running {
        stop: stop.clone(),
        worker_stops: vec![sender_stop, receiver_stop],
        threads: Vec::new(),
    };
    let router_stop = stop.clone();
    let router_forwarded = forwarded.clone();
    running.threads.push(thread::spawn(move || {
        let sockets = [first, second];
        let sessions = [7, 8];
        let mut sequences = [0_u64, 0];
        let mut replay = [ReplayWindow::default(), ReplayWindow::default()];
        let mut bytes = [0; UDP_MAX_PACKET];
        while !router_stop.load(Ordering::Acquire) {
            for index in 0..2 {
                for _ in 0..32 {
                    let (length, address) = match sockets[index].recv_from(&mut bytes) {
                        Ok(value) => value,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("owned relay read: {e}"),
                    };
                    assert_eq!(address, destinations[index]);
                    let (sequence, plain) =
                        open_packet(&ciphers[index], sessions[index], 0, &bytes[..length]).unwrap();
                    assert!(replay[index].accept(sequence));
                    let audio = plain.first() == Some(&AUDIO_PACKET_V3);
                    let target = if audio { 1 - index } else { index };
                    let packet = seal_packet(
                        &ciphers[target],
                        sessions[target],
                        sequences[target],
                        1,
                        &plain,
                    )
                    .unwrap();
                    sequences[target] += 1;
                    sockets[target]
                        .send_to(&packet, destinations[target])
                        .unwrap();
                    if audio && index == 0 {
                        router_forwarded.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    }));
    let drain_stop = stop.clone();
    let drain_output = output.clone();
    running.threads.push(thread::spawn(move || {
        while !drain_stop.load(Ordering::Acquire) {
            if let Ok(mut queue) = playback.lock() {
                drain_output.lock().unwrap().extend(queue.drain(..));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }));
    running
        .threads
        .push(thread::spawn(move || voice_worker(receiver)));
    running
        .threads
        .push(thread::spawn(move || voice_worker(sender)));
    feed(&capture, &vec![0.0; 24_000]);
    let silence_packets = forwarded.load(Ordering::Relaxed);
    assert_eq!(
        silence_packets, 0,
        "settled silence must not become microphone speech"
    );
    rendered(&output);
    feed(&capture, &quiet);
    let first_packets = forwarded.load(Ordering::Relaxed);
    let first_output = rendered(&output);
    assert!(
        first_packets > 100
            && first_output.0 > 48_000
            && first_output.1 > 0.001
            && first_output.2 < 1.0,
        "quiet speech must reach decoded playback: {first_packets}, {first_output:?}"
    );

    // Device privacy revisions are a real worker reset signal. No device is
    // opened, and this is not a claim about a physical driver switch.
    muted.store(true, Ordering::Release);
    deafened.store(true, Ordering::Release);
    controls.privacy_generation.fetch_add(1, Ordering::AcqRel);
    thread::sleep(Duration::from_millis(120));
    let before_muted = forwarded.load(Ordering::Relaxed);
    rendered(&output);
    feed(&capture, &quiet[..48_000]);
    assert_eq!(forwarded.load(Ordering::Relaxed), before_muted);
    assert!(muted.load(Ordering::Acquire) && deafened.load(Ordering::Acquire));
    let muted_output = rendered(&output);
    assert_eq!(muted_output.2, 0.0);

    muted.store(false, Ordering::Release);
    feed(&capture, &quiet[..48_000]);
    let before_resume = forwarded.load(Ordering::Relaxed);
    assert!(
        before_resume > before_muted,
        "explicit unmute must resume transmission"
    );
    let deaf_output = rendered(&output);
    assert_eq!(deaf_output.2, 0.0, "deafen must not queue remote speech");
    deafened.store(false, Ordering::Release);
    feed(&capture, &quiet[..2 * 48_000]);
    let resumed_output = rendered(&output);
    assert!(
        resumed_output.0 > 24_000 && resumed_output.1 > 0.001 && resumed_output.2 < 1.0,
        "explicit undeafen must restore decoded speech: {resumed_output:?}"
    );
    running.finish();
    for error in errors {
        let error = error.lock().unwrap().clone();
        assert!(error.is_none(), "owned worker error: {error:?}");
    }
    println!("Synthetic microphone path: initial_silence_packets={silence_packets}, quiet_audio_packets={first_packets}, quiet_decoded={first_output:?}, muted={muted_output:?}, deafened={deaf_output:?}, resumed={resumed_output:?}. Native RNNoise/VAD/AGC, real Opus and nested authenticated UDP; no device clock, hardware, account acceptance, WAN or subjective listening.");
}
