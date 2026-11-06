//! Voice-activation gate: open threshold with hysteresis, attack, hold and
//! release ramps, and the level meter that shows exactly what the gate measures.
//! Pure 48 kHz PCM code: no devices, threads or allocation per block.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

const SAMPLES_PER_MS: u32 = 48;
/// The gate closes 6 dB below the open threshold, so a level hovering at the
/// threshold does not flutter the gate open and shut.
pub(crate) const CLOSE_RATIO: f32 = 0.5;
pub(crate) const MAX_ATTACK_MS: u32 = 50;
pub(crate) const MAX_RELEASE_MS: u32 = 500;
/// Blocks per noise-floor window (0.25 s of 10 ms blocks).
pub(crate) const FLOOR_WINDOW_BLOCKS: usize = 25;
/// Windows the floor remembers: 2 s of 10 ms blocks.
const FLOOR_WINDOWS: usize = 8;
/// With a voice detector, a block must also stand this far above the quietest
/// level of the last 2 s (6 dB) to open the gate: a fan the detector mistakes
/// for a voice sits at that floor, a voice over it does not.
pub(crate) const DETECTOR_FLOOR_RATIO: f32 = 2.0;
/// Speech rises and falls with every syllable; a fan does not. The swing
/// memory is the quietest level of the last 0.3 s.
const SWING_WINDOW_BLOCKS: usize = 10;
const SWING_WINDOWS: usize = 3;
/// A speech-like level also stands this far above the last 0.3 s (8 dB).
pub(crate) const SWING_RATIO: f32 = 2.5;
/// -80 dBFS: a floor learned in digital silence is not taken as quieter.
const MIN_FLOOR: f32 = 0.0001;
/// The noise suppressor's verdict on a block: the share of its energy it kept
/// (`SuppressionRatio`). A block opens the gate only if the suppressor kept
/// at least this much (-20 dB). On the bench (recorded voice) a voice under a
/// fan at 5 dB keeps -6 dB, a quiet word start under it -15 to -20 dB, the
/// fan alone -40 dB or less; Studio also removes music.
pub(crate) const KEPT_TO_OPEN: f32 = 0.01;
/// Once open, the gate stays with blocks kept down to -25 dB, so the soft
/// end of a word is not cut while the suppressor is unsure.
pub(crate) const KEPT_TO_STAY: f32 = 0.003;
/// With RNNoise its own speech probability must also reach this to open the
/// gate. On the bench it reaches 0.02 within 30 ms at every word start in a
/// clean voice and in 12 % of key pauses (level, ratio and detector must
/// agree as well). It calls music speech, so music is left to Studio.
pub(crate) const VOICE_TO_OPEN: f32 = 0.05;

/// What the noise suppressor's model says about one block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SpeechModel {
    /// Share of the block's energy the suppressor kept (`SuppressionRatio`).
    pub kept: f32,
    /// The suppressor's speech probability, when it has one.
    pub voice: Option<f32>,
}

/// The quietest level of the last `memory` windows of `window_blocks` blocks
/// (minimum statistics). It needs no speech decision, so it learns while the
/// gate is open too, and it holds no audio.
#[derive(Clone, Copy)]
struct LevelFloor {
    current: f32,
    blocks: usize,
    windows: [f32; FLOOR_WINDOWS],
    filled: usize,
    next: usize,
    window_blocks: usize,
    memory: usize,
    ratio: f32,
}

impl LevelFloor {
    fn new(window_blocks: usize, memory: usize, ratio: f32) -> Self {
        Self {
            current: f32::INFINITY,
            blocks: 0,
            windows: [f32::INFINITY; FLOOR_WINDOWS],
            filled: 0,
            next: 0,
            window_blocks,
            memory: memory.clamp(1, FLOOR_WINDOWS),
            ratio,
        }
    }

    fn observe(&mut self, level: f32) {
        if !level.is_finite() {
            return;
        }
        self.current = self.current.min(level);
        self.blocks += 1;
        if self.blocks >= self.window_blocks {
            self.windows[self.next] = self.current;
            self.next = (self.next + 1) % self.memory;
            self.filled = (self.filled + 1).min(self.memory);
            self.current = f32::INFINITY;
            self.blocks = 0;
        }
    }

    fn value(&self) -> Option<f32> {
        (self.filled > 0).then(|| {
            self.windows[..self.memory]
                .iter()
                .copied()
                .fold(self.current, f32::min)
                .max(MIN_FLOOR)
        })
    }

