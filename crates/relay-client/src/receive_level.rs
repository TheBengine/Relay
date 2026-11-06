//! What you hear: everyone at the same loudness, and optionally the incoming
//! noise removed.
//!
//! **Leveling** ("Make everyone the same volume", on by default) runs per
//! speaker on each decoded 2.5 ms mixer quantum, before per-user volume. It
//! estimates the speaker's loudness as the RMS of their speech over the last
//! 3 s of speech (pauses do not count; from the first word on, all of it so
//! far), and moves a gain toward -20 dBFS: up at most 6 dB/s, down at most
//! 20 dB/s, at most +12 dB. A 400 ms window followed the loudness of each
//! phrase instead of the speaker (`docs/audio-receive-leveling-20261030.md`). The gain is a gain only: no
//! delay. It never lifts what is not speech above how it arrived (pauses and
//! the background in them stay as loud as they came), never moves while the
//! speaker is silent, never pushes a peak above -1 dBFS, and ignores a short
//! loud burst (a laugh, a cough) rather than turning the next words down.
//!
//! **Clean-up** ("Clean up what I hear", off by default) runs Studio's small
//! model (dpdfnet2, `dpdfnet::Studio`, on a thread of its own) on the mixed
//! incoming audio, for people who send without noise suppression. It adds
//! 67.5 ms to what you hear: Studio's 60 ms and three quanta to collect a
//! 10 ms block from the 2.5 ms mixer.
use crate::dpdfnet::{Studio, StudioKind, Variant};
use crate::noise_suppressor::{Suppressor, BLOCK};
use std::collections::HashMap;

/// The playback mixer's quantum: 2.5 ms at 48 kHz.
pub(crate) const QUANTUM: usize = 120;
const DT: f32 = QUANTUM as f32 / 48_000.0;

/// Where leveling puts each speaker's speech (RMS over speech).
pub(crate) const TARGET_DBFS: f32 = -20.0;
pub(crate) const MAX_BOOST_DB: f32 = 12.0;
/// A sender at -2 dBFS still lands on the target.
pub(crate) const MAX_CUT_DB: f32 = -18.0;
pub(crate) const RISE_DB_PER_S: f32 = 6.0;
pub(crate) const FALL_DB_PER_S: f32 = 20.0;
/// The leveler's loudness: speech power over the last 3 s of speech.
const LOUDNESS_S: f32 = 3.0;
/// The gain starts moving after 100 ms of speech.
const FIRST_ESTIMATE: u32 = 40;
/// Speech: the 10 ms level above -55 dBFS and 12 dB above the background.
const SPEECH_FLOOR_DB: f32 = -55.0;
const SPEECH_OVER_BACKGROUND_DB: f32 = 12.0;
/// Only quanta within this of the loudness count toward it: the 10 ms
/// level's decay after a word, and the quietest sounds, would pull it down.
const COUNTED_BELOW_DB: f32 = 20.0;
/// The background estimate drops at once and rises this fast.
const BACKGROUND_RISE_DB_PER_S: f32 = 3.0;
/// The lift stays on this long after speech (word endings, 200 ms) ...
const LIFT_HOLD: u32 = 80;
/// ... then returns to how the audio arrived at this rate.
const LIFT_RELEASE_DB_PER_S: f32 = 120.0;
/// The gain may still settle this long after speech (500 ms); after that it
/// does not move until the speaker talks again.
const ADAPT_HOLD: u32 = 200;
/// Speech this much above the loudness starts a burst: it does not count ...
const BURST_OVER_DB: f32 = 10.0;
/// ... until the level is back within this of the loudness ...
const BURST_END_DB: f32 = 4.0;
/// ... unless bursts add up to this much speech (600 ms) without 400 ms of
/// ordinary speech between them: then it is the speaker's new level.
const BURST_ACCEPT: u32 = 240;
const BURST_FORGET: u32 = 160;
/// No lifted peak goes above -1 dBFS.
pub(crate) const CEILING: f32 = 0.891_250_9;
const PEAK_RELEASE_S: f32 = 0.05;

fn db10(power: f32) -> f32 {
    10.0 * power.max(1e-12).log10()
}

fn gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One speaker's leveler.
pub(crate) struct Level {
    /// Speech power over the last 3 s of speech, and how many speech
    /// quanta it has heard.
    loudness: f32,
    heard: u32,
    /// The leveling gain, and the gain applied (the leveling gain while
    /// talking, never above 0 dB in pauses), both in dB.
    gain_db: f32,
    lift_db: f32,
    /// The linear gain at the end of the last quantum, after the ceiling.
    applied: f32,
    /// Power smoothed over 10 ms, and the background under it.
    short: f32,
    background_db: f32,
    since_speech: u32,
    bursting: bool,
    burst_run: u32,
    ordinary_run: u32,
    peak: f32,
    short_coefficient: f32,
    peak_decay: f32,
}

