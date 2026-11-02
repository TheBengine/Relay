//! Optional Sonora AGC2 after Relay's echo/noise processing, before its gate.
//! Owns no device, thread or PCM queue. The caller supplies whole 10 ms blocks.
use sonora::config::{AdaptiveDigital, GainController2, MaxProcessingRate, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};

pub(crate) const BLOCK: usize = 480;

pub(crate) struct SonoraGain {
    engine: AudioProcessing,
}

impl SonoraGain {
    pub(crate) fn new() -> Self {
        let config = Config {
            pipeline: Pipeline {
                maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
                ..Default::default()
            },
            gain_controller2: Some(GainController2 {
                // Start at unity, cap amplification at about Relay's 16x,
                // and retain AGC2's conservative rise/noise limits.
                adaptive_digital: Some(AdaptiveDigital {
                    initial_gain_db: 0.0,
                    max_gain_db: 24.0,
                    ..Default::default()
                }),
                // Never change the operating system's microphone volume.
                input_volume_controller: false,
                ..Default::default()
            }),
            // No second echo canceller, noise suppressor or high-pass filter.
            ..Default::default()
        };
        let stream = StreamConfig::new(48_000, 1);
        Self {
            engine: AudioProcessing::builder()
                .config(config)
                .capture_config(stream)
                .render_config(stream)
                .build(),
        }
    }

    /// Returns false on a framing/engine error; never emits a partial batch.
    pub(crate) fn process(&mut self, samples: &mut [f32]) -> bool {
        if samples.is_empty() || !samples.len().is_multiple_of(BLOCK) {
            samples.fill(0.0);
            return false;
        }
        let mut source = [0.0; BLOCK];
        let mut output = [0.0; BLOCK];
        for block in samples.chunks_exact_mut(BLOCK) {
            for (to, from) in source.iter_mut().zip(block.iter()) {
                *to = if from.is_finite() {
                    from.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
            if self
                .engine
                .process_capture_f32(&[&source], &mut [&mut output])
                .is_err()
                || output.iter().any(|sample| !sample.is_finite())
            {
                samples.fill(0.0);
                return false;
            }
            // Retain Relay's final peak ceiling even if AGC2 changes upstream.
            for (to, from) in block.iter_mut().zip(output) {
                *to = from.clamp(-0.95, 0.95);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_only_does_not_delay_or_replay_pcm() {
        let mut gain = SonoraGain::new();
        let mut frame = [0.0; BLOCK];
        frame[123] = 0.1;
        assert!(gain.process(&mut frame));
        assert!(frame[123] > 0.0);
        assert!(frame.iter().enumerate().all(|(i, x)| i == 123 || *x == 0.0));
        frame.fill(0.0);
        assert!(gain.process(&mut frame));
        assert!(frame.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn invalid_samples_and_loud_input_stay_finite_and_bounded() {
        let mut gain = SonoraGain::new();
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 4.0, -4.0, 0.0] {
            let mut frame = [value; BLOCK];
            assert!(gain.process(&mut frame));
            assert!(frame.iter().all(|x| x.is_finite() && x.abs() <= 0.95));
        }
    }

    #[test]
    fn twenty_ms_matches_two_ten_ms_calls_and_partial_frames_fail_closed() {
        let mut batch: Vec<f32> = (0..BLOCK * 2)
            .map(|i| (i as f32 * 0.04).sin() * 0.03)
            .collect();
        let mut split = batch.clone();
        assert!(SonoraGain::new().process(&mut batch));
        let mut gain = SonoraGain::new();
        for block in split.chunks_exact_mut(BLOCK) {
            assert!(gain.process(block));
        }
        assert_eq!(batch, split);
        for length in [0, 120, 240, 481] {
            let mut invalid = vec![0.1; length];
            assert!(!gain.process(&mut invalid));
            assert!(invalid.iter().all(|x| *x == 0.0));
        }
    }
}