    /// True until one whole window has been heard, so speech from the first
    /// block of a call is never measured against itself.
    fn clears(&self, level: f32) -> bool {
        self.value().is_none_or(|floor| level >= floor * self.ratio)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GateTiming {
    pub attack_ms: u32,
    pub hold_ms: u32,
    pub release_ms: u32,
}

pub(crate) struct NoiseGate {
    gain: f32,
    hold_left: u32,
    open: bool,
    floor: LevelFloor,
    swing: LevelFloor,
    /// Mean squared gain over the last block (the gate bench reads it).
    gain_power: f32,
    /// The last block was removed by the suppressor and sat at the noise
    /// floor: the hold ends there instead of keeping noise open.
    end_hold: bool,
}

impl Default for NoiseGate {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseGate {
    pub(crate) fn new() -> Self {
        Self {
            gain: 0.0,
            hold_left: 0,
            open: false,
            floor: LevelFloor::new(FLOOR_WINDOW_BLOCKS, FLOOR_WINDOWS, DETECTOR_FLOOR_RATIO),
            swing: LevelFloor::new(SWING_WINDOW_BLOCKS, SWING_WINDOWS, SWING_RATIO),
            gain_power: 0.0,
            end_hold: false,
        }
    }

    /// Speech decision for one block. A manual threshold opens at `threshold`
    /// and, once open, stays open down to `threshold * CLOSE_RATIO`. An external
    /// detector (automatic sensitivity) replaces the comparison, but only for
    /// blocks that also stand clear of the recent noise floor.
    ///
    /// `model` is the noise suppressor's verdict on the block (None without
    /// suppression). Level and suppressor must agree to open; the threshold
    /// still decides how loud speech must be, so the Input Sensitivity slider
    /// keeps its meaning. A block the suppressor removed that also sits at
    /// the noise floor ends the hold: the gate closes on the ratio alone there.
    pub(crate) fn detect(
        &mut self,
        level: f32,
        threshold: f32,
        detected: Option<bool>,
        model: Option<SpeechModel>,
    ) -> bool {
        let heard = match detected {
            Some(detected) => detected && self.above_noise_floor(level),
            None => {
                let limit = if self.open {
                    threshold * CLOSE_RATIO
                } else {
                    threshold
                };
                level >= limit
            }
        };
        let agrees = model.is_none_or(|model| {
            if self.open {
                model.kept >= KEPT_TO_STAY
            } else {
                model.kept >= KEPT_TO_OPEN && model.voice.is_none_or(|voice| voice >= VOICE_TO_OPEN)
            }
        });
        let speech = heard && agrees;
        let near_floor = self.floor.value().is_some() && !self.floor.clears(level);
        self.end_hold =
            !speech && near_floor && model.is_some_and(|model| model.kept < KEPT_TO_STAY);
        self.floor.observe(level);
        self.swing.observe(level);
        self.open = speech;
        speech
    }

    /// Whether `level` is clearly louder than the quietest level of the last
    /// 2 s. True until that floor has been learned.
    pub(crate) fn above_noise_floor(&self, level: f32) -> bool {
        self.floor.clears(level)
    }

    /// The quietest level of the last 2 s, once one window has been heard.
    // Automatic volume reads this and `speech_like_level` once the media.rs
    // change in docs/send-delay-budget-20261028.md lands.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn noise_floor(&self) -> Option<f32> {
        self.floor.value()
    }

    /// Stricter than the gate, for automatic volume to learn from: clear of
    /// the 2 s floor and of the last 0.3 s. A rising fan never is, a
    /// syllable is. Not learning from a long vowel costs nothing; cutting
    /// it would, so the gate itself does not use this.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn speech_like_level(&self, level: f32) -> bool {
        self.floor.clears(level) && self.swing.clears(level)
    }

    /// Applies the gate to one block. Returns whether anything in the block is
    /// transmitted. A fully closed block is left untouched, so a speech-start
    /// history can still prepend it when the next block opens the gate.
    /// `instant_open` skips the attack ramp when that history supplies the fade-in.
    pub(crate) fn apply(
        &mut self,
        block: &mut [f32],
        speech: bool,
        timing: GateTiming,
        instant_open: bool,
    ) -> bool {
        let samples = block.len() as u32;
        if !speech && std::mem::take(&mut self.end_hold) {
            self.hold_left = 0;
        }
        let target = if speech {
            self.hold_left = timing.hold_ms.saturating_mul(SAMPLES_PER_MS);
            1.0
        } else if self.hold_left > 0 {
            self.hold_left = self.hold_left.saturating_sub(samples);
            1.0
        } else {
            0.0
        };
        self.gain_power = 0.0;
        if target == 0.0 && self.gain <= 0.0 {
            self.gain = 0.0;
            return false;
        }
        if target == 1.0 && (instant_open || timing.attack_ms == 0) {
            self.gain = 1.0;
        }
        if target == 0.0 && timing.release_ms == 0 {
            self.gain = 0.0;
            return false;
        }
        let mut power = 0.0;
        let attack = 1.0 / timing.attack_ms.max(1).saturating_mul(SAMPLES_PER_MS) as f32;
        let release = 1.0 / timing.release_ms.max(1).saturating_mul(SAMPLES_PER_MS) as f32;
        for sample in block.iter_mut() {
            // Snap the last partial step, so float rounding never adds a block.
            if self.gain < target {
                self.gain += attack;
                if self.gain > 1.0 - attack * 0.5 {
                    self.gain = 1.0;
                }
            } else if self.gain > target {
                self.gain -= release;
                if self.gain < release * 0.5 {
                    self.gain = 0.0;
                }
            }
            *sample *= self.gain;
            power += self.gain * self.gain;
        }
        self.gain_power = power / block.len().max(1) as f32;
        true
    }

    /// Mean squared gain the last `apply` gave its block (0 when closed).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn gain_power(&self) -> f32 {
        self.gain_power
    }

    pub(crate) fn is_open(&self) -> bool {
        self.gain > 0.0
    }
}

/// Linear fade-in over the first `samples` of a staged speech start.
pub(crate) fn fade_in(block: &mut [f32], samples: usize) {
    let len = samples.min(block.len());
    for (index, sample) in block[..len].iter_mut().enumerate() {
        *sample *= (index + 1) as f32 / (len + 1) as f32;
    }
}

/// Longest "keep the microphone open after releasing push-to-talk".
pub(crate) const MAX_PTT_RELEASE_MS: u32 = 2000;

/// Push-to-talk stays open this long after the key comes up, so the end of
/// the last word is not cut off. Pressing again within the delay continues
/// the same talkspurt.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PushToTalkRelease {
    last_down: Option<std::time::Instant>,
}

