//! Native echo cancellation (WebRTC AEC3) with a reference to actually played
//! audio, used only while an acoustic echo path is detected.
//!
//! AEC3 in this build suppresses the near-end voice whenever the far end is
//! louder in a band and it has not learned that no echo exists. With
//! headphones there is never an echo to learn from, so the person's voice was
//! cut whenever someone else spoke. `EchoPath` therefore measures whether the
//! played audio actually returns through the microphone; until it does, the
//! microphone passes unchanged while AEC3 keeps adapting in the background.
use livekit::webrtc::native::apm::AudioProcessingModule;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TEN_MS: usize = 480;
const MAX_REFERENCE_FRAMES: usize = 32;
const MAX_REFERENCE_AGE: Duration = Duration::from_millis(400);

#[derive(Clone, Default)]
pub struct EchoReference(Arc<Mutex<ReferenceFrames>>);
type ReferenceFrames = VecDeque<(Instant, [i16; TEN_MS])>;
impl EchoReference {
    /// Called only with a complete 10 ms block in the 48 kHz playback domain.
    /// A busy audio worker drops the reference instead of blocking playback.
    pub fn rendered(&self, samples: &[f32; TEN_MS]) {
        let mut frame = [0; TEN_MS];
        for (out, input) in frame.iter_mut().zip(samples) {
            *out = to_i16(*input);
        }
        if let Ok(mut queue) = self.0.try_lock() {
            if queue.len() == MAX_REFERENCE_FRAMES {
                queue.pop_front();
            }
            queue.push_back((Instant::now(), frame));
        }
    }
    pub fn clear(&self) {
        if let Ok(mut queue) = self.0.lock() {
            queue.clear();
        }
    }
}

/// History length, maximum echo delay searched and how often it is searched.
const PATH_HISTORY: usize = 400;
const PATH_MAX_LAG: usize = 50;
const PATH_CHECK_EVERY: usize = 10;
/// Far-end blocks quieter than this say nothing about an echo path.
const PATH_ACTIVE_DB: f32 = -55.0;
const PATH_MIN_ACTIVE: usize = 80;
const PATH_PRESENT: f32 = 0.55;
const PATH_ABSENT: f32 = 0.3;
/// Once heard, an echo path is assumed for 30 s; double talk cannot hide it.
const PATH_LATCH_BLOCKS: usize = 3_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PathState {
    Unknown,
    Present,
    /// Also what a disabled canceller reports: nothing is cancelled.
    #[default]
    Absent,
}

/// Whether played audio returns through the microphone: the correlation of
/// the 10 ms log energies of the reference and of the raw microphone over
/// 0–500 ms of delay, using only blocks where the far end is audible.
pub(crate) struct EchoPath {
    reference: VecDeque<f32>,
    capture: VecDeque<f32>,
    since_check: usize,
    present_left: usize,
    state: PathState,
    correlation: f32,
}

impl EchoPath {
    pub(crate) fn new() -> Self {
        Self {
            reference: VecDeque::with_capacity(PATH_HISTORY + 1),
            capture: VecDeque::with_capacity(PATH_HISTORY + 1),
            since_check: 0,
            present_left: 0,
            state: PathState::Unknown,
            correlation: 0.0,
        }
    }

    /// One 10 ms block: mean power of the far end and of the raw microphone.
    pub(crate) fn observe(&mut self, reference_power: f32, capture_power: f32) -> PathState {
        let level = |power: f32| 10.0 * power.max(1e-10).log10();
        for (history, value) in [
            (&mut self.reference, level(reference_power)),
            (&mut self.capture, level(capture_power)),
        ] {
            if history.len() == PATH_HISTORY {
                history.pop_front();
            }
            history.push_back(value);
        }
        self.present_left = self.present_left.saturating_sub(1);
        self.since_check += 1;
        if self.since_check >= PATH_CHECK_EVERY {
            self.since_check = 0;
            if let Some(correlation) = self.best_correlation() {
                self.correlation = correlation;
                if correlation >= PATH_PRESENT {
                    self.present_left = PATH_LATCH_BLOCKS;
                    self.state = PathState::Present;
                } else if self.present_left == 0 && correlation < PATH_ABSENT {
                    self.state = PathState::Absent;
                }
            }
        }
        if self.state == PathState::Present && self.present_left == 0 {
            self.state = PathState::Unknown;
        }
        self.state
    }

