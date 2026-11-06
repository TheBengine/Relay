//! Keyboard and mouse click suppression plus voice isolation for Enhanced
//! noise suppression. RNNoise and WebRTC NS remove steady noise well but let
//! short broadband clicks through; this stage finds them by their sharp onset
//! and removes them, using RNNoise's voice probability to tell them from
//! speech. Pure 48 kHz PCM code with no lookahead beyond the current block,
//! so it adds no delay to RNNoise's 10 ms.
//!
//! WebRTC's own noise suppressor was measured in front of RNNoise as well: it
//! added 7-10 ms, smeared clicks so fewer were found, and thinned the voice,
//! so it is not used.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum NoiseStrength {
    /// Clicks between words only; the voice is never touched.
    Low = 0,
    #[default]
    Normal = 1,
    /// Strongest: also quiets clicks while talking, and most sound that is not voice.
    High = 2,
}

impl NoiseStrength {
    pub(crate) fn from_u32(value: u32) -> Self {
        match value {
            0 => Self::Low,
            2 => Self::High,
            _ => Self::Normal,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Normal => "Normal",
            Self::High => "High",
        }
    }
}

#[derive(Clone, Copy)]
struct Params {
    /// Onset: 1 ms high-band energy this many times above its background.
    onset_ratio: f32,
    /// Gain for a click between words.
    pause_gain: f32,
    /// Gains for a click while the person talks: the whole click, and its high band.
    speech_gain: f32,
    speech_high_gain: f32,
    /// Lowest gain for sound that is not voice (1 = no isolation).
    isolation_floor: f32,
}

impl NoiseStrength {
    fn params(self) -> Params {
        match self {
            Self::Low => Params {
                onset_ratio: 30.0,
                pause_gain: 0.2,
                speech_gain: 1.0,
                speech_high_gain: 1.0,
                isolation_floor: 1.0,
            },
            Self::Normal => Params {
                onset_ratio: 20.0,
                pause_gain: 0.05,
                speech_gain: 1.0,
                speech_high_gain: 0.3,
                isolation_floor: 0.25,
            },
            Self::High => Params {
                onset_ratio: 12.0,
                pause_gain: 0.01,
                speech_gain: 0.6,
                speech_high_gain: 0.15,
                isolation_floor: 0.03,
            },
        }
    }

}

const SUB: usize = 48;
const RECENT: usize = 10;
/// A click also stands 6 dB above the strongest pulse of the last 10 ms, which
/// voiced speech, with a glottal pulse every pitch period, never does.
const PEAK_RATIO: f32 = 4.0;
const CLICK_SAMPLES: u32 = 30 * 48;
const MAX_CLICK_SAMPLES: u32 = 80 * 48;
const SPEECH_HOLD: u32 = 60 * 48;
const SPEECH_PROBABILITY: f32 = 0.6;
/// One-pole split near 1.5 kHz: voiced speech below, click energy above.
const SPLIT: f32 = 0.178;

pub(crate) struct ClickSuppressor {
    previous: f32,
    background: f32,
    /// High-band energy of the last 10 sub-blocks: one pitch period at 100 Hz.
    recent: [f32; RECENT],
    recent_at: usize,
    low: f32,
    click_left: u32,
    click_age: u32,
    speech_left: u32,
    gain: f32,
    high_gain: f32,
    isolation: f32,
}

impl Default for ClickSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

impl ClickSuppressor {
    pub(crate) fn new() -> Self {
        Self {
            previous: 0.0,
            background: 1e-7,
            recent: [0.0; RECENT],
            recent_at: 0,
            low: 0.0,
            click_left: 0,
            click_age: 0,
            speech_left: 0,
            gain: 1.0,
            high_gain: 1.0,
            isolation: 1.0,
        }
    }

