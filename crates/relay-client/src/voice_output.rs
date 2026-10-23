//! Bounded playback clock matching; voice and application audio use separate clocks.
use crate::audio_resample::{SincFilter, TAPS};
use relay_protocol::AUDIO_SAMPLE_RATE;
#[cfg(test)]
use std::collections::VecDeque;
pub(crate) const FILTER_DELAY_MS: f32 = (TAPS / 2) as f32 * 1000.0 / AUDIO_SAMPLE_RATE as f32;
pub(crate) const DEFAULT_RESERVE_SAMPLES: usize = AUDIO_SAMPLE_RATE as usize / 200;
pub(crate) fn reserve_samples(period_ms: Option<f32>) -> usize {
    period_ms
        .filter(|ms| ms.is_finite() && *ms > 0.0)
        .map_or(DEFAULT_RESERVE_SAMPLES, |ms| {
            (ms * 48.0).round().clamp(120.0, 240.0) as usize
        })
}

pub(crate) type VoiceOutput = PlaybackOutput<1>;

/// A single clock consumes complete frames; channels never drift independently.
pub(crate) struct PlaybackOutput<const CHANNELS: usize> {
    nominal_step: f64,
    step: f64,
    phase: f64,
    correction: f64,
    primed: bool,
    waited_samples: usize,
    ready: bool,
    max_samples: usize,
    reserve_samples: usize,
    filters: [SincFilter; CHANNELS],
    tail: usize,
    application_dc_tail: [Option<f32>; CHANNELS],
}

#[derive(Clone, Copy)]
pub(crate) struct VoiceSample {
    pub value: f32,
    pub available: bool,
}

pub(crate) struct PlaybackFrame<const CHANNELS: usize> {
    pub values: [f32; CHANNELS],
    pub available: bool,
}