impl Default for Level {
    fn default() -> Self {
        Self::new()
    }
}

impl Level {
    pub(crate) fn new() -> Self {
        Self {
            loudness: 0.0,
            heard: 0,
            gain_db: 0.0,
            lift_db: 0.0,
            applied: 1.0,
            short: 0.0,
            background_db: f32::INFINITY,
            since_speech: u32::MAX,
            bursting: false,
            burst_run: 0,
            ordinary_run: 0,
            peak: 0.0,
            short_coefficient: 1.0 - (-DT / 0.010).exp(),
            peak_decay: (-DT / PEAK_RELEASE_S).exp(),
        }
    }

    /// The leveling gain now, in dB.
    #[cfg(test)]
    pub(crate) fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// The speaker's speech loudness, dBFS.
    fn loudness_db(&self) -> f32 {
        db10(self.loudness)
    }

    fn learn(&mut self, power: f32) {
        self.heard = self.heard.saturating_add(1);
        self.loudness += (1.0 / self.heard as f32).max(DT / LOUDNESS_S) * (power - self.loudness);
    }

    pub(crate) fn process(&mut self, quantum: &mut [f32; QUANTUM]) {
        let power = quantum.iter().map(|v| v * v).sum::<f32>() / QUANTUM as f32;
        if !power.is_finite() {
            // The output limiter after the mix silences it; keep the state.
            return;
        }
        let peak = quantum.iter().fold(0.0f32, |p, v| p.max(v.abs()));
        self.short += self.short_coefficient * (power - self.short);
        let short_db = db10(self.short);
        self.background_db = if short_db < self.background_db {
            short_db
        } else {
            self.background_db + BACKGROUND_RISE_DB_PER_S * DT
        };
        let estimated = self.heard >= FIRST_ESTIMATE;
        let loudness_db = self.loudness_db();
        if self.bursting && short_db < loudness_db + BURST_END_DB {
            self.bursting = false;
        }
        if short_db > SPEECH_FLOOR_DB && short_db > self.background_db + SPEECH_OVER_BACKGROUND_DB {
            self.since_speech = 0;
            if estimated && !self.bursting && short_db > loudness_db + BURST_OVER_DB {
                // A laugh, a cough, a shout: the peak ceiling handles it.
                self.bursting = true;
            }
            if self.bursting {
                self.burst_run = self.burst_run.saturating_add(1);
                self.ordinary_run = 0;
                if self.burst_run > BURST_ACCEPT {
                    self.learn(power);
                }
            } else {
                self.ordinary_run = self.ordinary_run.saturating_add(1);
                if self.ordinary_run >= BURST_FORGET {
                    self.burst_run = 0;
                }
                let floor = if estimated { loudness_db - COUNTED_BELOW_DB } else { SPEECH_FLOOR_DB };
                if db10(power) > floor.max(SPEECH_FLOOR_DB) {
                    // The few quanta of a burst's rise, before the 10 ms
                    // level shows it, count at most 10 dB over the loudness.
                    let limit = if estimated { self.loudness * 10f32.powf(BURST_OVER_DB / 10.0) } else { f32::MAX };
                    self.learn(power.min(limit));
                }
            }
        } else {
            self.since_speech = self.since_speech.saturating_add(1);
        }
        if estimated && self.since_speech <= ADAPT_HOLD {
            let wanted = (TARGET_DBFS - self.loudness_db()).clamp(MAX_CUT_DB, MAX_BOOST_DB);
            self.gain_db += (wanted - self.gain_db).clamp(-FALL_DB_PER_S * DT, RISE_DB_PER_S * DT);
        }
        let lift = if self.since_speech <= LIFT_HOLD { self.gain_db } else { self.gain_db.min(0.0) };
        self.lift_db = if lift >= self.lift_db {
            lift
        } else {
            (self.lift_db - LIFT_RELEASE_DB_PER_S * DT).max(lift)
        };
        // A lift never takes a peak above the ceiling (and the ceiling never
        // takes what arrived below 0 dB). Both ends of the ramp are under the
        // ceiling for this quantum's peak, so every sample between is too.
        self.peak = peak.max(self.peak * self.peak_decay);
        let mut end = gain(self.lift_db);
        if end > 1.0 {
            end = end.min((CEILING / self.peak.max(1e-9)).max(1.0));
        }
        let mut start = self.applied;
        if start > 1.0 && peak * start > CEILING {
            start = (CEILING / peak).max(1.0);
        }
        if start != 1.0 || end != 1.0 {
            for (n, sample) in quantum.iter_mut().enumerate() {
                *sample *= start + (end - start) * (n + 1) as f32 / QUANTUM as f32;
            }
        }
        self.applied = end;
    }
}