    fn best_correlation(&self) -> Option<f32> {
        let len = self.capture.len();
        if len < PATH_MIN_ACTIVE + PATH_MAX_LAG {
            return None;
        }
        let mut best: Option<f32> = None;
        for lag in 0..=PATH_MAX_LAG {
            let (mut n, mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0);
            for t in lag..len {
                let x = self.reference[t - lag];
                if x < PATH_ACTIVE_DB {
                    continue;
                }
                let y = self.capture[t];
                n += 1.0;
                sx += x;
                sy += y;
                sxx += x * x;
                syy += y * y;
                sxy += x * y;
            }
            if (n as usize) < PATH_MIN_ACTIVE {
                continue;
            }
            let cov = sxy - sx * sy / n;
            let vx = sxx - sx * sx / n;
            let vy = syy - sy * sy / n;
            if vx <= 1e-3 || vy <= 1e-3 {
                continue;
            }
            let r = cov / (vx * vy).sqrt();
            best = Some(best.map_or(r, |b: f32| b.max(r)));
        }
        best
    }

    pub(crate) fn state(&self) -> PathState {
        self.state
    }

    #[cfg(test)]
    pub(crate) fn correlation(&self) -> f32 {
        self.correlation
    }
}

/// What the canceller learned about this block, for the stages after it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct EchoOutcome {
    /// An acoustic echo path is active and AEC3's output is used.
    pub cancelling: bool,
    /// Loudest far-end RMS over the last ~500 ms (the longest delay searched).
    pub far_rms: f32,
    pub path: PathState,
}

