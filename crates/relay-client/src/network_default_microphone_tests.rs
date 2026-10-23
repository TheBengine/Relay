//! Real private-call admission and default microphone DSP, with owned PCM only.
//! This uses a fresh community fixture, not the public standalone Friends service.
use super::*;
use crate::media::LivingVoiceProbe;

#[derive(Default, Debug)]
struct Decoded {
    samples: usize,
    energy: f64,
    peak: f32,
}
impl Decoded {
    fn collect(&mut self, probe: &LivingVoiceProbe) {
        for sample in probe.take_rendered() {
            assert!(sample.is_finite());
            self.samples += 1;
            self.energy += f64::from(sample * sample);
            self.peak = self.peak.max(sample.abs());
        }
    }
    fn rms(&self) -> f64 {
        (self.energy / self.samples.max(1) as f64).sqrt()
    }
    fn assert_speech(&self) {
        assert!(
            self.samples > 48_000 && self.rms() > 0.001 && self.peak < 1.0,
            "default processed quiet speech must decode without clipping: {self:?}"
        );
    }
}

fn exchange(a: &LivingVoiceProbe, b: &LivingVoiceProbe, pcm: &[f32]) -> [Decoded; 2] {
    let mut output = [Decoded::default(), Decoded::default()];
    let started = Instant::now();
    // A new schedule for every phase avoids catch-up bursts after HTTP work.
    for (index, frame) in pcm.chunks_exact(480).enumerate() {
        let at = started + Duration::from_millis(index as u64 * 10);
        if let Some(wait) = at.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        a.feed_generated_frame(frame);
        b.feed_generated_frame(frame);
        output[0].collect(a);
        output[1].collect(b);
    }
    // Drain the ordinary jitter queue; no physical output callback is used.
    for _ in 0..25 {
        thread::sleep(Duration::from_millis(10));
        output[0].collect(a);
        output[1].collect(b);
    }
    a.assert_healthy();
    b.assert_healthy();
    output
}