/// Leveling for everyone in the call, by mixer stream (a person's whisper
/// stream is leveled on its own).
#[derive(Default)]
pub(crate) struct Leveler {
    speakers: HashMap<String, Level>,
}

impl Leveler {
    pub(crate) fn process(&mut self, speaker: &str, quantum: &mut [f32; QUANTUM]) {
        match self.speakers.get_mut(speaker) {
            Some(level) => level.process(quantum),
            None => self.speakers.entry(speaker.to_owned()).or_default().process(quantum),
        }
    }

    /// Forget whoever left.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.speakers.retain(|speaker, _| keep(speaker));
    }

    pub(crate) fn clear(&mut self) {
        self.speakers.clear();
    }

    #[cfg(test)]
    fn level(&self, speaker: &str) -> Option<&Level> {
        self.speakers.get(speaker)
    }
}

// ---------------------------------------------------------------- clean-up

const QUANTA_PER_BLOCK: usize = BLOCK / QUANTUM;
/// Studio's 60 ms and the three quanta that wait for a whole 10 ms block.
pub(crate) const CLEANUP_DELAY_SAMPLES: usize = crate::dpdfnet::DELAY_SAMPLES + (QUANTA_PER_BLOCK - 1) * QUANTUM;
pub(crate) const CLEANUP_DELAY_MS: f32 = CLEANUP_DELAY_SAMPLES as f32 / 48.0;
/// After the last incoming quantum, the mixer keeps playing until it is out.
const TAIL_QUANTA: usize = CLEANUP_DELAY_SAMPLES / QUANTUM + 1;

/// dpdfnet2 on the mixed incoming audio, through Studio's hand-off: the
/// model runs on its own thread, the mixer never waits for it, and a block
/// the model has not finished in time plays unprocessed at the same delay.
pub(crate) struct Cleanup {
    model: Box<dyn Suppressor>,
    input: [f32; BLOCK],
    output: [f32; BLOCK],
    slot: usize,
    tail: usize,
}

impl Cleanup {
    /// The app's: dpdfnet2 with Studio's overrun guard on its own thread.
    pub(crate) fn new() -> Self {
        Self::with(Box::new(Studio::new(StudioKind { variant: Variant::Two, probability: false, guarded: true, keys_before: None, keys_after: None })))
    }

    fn with(model: Box<dyn Suppressor>) -> Self {
        Self { model, input: [0.0; BLOCK], output: [0.0; BLOCK], slot: 0, tail: 0 }
    }

    /// One mixed quantum in, one out `CLEANUP_DELAY_SAMPLES` later. `active`:
    /// someone was heard in this quantum. Returns whether the quantum out
    /// should play (someone is heard, or what was heard is still coming out).
    pub(crate) fn process(&mut self, quantum: &mut [f32; QUANTUM], active: bool) -> bool {
        let at = self.slot * QUANTUM;
        self.input[at..at + QUANTUM].copy_from_slice(quantum);
        if self.slot == QUANTA_PER_BLOCK - 1 {
            // The block is whole: hand it over and play the block before it,
            // starting now.
            self.output = self.input;
            self.model.process(&mut self.output);
        }
        let out = (self.slot + 1) % QUANTA_PER_BLOCK * QUANTUM;
        quantum.copy_from_slice(&self.output[out..out + QUANTUM]);
        self.slot = (self.slot + 1) % QUANTA_PER_BLOCK;
        self.tail = if active { TAIL_QUANTA } else { self.tail.saturating_sub(1) };
        self.tail > 0
    }
}

/// The receive side of the voice worker's mixer.
#[derive(Default)]
pub(crate) struct Receive {
    leveling: bool,
    leveler: Leveler,
    cleanup: Option<Cleanup>,
}

impl Receive {
    /// The two settings, read once per mixer quantum. Turning leveling off
    /// forgets every speaker; turning the clean-up on starts its thread.
    pub(crate) fn configure(&mut self, leveling: bool, cleanup: bool) {
        if self.leveling && !leveling {
            self.leveler.clear();
        }
        self.leveling = leveling;
        match (cleanup, self.cleanup.is_some()) {
            (true, false) => self.cleanup = Some(Cleanup::new()),
            (false, true) => self.cleanup = None,
            _ => {}
        }
    }