impl PushToTalkRelease {
    /// Whether the microphone is open, given the key now.
    pub(crate) fn open(&mut self, key_down: bool, delay_ms: u32, now: std::time::Instant) -> bool {
        if key_down {
            self.last_down = Some(now);
            return true;
        }
        let delay = std::time::Duration::from_millis(delay_ms.min(MAX_PTT_RELEASE_MS).into());
        match self.last_down {
            Some(at) if now.saturating_duration_since(at) < delay => true,
            _ => {
                self.last_down = None;
                false
            }
        }
    }
}

/// Raw block energies remembered for the ratio: Studio's 60 ms plus one.
const RATIO_HISTORY: usize = 8;
/// Below this raw energy per sample (-100 dBFS) a block says nothing.
const RATIO_MIN_RAW: f32 = 1e-10;

/// How much of a block the noise suppressor kept: its output energy over the
/// energy of the raw block that output belongs to (the suppressor returns each
/// block `lag` blocks late). Near 1 on speech, near 0 on what it removed.
/// Only scalar energies are remembered; no audio is held back.
#[derive(Clone, Copy)]
pub(crate) struct SuppressionRatio {
    raw: [f32; RATIO_HISTORY],
    next: usize,
    seen: usize,
    kept: f32,
    heard: f32,
}

impl Default for SuppressionRatio {
    fn default() -> Self {
        Self {
            raw: [0.0; RATIO_HISTORY],
            next: 0,
            seen: 0,
            kept: 0.0,
            heard: 0.0,
        }
    }
}

fn energy(block: &[f32]) -> f32 {
    block.iter().map(|sample| sample * sample).sum()
}

impl SuppressionRatio {
    /// Whole blocks a suppressor with this delay returns its output late.
    pub(crate) fn lag_blocks(delay_samples: usize, block: usize) -> usize {
        (delay_samples + block / 2) / block.max(1)
    }

    /// The raw block, just before the suppressor takes it.
    pub(crate) fn raw(&mut self, block: &[f32]) {
        self.raw[self.next] = energy(block);
        self.next = (self.next + 1) % RATIO_HISTORY;
        self.seen = self.seen.saturating_add(1);
    }

    /// The suppressor's output for the block given to `raw` last, which
    /// belongs to the raw block `lag` blocks before it.
    pub(crate) fn kept(&mut self, block: &[f32], lag: usize) {
        if lag >= RATIO_HISTORY || self.seen <= lag {
            return;
        }
        let at = (self.next + RATIO_HISTORY - 1 - lag) % RATIO_HISTORY;
        self.kept += energy(block);
        self.heard += self.raw[at];
    }

    /// The ratio over the blocks since the last call (0..1, capped), or None
    /// when the raw input was silent or the suppressor's delay is not covered.
    pub(crate) fn take(&mut self, samples: usize) -> Option<f32> {
        let (kept, heard) = (self.kept, self.heard);
        self.kept = 0.0;
        self.heard = 0.0;
        (heard > RATIO_MIN_RAW * samples.max(1) as f32).then(|| (kept / heard).min(1.0))
    }
}