    /// One block (a multiple of 1 ms) after RNNoise, with that block's voice
    /// probability. Returns whether a click was found in it.
    pub(crate) fn process(&mut self, block: &mut [f32], voice: f32, strength: NoiseStrength) -> bool {
        let params = strength.params();
        if voice >= SPEECH_PROBABILITY {
            self.speech_left = SPEECH_HOLD;
        } else {
            self.speech_left = self.speech_left.saturating_sub(block.len() as u32);
        }
        let speaking = self.speech_left > 0;
        // Pass 1: which 1 ms sub-blocks are inside a click. The whole block is
        // still unsent, so suppression can start one sub-block before an onset.
        let subs = block.len() / SUB;
        let mut inside = [false; 64];
        let mut found = false;
        let mut previous = self.previous;
        for index in 0..subs.min(64) {
            let chunk = &block[index * SUB..(index + 1) * SUB];
            let mut energy = 0.0;
            for sample in chunk {
                let difference = sample - previous;
                previous = *sample;
                energy += difference * difference;
            }
            let energy = energy / SUB as f32;
            let recent_peak = self.recent.iter().fold(0.0f32, |peak, e| peak.max(*e));
            self.recent[self.recent_at] = energy;
            self.recent_at = (self.recent_at + 1) % RECENT;
            let onset = energy > self.background * params.onset_ratio
                && energy > recent_peak * PEAK_RATIO
                && energy > 1e-7;
            if onset && self.click_left == 0 {
                self.click_left = CLICK_SAMPLES;
                self.click_age = 0;
                found = true;
                if index > 0 {
                    inside[index - 1] = true;
                }
            } else if self.click_left > 0
                && energy > self.background * params.onset_ratio * 0.25
                && self.click_age < MAX_CLICK_SAMPLES
            {
                // Still ringing: keep going, but never longer than 80 ms.
                self.click_left = self.click_left.max(10 * 48);
            }
            inside[index] = self.click_left > 0;
            if self.click_left > 0 {
                self.click_left = self.click_left.saturating_sub(SUB as u32);
                self.click_age += SUB as u32;
            } else {
                // The background follows quiet sound quickly and loud sound slowly.
                let rate = if energy > self.background { 0.003 } else { 0.05 };
                self.background = (self.background + (energy - self.background) * rate).max(1e-9);
            }
        }
        self.previous = previous;
        // Pass 2: smooth gains (0.5 ms down, 8 ms back up) on a two-band split.
        let isolation = params.isolation_floor
            + (1.0 - params.isolation_floor) * ((voice - 0.15) / 0.45).clamp(0.0, 1.0);
        for (index, sample) in block.iter_mut().enumerate() {
            let click = inside[(index / SUB).min(63)];
            let full_target = match (click, speaking) {
                (false, _) => 1.0,
                (true, false) => params.pause_gain,
                (true, true) => params.speech_gain,
            };
            let high_target = if click && speaking { params.speech_high_gain } else { 1.0 };
            self.gain += (full_target - self.gain) * if full_target < self.gain { 0.04 } else { 0.0026 };
            self.high_gain += (high_target - self.high_gain) * if high_target < self.high_gain { 0.04 } else { 0.0026 };
            // Voice isolation opens within ~5 ms and closes over ~150 ms.
            self.isolation += (isolation - self.isolation) * if isolation > self.isolation { 0.004 } else { 0.00014 };
            self.low += (*sample - self.low) * SPLIT;
            let high = *sample - self.low;
            *sample = (self.low + high * self.high_gain) * self.gain * self.isolation;
        }
        found
    }
}