pub struct EchoCanceller {
    apm: AudioProcessingModule,
    reference: EchoReference,
    path: EchoPath,
    /// 0 passes the microphone, 1 uses AEC3's output; ramps over 10 ms.
    mix: f32,
    far_recent: VecDeque<f32>,
    reference_power: f32,
}
impl EchoCanceller {
    pub fn new(reference: EchoReference) -> Self {
        Self {
            apm: AudioProcessingModule::new(true, false, true, false),
            reference,
            path: EchoPath::new(),
            mix: 0.0,
            far_recent: VecDeque::with_capacity(PATH_MAX_LAG + 1),
            reference_power: 0.0,
        }
    }
    pub fn reset(&mut self) {
        self.reference.clear();
        self.apm = AudioProcessingModule::new(true, false, true, false);
        self.path = EchoPath::new();
        self.mix = 0.0;
        self.far_recent.clear();
        self.reference_power = 0.0;
    }
    /// Runs on the voice worker before gain, RNNoise and activity gating.
    /// This does not open any device or process arbitrary computer audio.
    pub(crate) fn process(&mut self, samples: &mut [f32], delay_ms: u32) -> Result<EchoOutcome, String> {
        self.apm
            .set_stream_delay_ms(delay_ms.min(250) as i32)
            .map_err(|e| format!("Echo timing failed: {e}"))?;
        // Take a fixed-size batch so even a stalled capture worker uses bounded
        // memory and does not hold the render queue during native DSP calls.
        let mut references = [[0i16; TEN_MS]; MAX_REFERENCE_FRAMES];
        let mut count = 0;
        if let Ok(mut queue) = self.reference.0.lock() {
            while let Some((at, frame)) = queue.pop_front() {
                if at.elapsed() <= MAX_REFERENCE_AGE && count < MAX_REFERENCE_FRAMES {
                    references[count] = frame;
                    count += 1;
                }
            }
        }
        if count > 0 {
            // A render callback that ran late delivers two frames at once; the
            // envelope keeps the previous value for a block that got none.
            self.reference_power = references[..count]
                .iter()
                .map(|frame| frame.iter().map(|s| (*s as f32 / 32768.0).powi(2)).sum::<f32>() / TEN_MS as f32)
                .sum::<f32>()
                / count as f32;
        }
        for reference in references.iter_mut().take(count) {
            self.apm
                .process_reverse_stream(reference, 48_000, 1)
                .map_err(|e| format!("Echo reference failed: {e}"))?;
        }
        if samples.is_empty() || !samples.len().is_multiple_of(TEN_MS) {
            return Err("Echo cancellation requires complete 10 ms blocks".into());
        }
        for chunk in samples.chunks_exact_mut(TEN_MS) {
            let capture_power = chunk.iter().map(|s| s * s).sum::<f32>() / TEN_MS as f32;
            let state = self.path.observe(self.reference_power, capture_power);
            if self.far_recent.len() == PATH_MAX_LAG {
                self.far_recent.pop_front();
            }
            self.far_recent.push_back(self.reference_power.sqrt());
            let mut frame = [0i16; TEN_MS];
            for (out, input) in frame.iter_mut().zip(chunk.iter()) {
                *out = to_i16(*input);
            }
            self.apm
                .process_stream(&mut frame, 48_000, 1)
                .map_err(|e| format!("Echo processing failed: {e}"))?;
            let target = if state == PathState::Present { 1.0 } else { 0.0 };
            let step = (target - self.mix) / TEN_MS as f32;
            for (index, (out, cancelled)) in chunk.iter_mut().zip(frame).enumerate() {
                let mix = self.mix + step * (index + 1) as f32;
                *out = *out * (1.0 - mix) + cancelled as f32 / 32768.0 * mix;
            }
            self.mix = target;
        }
        Ok(EchoOutcome {
            path: self.path.state(),
            cancelling: self.mix > 0.0,
            far_rms: self.far_recent.iter().fold(0.0f32, |peak, v| peak.max(*v)),
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &EchoPath {
        &self.path
    }
}

fn to_i16(sample: f32) -> i16 {
    if sample.is_finite() {
        (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_powers(signal: &[f32]) -> Vec<f32> {
        signal
            .chunks_exact(TEN_MS)
            .map(|block| block.iter().map(|s| s * s).sum::<f32>() / TEN_MS as f32)
            .collect()
    }

    #[test]
    fn echo_path_is_found_from_played_audio_but_not_from_independent_speech() {
        use crate::voice_test_signals::talker;
        let far = block_powers(&talker(12.0, (190.0, 230.0), 11, 0.5));
        let near = block_powers(&talker(12.0, (105.0, 135.0), 5, 0.3));
        let hiss = 1e-7;
        // Speakers: the far end returns 120 ms later at -12 dB, with the person talking too.
        let mut speakers = EchoPath::new();
        let mut found_at = None;
        for t in 0..far.len() {
            let echo = if t >= 12 { far[t - 12] * 0.063 } else { 0.0 };
            let near_part = if t >= 600 { near[t] } else { 0.0 };
            if speakers.observe(far[t], echo + near_part + hiss) == PathState::Present {
                found_at.get_or_insert(t);
            }
        }
        let found_at = found_at.expect("an echo path must be found");
        assert!(found_at <= 250, "found after {found_at} blocks");
        assert_eq!(speakers.state(), PathState::Present, "double talk does not hide a known path");
        // Headphones: only the person's own, independent speech reaches the microphone.
        let mut headphones = EchoPath::new();
        for t in 0..far.len() {
            let state = headphones.observe(far[t], near[t] + hiss);
            assert_ne!(state, PathState::Present, "block {t}: correlation {}", headphones.correlation());
        }
        assert_eq!(headphones.state(), PathState::Absent);
        // A silent far end proves nothing either way.
        let mut quiet = EchoPath::new();
        for power in &near {
            quiet.observe(0.0, power + hiss);
        }
        assert_eq!(quiet.state(), PathState::Unknown);
    }

    #[test]
    fn microphone_passes_unchanged_until_an_echo_path_is_heard() {
        let reference = EchoReference::default();
        let mut echo = EchoCanceller::new(reference.clone());
        let mut state = 0x5555_1234u32;
        for _ in 0..200 {
            let mut render = [0.0; TEN_MS];
            for sample in &mut render {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *sample = ((state >> 8) as f32 / 16_777_215.0 - 0.5) * 0.35;
            }
            reference.rendered(&render);
            let near: Vec<f32> = (0..TEN_MS).map(|i| (i as f32 * 0.05).sin() * 0.1).collect();
            let mut capture = near.clone();
            let outcome = echo.process(&mut capture, 40).unwrap();
            assert!(!outcome.cancelling);
            assert!(outcome.far_rms > 0.05);
            for (a, b) in capture.iter().zip(&near) {
                assert!((a - b).abs() < 1e-6, "headphones: the voice is untouched");
            }
        }
        assert_ne!(echo.path().state(), PathState::Present);
    }

    #[test]
    fn references_are_bounded_and_reset_discards_prior_call_history() {
        let reference = EchoReference::default();
        for _ in 0..100 {
            reference.rendered(&[0.1; TEN_MS]);
        }
        assert_eq!(reference.0.lock().unwrap().len(), MAX_REFERENCE_FRAMES);
        let mut echo = EchoCanceller::new(reference.clone());
        let mut silent = [0.0; 960];
        echo.process(&mut silent, 40).unwrap();
        assert!(silent.iter().all(|sample| sample.is_finite()));
        reference.rendered(&[0.2; TEN_MS]);
        echo.reset();
        assert!(reference.0.lock().unwrap().is_empty());
    }
    #[test]
    fn native_aec_reduces_a_delayed_synthetic_far_end_without_devices() {
        let reference = EchoReference::default();
        let mut echo = EchoCanceller::new(reference.clone());
        let mut history = VecDeque::from(vec![0.0f32; 1_920]);
        let mut state = 0x12345678u32;
        let (mut before, mut after, mut measured) = (0.0f64, 0.0f64, 0usize);
        for frame in 0..750 {
            let mut capture = [0.0; 960];
            for chunk in capture.chunks_exact_mut(TEN_MS) {
                let mut render = [0.0; TEN_MS];
                for (sample, microphone) in render.iter_mut().zip(chunk) {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *sample = ((state >> 8) as f32 / 16_777_215.0 - 0.5) * 0.35;
                    history.push_back(*sample);
                    *microphone = history.pop_front().unwrap() * 0.6;
                }
                reference.rendered(&render);
            }
            let energy = capture
                .iter()
                .map(|sample| (*sample as f64).powi(2))
                .sum::<f64>();
            echo.process(&mut capture, 40).unwrap();
            assert!(capture
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.0));
            if frame >= 500 {
                before += energy;
                after += capture
                    .iter()
                    .map(|sample| (*sample as f64).powi(2))
                    .sum::<f64>();
                measured += 1;
            }
        }
        let reduction_db = 10.0 * (before / after.max(f64::MIN_POSITIVE)).log10();
        println!("Synthetic delayed far-end AEC: {reduction_db:.2} dB reduction across {measured} measured20msframes; no devices");
        assert!(
            reduction_db > 10.0,
            "AEC must reduce a predictable40ms echo by at least10dB; got {reduction_db:.2}"
        );
    }
    #[test]
    fn native_aec_preserves_independent_near_end_during_double_talk() {
        let reference = EchoReference::default();
        let mut echo = EchoCanceller::new(reference.clone());
        let mut history = VecDeque::from(vec![0.0f32; 1_920]);
        let mut state = 0x87654321u32;
        let mut clock = 0usize;
        let (mut sine, mut cosine, mut measured) = (0.0f64, 0.0f64, 0usize);
        for frame in 0..750 {
            let start = clock;
            let mut capture = [0.0; 960];
            for chunk in capture.chunks_exact_mut(TEN_MS) {
                let mut render = [0.0; TEN_MS];
                for (sample, microphone) in render.iter_mut().zip(chunk) {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *sample = ((state >> 8) as f32 / 16_777_215.0 - 0.5) * 0.35;
                    history.push_back(*sample);
                    let echo = history.pop_front().unwrap() * 0.6;
                    let near = if frame >= 250 {
                        0.1 * (std::f32::consts::TAU * 330.0 * clock as f32 / 48_000.0).sin()
                    } else {
                        0.0
                    };
                    *microphone = echo + near;
                    clock += 1;
                }
                reference.rendered(&render);
            }
            echo.process(&mut capture, 40).unwrap();
            if frame >= 500 {
                for (index, sample) in capture.iter().enumerate() {
                    let phase = std::f64::consts::TAU * 330.0 * (start + index) as f64 / 48_000.0;
                    sine += *sample as f64 * phase.sin();
                    cosine += *sample as f64 * phase.cos();
                    measured += 1;
                }
            }
        }
        let amplitude = 2.0 * sine.hypot(cosine) / measured as f64;
        println!("Synthetic double-talk near-end330Hz amplitude: {amplitude:.4} (input0.1000); no devices");
        assert!(amplitude>0.04&&amplitude<0.14,"Echo reduction must preserve independent near-end speech components; got {amplitude:.4}");
    }
}