/// Written by the capture worker once per block, read by Settings. The level
/// is the RMS the gate compares, the threshold the one it used for that block.
#[derive(Default)]
pub(crate) struct GateMeter {
    level: AtomicU32,
    threshold: AtomicU32,
    open: AtomicBool,
    automatic: AtomicBool,
    blocks: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct GateReading {
    /// Loudest detection level since the previous read (RMS, 0..1).
    pub level: f32,
    /// The open threshold the gate used (RMS); automatic sensitivity moves it.
    pub threshold: f32,
    pub open: bool,
    pub automatic: bool,
    pub blocks: u64,
}

impl GateMeter {
    pub(crate) fn record(&self, level: f32, threshold: f32, open: bool, automatic: bool) {
        if level.is_finite() && level > 0.0 {
            // Non-negative finite f32 bit patterns order like their values.
            self.level
                .fetch_max(level.min(1.0).to_bits(), Ordering::Relaxed);
        }
        if threshold.is_finite() && threshold > 0.0 {
            self.threshold.store(threshold.to_bits(), Ordering::Relaxed);
        }
        self.open.store(open, Ordering::Relaxed);
        self.automatic.store(automatic, Ordering::Relaxed);
        self.blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn take(&self) -> GateReading {
        GateReading {
            level: f32::from_bits(self.level.swap(0, Ordering::Relaxed)),
            threshold: f32::from_bits(self.threshold.load(Ordering::Relaxed)),
            open: self.open.load(Ordering::Relaxed),
            automatic: self.automatic.load(Ordering::Relaxed),
            blocks: self.blocks.load(Ordering::Relaxed),
        }
    }
}

/// Display smoothing for the gate bar: rises at once, falls over ~300 ms.
#[derive(Default)]
pub(crate) struct GateDisplay {
    level: f32,
    blocks: u64,
    updated: Option<std::time::Instant>,
}

impl GateDisplay {
    /// Returns the smoothed level and whether audio arrived recently.
    pub(crate) fn update(&mut self, reading: GateReading, now: std::time::Instant) -> (f32, bool) {
        let elapsed = self
            .updated
            .map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f32());
        self.updated = Some(now);
        let live = reading.blocks != self.blocks;
        self.blocks = reading.blocks;
        self.level = reading.level.max(self.level * (-elapsed / 0.3).exp());
        (self.level, live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMING: GateTiming = GateTiming {
        attack_ms: 5,
        hold_ms: 40,
        release_ms: 50,
    };

    #[test]
    fn hysteresis_keeps_a_level_hovering_at_the_threshold_open() {
        let mut gate = NoiseGate::new();
        assert!(!gate.detect(0.009, 0.01, None, None), "below the open threshold");
        assert!(gate.detect(0.011, 0.01, None, None));
        // Once open, the level may sink 6 dB before the gate starts to close.
        assert!(gate.detect(0.0075, 0.01, None, None));
        assert!(gate.detect(0.0051, 0.01, None, None));
        assert!(!gate.detect(0.0049, 0.01, None, None));
        assert!(!gate.detect(0.0075, 0.01, None, None), "closed again: needs the full threshold");
        assert!(gate.detect(0.0, 0.01, Some(true), None), "an external detector decides");
    }

    #[test]
    fn attack_hold_and_release_ramp_instead_of_switching() {
        let mut gate = NoiseGate::new();
        let mut closed = [0.5; 480];
        assert!(!gate.apply(&mut closed, false, TIMING, false));
        assert!(closed.iter().all(|s| *s == 0.5), "a closed block is left for the preroll");
        let mut opening = [0.5; 480];
        assert!(gate.apply(&mut opening, true, TIMING, false));
        // 5 ms attack: the first sample is tiny, full level from 240 samples on.
        assert!(opening[0] < 0.01 && (opening[239] - 0.5).abs() < 1e-6 && opening[479] == 0.5);
        assert!(opening.windows(2).all(|w| w[1] >= w[0]), "attack never steps down");
        // 40 ms hold at full level, then a 50 ms linear release, then closed.
        let mut transmitted = 0;
        let mut last = 0.5;
        for _ in 0..12 {
            let mut block = [0.5; 480];
            if gate.apply(&mut block, false, TIMING, false) {
                transmitted += 1;
                assert!(block.windows(2).all(|w| w[1] <= w[0] + 1e-7), "release is monotonic");
                assert!(block[0] <= last + 1e-6, "no step between blocks");
                last = block[479];
            }
        }
        assert_eq!(transmitted, 4 + 5, "4 hold blocks, then 50 ms of release");
        assert_eq!(last, 0.0);
        assert!(!gate.is_open());
    }

    #[test]
    fn zero_timings_reproduce_the_former_hard_gate_and_instant_open_skips_attack() {
        let hard = GateTiming {
            attack_ms: 0,
            hold_ms: 0,
            release_ms: 0,
        };
        let mut gate = NoiseGate::new();
        let mut block = [0.3; 480];
        assert!(gate.apply(&mut block, true, hard, false));
        assert!(block.iter().all(|s| *s == 0.3));
        assert!(!gate.apply(&mut [0.3; 480], false, hard, false));
        let mut gate = NoiseGate::new();
        let mut block = [0.3; 480];
        assert!(gate.apply(&mut block, true, TIMING, true));
        assert!(block.iter().all(|s| *s == 0.3), "the staged history fades in instead");
        let mut start = [1.0; 96];
        fade_in(&mut start, 48);
        assert!(start[0] < 0.03 && start[47] < 1.0 && start[48] == 1.0);
    }

    #[test]
    fn default_attack_fades_the_current_block_in_without_lookahead() {
        let defaults = crate::audio_settings::AudioSettings::default();
        let timing = GateTiming {
            attack_ms: defaults.activation_attack_ms,
            hold_ms: defaults.activation_hold_ms,
            release_ms: defaults.activation_release_ms,
        };
        assert_eq!(timing.attack_ms, 5);
        let input: [f32; 480] = std::array::from_fn(|n| 0.001 * (n + 1) as f32);
        let mut block = input;
        let mut gate = NoiseGate::new();
        assert!(gate.apply(&mut block, true, timing, false));
        // Sample n leaves with sample n, only scaled: nothing waits for later audio.
        assert!(block[0] > 0.0, "the very first sample of the opening block is sent");
        for (n, (out, heard)) in block.iter().zip(input).enumerate() {
            let ramp = ((n + 1) as f32 / 240.0).min(1.0);
            assert!((out - heard * ramp).abs() < 1e-5, "sample {n} moved or was reshaped");
        }
        assert_eq!(block[240..], input[240..], "full level after 5 ms, unshifted");
    }

    #[test]
    fn a_detector_alone_cannot_hold_the_gate_open_on_a_steady_noise_floor() {
        let mut gate = NoiseGate::new();
        // A fan the voice detector mistakes for speech, at a steady level.
        let mut opened = 0;
        for block in 0..300 {
            let level = if block % 2 == 0 { 0.0040 } else { 0.0044 };
            opened += usize::from(gate.detect(level, 0.004, Some(true), None));
        }
        assert!(opened <= FLOOR_WINDOW_BLOCKS, "open {opened} of 300 fan blocks");
        assert!(!gate.detect(0.0042, 0.004, Some(true), None), "the fan stays shut out");
        assert!(gate.detect(0.02, 0.004, Some(true), None), "a voice over the fan opens it");
        assert!(!gate.detect(0.02, 0.004, Some(false), None), "the detector still has to agree");
        // Manual sensitivity keeps comparing the level with the threshold alone.
        assert!(gate.detect(0.0042, 0.004, None, None));
    }

    #[test]
    fn the_floor_never_shuts_out_speech_in_a_quiet_room_or_at_the_start() {
        let mut gate = NoiseGate::new();
        // Talking from the very first block: no floor has been learned yet.
        for _ in 0..FLOOR_WINDOW_BLOCKS - 1 {
            assert!(gate.detect(0.03, 0.0015, Some(true), None));
        }
        // Speech with short dips between syllables, in a near-silent room.
        let mut gate = NoiseGate::new();
        let mut spoken = 0;
        for block in 0..400 {
            let level = match block % 20 {
                0..=2 => 0.0002,
                3..=5 => 0.004,
                _ => 0.03,
            };
            let speech = level > 0.001;
            let open = gate.detect(level, 0.0015, Some(speech), None);
            if speech && block >= 100 {
                spoken += 1;
                assert!(open, "speech block {block} at {level} was shut out");
            }
        }
        assert!(spoken > 200);
    }

    #[test]
    fn speech_like_level_refuses_a_slowly_rising_fan_and_takes_syllables() {
        let mut gate = NoiseGate::new();
        // A fan that grows 10 dB over 3 s with block-to-block flutter, which a
        // voice detector calls speech: never something to learn a gain from.
        let mut learned = 0;
        for block in 0..300 {
            let level = 0.0003 * 10f32.powf(block as f32 / 300.0) * if block % 2 == 0 { 0.8 } else { 1.2 };
            if block >= FLOOR_WINDOW_BLOCKS {
                learned += usize::from(gate.speech_like_level(level));
            }
            gate.detect(level, 0.0015, Some(true), None);
        }
        assert_eq!(learned, 0, "learned from the fan in {learned} blocks");
        assert!(gate.noise_floor().is_some());
        // Then someone talks over it: syllables rise well above the last 0.3 s.
        let mut syllables = 0;
        for block in 0..100 {
            let level = if block % 10 < 5 { 0.03 } else { 0.003 };
            if level > 0.01 && gate.speech_like_level(level) {
                syllables += 1;
            }
            gate.detect(level, 0.0015, Some(true), None);
        }
        assert!(syllables >= 45, "only {syllables} of 50 syllable blocks were speech-like");
    }

    fn model(kept: f32, voice: f32) -> Option<SpeechModel> {
        Some(SpeechModel { kept, voice: Some(voice) })
    }

    #[test]
    fn level_and_suppressor_must_agree_to_open_and_the_first_word_block_opens() {
        let mut gate = NoiseGate::new();
        // A fan the detector calls speech and that sits above any floor (from
        // the first block on): the suppressor removed it, so the gate stays shut.
        for block in 0..300 {
            let level = 0.004 * if block % 2 == 0 { 1.0 } else { 3.0 };
            assert!(!gate.detect(level, 0.0015, Some(true), model(0.0002, 0.9)), "fan block {block} opened");
        }
        // A word: the very first block the suppressor keeps opens (no added delay).
        assert!(gate.detect(0.03, 0.0015, Some(true), model(0.2, 0.7)));
        // Once open, a soft syllable the suppressor is unsure of keeps it open...
        assert!(gate.detect(0.03, 0.0015, Some(true), model(0.004, 0.05)));
        // ...and a block it removed closes it.
        assert!(!gate.detect(0.03, 0.0015, Some(true), model(0.001, 0.9)));
        // Opening needs the suppressor's speech probability too (keys under RNNoise).
        assert!(!gate.detect(0.03, 0.0015, Some(true), model(0.8, 0.02)));
        assert!(gate.detect(0.03, 0.0015, Some(true), Some(SpeechModel { kept: 0.8, voice: None })));
    }

    #[test]
    fn the_input_sensitivity_threshold_keeps_its_meaning_with_the_speech_model() {
        let mut gate = NoiseGate::new();
        let voice = model(0.9, 0.9);
        // Manual sensitivity: below the slider's threshold nothing opens, however
        // sure the model is; above it, the model agreeing opens it as before.
        assert!(!gate.detect(0.009, 0.01, None, voice));
        assert!(gate.detect(0.011, 0.01, None, voice));
        assert!(gate.detect(0.0051, 0.01, None, voice), "hysteresis as before");
        assert!(!gate.detect(0.0049, 0.01, None, voice));
        // Without suppression there is no model and the gate is exactly the old one.
        let mut old = NoiseGate::new();
        let mut new = NoiseGate::new();
        for block in 0..200 {
            let level = 0.002 + 0.0001 * (block % 37) as f32;
            assert_eq!(old.detect(level, 0.004, None, None), new.detect(level, 0.004, None, None));
        }
    }

    #[test]
    fn noise_at_the_floor_after_a_word_ends_the_hold() {
        let timing = GateTiming { attack_ms: 0, hold_ms: 200, release_ms: 0 };
        let speak = |gate: &mut NoiseGate, after: Option<SpeechModel>| {
            // Learn a floor at 0.001, say a word, then hear the floor again.
            for _ in 0..FLOOR_WINDOW_BLOCKS * 2 {
                let speech = gate.detect(0.001, 0.0015, Some(false), model(0.0001, 0.0));
                gate.apply(&mut [0.1; 480], speech, timing, false);
            }
            let speech = gate.detect(0.05, 0.0015, Some(true), model(0.9, 0.9));
            assert!(gate.apply(&mut [0.1; 480], speech, timing, false));
            (0..30)
                .filter(|_| {
                    let speech = gate.detect(0.001, 0.0015, Some(false), after);
                    gate.apply(&mut [0.1; 480], speech, timing, false)
                })
                .count()
        };
        // The suppressor removed what follows the word: closed at once.
        assert_eq!(speak(&mut NoiseGate::new(), model(0.0001, 0.0)), 0);
        // Without that verdict the 200 ms hold runs as before.
        assert_eq!(speak(&mut NoiseGate::new(), None), 20);
        // A soft tail the suppressor kept is held as before.
        assert_eq!(speak(&mut NoiseGate::new(), model(0.5, 0.3)), 20);
    }

    #[test]
    fn push_to_talk_release_holds_for_the_delay_and_never_beyond_two_seconds() {
        let start = std::time::Instant::now();
        let at = |ms: u64| start + std::time::Duration::from_millis(ms);
        let mut ptt = PushToTalkRelease::default();
        assert!(!ptt.open(false, 200, start), "never pressed");
        assert!(ptt.open(true, 200, start));
        assert!(ptt.open(false, 200, at(199)));
        assert!(!ptt.open(false, 200, at(200)));
        assert!(!ptt.open(false, 200, at(201)), "stays closed");
        assert!(ptt.open(true, 0, at(300)));
        assert!(!ptt.open(false, 0, at(300)), "0 ms closes with the key");
        assert!(ptt.open(true, 60_000, at(400)));
        assert!(ptt.open(false, 60_000, at(2399)));
        assert!(!ptt.open(false, 60_000, at(2400)), "at most 2 s");
    }

    #[test]
    fn suppression_ratio_sets_each_output_against_the_raw_block_it_came_from() {
        let mut ratio = SuppressionRatio::default();
        assert_eq!(SuppressionRatio::lag_blocks(480, 480), 1, "RNNoise: one block late");
        assert_eq!(SuppressionRatio::lag_blocks(2880, 480), 6, "Studio: 60 ms");
        assert_eq!(SuppressionRatio::lag_blocks(335, 480), 1, "WebRTC NS rounds to a block");
        let loud = [0.1f32; 480];
        let quiet = [0.01f32; 480];
        // First block: the suppressor's output belongs to a block not yet heard.
        ratio.raw(&loud);
        ratio.kept(&quiet, 1);
        assert_eq!(ratio.take(480), None);
        // The second output belongs to the first (loud) raw block: -20 dB kept.
        ratio.raw(&quiet);
        ratio.kept(&quiet, 1);
        assert!((ratio.take(480).unwrap() - 0.01).abs() < 1e-4);
        // Now the quiet raw block's output comes back whole.
        ratio.raw(&[0.0; 480]);
        ratio.kept(&quiet, 1);
        assert!((ratio.take(480).unwrap() - 1.0).abs() < 1e-4);
        // Digital silence says nothing.
        ratio.raw(&[0.0; 480]);
        ratio.kept(&[0.0; 480], 1);
        assert_eq!(ratio.take(480), None);
        // A suppressor longer than the history gives no ratio.
        ratio.raw(&loud);
        ratio.kept(&loud, 9);
        assert_eq!(ratio.take(480), None);
    }

    #[test]
    fn meter_reports_the_loudest_level_and_the_threshold_used() {
        let meter = GateMeter::default();
        meter.record(0.01, 0.02, false, false);
        meter.record(0.05, 0.02, true, false);
        meter.record(0.002, 0.004, true, true);
        let reading = meter.take();
        assert_eq!(reading.level, 0.05);
        assert_eq!(reading.threshold, 0.004);
        assert!(reading.open && reading.automatic);
        assert_eq!(reading.blocks, 3);
        assert_eq!(meter.take().level, 0.0, "the maximum resets after a read");
        let mut display = GateDisplay::default();
        let now = std::time::Instant::now();
        let (level, live) = display.update(reading, now);
        assert!(live && level == 0.05);
        let (level, live) = display.update(meter.take(), now + std::time::Duration::from_millis(300));
        assert!(!live && level < 0.05 * 0.4 && level > 0.05 * 0.3);
    }
}

/// Gate figures on the audio bench rows, from the app's own microphone chain
/// (`MicrophoneProcessor`): openings on noise and chopped word starts.
/// `cargo test -p relay-client --release gate_bench -- --ignored --nocapture`
/// with `VOICE_TEST_WAV` set for the recorded voice; `GATE_TRACE_DIR` also
/// writes one CSV per row and chain.
#[cfg(test)]
pub(crate) mod bench {
    use crate::audio_bench::{corpus, CorpusItem, BLOCK, RATE};
    use crate::audio_settings::{AudioSettings, NoiseModel};
    use crate::media::{AudioControls, MicrophoneProcessor};
    use crate::noise_suppressor::SuppressorKind;

