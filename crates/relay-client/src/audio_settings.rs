//! Persisted, bounded settings for the native audio engine.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum TransmissionMode {
    Continuous = 0,
    #[default]
    VoiceActivation = 1,
    PushToTalk = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyMode {
    Adaptive,
    LowLatency,
    Balanced,
    Custom,
}

/// One factory for the native voice path and its settings timing model.
#[cfg(test)]
pub(crate) fn make_voice_encoder(bitrate_kbps: u32) -> Result<opus::Encoder, opus::Error> {
    make_voice_encoder_with_recovery(bitrate_kbps, false)
}

pub(crate) fn make_voice_encoder_with_recovery(
    bitrate_kbps: u32,
    recovery: bool,
) -> Result<opus::Encoder, opus::Error> {
    let mut encoder = opus::Encoder::new(
        relay_protocol::AUDIO_SAMPLE_RATE,
        opus::Channels::Mono,
        if recovery {
            opus::Application::Voip
        } else {
            opus::Application::LowDelay
        },
    )?;
    encoder.set_bitrate(opus::Bitrate::Bits((bitrate_kbps * 1000) as i32))?;
    encoder.set_inband_fec(recovery)?;
    // Explicit resilience preference, not an estimate of the other person's
    // receiving loss. Incoming counters cannot tell us outgoing packet loss.
    encoder.set_packet_loss_perc(if recovery { 10 } else { 0 })?;
    Ok(encoder)
}

/// A conservative timing budget for a voice path with these settings at both ends.
/// Receive targets are budgets, not a mandatory wait after every packet. Device,
/// network, scheduling and DSP execution time cannot be inferred from preferences.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceDelayBudget {
    pub capture_ms: f32,
    pub suppression_ms: u32,
    pub speech_start_ms: f32,
    pub codec_ms: Option<f32>,
    pub playback_ms: f32,
    pub receive_min_ms: u32,
    pub receive_max_ms: u32,
}

impl VoiceDelayBudget {
    pub fn total_ms(self) -> Option<(f32, f32)> {
        let outgoing = self.capture_ms
            + self.suppression_ms as f32
            + self.speech_start_ms
            + self.codec_ms?
            + self.playback_ms;
        Some((
            outgoing + self.receive_min_ms as f32,
            outgoing + self.receive_max_ms as f32,
        ))
    }