impl<const CHANNELS: usize> PlaybackOutput<CHANNELS> {
    /// Constructed before the device callback starts: no table allocation or
    /// kernel generation happens on the real-time render thread.
    pub(crate) fn new(native_rate: u32, max_samples: usize, reserve_samples: usize) -> Self {
        assert!(native_rate > 0 && CHANNELS > 0);
        let nominal_step = f64::from(AUDIO_SAMPLE_RATE) / f64::from(native_rate);
        Self {
            nominal_step,
            step: nominal_step,
            phase: 1.0,
            correction: 0.0,
            primed: false,
            waited_samples: 0,
            ready: false,
            max_samples,
            reserve_samples: reserve_samples.clamp(120, DEFAULT_RESERVE_SAMPLES),
            filters: std::array::from_fn(|_| SincFilter::new((1.0 / nominal_step).min(1.0))),
            tail: 0,
            application_dc_tail: [None; CHANNELS],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.step = self.nominal_step;
        self.phase = 1.0;
        self.correction = 0.0;
        self.primed = false;
        self.waited_samples = 0;
        self.ready = false;
        for filter in &mut self.filters {
            filter.reset();
        }
        self.tail = 0;
        self.application_dc_tail = [None; CHANNELS];
    }

    pub(crate) fn source_samples(&self, output_frames: usize) -> usize {
        (output_frames as f64 * self.nominal_step).ceil() as usize
    }

    pub(crate) fn required_capacity(&self, output_frames: usize) -> usize {
        self.source_samples(output_frames)
            .saturating_add(self.reserve_samples)
    }

    pub(crate) fn set_reserve_samples(&mut self, reserve: usize) {
        self.reserve_samples = reserve.clamp(120, DEFAULT_RESERVE_SAMPLES);
    }

    pub(crate) fn set_max_samples(&mut self, max_samples: usize) {
        self.max_samples = max_samples;
    }

    pub(crate) fn begin_block(&mut self, available: Option<usize>, output_frames: usize) {
        self.begin_with_headroom(available, output_frames, self.reserve_samples);
    }

    /// The decoded application queue already applies its bounded startup wait.
    /// Reuse the controller and filter without adding a second priming delay.
    pub(crate) fn begin_prepared_block(
        &mut self,
        available: Option<usize>,
        output_frames: usize,
        headroom: usize,
    ) {
        self.primed = available.is_some_and(|samples| samples > 0);
        self.begin_with_headroom(available, output_frames, headroom);
    }

    fn begin_with_headroom(
        &mut self,
        available: Option<usize>,
        output_frames: usize,
        headroom: usize,
    ) {
        let demand = self.source_samples(output_frames);
        let target = demand.saturating_add(headroom);
        let available = available.unwrap_or(0);
        // A callback exceeding the device queue's hard bound still plays available
        // audio; it must not wait for a target that can never be filled.
        if target > self.max_samples {
            self.correction = 0.0;
            self.step = self.nominal_step;
            self.ready = available > 0;
            self.primed = self.ready;
            return;
        }
        if available == 0 {
            self.ready = false;
            self.primed = false;
            self.waited_samples = 0;
            self.correction = 0.0;
            self.step = self.nominal_step;
            return;
        }
        // A short final syllable must not wait indefinitely for more speech.
        // Prime for at most one reserve, even if the target cannot be filled.
        if !self.primed && available < target && self.waited_samples < self.reserve_samples {
            self.ready = false;
            self.waited_samples = self.waited_samples.saturating_add(demand);
            return;
        }
        self.primed = true;
        self.waited_samples = 0;
        self.ready = true;
        let desired = ((available as f64 - target as f64) * 0.000008).clamp(-0.001, 0.001);
        let seconds = output_frames as f64 * self.nominal_step / f64::from(AUDIO_SAMPLE_RATE);
        self.correction += (desired - self.correction) * (1.0 - (-seconds / 0.1).exp());
        self.step = self.nominal_step * (1.0 + self.correction);
    }

    pub(crate) fn frame_from(
        &mut self,
        mut pop: impl FnMut() -> Option<[f32; CHANNELS]>,
        extend_start: bool,
    ) -> PlaybackFrame<CHANNELS> {
        let mut available = self.ready;
        if !self.ready && self.tail == 0 {
            self.phase = 1.0;
            return PlaybackFrame {
                values: [0.0; CHANNELS],
                available: false,
            };
        }
        while self.phase >= 1.0 {
            let source = if self.ready { pop() } else { None };
            let input = match source {
                Some(value) => {
                    self.application_dc_tail = [None; CHANNELS];
                    // Application PCM has no preceding samples. Extend its
                    // first sample across that boundary instead of introducing
                    // a synthetic zero-to-DC step. Explicit reset still erases
                    // the complete history; normal ends retain the FIR tail.
                    if extend_start && self.tail == 0 {
                        for _ in 0..TAPS {
                            for (filter, input) in self.filters.iter_mut().zip(value) {
                                filter.push(input);
                            }
                        }
                    }
                    self.tail = TAPS;
                    value
                }
                None => {
                    if extend_start && self.ready {
                        self.application_dc_tail =
                            std::array::from_fn(|channel| self.filters[channel].constant_value());
                    }
                    available = false;
                    self.ready = false;
                    self.primed = false;
                    self.waited_samples = 0;
                    self.correction = 0.0;
                    self.step = self.nominal_step;
                    self.tail = self.tail.saturating_sub(1);
                    [0.0; CHANNELS]
                }
            };
            for (filter, value) in self.filters.iter_mut().zip(input) {
                filter.push(value);
            }
            self.phase -= 1.0;
        }
        let mut values = std::array::from_fn(|channel| {
            if extend_start {
                self.filters[channel].at_exact_dc(self.phase)
            } else {
                self.filters[channel].at(self.phase)
            }
        });
        // A decoded DC plateau has no continuing oscillation to reconstruct.
        // Bound only its natural end, avoiding a synthetic undershoot/overshoot.
        // Tonal/music content keeps the complete, unclipped filter response.
        for (value, dc) in values.iter_mut().zip(self.application_dc_tail) {
            if let Some(dc) = dc {
                *value = value.clamp(dc.min(0.0), dc.max(0.0));
            }
        }
        self.phase += self.step;
        PlaybackFrame { values, available }
    }
}

impl VoiceOutput {
    #[cfg(test)]
    pub(crate) fn sample(&mut self, mut queue: Option<&mut VecDeque<f32>>) -> VoiceSample {
        self.sample_from(|| queue.as_mut().and_then(|q| q.pop_front()), false)
    }