    pub(crate) const ROWS: [&str; 4] = ["keyboard_between_words", "fan_plus_mouse", "music_bed_12", "clean_voice"];
    /// A speech block is within 45 dB of the loudest block of the clean voice.
    const SPEECH_BELOW_PEAK: f64 = 3.16e-5;
    /// A word start follows at least 300 ms without speech (hold and release
    /// have closed the gate by then).
    const ONSET_SILENCE_BLOCKS: usize = 30;
    /// A word, not a breath: one of its first 100 ms of blocks comes within
    /// 30 dB of the loudest block.
    const WORD_BLOCKS: usize = 10;
    const WORD_BELOW_PEAK: f64 = 1e-3;
    /// The part of a word start that must get through: its first 50 ms.
    const ONSET_BLOCKS: usize = 5;
    /// Chopped: those 50 ms lose more than 6 dB.
    pub(crate) const CHOPPED_DB: f32 = -6.0;
    /// A start the chain before the gate left more than 20 dB below the voice
    /// (suppressor or click stage) is lost before the gate, not chopped by it.
    const LOST_BEFORE_GATE_DB: f32 = -20.0;
    /// The default microphone gain, 1.5, as power: the voice as it reaches the chain.
    const MIC_GAIN_POWER: f64 = 2.25;

    pub(crate) struct Chain {
        pub name: &'static str,
        pub settings: AudioSettings,
        pub kind: Option<SuppressorKind>,
    }

