//! Optional native echo cancellation with a reference to actually played audio.
use livekit::webrtc::native::apm::AudioProcessingModule;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TEN_MS: usize = 480;
const MAX_REFERENCE_FRAMES: usize = 16;
const MAX_REFERENCE_AGE: Duration = Duration::from_millis(200);

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

pub struct EchoCanceller {
    apm: AudioProcessingModule,
    reference: EchoReference,
}
impl EchoCanceller {
    pub fn new(reference: EchoReference) -> Self {
        Self {
            apm: AudioProcessingModule::new(true, false, true, false),
            reference,
        }
    }
    pub fn reset(&mut self) {
        self.reference.clear();
        self.apm = AudioProcessingModule::new(true, false, true, false);
    }
    /// Runs on the voice worker before gain, RNNoise and activity gating.
    /// This does not open any device or process arbitrary computer audio.
    pub fn process(&mut self, samples: &mut [f32], delay_ms: u32) -> Result<(), String> {
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
        for reference in references.iter_mut().take(count) {
            self.apm
                .process_reverse_stream(reference, 48_000, 1)
                .map_err(|e| format!("Echo reference failed: {e}"))?;
        }
        if samples.is_empty() || !samples.len().is_multiple_of(TEN_MS) {
            return Err("Echo cancellation requires complete 10 ms blocks".into());
        }
        for chunk in samples.chunks_exact_mut(TEN_MS) {
            let mut frame = [0i16; TEN_MS];
            for (out, input) in frame.iter_mut().zip(chunk.iter()) {
                *out = to_i16(*input);
            }
            self.apm
                .process_stream(&mut frame, 48_000, 1)
                .map_err(|e| format!("Echo processing failed: {e}"))?;
            for (out, input) in chunk.iter_mut().zip(frame) {
                *out = input as f32 / 32768.0;
            }
        }
        Ok(())
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