#[test]
#[ignore = "owned server, real private-call MLS/admission and default RNNoise/VAD/AGC/Opus; generated PCM, no devices"]
fn actual_server_verified_private_call_carries_quiet_default_microphones_and_privacy() {
    let server = CryptoServer::start_with_udp();
    let (mut owner, mut a, mut b, _) = workers(&server);
    assert_eq!(
        a.worker.connection.as_ref().unwrap().session.user.role,
        "member"
    );
    assert_eq!(
        b.worker.connection.as_ref().unwrap().session.user.role,
        "member"
    );
    let _: serde_json::Value = a
        .worker
        .post(
            &format!("/api/friends/{}", b.origin().user_id),
            &json!(null),
        )
        .unwrap();
    let _: serde_json::Value = b
        .worker
        .post(
            &format!("/api/friends/{}", a.origin().user_id),
            &json!(null),
        )
        .unwrap();
    a.pin(&b.own());
    b.pin(&a.own());
    let ringing: relay_protocol::PrivateCall = a
        .worker
        .post("/api/private-calls", &json!({"peer_id":b.origin().user_id}))
        .unwrap();
    let accepted: relay_protocol::PrivateCall = b
        .worker
        .post(
            &format!("/api/private-calls/{}", ringing.id),
            &json!({"action":"accept"}),
        )
        .unwrap();
    assert_eq!(accepted.state, "accepted");
    let channel = accepted.channel_id;
    assert!(
        owner
            .worker
            .handle(Command::JoinVoiceRequest {
                channel_id: channel.clone(),
                request_id: 9940,
            })
            .is_err(),
        "the community owner is not a private-call participant"
    );
    let mut au = a.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9941,
    });
    let mut bu = b.handle(Command::JoinVoiceRequest {
        channel_id: channel.clone(),
        request_id: 9942,
    });
    let (ax, bx) = settle_voice_keys(&mut a, &mut b);
    au.extend(ax);
    bu.extend(bx);
    let (at, ak) = latest_voice_transport(au).expect("first actual private-call grant");
    let (bt, bk) = latest_voice_transport(bu).expect("second actual private-call grant");
    assert_eq!(ak.epoch, bk.epoch);
    assert_eq!(ak.group_id, bk.group_id);
    assert!(!ak.group_id.starts_with("server-managed:"));
    assert_eq!(ak.sender_keys.len(), 2);
    assert!(!ak.sender_keys.contains_key(&owner.origin().user_id));

    let mut audio_a = LivingVoiceProbe::start_default_microphone(&at, ak);
    let mut audio_b = LivingVoiceProbe::start_default_microphone(&bt, bk);
    let speech = LivingVoiceProbe::generated_quiet_speech();
    assert_eq!(speech.len(), 3 * 48_000);
    let silence = exchange(&audio_a, &audio_b, &vec![0.0; 24_000]);
    assert_eq!(
        (audio_a.microphone_packets(), audio_b.microphone_packets()),
        (0, 0)
    );
    let first = exchange(&audio_a, &audio_b, &speech);
    for decoded in &first {
        decoded.assert_speech();
    }
    audio_a.set_microphone_privacy(true, false);
    audio_b.set_microphone_privacy(false, true);
    exchange(&audio_a, &audio_b, &vec![0.0; 24_000]);
    let before = (audio_a.microphone_packets(), audio_b.microphone_packets());
    let private = exchange(&audio_a, &audio_b, &speech);
    assert_eq!(
        (audio_a.microphone_packets(), audio_b.microphone_packets()),
        before
    );
    assert!(private.iter().all(|decoded| decoded.peak == 0.0));
    let private_packets = [
        audio_a.microphone_packets() - before.0,
        audio_b.microphone_packets() - before.1,
    ];
    audio_a.set_microphone_privacy(false, false);
    audio_b.set_microphone_privacy(false, false);
    let resumed = exchange(&audio_a, &audio_b, &speech);
    for decoded in &resumed {
        decoded.assert_speech();
    }
    let stats = [audio_a.stats(), audio_b.stats()];
    assert!(stats
        .iter()
        .all(|s| s.sent_packets > 100 && s.received_packets > 100));
    assert!(stats
        .iter()
        .all(|s| s.udp_reachable && s.udp_round_trip_ms.is_some()));
    audio_a.finish();
    audio_b.finish();
    a.handle(Command::LeaveVoice);
    b.handle(Command::LeaveVoice);
    assert!(a.worker.protected_call_keys(&channel).is_err());
    assert!(b.worker.protected_call_keys(&channel).is_err());
    fs::write(
        server.root.join("default-private-microphone.json"),
        serde_json::to_vec_pretty(&json!({
            "actual_server_admission_and_mls":true, "two_non_owner_private_callers":true,
            "default_processing":true, "packet_ms":10, "jitter_ms":10,
            "generated_source_peak":0.005,
            "first_rms":[first[0].rms(), first[1].rms()],
            "resumed_rms":[resumed[0].rms(), resumed[1].rms()],
            "first_peaks":[first[0].peak,first[1].peak],
            "resumed_peaks":[resumed[0].peak,resumed[1].peak],
            "silence_peak":[silence[0].peak,silence[1].peak],
            "mute_deafen_peak":[private[0].peak,private[1].peak],
            "muted_deafened_audio_packets":private_packets,
            "sent_packets":[stats[0].sent_packets,stats[1].sent_packets],
            "received_packets":[stats[0].received_packets,stats[1].received_packets],
            "voice_udp_rtt_ms":[stats[0].udp_round_trip_ms,stats[1].udp_round_trip_ms],
            "physical_devices_opened":false, "standalone_friends_service":false,
            "wan_or_subjective_quality_accepted":false
        }))
        .unwrap(),
    )
    .unwrap();
}