    pub(crate) fn chains() -> Vec<Chain> {
        let studio = match crate::noise_suppressor::for_setting(NoiseModel::Studio, Default::default()) {
            SuppressorKind::DpdfNet(mut kind) => {
                // Built before the first block and never swapped: deterministic.
                kind.guarded = false;
                SuppressorKind::DpdfNet(kind)
            }
            other => other,
        };
        vec![
            Chain { name: "enhanced, automatic", settings: AudioSettings::default(), kind: None },
            Chain {
                name: "enhanced, manual -40 dB",
                settings: AudioSettings { automatic_sensitivity: Some(false), activation_threshold: 0.01, ..Default::default() },
                kind: None,
            },
            Chain {
                name: "studio, automatic",
                settings: AudioSettings { noise_model: NoiseModel::Studio, ..Default::default() },
                kind: Some(studio),
            },
        ]
    }

    #[derive(Clone, Debug, Default)]
    pub(crate) struct Figures {
        pub seconds: f32,
        pub noise_openings: usize,
        pub onsets: usize,
        pub chopped: usize,
        pub lost_before_gate: usize,
        pub worst_onset_db: f32,
        /// Energy the click stage left of the suppressor's output over the
        /// first 50 ms of word starts, before the gate (dB, mean).
        pub click_stage_at_starts_db: f32,
        /// Speech energy the gate let through, every speech block (dB).
        pub speech_kept_db: f32,
        /// Share of blocks far from any speech in which the gate was open.
        pub open_in_pauses: f32,
        pub cpu_ms_per_block: f32,
    }