    pub fn label(self) -> String {
        match self.total_ms() {
            Some((low, high)) if low == high => format!("{low:.1} ms"),
            Some((low, high)) => format!("{low:.1}–{high:.1} ms"),
            None => "Estimate unavailable".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// None uses the system default. Explicit device names must still exist.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub microphone_gain: f32,
    pub playback_volume: f32,
    pub transmission: TransmissionMode,
    /// RMS amplitude in normalized PCM, after gain and optional noise suppression.
    pub activation_threshold: f32,
    /// None migrates the old factory threshold to automatic sensitivity,
    /// while retaining an older user's custom manual threshold.
    pub automatic_sensitivity: Option<bool>,
    pub automatic_microphone_volume: bool,
    pub activation_hold_ms: u32,
    /// Retain 10 ms (one whole packet minimum) before the Voice Activity gate opens.
    pub preserve_speech_start: bool,
    pub bitrate_kbps: u32,
    /// Optional speech redundancy; inactive for packets shorter than 10 ms.
    pub recover_packet_loss: bool,
    /// Opus audio per transmitted packet: 5, 10 or 20 milliseconds.
    pub packet_ms: u32,
    /// Keep the legacy integer preference for older profiles. This selects 120 samples.
    pub ultra_short_packets: bool,
    /// Explicit opt-in because direct peers learn each other's network address.
    pub direct_voice_enabled: bool,
    /// Explicit opt-in: let an approved local voice gateway help carry this call's voice.
    /// Off by default. It is only ever used after a measured full voice path.
    #[serde(default)]
    pub local_voice_gateways: bool,
    /// Prefer the smallest supported Windows shared-mode audio period.
    pub low_latency_device: bool,
    pub overlay_enabled: bool,
    pub overlay_x: f32,
    pub overlay_y: f32,
    pub ducking_enabled: bool,
    pub ducking_volume: f32,
    pub priority_enabled: bool,
    pub priority_volume: f32,
    pub whisper_enabled: bool,
    pub tcp_fallback_enabled: bool,
    /// Best-effort voice traffic classification; drivers/routers may ignore it.
    pub voice_qos: bool,
    /// Initial receive jitter buffer. Larger values tolerate arrival jitter at extra latency.
    pub jitter_ms: u32,
    /// New profiles use automatic timing; old files that omit this field stay manual.
    #[serde(default)]
    pub adaptive_jitter: bool,
    pub noise_suppression: bool,
    /// Experimental native AEC. The reference is only Relay's actual speaker output.
    pub echo_cancellation: bool,
    pub echo_delay_ms: u32,
    pub sounds_enabled: bool,
    pub sound_volume: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            microphone_gain: 1.5,
            playback_volume: 1.0,
            transmission: TransmissionMode::VoiceActivation,
            activation_threshold: 0.02,
            automatic_sensitivity: None,
            automatic_microphone_volume: true,
            activation_hold_ms: 200,
            preserve_speech_start: true,
            bitrate_kbps: 96,
            recover_packet_loss: false,
            packet_ms: 10,
            ultra_short_packets: false,
            direct_voice_enabled: false,
            local_voice_gateways: false,
            low_latency_device: true,
            overlay_enabled: false,
            overlay_x: 24.0,
            overlay_y: 24.0,
            ducking_enabled: false,
            ducking_volume: 0.3,
            priority_enabled: false,
            priority_volume: 0.3,
            whisper_enabled: false,
            tcp_fallback_enabled: false,
            voice_qos: true,
            jitter_ms: 10,
            adaptive_jitter: true,
            noise_suppression: true,
            echo_cancellation: false,
            echo_delay_ms: 40,
            sounds_enabled: true,
            sound_volume: 0.25,
        }
    }
}

/// A call shortcut commits only its explicit choice. Other editor fields remain
/// pending until Save Changes, including capture mode and selected devices.
pub(crate) fn quick_audio_change(
    applied: &AudioSettings,
    edited: &mut AudioSettings,
    change: impl Fn(&mut AudioSettings),
) -> AudioSettings {
    let mut next = applied.clone();
    change(&mut next);
    change(edited);
    next.normalized()
}

/// Roll back only the failed device choice, retaining unrelated controls and
/// any newer unsaved selection in the editor.
pub(crate) fn restore_device_selection(
    settings: &mut AudioSettings,
    attempted: &AudioSettings,
    restored: &AudioSettings,
) {
    if settings.input_device == attempted.input_device {
        settings.input_device = restored.input_device.clone();
    }
    if settings.output_device == attempted.output_device {
        settings.output_device = restored.output_device.clone();
    }
    if settings.low_latency_device == attempted.low_latency_device {
        settings.low_latency_device = restored.low_latency_device;
    }
}

/// A zero volume is independent of the call's mute/deafen controls. Explain
/// that local cause before asking the other person to troubleshoot their mic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalVolumeIssue {
    Output,
    Microphone,
}

impl LocalVolumeIssue {
    pub(crate) fn current(settings: &AudioSettings, muted: bool, deafened: bool) -> Option<Self> {
        if deafened {
            None
        } else if settings.playback_volume <= 0.0 {
            Some(Self::Output)
        } else if !muted && settings.microphone_gain <= 0.0 {
            Some(Self::Microphone)
        } else {
            None
        }
    }

    /// Called only by the explicit recovery button. Other audio preferences,
    /// selected devices, and the call's mute/deafen state remain unchanged.
    pub(crate) fn restore(self, settings: &mut AudioSettings) {
        match self {
            Self::Output => settings.playback_volume = AudioSettings::default().playback_volume,
            Self::Microphone => settings.microphone_gain = AudioSettings::default().microphone_gain,
        }
    }
}