/// Keys struck while the user talks, for Studio (DPDFNet). Studio has no
/// voice probability fine enough for `ClickSuppressor`, so this stage looks
/// only at the band above `split_hz`: a 1 ms energy that jumps `onset_db`
/// above both the band's median over the last 50 ms and its own level 2 ms
/// earlier starts a window in which the band is held at its level from before
/// the jump (`hold_db` relative to that median) for at most `max_ms`. Below
/// the split nothing changes, so the voice's body is never touched.
///
/// It works on whole 10 ms blocks with no look-ahead beyond the block, so it
/// adds no delay. Where it sits (before the model or after it) is decided by
/// `dpdfnet::StudioKind`. Measured on the bench, no placement made keys on
/// words quieter without changing the voice, so the app does not use it
/// (`noise_suppressor::STUDIO_KEYS`); see `docs/audio-keys-on-words-20261030.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct KeyHoldParams {
    pub(crate) split_hz: u16,
    pub(crate) onset_db: u8,
    pub(crate) hold_db: i8,
    pub(crate) max_ms: u8,
    /// After the model only: 0, or how much of the jump (dB, in the model's
    /// input lined up with this output) the model must have removed for it
    /// to count as a key. The detector then looks at the model's input.
    pub(crate) removed_db: u8,
}

const KEY_MEDIAN: usize = 50;
/// Below this 1 ms energy (-80 dBFS) nothing counts as an onset.
const KEY_FLOOR: f32 = 1e-8;

pub(crate) struct KeyHold {
    params: KeyHoldParams,
    onset_ratio: f32,
    hold_ratio: f32,
    /// Second-order Butterworth low-pass at the split (b0, b1, b2, a1, a2).
    coefficients: [f32; 5],
    filter: [f32; 4],
    reference_filter: [f32; 4],
    removed_ratio: f32,
    /// High-band energy of the last 50 sub-blocks outside a window.
    history: [f32; KEY_MEDIAN],
    history_at: usize,
    /// The same for the band being held (differs only when guided).
    output_history: [f32; KEY_MEDIAN],
    /// The last two sub-block energies, windows included.
    recent: [f32; 2],
    left: u32,
    hold: f32,
    gain: f32,
    windows: u64,
    held_samples: u64,
}

impl KeyHold {
    pub(crate) fn new(params: KeyHoldParams) -> Self {
        let w = (std::f32::consts::PI * params.split_hz as f32 / 48_000.0).tan();
        let norm = 1.0 / (1.0 + std::f32::consts::SQRT_2 * w + w * w);
        let b0 = w * w * norm;
        Self {
            params,
            onset_ratio: 10f32.powf(params.onset_db as f32 / 10.0),
            hold_ratio: 10f32.powf(params.hold_db as f32 / 10.0),
            coefficients: [b0, 2.0 * b0, b0, 2.0 * (w * w - 1.0) * norm, (1.0 - std::f32::consts::SQRT_2 * w + w * w) * norm],
            filter: [0.0; 4],
            reference_filter: [0.0; 4],
            removed_ratio: 10f32.powf(-(params.removed_db as f32) / 10.0),
            history: [KEY_FLOOR; KEY_MEDIAN],
            output_history: [KEY_FLOOR; KEY_MEDIAN],
            history_at: 0,
            recent: [KEY_FLOOR; 2],
            left: 0,
            hold: 0.0,
            gain: 1.0,
            windows: 0,
            held_samples: 0,
        }
    }

    fn held_median(&self) -> f32 {
        let mut sorted = self.output_history;
        *sorted.select_nth_unstable_by(KEY_MEDIAN / 2, f32::total_cmp).1
    }