    /// One chain over one row, block by block.
    pub(crate) struct Run {
        /// The gate's mean squared gain per block.
        gains: Vec<f32>,
        /// The suppressor's kept ratio per block.
        ratios: Vec<Option<f32>>,
        /// Energy raw, after the suppressor, after click suppression; speech probability.
        stages: Vec<[f32; 4]>,
        /// The suppressor's lag in blocks.
        lag: usize,
        cpu_ms_per_block: f32,
    }

    pub(crate) fn run(chain: &Chain, item: &CorpusItem, ratio: bool) -> Run {
        let controls = AudioControls::new(&chain.settings);
        let mut processor = match chain.kind {
            Some(kind) => MicrophoneProcessor::with_suppressor(kind),
            None => MicrophoneProcessor::new(),
        };
        let kind = chain.kind.unwrap_or_else(|| {
            crate::noise_suppressor::for_setting(chain.settings.noise_model, chain.settings.noise_strength)
        });
        let delay = crate::noise_suppressor::make(kind).delay_samples();
        let lag = super::SuppressionRatio::lag_blocks(delay, BLOCK);
        processor.gate_without_ratio = !ratio;
        let noisy = item.noisy();
        let (mut gains, mut ratios, mut stages) = (Vec::new(), Vec::new(), Vec::new());
        let started = std::time::Instant::now();
        for chunk in noisy.chunks_exact(BLOCK) {
            let mut block = [0.0f32; BLOCK];
            block.copy_from_slice(chunk);
            processor.process(&mut block, &controls);
            let (gain, ratio) = processor.gate_trace();
            gains.push(gain);
            ratios.push(ratio);
            stages.push(processor.trace_energy);
        }
        let cpu_ms_per_block = started.elapsed().as_secs_f32() * 1000.0 / gains.len().max(1) as f32;
        Run { gains, ratios, stages, lag, cpu_ms_per_block }
    }

    /// The gain a call gives each block: the gate's own, plus the speech-start
    /// history, which sends the closed block before an opening, faded in over
    /// the attack time.
    fn as_sent(gains: &[f32], settings: &AudioSettings) -> Vec<f32> {
        let mut sent = gains.to_vec();
        if settings.preserve_speech_start {
            let mut ramp = [1.0f32; BLOCK];
            super::fade_in(&mut ramp, (settings.activation_attack_ms * 48) as usize);
            let preroll = ramp.iter().map(|g| g * g).sum::<f32>() / BLOCK as f32;
            for k in 1..gains.len() {
                if gains[k - 1] == 0.0 && gains[k] > 0.0 {
                    sent[k - 1] = preroll;
                }
            }
        }
        sent
    }

