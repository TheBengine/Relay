//! Integration checks for the optional gain controller in the actual send chain.
use super::*;
use crate::audio_settings::AutomaticVolumeModel;

fn settings() -> AudioSettings {
    AudioSettings {
        automatic_volume_model: AutomaticVolumeModel::Sonora,
        microphone_gain: 1.0,
        noise_suppression: false,
        echo_cancellation: false,
        transmission: TransmissionMode::Continuous,
        ..Default::default()
    }
}

#[test]
fn sonora_preference_is_opt_in_and_round_trips() {
    let old: AudioSettings =
        serde_json::from_str(r#"{"automatic_microphone_volume":true}"#).unwrap();
    assert_eq!(old.automatic_volume_model, AutomaticVolumeModel::Relay);
    assert_eq!(
        AudioSettings::default().automatic_volume_model,
        AutomaticVolumeModel::Relay
    );
    let selected = settings();
    let restored: AudioSettings =
        serde_json::from_str(&serde_json::to_string(&selected).unwrap()).unwrap();
    assert_eq!(restored, selected);
}

#[test]
fn sonora_capture_batch_and_delay_budget_agree_for_every_packet_size() {
    for samples in [120, 240, 480, 960] {
        for automatic in [false, true] {
            let mut settings = settings();
            settings.set_packet_samples(samples);
            settings.automatic_microphone_volume = automatic;
            let controls = AudioControls::new(&settings);
            let expected = if automatic { samples.max(480) } else { samples };
            assert_eq!(capture_processing_samples(samples, &controls), expected);
            assert_eq!(
                settings.voice_delay_budget().capture_ms,
                expected as f32 / 48.0
            );
            assert_eq!(settings.voice_delay_budget().suppression_ms, 0);
            // The worker still emits the selected Opus packet duration.
            assert_eq!(
                controls.packet_samples.load(Ordering::Relaxed),
                samples as u32
            );
        }
    }
}

#[test]
fn sonora_detailed_delay_budget_uses_the_processing_batch_for_callback_alignment() {
    for samples in [120, 240, 480, 960] {
        let mut settings = settings();
        settings.set_packet_samples(samples);
        let batch_ms = samples.max(480) as f32 / 48.0;
        let budget = send_delay_budget_for_capture(&settings, 48_000, 20.0);
        assert_eq!(budget.packetization, batch_ms);
        assert_eq!(budget.capture_period, 20.0 - batch_ms);
        assert_eq!(budget.suppression, 0.0);
        let resampled = send_delay_budget_for_capture(&settings, 44_100, 3.0);
        assert_eq!(resampled.packetization, batch_ms);
        assert_eq!(resampled.capture_period, 3.0);
        assert!(resampled.resample > 0.0);
        settings.automatic_microphone_volume = false;
        let bypass = send_delay_budget_for_capture(&settings, 48_000, 20.0);
        assert_eq!(bypass.packetization, samples as f32 / 48.0);
    }
}

#[test]
fn sonora_learns_quiet_speech_without_running_relay_gain_or_amplifying_silence() {
    let controls = AudioControls::new(&settings());
    let mut processor = MicrophoneProcessor::new();
    let input = crate::voice_test_signals::talker(10.0, (110.0, 170.0), 42, 0.03);
    let mut output = input.clone();
    for frame in output.chunks_exact_mut(480) {
        assert!(processor.process(frame, &controls));
    }
    let power = |samples: &[f32]| samples.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
    let amplification = power(&output[240_000..]) / power(&input[240_000..]);
    assert!(
        amplification > 1.1,
        "adaptive gain did not activate: {amplification}"
    );
    assert_eq!(processor.automatic_gain, 1.0);
    assert_eq!(processor.speech_power, 0.0);
    assert!(output.iter().all(|x| x.is_finite() && x.abs() <= 0.95));
    let mut silence = [0.0; 480];
    assert!(processor.process(&mut silence, &controls));
    assert!(silence.iter().all(|x| *x == 0.0));
}

#[test]
fn sonora_ptt_release_discards_history_and_restarts_like_a_fresh_processor() {
    let settings = AudioSettings {
        transmission: TransmissionMode::PushToTalk,
        ..settings()
    };
    let controls = AudioControls::new(&settings);
    controls.ptt_pressed.store(true, Ordering::Release);
    let mut processor = MicrophoneProcessor::new();
    for frame in
        crate::voice_test_signals::talker(2.0, (110.0, 170.0), 42, 0.03).chunks_exact_mut(480)
    {
        assert!(processor.process(frame, &controls));
    }
    assert!(processor.sonora_gain.is_some());
    controls.ptt_pressed.store(false, Ordering::Release);
    assert!(!processor.process(&mut [0.4; 480], &controls));
    assert!(processor.sonora_gain.is_none());
    controls.ptt_pressed.store(true, Ordering::Release);
    let mut actual = [0.02; 480];
    let mut expected = actual;
    assert!(processor.process(&mut actual, &controls));
    assert!(MicrophoneProcessor::new().process(&mut expected, &controls));
    assert_eq!(actual, expected);
}

#[test]
fn sonora_disabling_volume_discards_history_and_passes_manual_audio() {
    let mut settings = settings();
    let controls = AudioControls::new(&settings);
    let mut processor = MicrophoneProcessor::new();
    assert!(processor.process(&mut [0.02; 480], &controls));
    settings.automatic_microphone_volume = false;
    controls.apply_preview(&settings);
    let mut frame = [0.02; 120];
    assert!(processor.process(&mut frame, &controls));
    assert!(processor.sonora_gain.is_none());
    assert_eq!(frame, [0.02; 120]);
}

#[test]
fn sonora_model_switch_invalidates_queued_audio_in_call_and_preview() {
    for preview in [false, true] {
        let mut settings = settings();
        let controls = AudioControls::new(&settings);
        let before = controls.privacy_generation.load(Ordering::Acquire);
        settings.automatic_volume_model = AutomaticVolumeModel::Relay;
        controls.apply_with(&settings, preview);
        assert_eq!(
            controls.privacy_generation.load(Ordering::Acquire),
            before + 1
        );
        let mut actual = [0.02; 480];
        let mut expected = actual;
        let mut reused = MicrophoneProcessor::new();
        reused.sonora_gain = Some(crate::audio_gain::SonoraGain::new());
        assert!(reused.process(&mut actual, &controls));
        assert!(MicrophoneProcessor::new().process(&mut expected, &controls));
        assert_eq!(actual, expected);
        assert!(reused.sonora_gain.is_none());
    }
}

#[test]
fn sonora_does_not_open_manual_gate_by_amplifying_input() {
    let controls = AudioControls::new(&AudioSettings {
        transmission: TransmissionMode::VoiceActivation,
        automatic_sensitivity: Some(false),
        activation_threshold: 0.05,
        activation_hold_ms: 0,
        ..settings()
    });
    let mut processor = MicrophoneProcessor::new();
    for frame in
        crate::voice_test_signals::talker(2.0, (110.0, 170.0), 42, 0.03).chunks_exact_mut(480)
    {
        assert!(!processor.process(frame, &controls));
    }
}