impl AudioSettings {
    pub fn automatic_sensitivity(&self) -> bool {
        self.automatic_sensitivity
            .unwrap_or(self.activation_threshold == 0.02)
    }

    pub fn packet_samples(&self) -> usize {
        if self.ultra_short_packets {
            120
        } else {
            match self.packet_ms {
                5 => 240,
                20 => 960,
                _ => 480,
            }
        }
    }

    pub fn loss_recovery_enabled(&self) -> bool {
        self.recover_packet_loss && self.packet_samples() >= 480
    }

    pub fn packet_duration_ms(&self) -> f32 {
        self.packet_samples() as f32 / 48.0
    }

    pub fn set_packet_samples(&mut self, samples: usize) {
        self.ultra_short_packets = samples == 120;
        self.packet_ms = match samples {
            120 | 240 => 5,
            960 => 20,
            _ => 10,
        };
    }

    pub fn voice_delay_budget(&self) -> VoiceDelayBudget {
        let settings = self.clone().normalized();
        let needs_dsp = settings.noise_suppression;
        #[cfg(feature = "native-rtc")]
        let needs_dsp = needs_dsp || settings.echo_cancellation;
        // Query the actual linked Opus implementation once; never open audio devices.
        static CODEC_MS: [std::sync::OnceLock<Option<f32>>; 2] =
            [std::sync::OnceLock::new(), std::sync::OnceLock::new()];
        let recovery = settings.loss_recovery_enabled();
        let codec_ms = *CODEC_MS[usize::from(recovery)].get_or_init(|| {
            let mut encoder = make_voice_encoder_with_recovery(64, recovery).ok()?;
            let samples = encoder.get_lookahead().ok()?;
            (samples >= 0)
                .then_some(samples as f32 * 1000.0 / relay_protocol::AUDIO_SAMPLE_RATE as f32)
        });
        VoiceDelayBudget {
            capture_ms: if needs_dsp {
                settings.packet_duration_ms().max(10.0)
            } else {
                settings.packet_duration_ms()
            },
            // RNNoise's overlap/add synthesis retains one 480-sample block.
            suppression_ms: if settings.noise_suppression {
                (nnnoiseless::DenoiseState::FRAME_SIZE as u32 * 1000)
                    / relay_protocol::AUDIO_SAMPLE_RATE
            } else {
                0
            },
            speech_start_ms: if settings.preserve_speech_start
                && settings.transmission == TransmissionMode::VoiceActivation
            {
                settings.packet_duration_ms().max(10.0)
            } else {
                0.0
            },
            codec_ms,
            playback_ms: crate::voice_output::reserve_samples(None) as f32 / 48.0
                + crate::voice_output::FILTER_DELAY_MS,
            receive_min_ms: if settings.adaptive_jitter {
                crate::adaptive_audio::MIN_DELAY_MS
            } else {
                settings.jitter_ms
            },
            receive_max_ms: if settings.adaptive_jitter {
                crate::adaptive_audio::MAX_DELAY_MS
            } else {
                settings.jitter_ms
            },
        }
    }

    pub fn voice_delay_budget_for_output(&self, period_ms: Option<f32>) -> VoiceDelayBudget {
        let mut budget = self.voice_delay_budget();
        budget.playback_ms = crate::voice_output::reserve_samples(period_ms) as f32 / 48.0
            + crate::voice_output::FILTER_DELAY_MS;
        budget
    }

    pub fn latency_mode(&self) -> LatencyMode {
        if self.adaptive_jitter {
            return LatencyMode::Adaptive;
        }
        match self.jitter_ms {
            10 => LatencyMode::LowLatency,
            40 => LatencyMode::Balanced,
            _ => LatencyMode::Custom,
        }
    }

    pub fn set_latency_mode(&mut self, mode: LatencyMode) {
        self.adaptive_jitter = mode == LatencyMode::Adaptive;
        self.jitter_ms = match mode {
            LatencyMode::Adaptive => 10,
            LatencyMode::LowLatency => 10,
            LatencyMode::Balanced => 40,
            LatencyMode::Custom => self.jitter_ms.clamp(5, 120).div_ceil(5) * 5,
        };
    }