    pub(crate) fn figures(chain: &Chain, item: &CorpusItem, ratio: bool) -> Figures {
        let Run { gains, ratios, stages, lag, cpu_ms_per_block: cpu } = run(chain, item, ratio);
        let sent = as_sent(&gains, &chain.settings);
        let energies: Vec<f64> = item
            .clean
            .chunks_exact(BLOCK)
            .map(|b| b.iter().map(|s| (*s as f64).powi(2)).sum())
            .collect();
        let peak = energies.iter().cloned().fold(0.0, f64::max);
        let speech: Vec<bool> = energies.iter().map(|e| peak > 0.0 && *e >= peak * SPEECH_BELOW_PEAK).collect();
        let blocks = gains.len().min(speech.len() + lag);
        // Gate block k carries clean block k - lag.
        let clean = |k: usize| k.checked_sub(lag).filter(|j| *j < speech.len());
        let speech_near = |k: usize, before: usize, after: usize| {
            clean(k).is_some_and(|j| {
                speech[j.saturating_sub(before)..(j + after + 1).min(speech.len())]
                    .iter()
                    .any(|s| *s)
            })
        };
        let mut figures = Figures {
            seconds: (blocks * BLOCK) as f32 / RATE as f32,
            cpu_ms_per_block: cpu,
            ..Default::default()
        };
        for k in 1..blocks {
            if gains[k - 1] == 0.0 && gains[k] > 0.0 && !speech_near(k, 3, 1) {
                figures.noise_openings += 1;
            }
        }
        let (mut pauses, mut open) = (0usize, 0usize);
        for (k, gain) in gains.iter().enumerate().take(blocks) {
            if clean(k).is_some() && !speech_near(k, 40, 5) {
                pauses += 1;
                open += usize::from(*gain > 0.0);
            }
        }
        figures.open_in_pauses = open as f32 / pauses.max(1) as f32;
        let db = |kept: f64, all: f64| (10.0 * (kept / all.max(1e-30)).log10()) as f32;
        let (mut kept, mut all) = (0.0, 0.0);
        let (mut click_kept, mut click_heard) = (0.0f64, 0.0f64);
        for (j, is_speech) in speech.iter().enumerate() {
            let k = j + lag;
            if !is_speech || k >= blocks {
                continue;
            }
            kept += sent[k] as f64 * energies[j];
            all += energies[j];
            // A word begins with three speech blocks in a row (a lone blip or
            // breath before it does not count) after 300 ms without such a run.
            let run = |i: usize| i + 3 <= speech.len() && speech[i..i + 3].iter().all(|s| *s);
            let ahead = j..(j + WORD_BLOCKS).min(speech.len());
            let starts_word = j >= ONSET_SILENCE_BLOCKS
                && run(j)
                && !(j - ONSET_SILENCE_BLOCKS..j).any(run)
                && energies[ahead].iter().any(|e| *e >= peak * WORD_BELOW_PEAK);
            if starts_word && k + ONSET_BLOCKS <= blocks && j + ONSET_BLOCKS <= speech.len() {
                figures.onsets += 1;
                // What reached the gate (after suppressor and click stage) in
                // the word's first 50 ms, against the voice itself there.
                let stage = |n: usize, at: usize| stages.get(k + n).map_or(0.0, |s| s[at] as f64);
                let reached: f64 = (0..ONSET_BLOCKS).map(|n| stage(n, 2)).sum();
                let voice: f64 = (0..ONSET_BLOCKS).map(|n| energies[j + n]).sum::<f64>() * MIC_GAIN_POWER;
                click_kept += reached;
                click_heard += (0..ONSET_BLOCKS).map(|n| stage(n, 1)).sum::<f64>();
                let before_gate = db(reached, voice);
                let note = |what: &str, value: f32| {
                    if std::env::var_os("GATE_TRACE_DIR").is_some() {
                        println!("{what}: {} {} ratio={ratio} clean block {j}: {value:.1} dB", chain.name, item.name);
                    }
                };
                if before_gate < LOST_BEFORE_GATE_DB {
                    figures.lost_before_gate += 1;
                    note("lost before the gate", before_gate);
                    continue;
                }
                // The gate's own loss: of what it was given, how much it sent.
                let through: f64 = (0..ONSET_BLOCKS).map(|n| sent[k + n] as f64 * stage(n, 2)).sum();
                let onset = db(through, reached);
                figures.chopped += usize::from(onset < CHOPPED_DB);
                if onset < CHOPPED_DB {
                    note("chopped", onset);
                }
                figures.worst_onset_db = figures.worst_onset_db.min(onset);
            }
        }
        figures.speech_kept_db = db(kept, all);
        figures.click_stage_at_starts_db = db(click_kept, click_heard);
        if let Some(dir) = std::env::var_os("GATE_TRACE_DIR") {
            let mut csv = String::from("block,clean_db,speech,gain,ratio,raw_db,suppressed_db,clicks_db,voice\n");
            let level = |e: f32| 10.0 * (e / BLOCK as f32).max(1e-20).log10();
            for k in 0..blocks {
                let (clean_db, is_speech) = clean(k).map_or((-200.0, false), |j| {
                    (10.0 * (energies[j] / BLOCK as f64).max(1e-20).log10(), speech[j])
                });
                let ratio = ratios[k].map_or(String::new(), |r| format!("{r:.4}"));
                let [raw, suppressed, clicks, voice] = stages[k];
                let (raw, suppressed, clicks) = (level(raw), level(suppressed), level(clicks));
                csv += &format!(
                    "{k},{clean_db:.1},{},{:.3},{ratio},{raw:.1},{suppressed:.1},{clicks:.1},{voice:.2}\n",
                    u8::from(is_speech),
                    gains[k]
                );
            }
            let when = if ratio { "after" } else { "before" };
            let name = format!("{}-{}-{when}.csv", item.name, chain.name.replace([',', ' '], "_"));
            std::fs::write(std::path::Path::new(&dir).join(name), csv).expect("write gate trace");
        }
        figures
    }

    /// Every chain and row, without the ratio (the gate as shipped) and with it.
    pub(crate) fn table() -> Vec<(String, &'static str, Figures)> {
        let items = corpus();
        let mut rows = Vec::new();
        for chain in chains() {
            for ratio in [false, true] {
                for name in ROWS {
                    let item = items.iter().find(|item| item.name == name).expect("bench row");
                    let label = format!("{}{}", chain.name, if ratio { " + ratio" } else { "" });
                    rows.push((label, name, figures(&chain, item, ratio)));
                }
            }
        }
        rows
    }

    #[test]
    #[ignore = "bench: run in release with VOICE_TEST_WAV"]
    fn gate_bench() {
        let source = if std::env::var_os("VOICE_TEST_WAV").is_some() { "recorded voice" } else { "synthetic talker" };
        println!("Gate bench source: {source}");
        println!("| chain | row | s | openings on noise | per 30 s | word starts | lost before the gate | chopped by the gate | worst start (dB) | speech kept (dB) | click stage at starts (dB) | open in pauses | ms/block |");
        println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
        for (chain, row, f) in table() {
            println!(
                "| {chain} | {row} | {:.1} | {} | {:.1} | {} | {} | {} | {:.1} | {:.2} | {:.1} | {:.1} % | {:.3} |",
                f.seconds,
                f.noise_openings,
                f.noise_openings as f32 * 30.0 / f.seconds.max(1.0),
                f.onsets,
                f.lost_before_gate,
                f.chopped,
                f.worst_onset_db,
                f.speech_kept_db,
                f.click_stage_at_starts_db,
                f.open_in_pauses * 100.0,
                f.cpu_ms_per_block
            );
        }
    }
}
