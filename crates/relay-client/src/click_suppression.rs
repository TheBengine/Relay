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
}
