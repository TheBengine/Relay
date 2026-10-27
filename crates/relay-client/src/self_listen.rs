//! "Listen to Myself" state logic: which edits apply live, what the level meter
//! says, and an honest local-loop latency estimate. No devices are opened here.
use crate::audio_settings::{AudioSettings, TransmissionMode};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How an edited setting reaches a running preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewChange {
    None,
    /// Gain, processing, gate and volume changes take effect on the next block.
    Live,
    /// A different microphone, speaker or device timing needs new streams.
    Reopen,
}

pub(crate) fn preview_change(old: &AudioSettings, new: &AudioSettings) -> PreviewChange {
    if old.input_device != new.input_device
        || old.output_device != new.output_device
        || old.low_latency_device != new.low_latency_device
    {
        PreviewChange::Reopen
    } else if old != new {
        PreviewChange::Live
    } else {
        PreviewChange::None
    }
}

/// The preview is local: whisper keys and targets from a conversation never apply.
pub(crate) fn preview_settings(settings: &AudioSettings) -> AudioSettings {
    let mut settings = settings.clone().normalized();
    settings.whisper_enabled = false;
    settings
}

/// Written by the preview worker once per processed block, read by the UI.
#[derive(Default)]
pub(crate) struct MeterTaps {
    raw_peak: AtomicU32,
    rms: AtomicU32,
    peak: AtomicU32,
    gate_open: AtomicBool,
    blocks: AtomicU64,
}

// Non-negative finite f32 bit patterns order like their values.
fn store_max(slot: &AtomicU32, value: f32) {
    if value.is_finite() && value > 0.0 {
        slot.fetch_max(value.min(1.0).to_bits(), Ordering::Relaxed);
    }
}

pub(crate) fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .filter(|sample| sample.is_finite())
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

impl MeterTaps {
    /// Microphone signal before Relay's gain, even while push-to-talk is up.
    pub(crate) fn record_raw(&self, raw_peak: f32) {
        store_max(&self.raw_peak, raw_peak);
        self.blocks.fetch_add(1, Ordering::Relaxed);
    }

    /// The block exactly as the call pipeline would encode it.
    pub(crate) fn record_processed(&self, processed: &[f32], gate_open: bool) {
        if processed.is_empty() {
            return;
        }
        let power = processed
            .iter()
            .filter(|sample| sample.is_finite())
            .map(|sample| sample * sample)
            .sum::<f32>();
        store_max(&self.rms, (power / processed.len() as f32).sqrt());
        store_max(&self.peak, peak(processed));
        self.gate_open.store(gate_open, Ordering::Relaxed);
    }

