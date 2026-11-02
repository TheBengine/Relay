//! Opt-in, passive Silero VAD. Never changes PCM or transmission decisions.
//! Inference runs off the audio worker with bounded queues; dropped input
//! resets context and recurrent state before inference resumes.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tract_onnx::prelude::*;

const BLOCK: usize = 480;
const HOP: usize = 512;
const CONTEXT: usize = 64;
const MAX_AGE: Duration = Duration::from_millis(250);
static BYTES: &[u8] = include_bytes!("../../../vendor/models/silero/silero_vad_16k_fixed.onnx");
static MODEL: OnceLock<Result<Arc<TypedSimplePlan>, String>> = OnceLock::new();

fn model() -> Result<Arc<TypedSimplePlan>, String> {
    MODEL
        .get_or_init(|| {
            tract_onnx::onnx()
                .model_for_read(&mut &BYTES[..])
                .and_then(|m| m.with_input_fact(0, f32::fact([1, HOP + CONTEXT]).into()))
                .and_then(|m| m.with_input_fact(1, f32::fact([2, 1, 128]).into()))
                .and_then(|m| m.with_input_fact(2, tensor0(16_000i64).into()))
                .and_then(|m| m.into_optimized())
                .and_then(|m| m.into_runnable())
                .map_err(|e| format!("Silero model: {e:#}"))
        })
        .clone()
}

struct Core {
    model: Arc<TypedSimplePlan>,
    state: Tensor,
    context: [f32; CONTEXT],
}

impl Core {
    fn new() -> Result<Self, String> {
        Ok(Self {
            model: model()?,
            state: Tensor::zero::<f32>(&[2, 1, 128]).map_err(|e| e.to_string())?,
            context: [0.0; CONTEXT],
        })
    }

    fn process(&mut self, samples: &[f32; HOP]) -> Result<f32, String> {
        let mut input = [0.0; HOP + CONTEXT];
        input[..CONTEXT].copy_from_slice(&self.context);
        input[CONTEXT..].copy_from_slice(samples);
        let outputs = self
            .model
            .run(tvec![
                Tensor::from_shape(&[1, HOP + CONTEXT], &input)
                    .map_err(|e| e.to_string())?
                    .into(),
                self.state.clone().into(),
                tensor0(16_000i64).into(),
            ])
            .map_err(|e| format!("Silero inference: {e:#}"))?;
        let probability = *outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| e.to_string())?
            .iter()
            .next()
            .ok_or("Silero returned no probability")?;
        if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
            return Err("Silero returned an invalid probability".into());
        }
        self.state = outputs[1].clone().into_tensor();
        self.context.copy_from_slice(&samples[HOP - CONTEXT..]);
        Ok(probability)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Reading {
    Idle,
    Warming,
    Probability(f32),
    Lagging,
    Unavailable,
}

#[derive(Default)]
pub(crate) struct Meter(Mutex<Option<(Instant, Reading)>>);

impl Meter {
    pub(crate) fn record(&self, value: Reading) {
        if let Ok(mut reading) = self.0.try_lock() {
            *reading = Some((Instant::now(), value));
        }
    }
    pub(crate) fn read(&self) -> Reading {
        self.0
            .try_lock()
            .ok()
            .and_then(|v| *v)
            .filter(|(time, _)| time.elapsed() <= MAX_AGE)
            .map_or(Reading::Idle, |(_, value)| value)
    }
}

struct Frame {
    samples: [f32; BLOCK],
    sequence: u64,
    captured: Instant,
}
struct ResultFrame {
    probability: f32,
    captured: Instant,
}

struct Analysis {
    core: Core,
    filter: crate::audio_resample::SincFilter,
    samples: [f32; HOP],
    used: usize,
    expected: Option<u64>,
    last_capture: Option<Instant>,
}

impl Analysis {
    fn new() -> Result<Self, String> {
        Ok(Self {
            core: Core::new()?,
            filter: crate::audio_resample::SincFilter::new(1.0 / 3.0),
            samples: [0.0; HOP],
            used: 0,
            expected: None,
            last_capture: None,
        })
    }