    pub(crate) fn sample_from(
        &mut self,
        mut pop: impl FnMut() -> Option<f32>,
        extend_start: bool,
    ) -> VoiceSample {
        let frame = self.frame_from(|| pop().map(|value| [value]), extend_start);
        VoiceSample {
            value: frame.values[0],
            available: frame.available,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reserve_tracks_period_and_unknown_uses_legacy_reserve() {
        for (period, expected) in [
            (Some(1.0), 120),
            (Some(2.5), 120),
            (Some(3.0), 144),
            (Some(10.0), 240),
            (None, 240),
            (Some(f32::NAN), 240),
        ] {
            assert_eq!(reserve_samples(period), expected);
        }
        for reserve in [120, 240] {
            let mut output = VoiceOutput::new(48_000, 1920, reserve);
            let mut queue = VecDeque::from(vec![0.1; reserve]);
            for _ in 0..1000 {
                queue.extend([0.1; 120]);
                output.begin_block(Some(queue.len()), 120);
                assert!(output.ready);
                for _ in 0..120 {
                    assert!(output.sample(Some(&mut queue)).available);
                }
            }
        }
    }

    #[test]
    fn stereo_clock_filters_keep_polarity_and_channel_timing() {
        for rate in [44_100_u32, 48_000, 96_000] {
            for direction in [-1_i64, 1] {
                let mut output = PlaybackOutput::<2>::new(rate, 9600, DEFAULT_RESERVE_SAMPLES);
                let mut source_frame = 0_usize;
                let frames = rate as usize / 200;
                let demand = output.source_samples(frames);
                let mut energy = 0.0_f64;
                for block in 0..400 {
                    output.begin_prepared_block(
                        Some((demand as i64 + 480 + direction * 125) as usize),
                        frames,
                        480,
                    );
                    for _ in 0..frames {
                        let frame = output.frame_from(
                            || {
                                let value = (source_frame as f64 * std::f64::consts::TAU * 16_000.0
                                    / 48_000.0)
                                    .sin() as f32
                                    * 0.125;
                                source_frame += 1;
                                Some([value, -value])
                            },
                            true,
                        );
                        assert!(
                            frame.available
                                && frame
                                    .values
                                    .iter()
                                    .all(|v| v.is_finite() && v.abs() <= 0.250001)
                        );
                        assert_eq!(frame.values[0], -frame.values[1]);
                        if block >= 40 {
                            energy += f64::from(frame.values[0]).powi(2);
                        }
                    }
                }
                let gain = (energy / (360 * frames) as f64).sqrt() / (0.125 / 2.0_f64.sqrt());
                assert!((0.98..=1.02).contains(&gain));
                println!("Owned stereo clock rate={rate} direction={direction} rms_gain={gain} frames={source_frame}");
                output.reset();
                for _ in 0..100 {
                    assert_eq!(output.frame_from(|| None, true).values, [0.0; 2]);
                }
            }
        }
    }

    #[test]
    fn prepared_application_dc_end_stays_in_range_and_finishes() {
        for rate in [44_100_u32, 48_000, 96_000] {
            for dc in [-0.25_f32, 0.25] {
                let mut samples = VecDeque::from([dc; 120]);
                let mut output = VoiceOutput::new(rate, 9600, DEFAULT_RESERVE_SAMPLES);
                output.begin_prepared_block(Some(samples.len()), rate as usize / 200, 0);
                let mut audible = 0;
                for _ in 0..1000 {
                    let sample = output.sample_from(|| samples.pop_front(), true).value;
                    assert!(sample.is_finite() && sample >= dc.min(0.0) && sample <= dc.max(0.0));
                    audible += usize::from(sample.abs() > 0.1);
                }
                assert!(audible > 50 && samples.is_empty());
                for _ in 0..100 {
                    assert_eq!(output.sample_from(|| None, true).value, 0.0);
                }
            }
        }
    }

    #[test]
    #[ignore = "Owned generated application PCM tone sweep; no physical device/listening claim"]
    fn prepared_application_filter_preserves_tone_energy_and_reset() {
        for rate in [44_100_u32, 48_000, 96_000] {
            for hz in [150.0_f64, 1000.0, 10_000.0, 16_000.0, 20_000.0] {
                for direction in [-1_i64, 1] {
                    let source: Vec<_> = (0..110_000)
                        .map(|n| {
                            (n as f64 * std::f64::consts::TAU * hz / 48_000.0).sin() as f32 * 0.125
                        })
                        .collect();
                    let mut samples = source.into_iter();
                    let mut output = VoiceOutput::new(rate, 9600, DEFAULT_RESERVE_SAMPLES);
                    let frames = rate as usize / 200;
                    let demand = output.source_samples(frames);
                    let (mut energy, mut count) = (0.0_f64, 0_u64);
                    for block in 0..400 {
                        output.begin_prepared_block(
                            Some((demand as i64 + 480 + direction * 125) as usize),
                            frames,
                            480,
                        );
                        for _ in 0..frames {
                            let sample = output.sample_from(|| samples.next(), true);
                            assert!(sample.available && sample.value.is_finite());
                            assert!(sample.value.abs() <= 0.250001);
                            if block >= 40 {
                                energy += f64::from(sample.value).powi(2);
                                count += 1;
                            }
                        }
                    }
                    let gain = (energy / count as f64).sqrt() / (0.125 / 2.0_f64.sqrt());
                    println!("Owned application tone native_rate={rate} hz={hz} direction={direction} rms_gain={gain}");
                    assert!((0.98..=1.02).contains(&gain));
                    output.reset();
                    for _ in 0..100 {
                        assert_eq!(output.sample_from(|| None, true).value, 0.0);
                    }
                }
            }
        }
    }

    #[test]
    fn normal_end_flushes_filter_tail_and_large_callbacks_do_not_wait_forever() {
        let mut output = VoiceOutput::new(48_000, 1920, DEFAULT_RESERVE_SAMPLES);
        let mut queue = VecDeque::from([0.25; 480]);
        output.begin_block(Some(queue.len()), 2400);
        let mut total = 0.0_f32;
        for _ in 0..600 {
            total += output.sample(Some(&mut queue)).value;
        }
        assert!(queue.is_empty());
        assert!(
            (total - 120.0).abs() < 0.001,
            "A normal end must retain the FIR tail"
        );
        for _ in 0..100 {
            assert_eq!(output.sample(Some(&mut queue)).value, 0.0);
        }
    }

    #[test]
    fn explicit_reset_erases_history_without_flushing_private_audio() {
        let mut output = VoiceOutput::new(48_000, 1920, DEFAULT_RESERVE_SAMPLES);
        let mut queue = VecDeque::from([0.75; 480]);
        output.begin_block(Some(queue.len()), 240);
        for _ in 0..100 {
            output.sample(Some(&mut queue));
        }
        output.reset();
        queue.clear();
        output.begin_block(Some(0), 240);
        for _ in 0..480 {
            assert_eq!(output.sample(Some(&mut queue)).value, 0.0);
        }
        queue.extend([0.125; 480]);
        output.begin_block(Some(queue.len()), 240);
        for n in 0..240 {
            let value = output.sample(Some(&mut queue)).value;
            if n >= TAPS {
                assert!((value - 0.125).abs() < 0.0001);
            }
        }
    }

    #[test]
    fn short_final_syllable_plays_without_waiting_for_another_packet() {
        for rate in [44_100_u32, 48_000, 96_000] {
            let frames = (rate / 400) as usize;
            let mut output = VoiceOutput::new(rate, 1920, DEFAULT_RESERVE_SAMPLES);
            let mut queue = VecDeque::from([0.25; 120]);
            let mut first = None;
            let mut energy = 0.0_f32;
            for block in 0..10 {
                output.begin_block(Some(queue.len()), frames);
                for _ in 0..frames {
                    let value = output.sample(Some(&mut queue)).value;
                    if value.abs() > 0.01 {
                        first.get_or_insert(block);
                    }
                    energy += value * value;
                }
            }
            assert!(first.is_some_and(|block| block <= 3));
            assert!(
                queue.is_empty(),
                "Short speech was left waiting in the queue"
            );
            assert!(energy > 1.0);
            for _ in 0..frames {
                assert_eq!(output.sample(Some(&mut queue)).value, 0.0);
            }
        }
    }

    #[test]
    fn impulse_and_dc_are_finite_with_bounded_filter_tail() {
        for rate in [44_100_u32, 48_000, 96_000] {
            let frames = rate as usize / 200;
            for impulse in [true, false] {
                let mut output = VoiceOutput::new(rate, 1920, DEFAULT_RESERVE_SAMPLES);
                let mut queue = VecDeque::from([if impulse { 0.0 } else { 0.25 }; 960]);
                if impulse {
                    queue[100] = 1.0;
                }
                let mut total = 0.0_f64;
                let mut peak = 0.0_f32;
                for _ in 0..20 {
                    output.begin_block(Some(queue.len()), frames);
                    for _ in 0..frames {
                        let value = output.sample(Some(&mut queue)).value;
                        assert!(value.is_finite());
                        total += f64::from(value);
                        peak = peak.max(value.abs());
                    }
                }
                let expected = if impulse { 1.0 } else { 240.0 };
                let source_total = total * f64::from(AUDIO_SAMPLE_RATE) / f64::from(rate);
                assert!((source_total - expected).abs() < expected * 0.005);
                assert!(peak <= if impulse { 1.01 } else { 0.3 });
                assert!(queue.is_empty());
                assert_eq!(output.sample(Some(&mut queue)).value, 0.0);
            }
        }
    }
}