    /// Maxima since the previous call; the UI reads this once per frame.
    pub(crate) fn take(&self) -> MeterSample {
        let take = |slot: &AtomicU32| f32::from_bits(slot.swap(0, Ordering::Relaxed));
        MeterSample {
            raw_peak: take(&self.raw_peak),
            rms: take(&self.rms),
            peak: take(&self.peak),
            gate_open: self.gate_open.load(Ordering::Relaxed),
            blocks: self.blocks.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct MeterSample {
    pub raw_peak: f32,
    pub rms: f32,
    pub peak: f32,
    pub gate_open: bool,
    pub blocks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipSource {
    /// The signal already reaches full scale before Relay touches it.
    Microphone,
    /// Relay's Input Volume pushes it past full scale.
    InputVolume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Advice {
    Starting,
    /// Practically digital silence: wrong device, muted in Windows or blocked.
    NoSignal,
    WaitingForKey,
    SpeakNow,
    /// Speech is present but voice activation stays closed.
    ActivationClosed,
    TooQuiet,
    Good,
    TooLoud,
    Clipping(ClipSource),
}

impl Advice {
    pub(crate) fn text(self, ptt_key: &str) -> String {
        match self {
            Self::Starting => "Opening your microphone and speaker…".into(),
            Self::NoSignal => "No sound from this microphone. Check the input device, the mute switch on the headset, and Windows Settings › Privacy › Microphone.".into(),
            Self::WaitingForKey => format!("Hold {ptt_key} and speak. Push-to-talk applies here as in a call."),
            Self::SpeakNow => "Say something at your normal volume.".into(),
            Self::ActivationClosed => "You are speaking but voice activation stays closed, so others would not hear you. Lower the activation threshold or turn on automatic sensitivity.".into(),
            Self::TooQuiet => "You are too quiet. Move the microphone closer, raise Input Volume, or turn on automatic microphone volume.".into(),
            Self::Good => "Good level.".into(),
            Self::TooLoud => "You are loud. Lower Input Volume a little or move the microphone back.".into(),
            Self::Clipping(ClipSource::InputVolume) => "Clipping: Input Volume is too high and distorts your voice. Lower Input Volume.".into(),
            Self::Clipping(ClipSource::Microphone) => "Clipping before Relay: the microphone itself is too loud. Lower its level in Windows Sound settings or move it back; Relay's Input Volume cannot fix this.".into(),
        }
    }

    pub(crate) fn is_problem(self) -> bool {
        matches!(
            self,
            Self::NoSignal | Self::ActivationClosed | Self::TooQuiet | Self::TooLoud | Self::Clipping(_)
        )
    }
}

/// RMS dBFS of the processed signal; -60 dB is treated as silence on the meter.
pub(crate) const METER_FLOOR_DB: f32 = -60.0;
/// Speech whose loudest moments stay below this is hard to follow for others.
pub(crate) const QUIET_DB: f32 = -36.0;
/// Loudest moments above this leave little headroom.
pub(crate) const LOUD_DB: f32 = -10.0;
const SPEECH_DB: f32 = -50.0;
const NO_SIGNAL_PEAK: f32 = 0.0005;
const CLIP_PEAK: f32 = 0.99;
const WARMUP: Duration = Duration::from_millis(600);
const SILENCE: Duration = Duration::from_secs(2);
const SPEECH_WINDOW: Duration = Duration::from_secs(3);
const CLIP_HOLD: Duration = Duration::from_secs(2);

pub(crate) fn db(rms: f32) -> f32 {
    if rms > 0.0 && rms.is_finite() {
        (20.0 * rms.log10()).max(METER_FLOOR_DB)
    } else {
        METER_FLOOR_DB
    }
}

/// Position of a level on the meter, 0 at the floor and 1 at full scale.
pub(crate) fn meter_fraction(level_db: f32) -> f32 {
    ((level_db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Reading {
    /// Smoothed processed level for the meter bar.
    pub level_db: f32,
    /// Loudest recent speech level, which the advice is based on.
    pub speech_db: f32,
    pub gate_open: bool,
    pub advice: Advice,
}

pub(crate) struct LevelAnalyzer {
    started: Instant,
    updated: Instant,
    blocks: u64,
    display: f32,
    speech: f32,
    last_audio: Option<Instant>,
    last_signal: Option<Instant>,
    last_speech: Option<Instant>,
    last_gate: Option<Instant>,
    clip: Option<(Instant, ClipSource)>,
    gate_open: bool,
}

impl LevelAnalyzer {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            started: now,
            updated: now,
            blocks: 0,
            display: 0.0,
            speech: 0.0,
            last_audio: None,
            last_signal: None,
            last_speech: None,
            last_gate: None,
            clip: None,
            gate_open: false,
        }
    }

    pub(crate) fn update(
        &mut self,
        sample: MeterSample,
        settings: &AudioSettings,
        key_held: bool,
        now: Instant,
    ) -> Reading {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f32();
        self.updated = now;
        let decay = |time_constant: f32| (-elapsed / time_constant).exp();
        if sample.blocks != self.blocks {
            self.blocks = sample.blocks;
            self.last_audio = Some(now);
            if sample.raw_peak >= NO_SIGNAL_PEAK {
                self.last_signal = Some(now);
            }
        }
        self.display = sample.rms.max(self.display * decay(0.3));
        if db(sample.rms) >= SPEECH_DB {
            // Decay by the time since the previous speech, pauses included, so a
            // loud cough does not judge the next sentence. During pauses the
            // level verdict is held rather than drifting towards "too quiet".
            let since = self.last_speech.map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f32());
            self.speech = sample.rms.max(self.speech * (-since / 1.0).exp());
            self.last_speech = Some(now);
        }
        self.gate_open = sample.gate_open && sample.rms > 0.0;
        if self.gate_open {
            self.last_gate = Some(now);
        }
        if sample.raw_peak >= CLIP_PEAK {
            self.clip = Some((now, ClipSource::Microphone));
        } else if !settings.automatic_microphone_volume
            && sample.raw_peak * settings.microphone_gain >= CLIP_PEAK
            && !matches!(self.clip, Some((at, ClipSource::Microphone)) if now.saturating_duration_since(at) < CLIP_HOLD)
        {
            self.clip = Some((now, ClipSource::InputVolume));
        }
        let recent = |at: Option<Instant>, window: Duration| {
            at.is_some_and(|at| now.saturating_duration_since(at) < window)
        };
        let push_to_talk = settings.transmission == TransmissionMode::PushToTalk;
        let running = now.saturating_duration_since(self.started);
        let advice = if running < WARMUP || (self.last_audio.is_none() && running < SILENCE) {
            Advice::Starting
        } else if let Some((_, source)) = self.clip.filter(|(at, _)| now.saturating_duration_since(*at) < CLIP_HOLD) {
            Advice::Clipping(source)
        } else if !recent(self.last_signal, SILENCE) {
            Advice::NoSignal
        } else if push_to_talk && !key_held {
            Advice::WaitingForKey
        } else if !recent(self.last_speech, SPEECH_WINDOW) {
            Advice::SpeakNow
        } else if settings.transmission == TransmissionMode::VoiceActivation
            && !recent(self.last_gate, SILENCE)
        {
            Advice::ActivationClosed
        } else {
            let speech = db(self.speech);
            if speech < QUIET_DB {
                Advice::TooQuiet
            } else if speech > LOUD_DB {
                Advice::TooLoud
            } else {
                Advice::Good
            }
        };
        Reading {
            level_db: db(self.display),
            speech_db: db(self.speech),
            gate_open: self.gate_open,
            advice,
        }
    }
}

/// Manual voice-activation threshold on the meter, when one applies. With
/// automatic microphone volume the gate sees the level before that volume
/// change and the meter after it, so a marker would mislead.
pub(crate) fn threshold_db(settings: &AudioSettings) -> Option<f32> {
    (settings.transmission == TransmissionMode::VoiceActivation
        && !settings.automatic_sensitivity()
        && !settings.automatic_microphone_volume)
    .then(|| db(settings.activation_threshold))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LoopLatency {
    pub ms: f32,
    /// Both device periods are known from the opened streams.
    pub measured_devices: bool,
}

/// Microphone to speaker on this PC: device periods, the processing batch,
/// noise-suppression lookahead and the playback reserve. It is an estimate,
/// not a measurement; codec, network and the listener's buffer are excluded.
pub(crate) fn loop_latency(
    settings: &AudioSettings,
    input_period_ms: Option<f32>,
    output_period_ms: Option<f32>,
) -> LoopLatency {
    let budget = settings.voice_delay_budget_for_output(output_period_ms);
    let device = |period: Option<f32>| period.filter(|ms| ms.is_finite() && *ms > 0.0);
    let input = device(input_period_ms);
    let output = device(output_period_ms);
    LoopLatency {
        ms: input.unwrap_or(0.0)
            + output.unwrap_or(0.0)
            + budget.capture_ms
            + budget.suppression_ms as f32
            + budget.playback_ms,
        measured_devices: input.is_some() && output.is_some(),
    }
}

impl LoopLatency {
    pub(crate) fn label(self) -> String {
        if self.measured_devices {
            format!("Local delay ≈ {:.0} ms (estimate from your device timing and processing). In a call, the voice codec, the network and the listener's buffer add more.", self.ms)
        } else {
            format!("Local delay ≈ {:.0} ms plus device buffers, which this device does not report. In a call, the voice codec, the network and the listener's buffer add more.", self.ms)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: TransmissionMode) -> AudioSettings {
        AudioSettings {
            transmission: mode,
            automatic_microphone_volume: false,
            microphone_gain: 1.0,
            ..Default::default()
        }
        .normalized()
    }

    fn sample(blocks: u64, raw_peak: f32, rms: f32, gate_open: bool) -> MeterSample {
        MeterSample { raw_peak, rms, peak: rms * 1.4, gate_open, blocks }
    }

    /// A preview clock feeding blocks at 50 Hz into one analyzer.
    struct Feed {
        analyzer: LevelAnalyzer,
        settings: AudioSettings,
        now: Instant,
        blocks: u64,
    }

    impl Feed {
        fn new(settings: AudioSettings) -> Self {
            let now = Instant::now();
            Self { analyzer: LevelAnalyzer::new(now), settings, now, blocks: 0 }
        }

        /// Hold one level for `millis` and return the last reading.
        fn hold(&mut self, millis: u64, raw_peak: f32, rms: f32, gate_open: bool, key_held: bool) -> Reading {
            let mut reading = None;
            for _ in 0..millis.div_ceil(20) {
                self.now += Duration::from_millis(20);
                self.blocks += 1;
                let sample = sample(self.blocks, raw_peak, rms, gate_open);
                reading = Some(self.analyzer.update(sample, &self.settings, key_held, self.now));
            }
            reading.unwrap()
        }

        fn restart(&mut self, settings: AudioSettings) {
            self.analyzer = LevelAnalyzer::new(self.now);
            self.settings = settings;
        }
    }

    #[test]
    fn device_edits_reopen_and_other_edits_apply_live() {
        let old = AudioSettings::default();
        assert_eq!(preview_change(&old, &old.clone()), PreviewChange::None);
        for edit in [
            |s: &mut AudioSettings| s.microphone_gain = 2.0,
            |s: &mut AudioSettings| s.noise_suppression = !s.noise_suppression,
            |s: &mut AudioSettings| s.echo_cancellation = !s.echo_cancellation,
            |s: &mut AudioSettings| s.activation_threshold = 0.05,
            |s: &mut AudioSettings| s.transmission = TransmissionMode::PushToTalk,
            |s: &mut AudioSettings| s.playback_volume = 0.5,
        ] {
            let mut new = old.clone();
            edit(&mut new);
            assert_eq!(preview_change(&old, &new), PreviewChange::Live);
        }
        for edit in [
            |s: &mut AudioSettings| s.input_device = Some("USB headset".into()),
            |s: &mut AudioSettings| s.output_device = Some("Speakers".into()),
            |s: &mut AudioSettings| s.low_latency_device = !s.low_latency_device,
        ] {
            let mut new = old.clone();
            edit(&mut new);
            assert_eq!(preview_change(&old, &new), PreviewChange::Reopen);
        }
    }

    #[test]
    fn preview_never_enables_whisper() {
        let draft = AudioSettings { whisper_enabled: true, ..Default::default() };
        assert!(!preview_settings(&draft).whisper_enabled);
    }

    #[test]
    fn taps_report_maxima_since_the_previous_read() {
        let taps = MeterTaps::default();
        taps.record_raw(0.2);
        taps.record_processed(&[0.1, -0.1, 0.1, -0.1], true);
        taps.record_raw(0.5);
        taps.record_processed(&[0.05; 4], false);
        let first = taps.take();
        assert_eq!(first.raw_peak, 0.5);
        assert!((first.rms - 0.1).abs() < 1e-6);
        assert!((first.peak - 0.1).abs() < 1e-6);
        assert!(!first.gate_open, "gate reflects the latest block");
        assert_eq!(first.blocks, 2);
        let second = taps.take();
        assert_eq!((second.raw_peak, second.rms, second.peak), (0.0, 0.0, 0.0));
        assert_eq!(second.blocks, 2);
        taps.record_processed(&[f32::NAN, 0.3], true);
        assert!(taps.take().peak > 0.29);
    }

    #[test]
    fn advice_covers_silence_quiet_good_loud_and_clipping() {
        let mut feed = Feed::new(settings(TransmissionMode::Continuous));
        let first = feed.analyzer.update(MeterSample::default(), &feed.settings, false, feed.now);
        assert_eq!(first.advice, Advice::Starting);
        let r = feed.hold(3000, 0.0, 0.0, true, false);
        assert_eq!(r.advice, Advice::NoSignal, "digital silence is a device problem");
        // Background noise but no speech.
        let r = feed.hold(4000, 0.002, 0.0005, true, false);
        assert_eq!(r.advice, Advice::SpeakNow);
        // -40 dBFS speech.
        let r = feed.hold(1000, 0.03, 0.01, true, false);
        assert_eq!(r.advice, Advice::TooQuiet);
        assert!(r.speech_db < QUIET_DB);
        // -20 dBFS speech, after the quiet envelope decays.
        let r = feed.hold(1000, 0.3, 0.1, true, false);
        assert_eq!(r.advice, Advice::Good);
        assert!(r.level_db > -21.0 && r.level_db < -19.0);
        // -6 dBFS speech.
        let r = feed.hold(1000, 0.9, 0.5, true, false);
        assert_eq!(r.advice, Advice::TooLoud);
        assert!(r.advice.is_problem());
    }

    #[test]
    fn clipping_names_its_cause_and_is_held_briefly() {
        let mut loud_gain = settings(TransmissionMode::Continuous);
        loud_gain.microphone_gain = 3.0;
        let mut feed = Feed::new(loud_gain.clone());
        let r = feed.hold(1000, 0.4, 0.15, true, false);
        assert_eq!(r.advice, Advice::Clipping(ClipSource::InputVolume));
        // Quieter speech: the hint stays long enough to read, then clears.
        let r = feed.hold(1000, 0.1, 0.1, true, false);
        assert_eq!(r.advice, Advice::Clipping(ClipSource::InputVolume));
        let r = feed.hold(2000, 0.1, 0.1, true, false);
        assert_eq!(r.advice, Advice::Good);
        // Full scale before gain is the microphone's own clipping.
        let r = feed.hold(100, 1.0, 0.3, true, false);
        assert_eq!(r.advice, Advice::Clipping(ClipSource::Microphone));
        // Automatic microphone volume caps Relay's gain, so only the microphone can clip.
        let mut agc = loud_gain;
        agc.automatic_microphone_volume = true;
        feed.restart(agc);
        let r = feed.hold(1000, 0.4, 0.15, true, false);
        assert_eq!(r.advice, Advice::Good);
    }

    #[test]
    fn push_to_talk_and_voice_activation_explain_why_nothing_is_heard() {
        let mut feed = Feed::new(settings(TransmissionMode::PushToTalk));
        let r = feed.hold(1000, 0.05, 0.0, false, false);
        assert_eq!(r.advice, Advice::WaitingForKey);
        assert!(r.advice.text("F13").contains("F13"));
        let r = feed.hold(1000, 0.3, 0.1, true, true);
        assert_eq!(r.advice, Advice::Good);

        let mut vad = settings(TransmissionMode::VoiceActivation);
        vad.automatic_sensitivity = Some(false);
        vad.activation_threshold = 0.2;
        assert!((threshold_db(&vad).unwrap() - db(0.2)).abs() < 1e-4);
        feed.restart(vad.clone());
        let r = feed.hold(3000, 0.3, 0.1, false, false);
        assert_eq!(r.advice, Advice::ActivationClosed);
        assert!(!r.gate_open);
        let r = feed.hold(200, 0.6, 0.25, true, false);
        assert!(r.gate_open);
        assert_ne!(r.advice, Advice::ActivationClosed);
        vad.automatic_microphone_volume = true;
        assert_eq!(threshold_db(&vad), None, "the gate measures before automatic volume");
        vad.automatic_microphone_volume = false;
        vad.automatic_sensitivity = Some(true);
        assert_eq!(threshold_db(&vad), None, "automatic sensitivity has no fixed marker");
    }

    #[test]
    fn a_loud_moment_does_not_outlive_the_pause_after_it() {
        let mut feed = Feed::new(settings(TransmissionMode::Continuous));
        let r = feed.hold(1000, 0.9, 0.5, true, false);
        assert_eq!(r.advice, Advice::TooLoud);
        feed.hold(2000, 0.002, 0.0005, true, false);
        let r = feed.hold(300, 0.3, 0.1, true, false);
        assert_eq!(r.advice, Advice::Good, "normal speech after a pause is judged on its own");
        // Quieter but fine speech keeps its verdict through a short pause.
        feed.hold(1000, 0.1, 0.03, true, false);
        let r = feed.hold(2000, 0.002, 0.0005, true, false);
        assert_eq!(r.advice, Advice::Good, "a pause between sentences is not 'too quiet'");
    }

    #[test]
    fn meter_scale_is_bounded() {
        assert_eq!(db(0.0), METER_FLOOR_DB);
        assert_eq!(db(f32::NAN), METER_FLOOR_DB);
        assert!((db(1.0)).abs() < 1e-6);
        assert_eq!(meter_fraction(METER_FLOOR_DB - 10.0), 0.0);
        assert_eq!(meter_fraction(0.0), 1.0);
        assert!((meter_fraction(-30.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn latency_estimate_adds_devices_processing_and_says_what_it_excludes() {
        let mut plain = AudioSettings { noise_suppression: false, echo_cancellation: false, ..Default::default() };
        plain.set_packet_samples(480);
        let unmeasured = loop_latency(&plain, None, None);
        assert!(!unmeasured.measured_devices);
        assert!(unmeasured.label().contains("does not report"));
        let measured = loop_latency(&plain, Some(10.0), Some(10.0));
        assert!(measured.measured_devices);
        assert!(measured.ms >= unmeasured.ms + 20.0 - 0.01);
        assert!(measured.label().contains("estimate"));
        assert!(measured.label().contains("network"));
        let mut suppressed = plain.clone();
        suppressed.noise_suppression = true;
        assert!(loop_latency(&suppressed, Some(10.0), Some(10.0)).ms >= measured.ms + 10.0 - 0.01);
        assert!(!loop_latency(&plain, Some(f32::NAN), Some(10.0)).measured_devices);
    }
}