    /// One speaker's decoded quantum, before their volume.
    pub(crate) fn level(&mut self, speaker: &str, quantum: &mut [f32; QUANTUM]) {
        if self.leveling {
            self.leveler.process(speaker, quantum);
        }
    }

    /// The mixed quantum; returns whether it plays.
    pub(crate) fn finish(&mut self, mixed: &mut [f32; QUANTUM], active: bool) -> bool {
        match self.cleanup.as_mut() {
            Some(cleanup) => cleanup.process(mixed, active),
            None => active,
        }
    }

    pub(crate) fn retain(&mut self, keep: impl FnMut(&str) -> bool) {
        self.leveler.retain(keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_test_signals::talker;

    const RATE: usize = 48_000;
    const TEN_MS: usize = 480;

    /// Speech level the way the bench measures it: 10 ms blocks within 30 dB
    /// of the loudest block of the reference, power averaged over them.
    fn speech_db(signal: &[f32], reference: &[f32], from: usize, to: usize) -> f32 {
        let power = |s: &[f32]| s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32;
        let loudest = reference.chunks_exact(TEN_MS).map(power).fold(0.0f32, f32::max);
        let (mut sum, mut count) = (0.0f64, 0usize);
        for start in (from..to.min(signal.len()) - TEN_MS + 1).step_by(TEN_MS) {
            if power(&reference[start..start + TEN_MS]) > loudest * 1e-3 {
                sum += power(&signal[start..start + TEN_MS]) as f64;
                count += 1;
            }
        }
        db10((sum / count.max(1) as f64) as f32)
    }

    fn rms_db(signal: &[f32]) -> f32 {
        db10(signal.iter().map(|v| v * v).sum::<f32>() / signal.len().max(1) as f32)
    }

    /// A talker whose speech is at `level` dBFS.
    fn speaker(seconds: f32, seed: u32, level: f32) -> Vec<f32> {
        let mut speech = talker(seconds, (105.0, 135.0), seed, 0.3);
        let scale = gain(level - speech_db(&speech, &speech.clone(), 0, speech.len()));
        speech.iter_mut().for_each(|v| *v *= scale);
        speech
    }

    /// Through one leveler, quantum by quantum; the leveling gain per quantum.
    fn through_leveler(input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut leveler = Level::new();
        let mut out = Vec::with_capacity(input.len());
        let mut gains = Vec::with_capacity(input.len() / QUANTUM);
        for chunk in input.chunks_exact(QUANTUM) {
            let mut quantum: [f32; QUANTUM] = chunk.try_into().unwrap();
            leveler.process(&mut quantum);
            out.extend_from_slice(&quantum);
            gains.push(leveler.gain_db());
        }
        (out, gains)
    }

    fn noise(len: usize, level: f32, seed: u32) -> Vec<f32> {
        let mut state = seed;
        let mut out: Vec<f32> = (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / 8_388_607.5 - 1.0
            })
            .collect();
        let scale = gain(level - rms_db(&out));
        out.iter_mut().for_each(|v| *v *= scale);
        out
    }

    /// When the gain first stays within `tolerance` of the gain that puts a
    /// speaker at `level` on the target, in seconds.
    fn settled_after(gains: &[f32], level: f32, tolerance: f32) -> f32 {
        let ideal = (TARGET_DBFS - level).clamp(MAX_CUT_DB, MAX_BOOST_DB);
        let last_off = gains.iter().rposition(|g| (g - ideal).abs() > tolerance);
        last_off.map_or(0, |index| index + 1) as f32 * DT
    }

    #[test]
    fn a_quiet_and_a_loud_speaker_land_within_2_db_of_the_target_within_3_s() {
        for (level, seed) in [(-32.0, 13), (-12.0, 29)] {
            let input = speaker(8.0, seed, level);
            // The -12 dBFS talker peaks at +0.9 dBFS; Opus's float decoder
            // passes that unclamped.
            let peak = input.iter().fold(0.0f32, |p, v| p.max(v.abs()));
            let (output, gains) = through_leveler(&input);
            let after_3s = speech_db(&output, &input, 3 * RATE, output.len());
            let settled = settled_after(&gains, level, 2.0);
            let out_peak = output.iter().fold(0.0f32, |p, v| p.max(v.abs()));
            eprintln!(
                "speaker at {level} dBFS: speech after 3 s {after_3s:.2} dBFS, within ±2 dB from {settled:.1} s, \
                 within ±1 dB from {:.1} s, final gain {:+.2} dB, output peak {:.2} dBFS",
                settled_after(&gains, level, 1.0),
                gains.last().unwrap(),
                20.0 * out_peak.log10()
            );
            assert!((after_3s - TARGET_DBFS).abs() <= 2.0, "{level} dBFS speaker came out at {after_3s:.2} dBFS");
            assert!(settled <= 3.0, "{level} dBFS speaker settled only after {settled} s");
            assert!(out_peak <= CEILING + 1e-6 || out_peak <= peak, "a lift went over the ceiling: {out_peak}");
        }
    }

    #[test]
    fn the_gain_moves_no_faster_than_6_db_up_and_20_db_down_per_second() {
        for level in [-40.0, -5.0] {
            let (_, gains) = through_leveler(&speaker(4.0, 7, level));
            let per_second = 1.0 / DT;
            for pair in gains.windows(2) {
                let rate = (pair[1] - pair[0]) * per_second;
                assert!((-FALL_DB_PER_S - 1e-3..=RISE_DB_PER_S + 1e-3).contains(&rate), "{rate} dB/s");
            }
            let last = *gains.last().unwrap();
            assert!((MAX_CUT_DB..=MAX_BOOST_DB).contains(&last), "{last}");
        }
        // Far too quiet: +12 dB and no more.
        let (_, gains) = through_leveler(&speaker(6.0, 7, -45.0));
        assert!((gains.last().unwrap() - MAX_BOOST_DB).abs() < 1e-4, "{:?}", gains.last());
    }

    #[test]
    fn silence_between_words_is_not_lifted_and_a_silent_speaker_keeps_the_gain() {
        // A quiet speaker over a -60 dBFS background, leveled +12 dB, then
        // three seconds of the background alone.
        let words = speaker(5.0, 13, -32.0);
        let background = noise(words.len() + 3 * RATE, -60.0, 5);
        let mut input = background.clone();
        input.iter_mut().zip(&words).for_each(|(v, w)| *v += w);
        let (output, gains) = through_leveler(&input);
        let gain_at_end_of_words = gains[words.len() / QUANTUM - 1];
        assert!(gain_at_end_of_words > 10.0, "{gain_at_end_of_words}");
        // The pauses between words after the first 2 s: past the 200 ms the
        // lift stays on and the 100 ms it takes to return to 0 dB.
        let mut lifted: Vec<f32> = Vec::new();
        let mut index = 2 * RATE;
        while index < words.len() {
            if words[index] != 0.0 {
                index += 1;
                continue;
            }
            let end = (index..words.len()).find(|&n| words[n] != 0.0).unwrap_or(words.len());
            let quiet_from = index + RATE * 3 / 10;
            if end > quiet_from + RATE / 20 {
                lifted.push(rms_db(&output[quiet_from..end]) - rms_db(&input[quiet_from..end]));
            }
            index = end;
        }
        assert!(!lifted.is_empty());
        let most = lifted.iter().copied().fold(f32::MIN, f32::max);
        // The background alone, after the same 300 ms.
        let tail = words.len() + RATE * 3 / 10;
        let background_lift = rms_db(&output[tail..]) - rms_db(&input[tail..]);
        let gain_after_silence = *gains.last().unwrap();
        eprintln!(
            "pauses between words (n = {}): lifted at most {most:+.2} dB; background alone {background_lift:+.2} dB; \
             gain {gain_at_end_of_words:+.2} dB before 3 s of silence, {gain_after_silence:+.2} dB after",
            lifted.len()
        );
        assert!(most <= 0.5, "pauses lifted {most:+.2} dB");
        assert!(background_lift <= 0.5, "background lifted {background_lift:+.2} dB");
        assert!((gain_after_silence - gain_at_end_of_words).abs() < 1e-6, "the gain moved during silence");
    }

    #[test]
    fn a_loud_burst_does_not_pump_the_following_words() {
        // Leveled speech at -26 dBFS (+6 dB), a 300 ms burst 12 dB louder
        // at 4 s, then the same speaker as before.
        let mut input = speaker(8.0, 13, -26.0);
        let reference = input.clone();
        let (burst_from, burst_to) = (4 * RATE, 4 * RATE + RATE * 3 / 10);
        let loud = speaker(0.3, 31, -14.0);
        input[burst_from..burst_to].iter_mut().zip(&loud).for_each(|(v, l)| *v += l);
        let (output, gains) = through_leveler(&input);
        let before = speech_db(&output, &reference, 2 * RATE + RATE / 2, burst_from);
        let after = speech_db(&output, &reference, burst_to + RATE * 15 / 100, burst_to + RATE * 165 / 100);
        let gain_before = gains[burst_from / QUANTUM - 1];
        let lowest = gains[burst_from / QUANTUM..(burst_to + RATE * 165 / 100) / QUANTUM]
            .iter()
            .copied()
            .fold(f32::MAX, f32::min);
        let burst_peak = output[burst_from..burst_to].iter().fold(0.0f32, |p, v| p.max(v.abs()));
        eprintln!(
            "burst: words {before:.2} dBFS before, {after:.2} dBFS after ({:+.2} dB); leveling gain {gain_before:+.2} dB \
             before, lowest {lowest:+.2} dB in the 1.95 s from the burst; burst peak out {:.2} dBFS",
            after - before,
            20.0 * burst_peak.log10()
        );
        assert!((after - before).abs() <= 1.0, "words after the burst moved {:+.2} dB", after - before);
        assert!(gain_before - lowest <= 1.0, "the leveling gain dipped {:.2} dB", gain_before - lowest);
        assert!(burst_peak <= CEILING + 1e-6, "the burst went over the ceiling: {burst_peak}");
    }

    #[test]
    fn a_lift_never_takes_a_peak_over_minus_1_dbfs_and_never_limits_what_arrived() {
        // A quiet speaker (lifted +12 dB) with a sharp loud consonant.
        let mut input = speaker(5.0, 13, -32.0);
        for (n, v) in input[4 * RATE..4 * RATE + 96].iter_mut().enumerate() {
            *v += if n % 2 == 0 { 0.5 } else { -0.5 };
        }
        let (output, _) = through_leveler(&input);
        let peak = output.iter().fold(0.0f32, |p, v| p.max(v.abs()));
        assert!(peak <= CEILING + 1e-6, "{peak}");
        // A loud speaker arriving above the ceiling is turned down, not limited.
        let loud: Vec<f32> = speaker(3.0, 13, -12.0).iter().map(|v| v * 1.5).collect();
        let (output, _) = through_leveler(&loud);
        for (o, i) in output.iter().zip(&loud) {
            assert!(o.abs() <= i.abs() + 1e-6);
        }
    }

    #[test]
    fn noise_alone_is_never_taken_for_speech() {
        // A sender who transmits only a fan (no suppression, no gate).
        let fan = noise(5 * RATE, -40.0, 9);
        let (output, gains) = through_leveler(&fan);
        assert!(gains.iter().all(|g| *g == 0.0), "{:?}", gains.iter().copied().fold(0.0f32, f32::max));
        assert_eq!(output, fan);
    }

    #[test]
    fn each_speaker_has_their_own_gain_and_leaving_forgets_it() {
        let quiet = speaker(4.0, 13, -32.0);
        let loud = speaker(4.0, 29, -12.0);
        let mut leveler = Leveler::default();
        for (q, l) in quiet.chunks_exact(QUANTUM).zip(loud.chunks_exact(QUANTUM)) {
            let mut a: [f32; QUANTUM] = q.try_into().unwrap();
            let mut b: [f32; QUANTUM] = l.try_into().unwrap();
            leveler.process("quiet", &mut a);
            leveler.process("loud", &mut b);
        }
        assert!(leveler.level("quiet").unwrap().gain_db() > 10.0);
        assert!(leveler.level("loud").unwrap().gain_db() < -6.0);
        leveler.retain(|speaker| speaker != "quiet");
        assert!(leveler.level("quiet").is_none());
        assert!(leveler.level("loud").is_some());
    }

    #[test]
    fn turning_leveling_off_leaves_audio_untouched_and_forgets_the_gains() {
        let quiet = speaker(3.0, 13, -32.0);
        let mut receive = Receive::default();
        receive.configure(true, false);
        for chunk in quiet.chunks_exact(QUANTUM) {
            let mut q: [f32; QUANTUM] = chunk.try_into().unwrap();
            receive.level("a", &mut q);
        }
        assert!(receive.leveler.level("a").is_some());
        receive.configure(false, false);
        assert!(receive.leveler.level("a").is_none());
        let mut q: [f32; QUANTUM] = quiet[RATE..RATE + QUANTUM].try_into().unwrap();
        let original = q;
        receive.level("a", &mut q);
        let mut mixed = q;
        assert!(receive.finish(&mut mixed, true));
        assert_eq!(mixed, original);
        assert!(!receive.finish(&mut mixed, false));
    }

    /// The clean-up with the model on this thread (deterministic), as the
    /// bench runs Studio.
    fn inline_cleanup() -> Cleanup {
        Cleanup::with(Box::new(Studio::new(StudioKind { variant: Variant::Two, probability: false, guarded: false, keys_before: None, keys_after: None })))
    }

    fn clean_up(cleanup: &mut Cleanup, input: &[f32]) -> (Vec<f32>, Vec<bool>) {
        let mut out = Vec::with_capacity(input.len());
        let mut playing = Vec::new();
        for chunk in input.chunks_exact(QUANTUM) {
            let mut quantum: [f32; QUANTUM] = chunk.try_into().unwrap();
            let active = quantum.iter().any(|v| *v != 0.0);
            playing.push(cleanup.process(&mut quantum, active));
            out.extend_from_slice(&quantum);
        }
        (out, playing)
    }

    /// Studio's timing without the model: its 60 ms as a plain delay line.
    struct DelayLine(std::collections::VecDeque<f32>);
    impl Suppressor for DelayLine {
        fn process(&mut self, block: &mut [f32; BLOCK]) {
            self.0.extend(block.iter());
            for sample in block.iter_mut() {
                *sample = self.0.pop_front().unwrap();
            }
        }
        fn delay_samples(&self) -> usize {
            crate::dpdfnet::DELAY_SAMPLES
        }
        fn name(&self) -> &'static str {
            "delay line"
        }
        fn voice_probability(&self) -> Option<f32> {
            None
        }
    }

