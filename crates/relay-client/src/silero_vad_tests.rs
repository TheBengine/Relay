//! Passive analysis must leave the real send path and privacy controls intact.
use super::*;

#[test]
fn silero_live_comparison_can_be_previewed_and_reverted_without_resetting_audio() {
    let engine = MediaEngine::new().unwrap();
    let controls = &engine.shared.controls;
    let generation = controls.privacy_generation.load(Ordering::Acquire);
    engine.preview_gate(&AudioSettings {
        compare_silero_vad: true,
        ..Default::default()
    });
    assert!(controls.compare_silero_vad.load(Ordering::Relaxed));
    engine.preview_gate(&AudioSettings::default());
    assert!(!controls.compare_silero_vad.load(Ordering::Relaxed));
    assert_eq!(
        controls.privacy_generation.load(Ordering::Acquire),
        generation
    );
}

#[test]
fn silero_is_opt_in_and_preserves_packet_and_delay_settings() {
    let old: AudioSettings = serde_json::from_str("{}").unwrap();
    assert!(!old.compare_silero_vad);
    for samples in [120, 240, 480, 960] {
        let mut settings = AudioSettings::default();
        settings.set_packet_samples(samples);
        let before = settings.voice_delay_budget();
        let controls = AudioControls::new(&settings);
        let batch = capture_processing_samples(samples, &controls);
        settings.compare_silero_vad = true;
        controls.apply_preview(&settings);
        assert_eq!(capture_processing_samples(samples, &controls), batch);
        assert_eq!(settings.voice_delay_budget().total_ms(), before.total_ms());
        let restored: AudioSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(restored.compare_silero_vad);
    }
}

#[test]
fn silero_does_not_change_pcm_or_transmission_with_or_without_suppression() {
    for suppression in [false, true] {
        for mode in [
            TransmissionMode::Continuous,
            TransmissionMode::VoiceActivation,
        ] {
            let settings = AudioSettings {
                noise_suppression: suppression,
                echo_cancellation: false,
                transmission: mode,
                ..Default::default()
            };
            let off = AudioControls::new(&settings);
            let on = AudioControls::new(&AudioSettings {
                compare_silero_vad: true,
                ..settings
            });
            let (mut original, mut compared) =
                (MicrophoneProcessor::new(), MicrophoneProcessor::new());
            let mut signal = crate::voice_test_signals::talker(1.0, (110.0, 170.0), 42, 0.03);
            signal.extend_from_slice(&[0.0; 4800]);
            for chunk in signal.chunks_exact(480) {
                let (mut a, mut b) = (chunk.to_vec(), chunk.to_vec());
                assert_eq!(
                    original.process(&mut a, &off),
                    compared.process(&mut b, &on)
                );
                assert_eq!(a, b);
            }
            assert!(compared.silero.is_some());
            on.compare_silero_vad.store(false, Ordering::Relaxed);
            compared.process(&mut [0.0; 480], &on);
            assert!(compared.silero.is_none());
            assert_eq!(on.silero_meter.read(), crate::silero_vad::Reading::Idle);
        }
    }
}

#[test]
fn silero_push_to_talk_discards_private_analysis() {
    let controls = AudioControls::new(&AudioSettings {
        compare_silero_vad: true,
        noise_suppression: false,
        echo_cancellation: false,
        transmission: TransmissionMode::PushToTalk,
        ..Default::default()
    });
    let mut processor = MicrophoneProcessor::new();
    controls.ptt_pressed.store(true, Ordering::Release);
    assert!(processor.process(&mut [0.02; 480], &controls));
    assert!(processor.silero.is_some());
    controls.ptt_pressed.store(false, Ordering::Release);
    assert!(!processor.process(&mut [0.02; 480], &controls));
    assert!(processor.silero.is_none());
    assert_eq!(
        controls.silero_meter.read(),
        crate::silero_vad::Reading::Idle
    );
}