    fn process(&mut self, frame: &Frame, now: Instant) -> Result<Option<ResultFrame>, String> {
        if now.saturating_duration_since(frame.captured) > MAX_AGE {
            self.expected = None;
            return Ok(None);
        }
        if self.expected != Some(frame.sequence)
            || self
                .last_capture
                .is_some_and(|at| frame.captured.saturating_duration_since(at) > MAX_AGE)
        {
            self.core = Core::new()?;
            self.filter.reset();
            self.samples.fill(0.0);
            self.used = 0;
        }
        self.expected = frame.sequence.checked_add(1);
        self.last_capture = Some(frame.captured);
        let mut result = None;
        for (index, value) in frame.samples.iter().enumerate() {
            self.filter.push(*value);
            if index % 3 == 2 {
                self.samples[self.used] = self.filter.at(0.0);
                self.used += 1;
                if self.used == HOP {
                    result = Some(ResultFrame {
                        probability: self.core.process(&self.samples)?,
                        captured: frame.captured,
                    });
                    self.used = 0;
                }
            }
        }
        Ok(result)
    }
}

pub(crate) struct ShadowDetector {
    input: SyncSender<Frame>,
    output: Receiver<ResultFrame>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    pending: [f32; BLOCK],
    used: usize,
    sequence: u64,
    last: Option<ResultFrame>,
    last_observation: Option<Instant>,
}

impl ShadowDetector {
    pub(crate) fn new() -> Self {
        let (input, rx) = sync_channel::<Frame>(4);
        let (tx, output) = sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (worker_stop, worker_failed) = (stop.clone(), failed.clone());
        let spawned = std::thread::Builder::new()
            .name("relay-silero-vad".into())
            .spawn(move || {
                let run = || -> Result<(), String> {
                    let mut analysis = Analysis::new()?;
                    while let Ok(frame) = rx.recv() {
                        if worker_stop.load(Ordering::Acquire) {
                            break;
                        }
                        if let Some(result) = analysis.process(&frame, Instant::now())? {
                            let _ = tx.try_send(result);
                        }
                    }
                    Ok(())
                };
                if let Err(error) = run() {
                    eprintln!("{error}");
                    worker_failed.store(true, Ordering::Release);
                }
            });
        if spawned.is_err() {
            failed.store(true, Ordering::Release);
        }
        Self {
            input,
            output,
            stop,
            failed,
            pending: [0.0; BLOCK],
            used: 0,
            sequence: 0,
            last: None,
            last_observation: None,
        }
    }

    pub(crate) fn observe(&mut self, samples: &[f32]) -> Reading {
        self.observe_at(samples, Instant::now())
    }

    fn observe_at(&mut self, samples: &[f32], now: Instant) -> Reading {
        if self
            .last_observation
            .is_some_and(|at| now.saturating_duration_since(at) > MAX_AGE)
        {
            // Short capture packets can leave an incomplete 10 ms frame here.
            // Do not stamp pre-stall PCM as fresh or carry it into a new window.
            self.pending.fill(0.0);
            self.used = 0;
            self.sequence = self.sequence.wrapping_add(1);
            self.last = None;
        }
        self.last_observation = Some(now);
        for &sample in samples {
            self.pending[self.used] = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            self.used += 1;
            if self.used == BLOCK {
                let _ = self.input.try_send(Frame {
                    samples: self.pending,
                    sequence: self.sequence,
                    captured: now,
                });
                self.sequence = self.sequence.wrapping_add(1);
                self.used = 0;
            }
        }
        while let Ok(value) = self.output.try_recv() {
            self.last = Some(value);
        }
        if self.failed.load(Ordering::Acquire) {
            return Reading::Unavailable;
        }
        match &self.last {
            Some(value) if now.saturating_duration_since(value.captured) <= MAX_AGE => {
                Reading::Probability(value.probability)
            }
            Some(_) => Reading::Lagging,
            None => Reading::Warming,
        }
    }
}