    #[test]
    fn the_cleanup_plays_67_5_ms_late_and_plays_out_its_tail() {
        assert_eq!(CLEANUP_DELAY_SAMPLES, 3_240);
        assert_eq!(CLEANUP_DELAY_MS, 67.5);
        let mut input = speaker(2.0, 13, -20.0);
        input.extend(vec![0.0; RATE / 2]);
        // The re-blocking, exactly: Studio's 60 ms plus three quanta.
        let line = DelayLine(std::iter::repeat_n(0.0, crate::dpdfnet::DELAY_SAMPLES).collect());
        let (exact, _) = clean_up(&mut Cleanup::with(Box::new(line)), &input);
        assert_eq!(&exact[CLEANUP_DELAY_SAMPLES..], &input[..input.len() - CLEANUP_DELAY_SAMPLES]);
        assert!(exact[..CLEANUP_DELAY_SAMPLES].iter().all(|v| *v == 0.0));
        // The model: its output lines up there too.
        let (output, playing) = clean_up(&mut inline_cleanup(), &input);
        let search = crate::audio_bench::DELAY_SEARCH_SAMPLES;
        let lag = crate::audio_bench::measure_delay_between(
            &input,
            &output,
            CLEANUP_DELAY_SAMPLES - search,
            CLEANUP_DELAY_SAMPLES + search,
        );
        assert_eq!(lag, CLEANUP_DELAY_SAMPLES);
        // Plays while someone is heard and until the last of it is out.
        let heard: Vec<bool> = input.chunks_exact(QUANTUM).map(|q| q.iter().any(|v| *v != 0.0)).collect();
        let delay = CLEANUP_DELAY_SAMPLES / QUANTUM;
        for (index, played) in playing.iter().enumerate() {
            let expected = heard[index.saturating_sub(delay)..=index].iter().any(|h| *h);
            assert_eq!(*played, expected, "quantum {index}");
        }
        let last_heard = heard.iter().rposition(|h| *h).unwrap();
        assert_eq!(playing.iter().rposition(|p| *p).unwrap(), last_heard + delay);
    }