    pub fn normalized(mut self) -> Self {
        let defaults = Self::default();
        fn bounded(value: f32, fallback: f32, low: f32, high: f32) -> f32 {
            if value.is_finite() {
                value.clamp(low, high)
            } else {
                fallback
            }
        }
        self.microphone_gain = bounded(self.microphone_gain, defaults.microphone_gain, 0.0, 4.0);
        self.playback_volume = bounded(self.playback_volume, defaults.playback_volume, 0.0, 2.0);
        self.activation_threshold = bounded(
            self.activation_threshold,
            defaults.activation_threshold,
            0.001,
            0.5,
        );
        self.overlay_x = bounded(self.overlay_x, defaults.overlay_x, 0.0, 16000.0);
        self.overlay_y = bounded(self.overlay_y, defaults.overlay_y, 0.0, 16000.0);
        self.ducking_volume = bounded(self.ducking_volume, defaults.ducking_volume, 0.0, 1.0);
        self.priority_volume = bounded(self.priority_volume, defaults.priority_volume, 0.0, 1.0);
        self.sound_volume = bounded(self.sound_volume, defaults.sound_volume, 0.0, 1.0);
        self.activation_hold_ms = self.activation_hold_ms.min(1000);
        self.bitrate_kbps = self.bitrate_kbps.clamp(16, 128);
        if !matches!(self.packet_ms, 5 | 10 | 20) {
            self.packet_ms = defaults.packet_ms;
        }
        self.jitter_ms = self.jitter_ms.clamp(5, 120).div_ceil(5) * 5;
        self.echo_delay_ms = self.echo_delay_ms.min(250);
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct AudioDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub bluetooth: std::collections::HashSet<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

pub(crate) fn bluetooth_enumerator(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "BTHENUM" | "BTHLEDEVICE"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCue {
    Ring,
    Ringback,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    Join,
    Leave,
    CallEnd,
    Message,
}

#[cfg(test)]
mod tests {
    #[test]
    fn zero_volume_recovery_preserves_call_policy_and_unsaved_device_choices() {
        let applied = super::AudioSettings {
            input_device: Some("call microphone".into()),
            output_device: Some("call headset".into()),
            microphone_gain: 0.0,
            playback_volume: 0.0,
            transmission: super::TransmissionMode::PushToTalk,
            automatic_sensitivity: Some(false),
            activation_threshold: 0.03,
            ..Default::default()
        };
        let mut edited = applied.clone();
        edited.input_device = Some("pending microphone".into());
        edited.activation_threshold = 0.05;
        assert_eq!(super::LocalVolumeIssue::current(&applied, false, true), None);
        let issue = super::LocalVolumeIssue::current(&applied, true, false).unwrap();
        assert_eq!(issue, super::LocalVolumeIssue::Output);
        let restored = super::quick_audio_change(&applied, &mut edited, |audio| issue.restore(audio));
        let mut expected = applied.clone();
        expected.playback_volume = super::AudioSettings::default().playback_volume;
        assert_eq!(restored, expected);
        assert_eq!(edited.input_device.as_deref(), Some("pending microphone"));
        assert_eq!(edited.activation_threshold, 0.05);
        assert_eq!(edited.playback_volume, restored.playback_volume);
        // A deliberately muted microphone needs no volume recovery suggestion.
        assert_eq!(super::LocalVolumeIssue::current(&restored, true, false), None);
        let issue = super::LocalVolumeIssue::current(&restored, false, false).unwrap();
        assert_eq!(issue, super::LocalVolumeIssue::Microphone);
        let restored = super::quick_audio_change(&restored, &mut edited, |audio| issue.restore(audio));
        expected.microphone_gain = super::AudioSettings::default().microphone_gain;
        assert_eq!(restored, expected);
        assert_eq!(super::LocalVolumeIssue::current(&restored, false, false), None);
        assert_eq!(edited.input_device.as_deref(), Some("pending microphone"));
        assert_eq!(edited.activation_threshold, 0.05);
    }

    #[test]
    fn bluetooth_names_and_measured_playback_reserves_are_conservative() {
        for name in ["BTHENUM", "BTHLEDevice"] {
            assert!(super::bluetooth_enumerator(name));
        }
        for name in ["USB", "", "USB BTHENUM"] {
            assert!(!super::bluetooth_enumerator(name));
        }
        let settings = super::AudioSettings::default();
        assert_eq!(
            settings
                .voice_delay_budget_for_output(Some(2.5))
                .playback_ms,
            2.5 + super::super::voice_output::FILTER_DELAY_MS
        );
        assert_eq!(
            settings.voice_delay_budget_for_output(None).playback_ms,
            5.0 + super::super::voice_output::FILTER_DELAY_MS
        );
    }

    #[test]
    fn older_manual_sensitivity_is_kept_and_factory_sensitivity_becomes_automatic() {
        let factory: super::AudioSettings =
            serde_json::from_str(r#"{"activation_threshold":0.02,"microphone_gain":1.0}"#).unwrap();
        assert!(factory.automatic_sensitivity());
        assert_eq!(factory.microphone_gain, 1.0);
        assert!(factory.automatic_microphone_volume);
        let manual: super::AudioSettings =
            serde_json::from_str(r#"{"activation_threshold":0.013}"#).unwrap();
        assert!(!manual.automatic_sensitivity());
        let explicit: super::AudioSettings = serde_json::from_str(r#"{"activation_threshold":0.02,"automatic_sensitivity":false,"automatic_microphone_volume":false}"#).unwrap();
        assert!(!explicit.automatic_sensitivity());
        assert!(!explicit.automatic_microphone_volume);
        assert!(super::AudioSettings::default().voice_qos);
        assert!(factory.voice_qos);
        assert_eq!(super::AudioSettings::default().packet_ms, 10);
        assert_eq!(super::AudioSettings::default().jitter_ms, 10);
    }
    use super::*;

    #[test]
    fn voice_delay_budget_models_batching_without_double_counting_and_adaptive_range() {
        // Fractional FIR delay introduces ordinary f32 rounding in differences.
        let assert_ms = |actual: f32, expected: f32| {
            assert!(
                (actual - expected).abs() < 0.00001,
                "{actual} != {expected}"
            );
        };
        let mut settings = AudioSettings {
            packet_ms: 5,
            noise_suppression: false,
            ..Default::default()
        };
        let dry = settings.voice_delay_budget();
        assert_eq!(dry.capture_ms, 5.0);
        assert_eq!(dry.suppression_ms, 0);
        assert_eq!(dry.playback_ms, 5.0 + 32.0 / 48.0);
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::LowDelay).unwrap();
        assert_eq!(
            dry.codec_ms.unwrap(),
            encoder.get_lookahead().unwrap() as f32 / 48.0
        );
        settings.noise_suppression = true;
        let denoised = settings.voice_delay_budget();
        assert_ms(
            denoised.total_ms().unwrap().0 - dry.total_ms().unwrap().0,
            15.0,
        );
        settings.packet_ms = 10;
        assert_eq!(settings.voice_delay_budget(), denoised);
        settings.packet_ms = 20;
        assert_ms(
            settings.voice_delay_budget().total_ms().unwrap().0 - denoised.total_ms().unwrap().0,
            20.0,
        );
        settings.set_latency_mode(LatencyMode::Adaptive);
        let automatic = settings.voice_delay_budget();
        let (low, high) = automatic.total_ms().unwrap();
        assert_ms(high - low, 70.0);
        assert!(automatic.label().contains('–'));
        settings.set_latency_mode(LatencyMode::Balanced);
        assert_ms(
            settings.voice_delay_budget().total_ms().unwrap().0 - low,
            30.0,
        );
    }

    #[test]
    fn trigger_reference_and_volume_settings_do_not_invent_fixed_delay() {
        let base = AudioSettings::default();
        let varied = AudioSettings {
            activation_hold_ms: 1000,
            activation_threshold: 0.5,
            transmission: TransmissionMode::VoiceActivation,
            echo_delay_ms: 250,
            bitrate_kbps: 128,
            microphone_gain: 4.0,
            playback_volume: 2.0,
            tcp_fallback_enabled: true,
            overlay_enabled: true,
            whisper_enabled: true,
            ducking_enabled: true,
            priority_enabled: true,
            input_device: Some("Different device".into()),
            ..base.clone()
        };
        assert_eq!(base.voice_delay_budget(), varied.voice_delay_budget());
        let invalid = AudioSettings {
            packet_ms: u32::MAX,
            jitter_ms: u32::MAX,
            adaptive_jitter: false,
            ..base
        };
        let bounded = invalid.voice_delay_budget();
        assert_eq!(bounded.capture_ms, 10.0);
        assert_eq!(bounded.receive_max_ms, 120);
    }

    #[test]
    fn recovery_is_optional_keeps_ten_ms_and_reports_its_actual_codec_delay() {
        let mut settings = AudioSettings::default();
        assert!(!settings.loss_recovery_enabled());
        let ordinary = settings.voice_delay_budget();
        settings.recover_packet_loss = true;
        assert!(settings.loss_recovery_enabled());
        assert_eq!((settings.packet_ms, settings.jitter_ms), (10, 10));
        let robust = settings.voice_delay_budget();
        let mut encoder = make_voice_encoder_with_recovery(96, true).unwrap();
        assert!(encoder.get_inband_fec().unwrap());
        assert_eq!(encoder.get_packet_loss_perc().unwrap(), 10);
        assert_eq!(
            robust.codec_ms,
            Some(encoder.get_lookahead().unwrap() as f32 / 48.0)
        );
        assert!(robust.codec_ms.unwrap() >= ordinary.codec_ms.unwrap());
        encoder.reset_state().unwrap();
        assert!(encoder.get_inband_fec().unwrap());
        settings.set_packet_samples(120);
        assert!(!settings.loss_recovery_enabled());
        assert_eq!(settings.voice_delay_budget().codec_ms, ordinary.codec_ms);
        settings.set_packet_samples(960);
        assert!(settings.loss_recovery_enabled());
        let saved = serde_json::to_string(&settings).unwrap();
        assert!(
            serde_json::from_str::<AudioSettings>(&saved)
                .unwrap()
                .recover_packet_loss
        );
        assert!(
            !serde_json::from_str::<AudioSettings>("{}")
                .unwrap()
                .recover_packet_loss
        );
    }

    #[test]
    fn speech_start_budget_is_explicit_optional_and_never_applies_to_push_to_talk() {
        let mut settings = AudioSettings::default();
        let with = settings.voice_delay_budget();
        assert_eq!(with.speech_start_ms, 10.0);
        assert_eq!((settings.packet_ms, settings.jitter_ms), (10, 10));
        settings.preserve_speech_start = false;
        assert_eq!(settings.voice_delay_budget().speech_start_ms, 0.0);
        assert!(
            (with.total_ms().unwrap().0
                - settings.voice_delay_budget().total_ms().unwrap().0
                - 10.0)
                .abs()
                < 0.00001
        );
        settings.preserve_speech_start = true;
        settings.packet_ms = 20;
        assert_eq!(settings.voice_delay_budget().speech_start_ms, 20.0);
        settings.transmission = TransmissionMode::PushToTalk;
        assert_eq!(settings.voice_delay_budget().speech_start_ms, 0.0);
        settings.transmission = TransmissionMode::Continuous;
        assert_eq!(settings.voice_delay_budget().speech_start_ms, 0.0);
        let old: AudioSettings = serde_json::from_str("{}").unwrap();
        assert!(old.preserve_speech_start);
        let explicit: AudioSettings =
            serde_json::from_str(r#"{"preserve_speech_start":false}"#).unwrap();
        assert!(!explicit.preserve_speech_start);
    }

    #[test]
    fn gaming_features_default_off_round_trip_independently_and_normalize_levels() {
        let defaults = AudioSettings::default();
        assert_eq!(defaults.packet_ms, 10);
        assert!(
            !defaults.overlay_enabled
                && !defaults.ducking_enabled
                && !defaults.priority_enabled
                && !defaults.whisper_enabled
                && !defaults.tcp_fallback_enabled
        );
        for field in [
            "overlay_enabled",
            "ducking_enabled",
            "priority_enabled",
            "whisper_enabled",
            "tcp_fallback_enabled",
        ] {
            let value = serde_json::json!({field: true});
            let settings: AudioSettings = serde_json::from_value(value).unwrap();
            let restored: AudioSettings =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(settings, restored);
            let enabled: Vec<_> = [
                settings.overlay_enabled,
                settings.ducking_enabled,
                settings.priority_enabled,
                settings.whisper_enabled,
                settings.tcp_fallback_enabled,
            ]
            .into_iter()
            .filter(|enabled| *enabled)
            .collect();
            assert_eq!(enabled.len(), 1);
        }
        let settings = AudioSettings {
            ducking_volume: f32::NAN,
            priority_volume: 4.0,
            overlay_x: -2.0,
            overlay_y: f32::INFINITY,
            ..Default::default()
        }
        .normalized();
        assert_eq!(settings.ducking_volume, defaults.ducking_volume);
        assert_eq!(settings.priority_volume, 1.0);
        assert_eq!(settings.overlay_x, 0.0);
        assert_eq!(settings.overlay_y, defaults.overlay_y);
    }
    #[test]
    fn packet_duration_preferences_round_trip_and_preserve_explicit_choices() {
        assert_eq!(
            serde_json::from_str::<AudioSettings>("{}")
                .unwrap()
                .packet_ms,
            10
        );
        for duration in [5, 10, 20] {
            let settings = AudioSettings {
                packet_ms: duration,
                jitter_ms: duration,
                ..Default::default()
            }
            .normalized();
            let restored: AudioSettings =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(restored.packet_ms, duration);
            assert_eq!(restored.jitter_ms, duration);
        }
        for duration in [0, 6, 15, u32::MAX] {
            assert_eq!(
                AudioSettings {
                    packet_ms: duration,
                    ..Default::default()
                }
                .normalized()
                .packet_ms,
                10
            );
        }
    }

    #[test]
    fn old_preferences_have_defaults_and_invalid_dsp_values_are_bounded() {
        let old: AudioSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            old,
            AudioSettings {
                adaptive_jitter: false,
                ..Default::default()
            }
        );
        let invalid = AudioSettings {
            microphone_gain: f32::NAN,
            playback_volume: f32::INFINITY,
            activation_threshold: -1.0,
            jitter_ms: 21,
            bitrate_kbps: u32::MAX,
            activation_hold_ms: u32::MAX,
            ..Default::default()
        }
        .normalized();
        assert_eq!(
            invalid.microphone_gain,
            AudioSettings::default().microphone_gain
        );
        assert_eq!(invalid.playback_volume, 1.0);
        assert_eq!(invalid.activation_threshold, 0.001);
        assert_eq!(invalid.jitter_ms, 25);
        assert_eq!(invalid.bitrate_kbps, 128);
        assert_eq!(invalid.activation_hold_ms, 1000);
    }

    #[test]
    fn new_preferences_use_one_frame_but_explicit_old_receive_buffers_survive() {
        let factory = AudioSettings::default();
        assert_eq!(factory.latency_mode(), LatencyMode::Adaptive);
        let explicit: AudioSettings =
            serde_json::from_str(r#"{"adaptive_jitter":false,"jitter_ms":10}"#).unwrap();
        assert_eq!(explicit.latency_mode(), LatencyMode::LowLatency);
        let fresh: AudioSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(fresh.jitter_ms, 10);
        assert_eq!(fresh.latency_mode(), LatencyMode::LowLatency);
        for value in [20, 40, 60, 120] {
            let saved: AudioSettings =
                serde_json::from_str(&format!("{{\"jitter_ms\":{value}}}")).unwrap();
            let restored: AudioSettings =
                serde_json::from_str(&serde_json::to_string(&saved.normalized()).unwrap()).unwrap();
            assert_eq!(restored.jitter_ms, value);
            assert!(!restored.adaptive_jitter);
        }
    }

    #[test]
    fn receive_buffer_presets_only_change_the_buffer_and_custom_values_stay_bounded() {
        let mut audio = AudioSettings {
            input_device: Some("Chosen input".into()),
            output_device: Some("Chosen output".into()),
            transmission: TransmissionMode::PushToTalk,
            microphone_gain: 1.5,
            playback_volume: 0.7,
            jitter_ms: 80,
            adaptive_jitter: false,
            ..Default::default()
        };
        let original = audio.clone();
        assert_eq!(audio.latency_mode(), LatencyMode::Custom);
        audio.set_latency_mode(LatencyMode::Custom);
        assert_eq!(audio, original);
        for (mode, expected) in [(LatencyMode::Balanced, 40), (LatencyMode::LowLatency, 10)] {
            audio.set_latency_mode(mode);
            let mut expected_audio = original.clone();
            expected_audio.jitter_ms = expected;
            assert_eq!(audio, expected_audio);
            assert_eq!(audio.latency_mode(), mode);
        }
        for (input, expected) in [(0, 5), (6, 10), (21, 25), (u32::MAX, 120)] {
            audio.jitter_ms = input;
            audio.set_latency_mode(LatencyMode::Custom);
            assert_eq!(audio.jitter_ms, expected);
        }
    }

    #[test]
    fn adaptive_choice_round_trips_and_manual_presets_disable_it() {
        let mut audio = AudioSettings::default();
        audio.set_latency_mode(LatencyMode::Adaptive);
        assert_eq!(audio.latency_mode(), LatencyMode::Adaptive);
        let restored: AudioSettings =
            serde_json::from_str(&serde_json::to_string(&audio).unwrap()).unwrap();
        assert_eq!(restored, audio);
        for mode in [
            LatencyMode::LowLatency,
            LatencyMode::Balanced,
            LatencyMode::Custom,
        ] {
            audio.set_latency_mode(LatencyMode::Adaptive);
            audio.set_latency_mode(mode);
            assert!(!audio.adaptive_jitter);
        }
    }
    #[test]
    fn fractional_packets_preserve_existing_profiles_and_dsp_batch_floor() {
        let mut settings = AudioSettings {
            noise_suppression: false,
            echo_cancellation: false,
            ..Default::default()
        };
        settings.set_packet_samples(120);
        assert_eq!(settings.packet_duration_ms(), 2.5);
        assert_eq!(settings.voice_delay_budget().capture_ms, 2.5);
        let restored: AudioSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.packet_samples(), 120);
        settings.noise_suppression = true;
        assert_eq!(settings.voice_delay_budget().capture_ms, 10.0);
        settings.set_packet_samples(480);
        assert!(!settings.ultra_short_packets);
        assert_eq!(settings.packet_duration_ms(), 10.0);
        let old: AudioSettings = serde_json::from_str("{\"packet_ms\":5}").unwrap();
        assert_eq!(old.packet_samples(), 240);
    }

    #[test]
    fn linked_low_delay_opus_reports_real_lookahead_for_every_packet_duration() {
        for bitrate in [32, 64, 128] {
            for samples in [120, 240, 480, 960] {
                let mut encoder = make_voice_encoder(bitrate).unwrap();
                let mut encoded = [0; 1500];
                encoder
                    .encode_float(&vec![0.1; samples], &mut encoded)
                    .unwrap();
                let lookahead = encoder.get_lookahead().unwrap();
                assert_eq!(
                    lookahead, 120,
                    "linked native low-delay Opus changed its lookahead"
                );
                assert_eq!(
                    AudioSettings::default().voice_delay_budget().codec_ms,
                    Some(lookahead as f32 / 48.0)
                );
                println!(
                    "Opus: {bitrate} kbps, {} ms packet, lookahead {lookahead} samples = {} ms",
                    samples as f32 / 48.0,
                    lookahead as f32 / 48.0
                );
            }
        }
    }
}