impl Drop for ShadowDetector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn silero_short_capture_discards_partial_input_after_a_stall() {
        for size in [120, 240] {
            let mut detector = ShadowDetector::new();
            let start = Instant::now();
            detector.observe_at(&vec![0.8; size], start);
            let sequence = detector.sequence;
            detector.observe_at(&vec![0.1; size], start + MAX_AGE * 2);
            assert_eq!(detector.used, size);
            assert_eq!(detector.sequence, sequence + 1);
            assert!(detector.pending[..size].iter().all(|&v| v == 0.1));
            assert!(detector.pending[size..].iter().all(|&v| v == 0.0));
        }
    }
    #[test]
    fn silero_fresh_capture_after_stall_resets_context_even_without_dropped_sequence() {
        let mut reused = Analysis::new().unwrap();
        let mut fresh = Analysis::new().unwrap();
        let start = Instant::now();
        for sequence in 0..20 {
            reused
                .process(
                    &Frame {
                        samples: [0.2; BLOCK],
                        sequence,
                        captured: start,
                    },
                    start,
                )
                .unwrap();
        }
        let now = start + MAX_AGE * 2;
        for sequence in 20..40 {
            let frame = Frame {
                samples: [0.01; BLOCK],
                sequence,
                captured: now,
            };
            assert_eq!(
                reused.process(&frame, now).unwrap().map(|r| r.probability),
                fresh.process(&frame, now).unwrap().map(|r| r.probability)
            );
        }
    }

    #[test]
    #[ignore = "synthetic analysis cost report; no devices or speech accuracy claim"]
    fn report_silero_analysis_cost() {
        let load = Instant::now();
        let mut analysis = Analysis::new().unwrap();
        let load_ms = load.elapsed().as_secs_f64() * 1000.0;
        let signal = crate::voice_test_signals::talker(10.0, (110.0, 170.0), 42, 0.03);
        let mut times = Vec::new();
        let mut results = 0;
        for (sequence, samples) in signal.chunks_exact(BLOCK).enumerate() {
            let now = Instant::now();
            let frame = Frame {
                samples: samples.try_into().unwrap(),
                sequence: sequence as u64,
                captured: now,
            };
            results += usize::from(analysis.process(&frame, now).unwrap().is_some());
            times.push(now.elapsed().as_secs_f64() * 1000.0);
        }
        assert_eq!(results, 16_000 * 10 / HOP);
        times.sort_by(f64::total_cmp);
        println!("Silero model_init_ms={load_ms:.3} analysis_frames={} inference_windows={results} mean_ms_per_10ms={:.4} p95_ms={:.4} max_ms={:.4}", times.len(), times.iter().sum::<f64>() / times.len() as f64, times[times.len() * 95 / 100], times.last().unwrap());
    }
    #[test]
    fn silero_dropped_and_expired_input_discard_all_streaming_history() {
        for expired in [false, true] {
            let mut reused = Analysis::new().unwrap();
            let mut fresh = Analysis::new().unwrap();
            let now = Instant::now();
            for sequence in 0..20 {
                reused
                    .process(
                        &Frame {
                            samples: [0.2; BLOCK],
                            sequence,
                            captured: now,
                        },
                        now,
                    )
                    .unwrap();
            }
            if expired {
                assert!(reused
                    .process(
                        &Frame {
                            samples: [0.2; BLOCK],
                            sequence: 20,
                            captured: now - MAX_AGE * 2
                        },
                        now
                    )
                    .unwrap()
                    .is_none());
            }
            for sequence in 21..41 {
                let frame = Frame {
                    samples: std::array::from_fn(|i| (i as f32 * 0.02).sin() * 0.05),
                    sequence,
                    captured: now,
                };
                let a = reused.process(&frame, now).unwrap().map(|r| r.probability);
                let b = fresh.process(&frame, now).unwrap().map(|r| r.probability);
                assert_eq!(a, b);
            }
        }
    }
    #[test]
    fn silero_matches_pinned_onnxruntime_reference_and_resets_state() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../../../vendor/models/silero/reference.json"))
                .unwrap();
        let expected = reference["probabilities"].as_array().unwrap();
        for _ in 0..2 {
            let mut core = Core::new().unwrap();
            for (block, expected) in expected.iter().enumerate() {
                let input =
                    std::array::from_fn(|i| (((block * HOP + i) % 97) as f32 - 48.0) / 512.0);
                let actual = core.process(&input).unwrap();
                assert!(
                    (actual - expected.as_f64().unwrap() as f32).abs() < 1e-4,
                    "block {block}: {actual} vs {expected}"
                );
            }
        }
    }
    #[test]
    fn silero_silence_stays_below_speech_threshold() {
        let mut core = Core::new().unwrap();
        for _ in 0..20 {
            assert!(core.process(&[0.0; HOP]).unwrap() < 0.5);
        }
    }
    #[test]
    fn silero_meter_expires_inactive_audio() {
        let meter = Meter::default();
        meter.record(Reading::Probability(0.8));
        assert_eq!(meter.read(), Reading::Probability(0.8));
        *meter.0.lock().unwrap() = Some((Instant::now() - MAX_AGE * 2, Reading::Probability(0.8)));
        assert_eq!(meter.read(), Reading::Idle);
    }
}