    fn percentiles(mut spent: Vec<f64>) -> (f64, f64, f64) {
        spent.sort_by(f64::total_cmp);
        let at = |fraction: f64| spent[((spent.len() as f64 * fraction).ceil() as usize).clamp(1, spent.len()) - 1];
        (at(0.5), at(0.99), *spent.last().unwrap())
    }

    /// Opt-in, release: what leveling costs the voice worker per 10 ms of
    /// playback (four mixer quanta) for 1, 8 and 32 people talking at once.
    #[test]
    #[ignore = "measurement; run in release with --nocapture"]
    fn leveling_cpu_per_block() {
        let speech: Vec<Vec<f32>> = (0..32).map(|n| speaker(20.0, 13 + n, -40.0 + n as f32)).collect();
        for people in [1usize, 8, 32] {
            let mut leveler = Leveler::default();
            let names: Vec<String> = (0..people).map(|n| format!("person-{n}")).collect();
            let mut spent = Vec::new();
            for block in 0..speech[0].len() / TEN_MS {
                let started = std::time::Instant::now();
                for quantum in 0..QUANTA_PER_BLOCK {
                    let at = block * TEN_MS + quantum * QUANTUM;
                    for (name, talk) in names.iter().zip(&speech) {
                        let mut q: [f32; QUANTUM] = talk[at..at + QUANTUM].try_into().unwrap();
                        leveler.process(name, &mut q);
                        std::hint::black_box(&q);
                    }
                }
                spent.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            let (p50, p99, max) = percentiles(spent);
            eprintln!("leveling, {people} people: per 10 ms block p50 {p50:.4} ms, p99 {p99:.4} ms, max {max:.4} ms");
        }
    }

    /// Opt-in: the bench rows as an incoming speaker who sends without noise
    /// suppression, through the clean-up (model inline, as the bench runs
    /// Studio). `VOICE_TEST_WAV` selects the recorded voice. Prints a
    /// Markdown table and the model's time per block.
    #[test]
    #[ignore = "measurement; run in release with --nocapture"]
    fn cleanup_on_the_bench_rows() {
        use crate::audio_bench::{corpus, metrics, speech_noise_leak_db};
        let fmt = |v: f32| if v.is_nan() { "n/a".to_owned() } else { format!("{v:.1}") };
        println!("| row | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) |");
        println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
        let mut spent = Vec::new();
        for item in corpus() {
            let noisy = item.noisy();
            let run = |input: &[f32], spent: &mut Vec<f64>| {
                let mut cleanup = inline_cleanup();
                let mut out = Vec::with_capacity(input.len() + CLEANUP_DELAY_SAMPLES);
                let mut padded = input.to_vec();
                padded.resize(input.len().div_ceil(TEN_MS) * TEN_MS + CLEANUP_DELAY_SAMPLES + TEN_MS, 0.0);
                for chunk in padded.chunks_exact(QUANTUM) {
                    let mut q: [f32; QUANTUM] = chunk.try_into().unwrap();
                    let started = std::time::Instant::now();
                    cleanup.process(&mut q, true);
                    if cleanup.slot == 0 {
                        spent.push(started.elapsed().as_secs_f64() * 1000.0);
                    }
                    out.extend_from_slice(&q);
                }
                out
            };
            let out_noisy = run(&noisy, &mut spent);
            let out_clean = run(&item.clean, &mut Vec::new());
            let m = metrics(&item.clean, &noisy, &out_noisy, CLEANUP_DELAY_SAMPLES);
            let leak = speech_noise_leak_db(&item.clean, &noisy, &out_noisy, &out_clean, CLEANUP_DELAY_SAMPLES);
            println!(
                "| {} | {} | {} | {} | {} | {} | {} |",
                item.name,
                fmt(item.snr_db),
                fmt(m.residual_noise_db),
                fmt(leak),
                fmt(m.speech_segsnr_db),
                fmt(m.lsd_db),
                fmt(m.speech_attenuation_db)
            );
        }
        let (p50, p99, max) = percentiles(spent);
        println!("model (dpdfnet2, inline) per 10 ms block: p50 {p50:.3} ms, p99 {p99:.3} ms, max {max:.3} ms");
    }

    /// Opt-in, release: the app's clean-up (model on its own thread), fed one
    /// mixer quantum every 2.5 ms for 20 s; the time the voice worker spends
    /// per 10 ms. Studio logs, when it is dropped, how many blocks played
    /// unprocessed because the model was late.
    #[test]
    #[ignore = "measurement; run in release with --nocapture"]
    fn paced_cleanup_in_real_time() {
        crate::dpdfnet::load_blocking(Variant::Two).unwrap();
        let speech = speaker(20.0, 13, -20.0);
        let mut cleanup = Cleanup::new();
        let mut spent = Vec::new();
        let mut block = 0.0f64;
        let started = std::time::Instant::now();
        for (index, chunk) in speech.chunks_exact(QUANTUM).enumerate() {
            let due = started + std::time::Duration::from_micros(2_500 * index as u64);
            while std::time::Instant::now() < due {
                std::hint::spin_loop();
            }
            let mut q: [f32; QUANTUM] = chunk.try_into().unwrap();
            let at = std::time::Instant::now();
            cleanup.process(&mut q, true);
            block += at.elapsed().as_secs_f64() * 1000.0;
            if cleanup.slot == 0 {
                spent.push(block);
                block = 0.0;
            }
        }
        let (p50, p99, max) = percentiles(spent);
        eprintln!("clean-up on the voice worker per 10 ms block: p50 {p50:.4} ms, p99 {p99:.4} ms, max {max:.4} ms");
        drop(cleanup);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