    /// How many windows were opened, and how many samples were in one.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn counts(&self) -> (u64, u64) {
        (self.windows, self.held_samples)
    }

    fn low_pass(
        coefficients: [f32; 5],
        state: &mut [f32; 4],
        block: &[f32; crate::noise_suppressor::BLOCK],
    ) -> [f32; crate::noise_suppressor::BLOCK] {
        let [b0, b1, b2, a1, a2] = coefficients;
        let mut low = [0.0f32; crate::noise_suppressor::BLOCK];
        let [mut x1, mut x2, mut y1, mut y2] = *state;
        for (to, &x) in low.iter_mut().zip(block.iter()) {
            let y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            (x2, x1, y2, y1) = (x1, x, y1, y);
            *to = y;
        }
        *state = [x1, x2, y1, y2];
        low
    }

    pub(crate) fn process(&mut self, block: &mut [f32; crate::noise_suppressor::BLOCK]) {
        self.process_with(block, None);
    }

    /// `reference`: the model's input lined up with `block`, its output.
    pub(crate) fn process_with(
        &mut self,
        block: &mut [f32; crate::noise_suppressor::BLOCK],
        reference: Option<&[f32; crate::noise_suppressor::BLOCK]>,
    ) {
        const SUBS: usize = crate::noise_suppressor::BLOCK / SUB;
        let low = Self::low_pass(self.coefficients, &mut self.filter, block);
        let reference = match reference {
            Some(r) if self.params.removed_db > 0 => {
                Some((r, Self::low_pass(self.coefficients, &mut self.reference_filter, r)))
            }
            _ => None,
        };
        let high_energy = |signal: &[f32], low: &[f32], index: usize| {
            let range = index * SUB..(index + 1) * SUB;
            signal[range.clone()].iter().zip(&low[range]).map(|(x, l)| (x - l) * (x - l)).sum::<f32>() / SUB as f32
        };
        // Pass 1: the target gain of the high band per 1 ms sub-block.
        let mut target = [1.0f32; SUBS];
        let mut sorted = [0.0f32; KEY_MEDIAN];
        for (index, wanted) in target.iter_mut().enumerate() {
            let energy = high_energy(block, &low, index);
            // Guided: the jump is looked for in the model's input, and counts
            // only where the model took most of it away.
            let (detected, removed) = match &reference {
                Some((r, r_low)) => {
                    let input = high_energy(&r[..], r_low, index);
                    (input, energy < input * self.removed_ratio)
                }
                None => (energy, true),
            };
            if self.left == 0 {
                sorted.copy_from_slice(&self.history);
                let (_, median, _) = sorted.select_nth_unstable_by(KEY_MEDIAN / 2, f32::total_cmp);
                let median = *median;
                if detected > KEY_FLOOR
                    && detected > median * self.onset_ratio
                    && detected > self.recent[0] * self.onset_ratio
                    && removed
                {
                    self.left = self.params.max_ms as u32;
                    self.hold = self.held_median() * self.hold_ratio;
                    self.windows += 1;
                }
            }
            if self.left > 0 && energy > self.hold {
                *wanted = (self.hold / energy).sqrt();
                self.left -= 1;
                self.held_samples += SUB as u64;
            } else {
                self.left = 0;
                self.history[self.history_at] = detected.max(KEY_FLOOR * 0.01);
                self.output_history[self.history_at] = energy.max(KEY_FLOOR * 0.01);
                self.history_at = (self.history_at + 1) % KEY_MEDIAN;
            }
            self.recent = [self.recent[1], detected];
        }
        // The whole block is still unsent: start one sub-block early.
        for index in 1..SUBS {
            target[index - 1] = target[index - 1].min(target[index]);
        }
        // Pass 2: down within 0.1 ms, back up over about 2 ms.
        for (index, (sample, low)) in block.iter_mut().zip(low).enumerate() {
            let wanted = target[index / SUB];
            self.gain += (wanted - self.gain) * if wanted < self.gain { 0.2 } else { 0.01 };
            *sample = low + (*sample - low) * self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_test_signals::{clicks, db, rms, talker, RATE};

    fn run(signal: &[f32], voice: impl Fn(usize) -> f32, strength: NoiseStrength) -> Vec<f32> {
        let mut suppressor = ClickSuppressor::new();
        let mut out = signal.to_vec();
        for (index, block) in out.chunks_exact_mut(480).enumerate() {
            suppressor.process(block, voice(index), strength);
        }
        out
    }

    #[test]
    fn clicks_between_words_are_removed_and_speech_is_left_alone() {
        let (keys, onsets) = clicks(6.0, 3.0, 0.3, 9);
        let speech = talker(6.0, (105.0, 135.0), 4, 0.3);
        // Clicks alone, nobody talking.
        for (strength, wanted_db) in [(NoiseStrength::Low, 10.0), (NoiseStrength::Normal, 20.0), (NoiseStrength::High, 30.0)] {
            let out = run(&keys, |_| 0.0, strength);
            let mut before = 0.0;
            let mut after = 0.0;
            for onset in &onsets {
                let end = (onset + 1_440).min(keys.len());
                before += keys[*onset..end].iter().map(|s| s * s).sum::<f32>();
                after += out[*onset..end].iter().map(|s| s * s).sum::<f32>();
            }
            let reduction = 10.0 * (before / after.max(1e-12)).log10();
            println!("Clicks in pauses, {}: reduced {reduction:.1} dB", strength.label());
            assert!(reduction >= wanted_db, "{}: {reduction:.1} dB", strength.label());
        }
        // Speech alone with RNNoise saying it is speech: nothing may change much.
        for strength in [NoiseStrength::Low, NoiseStrength::Normal, NoiseStrength::High] {
            let out = run(&speech, |index| if rms(&speech[index * 480..index * 480 + 480]) > 0.003 { 0.95 } else { 0.0 }, strength);
            let mut changes: Vec<f32> = speech
                .chunks_exact(960)
                .zip(out.chunks_exact(960))
                .filter(|(a, _)| db(rms(a)) > -35.0)
                .map(|(a, b)| db(rms(b)) - db(rms(a)))
                .collect();
            changes.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = changes[changes.len() / 2];
            let low = changes[changes.len() / 20];
            println!("Speech through click suppression, {}: median {median:+.2} dB, 5th percentile {low:+.2} dB", strength.label());
            // High's voice isolation also lowers the quiet ends of syllables.
            let wanted = if strength == NoiseStrength::High { -4.0 } else { -1.0 };
            assert!(median > wanted && low > -6.0, "{}: {median:.2} / {low:.2}", strength.label());
        }
    }

    #[test]
    fn isolation_quiets_sound_that_is_not_voice_only_at_normal_and_high() {
        let hum: Vec<f32> = (0..(RATE as usize)).map(|i| (i as f32 * 0.02).sin() * 0.05).collect();
        for (strength, floor) in [(NoiseStrength::Low, 1.0f32), (NoiseStrength::Normal, 0.25), (NoiseStrength::High, 0.03)] {
            let out = run(&hum, |_| 0.0, strength);
            let ratio = rms(&out[24_000..]) / rms(&hum[24_000..]);
            assert!((ratio - floor).abs() < floor * 0.15 + 0.01, "{}: {ratio}", strength.label());
            let voiced = run(&hum, |_| 1.0, strength);
            assert!(rms(&voiced[24_000..]) / rms(&hum[24_000..]) > 0.95, "voice passes at {}", strength.label());
        }
    }

    const HOLD: KeyHoldParams = KeyHoldParams { split_hz: 1_000, onset_db: 12, hold_db: 0, max_ms: 40, removed_db: 0 };

    fn hold(signal: &[f32], params: KeyHoldParams) -> (Vec<f32>, (u64, u64)) {
        let mut stage = KeyHold::new(params);
        let mut out = signal.to_vec();
        for block in out.chunks_exact_mut(480) {
            stage.process(block.try_into().unwrap());
        }
        (out, stage.counts())
    }

    /// Energy above 1 kHz (a first difference is enough to compare).
    fn high_energy(signal: &[f32]) -> f32 {
        signal.windows(2).map(|w| (w[1] - w[0]) * (w[1] - w[0])).sum()
    }

    /// Why Studio does not use the plain hold: on a voice alone, with no key
    /// anywhere, its transient detector still opens on the voice's own
    /// onsets and changes it (on the recorded voice, LSD +1.0 to +2.0 dB).
    #[test]
    fn a_plain_key_hold_mistakes_a_voice_for_keys() {
        let speech = talker(4.0, (105.0, 135.0), 13, 0.3);
        let (out, (windows, held)) = hold(&speech, HOLD);
        let error: f32 = speech.iter().zip(&out).map(|(a, b)| (a - b) * (a - b)).sum();
        let ratio_db = 10.0 * (error / speech.iter().map(|s| s * s).sum::<f32>()).log10();
        println!("Key hold on a voice alone: {windows} windows, {held} samples held, change {ratio_db:.1} dB");
        assert!(windows > 10 && ratio_db > -20.0, "{windows} windows, {ratio_db:.1} dB");
    }

    #[test]
    fn key_hold_adds_no_delay_below_the_split() {
        // A sine below the split passes untouched and at lag 0. Its start out
        // of digital silence is a jump and may open one window; nothing after.
        let tone: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.02).sin() * 0.3).collect();
        let (out, (windows, _)) = hold(&tone, HOLD);
        assert!(windows <= 1, "{windows} windows");
        let error = tone.iter().zip(&out).skip(4_800).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(error < 1e-4, "{error}");
    }

    #[test]
    fn key_hold_takes_keys_on_a_voice_down_to_the_voice() {
        let speech = talker(6.0, (105.0, 135.0), 4, 0.3);
        let (keys, onsets) = clicks(6.0, 3.0, 0.3, 9);
        let noisy: Vec<f32> = speech.iter().zip(&keys).map(|(s, k)| s + k).collect();
        let (out, (windows, _)) = hold(&noisy, HOLD);
        let (mut before, mut after) = (0.0, 0.0);
        for &onset in &onsets {
            let end = (onset + 480).min(noisy.len());
            before += high_energy(&noisy[onset..end]) - high_energy(&speech[onset..end]);
            after += (high_energy(&out[onset..end]) - high_energy(&speech[onset..end])).max(0.0);
        }
        let reduction = 10.0 * (before / after.max(1e-12)).log10();
        println!("Key hold: {windows} windows for {} keys, key energy above the voice down {reduction:.1} dB", onsets.len());
        assert!(windows as usize >= onsets.len(), "{windows} windows for {} keys", onsets.len());
        assert!(reduction >= 6.0, "{reduction:.1} dB");
    }

    /// Guided by the model's input: a jump the model kept (a consonant) is
    /// left alone; a jump it took most of away (a key) opens a window.
    #[test]
    fn guided_key_hold_acts_only_where_the_model_removed_the_jump() {
        let guided = KeyHoldParams { removed_db: 6, ..HOLD };
        let speech = talker(4.0, (105.0, 135.0), 4, 0.3);
        let (keys, onsets) = clicks(4.0, 3.0, 0.3, 9);
        let input: Vec<f32> = speech.iter().zip(&keys).map(|(s, k)| s + k).collect();
        let run = |output: &[f32]| {
            let mut stage = KeyHold::new(guided);
            let mut out = output.to_vec();
            for (block, reference) in out.chunks_exact_mut(480).zip(input.chunks_exact(480)) {
                stage.process_with(block.try_into().unwrap(), Some(reference.try_into().unwrap()));
            }
            (out, stage.counts().0)
        };
        // The "model" kept everything: nothing counts as a key.
        let (kept, windows) = run(&input);
        assert_eq!(windows, 0);
        assert!(kept.iter().zip(&input).all(|(a, b)| (a - b).abs() < 1e-4));
        // The "model" took the keys 14 dB down: their onsets open windows.
        let removed: Vec<f32> = speech.iter().zip(&keys).map(|(s, k)| s + k * 0.2).collect();
        let (_, windows) = run(&removed);
        println!("Guided key hold: {windows} windows for {} keys", onsets.len());
        assert!(windows as usize >= onsets.len() / 2, "{windows} windows for {} keys", onsets.len());
    }
}
