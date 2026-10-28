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
        }
    }

    /// Speech decision for one block. A manual threshold opens at `threshold`
    /// and, once open, stays open down to `threshold * CLOSE_RATIO`. An external
    /// detector (automatic sensitivity) replaces the comparison, but only for
    /// blocks that also stand clear of the recent noise floor.
    pub(crate) fn detect(&mut self, level: f32, threshold: f32, detected: Option<bool>) -> bool {
        let speech = match detected {
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
        let target = if speech {
            self.hold_left = timing.hold_ms.saturating_mul(SAMPLES_PER_MS);
            1.0
        } else if self.hold_left > 0 {
            self.hold_left = self.hold_left.saturating_sub(samples);
            1.0
        } else {
            0.0
        };
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
        }
        true
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
        assert!(!gate.detect(0.009, 0.01, None), "below the open threshold");
        assert!(gate.detect(0.011, 0.01, None));
        // Once open, the level may sink 6 dB before the gate starts to close.
        assert!(gate.detect(0.0075, 0.01, None));
        assert!(gate.detect(0.0051, 0.01, None));
        assert!(!gate.detect(0.0049, 0.01, None));
        assert!(!gate.detect(0.0075, 0.01, None), "closed again: needs the full threshold");
        assert!(gate.detect(0.0, 0.01, Some(true)), "an external detector decides");
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
            opened += usize::from(gate.detect(level, 0.004, Some(true)));
        }
        assert!(opened <= FLOOR_WINDOW_BLOCKS, "open {opened} of 300 fan blocks");
        assert!(!gate.detect(0.0042, 0.004, Some(true)), "the fan stays shut out");
        assert!(gate.detect(0.02, 0.004, Some(true)), "a voice over the fan opens it");
        assert!(!gate.detect(0.02, 0.004, Some(false)), "the detector still has to agree");
        // Manual sensitivity keeps comparing the level with the threshold alone.
        assert!(gate.detect(0.0042, 0.004, None));
    }

    #[test]
    fn the_floor_never_shuts_out_speech_in_a_quiet_room_or_at_the_start() {
        let mut gate = NoiseGate::new();
        // Talking from the very first block: no floor has been learned yet.
        for _ in 0..FLOOR_WINDOW_BLOCKS - 1 {
            assert!(gate.detect(0.03, 0.0015, Some(true)));
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
            let open = gate.detect(level, 0.0015, Some(speech));
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
            gate.detect(level, 0.0015, Some(true));
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
            gate.detect(level, 0.0015, Some(true));
        }
        assert!(syllables >= 45, "only {syllables} of 50 syllable blocks were speech-like");
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
