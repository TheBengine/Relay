//! Studio noise suppression: DPDFNet (Ceva, Apache-2.0; `vendor/models/dpdfnet`)
//! run with tract, one STFT frame per 10 ms block, behind the same
//! `noise_suppressor::Suppressor` interface as RNNoise.
//!
//! Per 480-sample block: a 960-point Vorbis-window STFT frame over the last
//! two blocks, one model step with the state carried from the previous step,
//! and an inverse STFT with overlap-add. This is Worker F's `dpprobe`
//! (`docs/audio-dpdfnet-eval-20261029.md`), which matches the Python reference
//! to 1e-7 RMS. The model's output follows its input by 50 ms: four 10 ms
//! frames of look-ahead in the trained weights plus the overlap-add's 10 ms.
//!
//! In the app the model runs on a thread of its own (`Studio`): the voice
//! worker hands it each block and sends the result for the block before,
//! never waiting, so Studio's delay is 60 ms. A late result means that block
//! goes out unprocessed. On the model thread (`Core`), the models, embedded
//! in the executable, take 0.2-0.6 s to build; until one is ready RNNoise's
//! output, delayed to the same 50 ms, stands in, with a one-block fade at
//! each switch. The same path takes over if the model fails, and the overrun
//! guard steps down from dpdfnet8 to dpdfnet2 (then to RNNoise) when the
//! model keeps taking longer than the block it processes.
use crate::noise_suppressor::{RnNoise, Suppressor, BLOCK};
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tract_onnx::prelude::*;

const HOP: usize = BLOCK;
const WIN: usize = 2 * HOP;
const BINS: usize = WIN / 2 + 1;
/// The model's output lags its input by this much, for every model
/// (measured: Worker F's cross-correlation).
const MODEL_DELAY_SAMPLES: usize = 2_400;
const DELAY_BLOCKS: usize = MODEL_DELAY_SAMPLES / BLOCK;
/// Studio's delay: the model's and one block of hand-off to its thread
/// (`declared_delay_matches_the_output`).
pub(crate) const DELAY_SAMPLES: usize = MODEL_DELAY_SAMPLES + BLOCK;
/// RNNoise is one block late; the rest of the 50 ms is this delay line.
const FALLBACK_LINE_BLOCKS: usize = DELAY_BLOCKS - 1;
/// A model stream started mid-call is used once its output covers input it
/// has seen whole: after the look-ahead and the overlap-add.
const WARM_UP_BLOCKS: usize = DELAY_BLOCKS + 1;
/// Overrun guard: this many blocks in a row over the block's own 10 ms.
pub(crate) const OVERRUN_LIMIT: u32 = 3;
const BLOCK_TIME: Duration = Duration::from_millis(10);

static DPDFNET8: &[u8] = include_bytes!("../../../vendor/models/dpdfnet/dpdfnet8_48khz_hr.onnx");
static DPDFNET2: &[u8] = include_bytes!("../../../vendor/models/dpdfnet/dpdfnet2_48khz_hr.onnx");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Variant {
    /// dpdfnet8_48khz_hr: 3.6 M parameters, the one the user chose.
    Eight = 0,
    /// dpdfnet2_48khz_hr: 2.6 M parameters, a third of the CPU.
    Two = 1,
}

impl Variant {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Eight => DPDFNET8,
            Self::Two => DPDFNET2,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Eight => "dpdfnet8",
            Self::Two => "dpdfnet2",
        }
    }
}

// ------------------------------------------------------------------ model

pub(crate) struct Model {
    plan: Arc<TypedSimplePlan>,
    initial_state: Vec<f32>,
}

fn load(variant: Variant) -> Result<Model, String> {
    let onnx = tract_onnx::onnx();
    let proto = onnx
        .proto_model_for_read(&mut variant.bytes())
        .map_err(|e| format!("read ONNX: {e}"))?;
    let meta = |key: &str| -> Result<&str, String> {
        proto
            .metadata_props
            .iter()
            .find(|p| p.key == key)
            .map(|p| p.value.as_str())
            .ok_or_else(|| format!("metadata {key} missing"))
    };
    let size = |key: &str| -> Result<usize, String> { meta(key)?.parse().map_err(|e| format!("{key}: {e}")) };
    let floats = |key: &str| -> Result<Vec<f32>, String> {
        meta(key)?
            .split(',')
            .map(|v| v.trim().parse::<f32>().map_err(|e| format!("{key}: {e}")))
            .collect()
    };
    let (state_size, erb_size, spec_size) = (
        size("state_size")?,
        size("erb_norm_state_size")?,
        size("spec_norm_state_size")?,
    );
    let (erb_init, spec_init) = (floats("erb_norm_init")?, floats("spec_norm_init")?);
    if erb_init.len() != erb_size || spec_init.len() != spec_size || erb_size + spec_size > state_size {
        return Err(format!(
            "norm init sizes {} / {} do not match {erb_size} / {spec_size} in {state_size}",
            erb_init.len(),
            spec_init.len()
        ));
    }
    // The layout of load_initial_state_from_metadata in the reference.
    let mut initial_state = vec![0.0f32; state_size];
    initial_state[..erb_size].copy_from_slice(&erb_init);
    initial_state[erb_size..erb_size + spec_size].copy_from_slice(&spec_init);
    let plan = onnx
        .model_for_proto_model(&proto)
        .and_then(|model| model.with_input_fact(0, f32::fact([1, 1, BINS, 2]).into()))
        .and_then(|model| model.with_input_fact(1, f32::fact([state_size]).into()))
        .and_then(|model| model.into_optimized())
        .and_then(|model| model.into_runnable())
        .map_err(|e| format!("build: {e}"))?;
    Ok(Model { plan, initial_state })
}

static MODELS: [OnceLock<Result<Arc<Model>, String>>; 2] = [OnceLock::new(), OnceLock::new()];
static LOADING: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

/// Builds the model now, or waits for the loader that is building it.
pub(crate) fn load_blocking(variant: Variant) -> Result<Arc<Model>, String> {
    MODELS[variant as usize]
        .get_or_init(|| {
            let started = Instant::now();
            let loaded = load(variant).map(Arc::new);
            match &loaded {
                Ok(_) => eprintln!(
                    "Studio noise suppression: {} ready in {:.2} s",
                    variant.name(),
                    started.elapsed().as_secs_f32()
                ),
                Err(error) => eprintln!("Studio noise suppression: {} failed to load: {error}", variant.name()),
            }
            loaded
        })
        .clone()
}

/// Starts building the model on its own thread, once; never blocks.
pub(crate) fn request(variant: Variant) {
    let index = variant as usize;
    if MODELS[index].get().is_some() || LOADING[index].swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name(format!("relay-{}-load", variant.name()))
        .spawn(move || {
            let _ = load_blocking(variant);
        });
    if spawned.is_err() {
        LOADING[index].store(false, Ordering::Release);
    }
}

/// Starts building the model the app's Studio suppressor will use.
pub(crate) fn prepare() {
    if !APP_STICKY.eight_too_slow.load(Ordering::Acquire) {
        request(Variant::Eight);
    } else if !APP_STICKY.two_too_slow.load(Ordering::Acquire) {
        request(Variant::Two);
    }
}

/// The model if it is built; `Err` if building it failed.
fn ready(variant: Variant) -> Option<Result<Arc<Model>, String>> {
    MODELS[variant as usize].get().cloned()
}

fn vorbis_window() -> Vec<f32> {
    let half = WIN as f64 / 2.0;
    (0..WIN)
        .map(|n| {
            let s = (0.5 * std::f64::consts::PI * (n as f64 + 0.5) / half).sin();
            (0.5 * std::f64::consts::PI * s * s).sin() as f32
        })
        .collect()
}

// ----------------------------------------------------------------- stream

/// One stream through the model: 480 samples in, 480 samples out, 50 ms late.
pub(crate) struct Stream {
    model: Arc<Model>,
    variant: Variant,
    state: Tensor,
    window: Vec<f32>,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    analysis: Vec<f32>,
    overlap: Vec<f32>,
    frame: Vec<f32>,
    spectrum: Vec<Complex32>,
    enhanced: Vec<Complex32>,
    packed: Vec<f32>,
    scratch_forward: Vec<Complex32>,
    scratch_inverse: Vec<Complex32>,
}

impl Stream {
    pub(crate) fn new(model: Arc<Model>, variant: Variant) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(WIN);
        let inverse = planner.plan_fft_inverse(WIN);
        let state = Tensor::from_shape(&[model.initial_state.len()], &model.initial_state)
            .expect("state shape matches its length");
        Self {
            model,
            variant,
            state,
            window: vorbis_window(),
            scratch_forward: forward.make_scratch_vec(),
            scratch_inverse: inverse.make_scratch_vec(),
            forward,
            inverse,
            analysis: vec![0.0; WIN],
            overlap: vec![0.0; HOP],
            frame: vec![0.0; WIN],
            spectrum: vec![Complex32::default(); BINS],
            enhanced: vec![Complex32::default(); BINS],
            packed: vec![0.0; BINS * 2],
        }
    }

    pub(crate) fn process(&mut self, block: &mut [f32; BLOCK]) -> Result<(), String> {
        // Frame t spans the previous hop and this one, as librosa's centred
        // frame t does.
        self.analysis.copy_within(HOP.., 0);
        self.analysis[HOP..].copy_from_slice(block);
        for ((f, a), w) in self.frame.iter_mut().zip(&self.analysis).zip(&self.window) {
            *f = a * w;
        }
        self.forward
            .process_with_scratch(&mut self.frame, &mut self.spectrum, &mut self.scratch_forward)
            .map_err(|e| format!("rfft: {e}"))?;
        for (pair, c) in self.packed.chunks_exact_mut(2).zip(&self.spectrum) {
            pair[0] = c.re;
            pair[1] = c.im;
        }
        let spec = Tensor::from_shape(&[1, 1, BINS, 2], &self.packed).map_err(|e| e.to_string())?;
        let state = std::mem::take(&mut self.state);
        let mut outputs = self
            .model
            .plan
            .run(tvec!(spec.into(), state.into()))
            .map_err(|e| format!("run: {e}"))?;
        if outputs.len() != 2 {
            return Err(format!("{} outputs, expected 2", outputs.len()));
        }
        self.state = outputs.remove(1).into_tensor();
        let enhanced = outputs.remove(0).into_tensor();
        let enhanced = enhanced.to_plain_array_view::<f32>().map_err(|e| e.to_string())?;
        let enhanced = enhanced.as_slice().ok_or("enhanced spectrum is not contiguous")?;
        if enhanced.len() != BINS * 2 {
            return Err(format!("{} output values, expected {}", enhanced.len(), BINS * 2));
        }
        for (c, pair) in self.enhanced.iter_mut().zip(enhanced.chunks_exact(2)) {
            *c = Complex32::new(pair[0], pair[1]);
        }
        // numpy's irfft ignores the imaginary part of DC and Nyquist.
        self.enhanced[0].im = 0.0;
        self.enhanced[BINS - 1].im = 0.0;
        self.inverse
            .process_with_scratch(&mut self.enhanced, &mut self.frame, &mut self.scratch_inverse)
            .map_err(|e| format!("irfft: {e}"))?;
        // The Vorbis window is power-complementary at 50 % overlap, so the
        // overlap-add needs no further normalisation.
        let scale = 1.0 / WIN as f32;
        for (n, sample) in block.iter_mut().enumerate() {
            *sample = self.overlap[n] + self.frame[n] * self.window[n] * scale;
        }
        for n in 0..HOP {
            self.overlap[n] = self.frame[HOP + n] * self.window[HOP + n] * scale;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- studio

/// What the guard has decided, for every Studio suppressor that shares it.
/// The app's is process-wide: once dpdfnet8 has fallen behind on this PC,
/// later calls start on dpdfnet2 instead of falling behind again.
#[derive(Default)]
pub(crate) struct Sticky {
    eight_too_slow: AtomicBool,
    two_too_slow: AtomicBool,
    logged: [AtomicBool; 3],
}

static APP_STICKY: Sticky = Sticky {
    eight_too_slow: AtomicBool::new(false),
    two_too_slow: AtomicBool::new(false),
    logged: [AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)],
};

impl Sticky {
    fn log_once(&self, slot: usize, message: impl FnOnce() -> String) {
        if !self.logged[slot].swap(true, Ordering::AcqRel) {
            eprintln!("{}", message());
        }
    }
}

struct Guard {
    budget: Duration,
    run: u32,
    sticky: &'static Sticky,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StudioKind {
    pub(crate) variant: Variant,
    /// Give click suppression a voice probability: the model's output-to-input
    /// energy ratio. Without it, the Studio chain runs no click suppression.
    pub(crate) probability: bool,
    /// The app's suppressor: overrun guard on, model loaded in the
    /// background. Off (bench, tests): the model is built before the first
    /// block and never swapped, so the output is the model's alone.
    pub(crate) guarded: bool,
}

/// Wall time of the model per block, 50 µs buckets up to 50 ms.
struct Timing {
    buckets: Vec<u32>,
    blocks: u32,
    max: Duration,
}

impl Timing {
    const STEP_US: u128 = 50;
    fn new() -> Self {
        Self { buckets: vec![0; 1_001], blocks: 0, max: Duration::ZERO }
    }
    fn record(&mut self, spent: Duration) {
        let bucket = (spent.as_micros() / Self::STEP_US).min(1_000) as usize;
        self.buckets[bucket] += 1;
        self.blocks += 1;
        self.max = self.max.max(spent);
    }
    /// Upper edge of the bucket holding this fraction of the blocks, in ms.
    fn percentile_ms(&self, fraction: f64) -> f64 {
        let wanted = ((self.blocks as f64 * fraction).ceil() as u32).max(1);
        let mut seen = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= wanted {
                return (index as u128 + 1) as f64 * Self::STEP_US as f64 / 1000.0;
            }
        }
        f64::INFINITY
    }
}

/// The model with its stand-in and overrun guard, on the model thread.
struct Core {
    kind: StudioKind,
    /// The model being used and the one warming up to take over.
    active: Option<Stream>,
    pending: Option<(Stream, usize)>,
    /// The model this suppressor wants next; `None`: RNNoise only.
    wanted: Option<Variant>,
    fallback: RnNoise,
    fallback_line: VecDeque<([f32; BLOCK], f32)>,
    /// Input energy of the last blocks, to line up with the output.
    energies: VecDeque<f32>,
    voice: f32,
    guard: Option<Guard>,
    timing: Timing,
}

impl Core {
    fn new(kind: StudioKind) -> Self {
        let guard = kind.guarded.then_some(Guard { budget: BLOCK_TIME, run: 0, sticky: &APP_STICKY });
        Self::with_guard(kind, guard)
    }

    fn with_guard(kind: StudioKind, guard: Option<Guard>) -> Self {
        let wanted = match &guard {
            Some(guard) if guard.sticky.two_too_slow.load(Ordering::Acquire) => None,
            Some(guard) if guard.sticky.eight_too_slow.load(Ordering::Acquire) => Some(Variant::Two),
            _ => Some(kind.variant),
        };
        let active = wanted.and_then(|variant| {
            let model = if guard.is_some() {
                request(variant);
                ready(variant)?
            } else {
                load_blocking(variant)
            };
            model.ok().map(|model| Stream::new(model, variant))
        });
        Self {
            kind,
            active,
            pending: None,
            wanted,
            fallback: RnNoise::new(),
            fallback_line: VecDeque::with_capacity(FALLBACK_LINE_BLOCKS + 1),
            energies: VecDeque::with_capacity(DELAY_BLOCKS + 2),
            voice: 0.0,
            guard,
            timing: Timing::new(),
        }
    }

    /// The model whose output is being sent, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn active_variant(&self) -> Option<Variant> {
        self.active.as_ref().map(|stream| stream.variant)
    }

    fn give_up(&mut self, variant: Variant, why: &str) {
        let sticky = self.guard.as_ref().map_or(&APP_STICKY, |guard| guard.sticky);
        let next = match variant {
            Variant::Eight => {
                sticky.eight_too_slow.store(true, Ordering::Release);
                Some(Variant::Two)
            }
            Variant::Two => {
                sticky.two_too_slow.store(true, Ordering::Release);
                None
            }
        };
        sticky.log_once(variant as usize, || {
            format!(
                "Studio noise suppression: {} {why}; switching to {}",
                variant.name(),
                next.map_or("RNNoise (Enhanced's suppressor)", Variant::name)
            )
        });
        if let Some(next) = next {
            request(next);
        }
        self.wanted = next;
    }
}

fn energy(block: &[f32; BLOCK]) -> f32 {
    block.iter().map(|v| v * v).sum::<f32>() / BLOCK as f32
}

fn crossfade(from: &[f32; BLOCK], to: &[f32; BLOCK]) -> [f32; BLOCK] {
    let mut out = [0.0; BLOCK];
    for (n, sample) in out.iter_mut().enumerate() {
        let w = (n + 1) as f32 / BLOCK as f32;
        *sample = from[n] * (1.0 - w) + to[n] * w;
    }
    out
}

impl Suppressor for Core {
    fn process(&mut self, block: &mut [f32; BLOCK]) {
        let input = *block;
        self.energies.push_back(energy(&input));
        if self.energies.len() > DELAY_BLOCKS + 1 {
            self.energies.pop_front();
        }
        // RNNoise runs all the time (0.04 ms), so taking over from the model
        // never starts from a cold state; it is delayed to the model's 50 ms.
        let mut rnnoise = input;
        self.fallback.process(&mut rnnoise);
        self.fallback_line
            .push_back((rnnoise, self.fallback.voice_probability().unwrap_or(0.0)));
        let (fallback, fallback_voice) = if self.fallback_line.len() > FALLBACK_LINE_BLOCKS {
            self.fallback_line.pop_front().expect("line is not empty")
        } else {
            ([0.0; BLOCK], 0.0)
        };

        // A model that became ready, or the next one after a step down.
        if let (None, None, Some(variant)) = (&self.active, &self.pending, self.wanted) {
            match ready(variant) {
                Some(Ok(model)) => self.pending = Some((Stream::new(model, variant), 0)),
                Some(Err(_)) => self.give_up(variant, "could not be loaded"),
                None => request(variant),
            }
        }

        let started = Instant::now();
        let mut model_out = None;
        if let Some(stream) = self.active.as_mut() {
            let mut out = input;
            match stream.process(&mut out) {
                Ok(()) => model_out = Some(out),
                Err(error) => {
                    let variant = stream.variant;
                    self.active = None;
                    self.give_up(variant, &format!("failed ({error})"));
                }
            }
        }
        let mut warmed = None;
        if let Some((stream, fed)) = self.pending.as_mut() {
            let mut out = input;
            match stream.process(&mut out) {
                Ok(()) => {
                    *fed += 1;
                    if *fed > WARM_UP_BLOCKS {
                        warmed = Some(out);
                    }
                }
                Err(error) => {
                    let variant = stream.variant;
                    self.pending = None;
                    self.give_up(variant, &format!("failed ({error})"));
                }
            }
        }
        let spent = started.elapsed();
        if model_out.is_some() || self.pending.is_some() {
            self.timing.record(spent);
        }

        let mut out = match (model_out, warmed) {
            (from, Some(to)) => {
                let (stream, _) = self.pending.take().expect("warmed implies pending");
                self.active = Some(stream);
                crossfade(&from.unwrap_or(fallback), &to)
            }
            (Some(model), None) => model,
            (None, None) => fallback,
        };
        let from_model = self.active.is_some();

        // Overrun guard: the model alone over the block's 10 ms, several
        // blocks in a row, means this PC cannot keep up with it.
        if let (Some(guard), Some(variant)) = (self.guard.as_mut(), self.active.as_ref().map(|s| s.variant)) {
            guard.run = if spent > guard.budget { guard.run + 1 } else { 0 };
            if guard.run >= OVERRUN_LIMIT {
                guard.run = 0;
                self.active = None;
                self.pending = None;
                out = crossfade(&out, &fallback);
                self.give_up(
                    variant,
                    &format!("took longer than 10 ms for {OVERRUN_LIMIT} blocks in a row"),
                );
            }
        }

        self.voice = if from_model && self.active.is_some() {
            // The model keeps speech and removes the rest: what is left of
            // the input block it is answering is a cheap voice probability.
            let input_energy = *self.energies.front().expect("energies is not empty");
            if self.energies.len() > DELAY_BLOCKS && input_energy > 1e-10 {
                (energy(&out) / input_energy).sqrt().clamp(0.0, 1.0)
            } else {
                0.0
            }
        } else {
            fallback_voice
        };
        for (to, from) in block.iter_mut().zip(out) {
            *to = if from.is_finite() { from.clamp(-1.0, 1.0) } else { 0.0 };
        }
    }

    fn delay_samples(&self) -> usize {
        MODEL_DELAY_SAMPLES
    }

    fn name(&self) -> &'static str {
        studio_name(self.kind)
    }

    fn voice_probability(&self) -> Option<f32> {
        self.kind.probability.then_some(self.voice)
    }
}

fn studio_name(kind: StudioKind) -> &'static str {
    match (kind.variant, kind.probability) {
        (Variant::Eight, false) => "dpdfnet8",
        (Variant::Eight, true) => "dpdfnet8+ratio",
        (Variant::Two, false) => "dpdfnet2",
        (Variant::Two, true) => "dpdfnet2+ratio",
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // The app's own CPU figure for the model, once per call or reset.
        if self.guard.is_some() && self.timing.blocks >= 500 {
            eprintln!(
                "Studio noise suppression: {} blocks, model time per 10 ms block p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms; ended on {}",
                self.timing.blocks,
                self.timing.percentile_ms(0.5),
                self.timing.percentile_ms(0.99),
                self.timing.max.as_secs_f64() * 1000.0,
                self.active.as_ref().map_or("RNNoise", |stream| stream.variant.name())
            );
        }
    }
}

// --------------------------------------------------------------- hand-off

/// Input blocks that may wait for the model thread; past that, blocks are
/// not queued and go out unprocessed.
const QUEUE_BLOCKS: usize = 4;
/// Studio's whole delay in blocks: the model's 50 ms and the hand-off's 10.
const STUDIO_DELAY_BLOCKS: usize = DELAY_SAMPLES / BLOCK;

/// (block number, the model's output for it, its voice probability)
type Done = (u64, [f32; BLOCK], Option<f32>);

static LOGGED_LATE: AtomicBool = AtomicBool::new(false);

enum Runner {
    /// Bench and tests: the model runs on the caller's thread, its output
    /// held back one block so the delay is the app's 60 ms.
    Inline {
        core: Box<Core>,
        held: Option<([f32; BLOCK], Option<f32>)>,
    },
    /// The app: the model runs on a thread of its own.
    Thread {
        jobs: Option<SyncSender<(u64, [f32; BLOCK])>>,
        done: Receiver<Done>,
        /// A result read before the block it belongs to was due.
        early: Option<Done>,
        /// Wait for each result (tests only); the app never waits.
        patient: bool,
    },
}

/// The Studio suppressor the send chain holds. In the app it gives block t
/// to the model thread and sends the model's output for block t-1, which the
/// thread has had a whole block (10 ms) to produce. It never waits: when that
/// output is not there (the thread is slow or stuck), the microphone goes out
/// unprocessed, delayed to the same 60 ms, so capture and playback on the
/// voice worker never stall on the model.
pub(crate) struct Studio {
    kind: StudioKind,
    runner: Runner,
    /// The raw microphone, held to line up with the model's output.
    raw: VecDeque<[f32; BLOCK]>,
    next: u64,
    voice: f32,
    last_sample: f32,
    from_model: bool,
    passed_through: u64,
    not_queued: u64,
}

impl Studio {
    pub(crate) fn new(kind: StudioKind) -> Self {
        let runner = if kind.guarded {
            Self::spawn(kind, false, None)
        } else {
            Runner::Inline { core: Box::new(Core::new(kind)), held: None }
        };
        Self::with_runner(kind, runner)
    }

    fn with_runner(kind: StudioKind, runner: Runner) -> Self {
        Self {
            kind,
            runner,
            raw: VecDeque::with_capacity(STUDIO_DELAY_BLOCKS + 1),
            next: 0,
            voice: 0.0,
            last_sample: 0.0,
            from_model: true,
            passed_through: 0,
            not_queued: 0,
        }
    }

    /// `gate`: held by a test to stall the thread.
    fn spawn(kind: StudioKind, patient: bool, gate: Option<Arc<Mutex<()>>>) -> Runner {
        let (jobs, inbox) = sync_channel::<(u64, [f32; BLOCK])>(QUEUE_BLOCKS);
        let (outbox, done) = channel::<Done>();
        let spawned = std::thread::Builder::new()
            .name("relay-studio".into())
            .spawn(move || {
                let mut core = Core::new(kind);
                while let Ok((number, mut block)) = inbox.recv() {
                    let _stalled = gate.as_ref().map(|gate| gate.lock());
                    core.process(&mut block);
                    if outbox.send((number, block, core.voice_probability())).is_err() {
                        break;
                    }
                }
            });
        if let Err(error) = &spawned {
            eprintln!("Studio noise suppression: no model thread ({error}); sending the microphone unprocessed");
        }
        Runner::Thread { jobs: spawned.is_ok().then_some(jobs), done, early: None, patient }
    }

    /// The model's output for block `wanted`, if it is there.
    fn take(
        done: &Receiver<Done>,
        early: &mut Option<Done>,
        wanted: u64,
        patient: bool,
    ) -> Option<([f32; BLOCK], Option<f32>)> {
        loop {
            let next = match early.take() {
                Some(result) => result,
                None if patient => done.recv_timeout(Duration::from_secs(10)).ok()?,
                None => done.try_recv().ok()?,
            };
            match next.0.cmp(&wanted) {
                // Too late to use: that block already went out.
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => return Some((next.1, next.2)),
                // `wanted` was never queued; keep this one for its turn.
                std::cmp::Ordering::Greater => {
                    *early = Some(next);
                    return None;
                }
            }
        }
    }
}

impl Suppressor for Studio {
    fn process(&mut self, block: &mut [f32; BLOCK]) {
        let input = *block;
        let number = self.next;
        self.next += 1;
        self.raw.push_back(input);
        let raw = if self.raw.len() > STUDIO_DELAY_BLOCKS {
            self.raw.pop_front().expect("raw is not empty")
        } else {
            [0.0; BLOCK]
        };
        let result = match &mut self.runner {
            Runner::Inline { core, held } => {
                let mut out = input;
                core.process(&mut out);
                let previous = held.replace((out, core.voice_probability()));
                Some(previous.unwrap_or(([0.0; BLOCK], None)))
            }
            Runner::Thread { jobs, done, early, patient } => {
                if let Some(sender) = jobs.as_ref() {
                    match sender.try_send((number, input)) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => self.not_queued += 1,
                        Err(TrySendError::Disconnected(_)) => *jobs = None,
                    }
                }
                match number.checked_sub(1) {
                    Some(wanted) => Self::take(done, early, wanted, *patient),
                    None => Some(([0.0; BLOCK], None)),
                }
            }
        };
        let (mut out, voice, from_model) = match result {
            Some((out, voice)) => (out, voice, true),
            None => {
                if number >= STUDIO_DELAY_BLOCKS as u64 {
                    self.passed_through += 1;
                    if !LOGGED_LATE.swap(true, Ordering::AcqRel) {
                        eprintln!("Studio noise suppression: the model thread fell behind; sending the microphone unprocessed until it catches up");
                    }
                }
                (raw, None, false)
            }
        };
        // Switching between the model and the raw microphone: start where the
        // last block ended and ease the difference out over the block, so the
        // switch is not a click.
        if from_model != self.from_model {
            let step = self.last_sample - out[0];
            for (n, sample) in out.iter_mut().enumerate() {
                *sample += step * (1.0 - n as f32 / BLOCK as f32);
            }
        }
        self.from_model = from_model;
        self.voice = voice.unwrap_or(0.0);
        for (to, from) in block.iter_mut().zip(out) {
            *to = if from.is_finite() { from.clamp(-1.0, 1.0) } else { 0.0 };
        }
        self.last_sample = block[BLOCK - 1];
    }

    fn delay_samples(&self) -> usize {
        DELAY_SAMPLES
    }

    fn name(&self) -> &'static str {
        studio_name(self.kind)
    }

    fn voice_probability(&self) -> Option<f32> {
        self.kind.probability.then_some(self.voice)
    }
}

impl Drop for Studio {
    fn drop(&mut self) {
        // Dropping the sender ends the model thread once it is done with what
        // it has; nothing here waits for it.
        if matches!(self.runner, Runner::Thread { patient: false, .. }) && self.next >= 500 {
            eprintln!(
                "Studio noise suppression: {} blocks, {} sent unprocessed because the model was late, {} not queued",
                self.next, self.passed_through, self.not_queued
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn run(suppressor: &mut dyn Suppressor, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        for chunk in input.chunks(BLOCK) {
            let mut block = [0.0f32; BLOCK];
            block[..chunk.len()].copy_from_slice(chunk);
            suppressor.process(&mut block);
            out.extend_from_slice(&block[..chunk.len()]);
        }
        out
    }

    fn unguarded(variant: Variant) -> Studio {
        Studio::new(StudioKind { variant, probability: false, guarded: false })
    }

    /// Opt-in check against another implementation's output on the same
    /// 16-bit WAV (Worker F's `dpprobe --f32-out`, not shifted back):
    /// DPDFNET_COMPARE_IN=<wav> DPDFNET_COMPARE_F32=<f32> [DPDFNET_COMPARE_MODEL=2]
    /// cargo test ... -- --ignored --nocapture compare_with_another_implementation
    #[test]
    #[ignore]
    fn compare_with_another_implementation() {
        let var = |name: &str| std::env::var_os(name).unwrap_or_else(|| panic!("{name}"));
        let bytes = std::fs::read(var("DPDFNET_COMPARE_IN")).unwrap();
        let mut at = 12;
        while &bytes[at..at + 4] != b"data" {
            at += 8 + u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        }
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let input: Vec<f32> = bytes[at + 8..at + 8 + size]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect();
        let other: Vec<f32> = std::fs::read(var("DPDFNET_COMPARE_F32"))
            .unwrap()
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let variant = match std::env::var("DPDFNET_COMPARE_MODEL").as_deref() {
            Ok("2") => Variant::Two,
            _ => Variant::Eight,
        };
        let generated = reference_input();
        if generated.len() == input.len() {
            let differ: Vec<usize> = (0..input.len()).filter(|&i| input[i] != generated[i]).collect();
            println!("WAV against reference_input(): {} samples differ, first {:?}", differ.len(), differ.first().map(|&i| (i, input[i] * 32768.0, generated[i] * 32768.0)));
        }
        let ours = run(&mut unguarded(variant), &input);
        let again = run(&mut unguarded(variant), &input);
        println!("two runs bit-identical: {}", ours == again);
        if let Some(path) = std::env::var_os("DPDFNET_COMPARE_DUMP") {
            std::fs::write(path, ours.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
        }
        // DPDFNET_COMPARE_OFFSET: the other output is that many samples earlier
        // (2400 for the offline Python reference).
        let offset: usize = std::env::var("DPDFNET_COMPARE_OFFSET").map_or(0, |v| v.parse().unwrap());
        // Studio holds the model's output back one block (the hand-off).
        let ours = &ours[(BLOCK + offset).min(ours.len())..];
        let n = ours.len().min(other.len());
        let blocks: Vec<f64> = (0..n / BLOCK)
            .map(|b| {
                let span = b * BLOCK..(b + 1) * BLOCK;
                let d: f64 = span.map(|i| ((ours[i] - other[i]) as f64).powi(2)).sum();
                (d / BLOCK as f64).sqrt()
            })
            .collect();
        let exact = blocks.iter().take_while(|&&d| d == 0.0).count();
        let total = (blocks.iter().map(|d| d * d).sum::<f64>() / blocks.len() as f64).sqrt();
        println!(
            "{}: {} blocks, first {exact} bit-identical, RMS difference {total:.3e}, per 10 blocks: {}",
            variant.name(),
            blocks.len(),
            blocks.chunks(10).map(|c| format!("{:.0e}", c.iter().cloned().fold(0.0, f64::max))).collect::<Vec<_>>().join(" ")
        );
    }

    /// 0.5 s of the synthetic talker with quiet white noise (-51 dBFS),
    /// quantised to 16 bits exactly as a WAV holds it, so an outside tool
    /// reads the same samples (`write_reference_input` writes them).
    fn reference_input() -> Vec<f32> {
        let quantise = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() / 32768.0;
        let mut seed = 0x2545_f491u32;
        crate::voice_test_signals::talker(0.5, (105.0, 135.0), 13, 0.3)
            .into_iter()
            .map(|speech| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                quantise(quantise(speech) + noise * 0.01)
            })
            .collect()
    }

    /// Worker F's `dpprobe` (docs/audio-dpdfnet-eval-20261029.md, the tool
    /// behind the bench's `replay:dpdfnet8`) on `reference_input()`, not
    /// shifted back, as 16-bit little-endian samples. On this input it is
    /// 1.3e-4 RMS from onnxruntime through the upstream Python script (56 dB
    /// below the signal); on the recorded clean voice 4.7e-8.
    static REFERENCE: &[u8] = include_bytes!("testdata/dpdfnet8_talker_dpprobe.i16");
    const REFERENCE_SHA256: &str = "ab2bf92e0914cd3fb7915a3f9d6db56eba677141a45b46324ce34aea05ce8b65";

    /// One unguarded dpdfnet8 run over the reference input, shared by the
    /// tests that need it.
    fn dpdfnet8_output() -> &'static [f32] {
        static OUTPUT: OnceLock<Vec<f32>> = OnceLock::new();
        OUTPUT.get_or_init(|| run(&mut unguarded(Variant::Eight), &reference_input()))
    }

    /// Writes `reference_input()` as a 48 kHz 16-bit WAV for outside tools:
    /// `DPDFNET_REFERENCE_INPUT=<path> cargo test ... -- --ignored write_reference_input`.
    #[test]
    #[ignore]
    fn write_reference_input() {
        let path = std::env::var_os("DPDFNET_REFERENCE_INPUT").expect("DPDFNET_REFERENCE_INPUT");
        crate::audio_bench::write_wav(std::path::Path::new(&path), &reference_input()).unwrap();
    }

    #[test]
    fn dpdfnet8_matches_the_replayed_probe_within_1e_4_rms() {
        let digest = Sha256::digest(REFERENCE);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, REFERENCE_SHA256, "the committed reference changed");
        let reference: Vec<f32> = REFERENCE
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect();
        let output = dpdfnet8_output();
        assert_eq!(reference.len(), output.len());
        // The model's output, one block later for the hand-off.
        let output = &output[BLOCK..];
        let (mut difference, mut power) = (0.0f64, 0.0f64);
        for (ours, theirs) in output.iter().zip(&reference) {
            difference += ((ours - theirs) as f64).powi(2);
            power += (*theirs as f64).powi(2);
        }
        let rms = (difference / output.len() as f64).sqrt();
        let level = (power / output.len() as f64).sqrt();
        assert_eq!(output.len(), reference.len() - BLOCK);
        println!("dpdfnet8 against dpprobe: RMS difference {rms:.2e} (16-bit reference), reference RMS {level:.3}");
        // The model must be doing something: the talker comes through.
        assert!(level > 0.02, "reference RMS {level}");
        assert!(rms <= 1e-4, "RMS difference {rms:.2e}");
    }

    #[test]
    fn declared_delay_matches_the_output() {
        let input = reference_input();
        let output = dpdfnet8_output();
        let measured = crate::audio_bench::measure_delay_samples(&input, output, 3_400);
        assert!(measured.abs_diff(DELAY_SAMPLES) <= 24, "measured {measured}");
        assert_eq!(unguarded(Variant::Two).delay_samples(), DELAY_SAMPLES);
    }

    #[test]
    fn the_ratio_probability_follows_speech_and_is_off_unless_asked() {
        let input = reference_input();
        let mut plain = unguarded(Variant::Eight);
        let mut ratio = Studio::new(StudioKind { variant: Variant::Eight, probability: true, guarded: false });
        let mut probabilities = Vec::new();
        for chunk in input.chunks_exact(BLOCK) {
            let mut a: [f32; BLOCK] = chunk.try_into().unwrap();
            let mut b = a;
            plain.process(&mut a);
            ratio.process(&mut b);
            assert_eq!(a, b, "the probability never changes the audio");
            assert_eq!(plain.voice_probability(), None);
            probabilities.push(ratio.voice_probability().unwrap());
        }
        assert!(probabilities.iter().all(|p| (0.0..=1.0).contains(p)));
        // The first 60 ms answer input from before the stream began.
        assert!(probabilities[..STUDIO_DELAY_BLOCKS].iter().all(|&p| p == 0.0));
        // Blocks answering speech keep most of their energy; the talker's
        // gaps (the 0.25 % noise alone) keep little.
        let (mut speech, mut gaps) = (Vec::new(), Vec::new());
        for (k, chunk) in input.chunks_exact(BLOCK).enumerate().take(probabilities.len() - STUDIO_DELAY_BLOCKS) {
            let level = energy(chunk.try_into().unwrap());
            let p = probabilities[k + STUDIO_DELAY_BLOCKS];
            if level > 1e-3 {
                speech.push(p);
            } else if level < 1e-5 {
                gaps.push(p);
            }
        }
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
        assert!(!speech.is_empty() && mean(&speech) > 0.5, "speech {speech:?}");
        assert!(mean(&gaps) < 0.3, "gaps {gaps:?}");
    }

    /// With a budget every block misses, the guard steps from dpdfnet8 to
    /// dpdfnet2 after OVERRUN_LIMIT blocks, then to RNNoise; every block
    /// still comes out, finite and without a jump at the switches, and the
    /// decision sticks for the next suppressor that shares it.
    #[test]
    fn the_overrun_guard_steps_down_without_dropping_a_block() {
        load_blocking(Variant::Eight).unwrap();
        load_blocking(Variant::Two).unwrap();
        let sticky: &'static Sticky = Box::leak(Box::default());
        let guard = Guard { budget: Duration::ZERO, run: 0, sticky };
        let kind = StudioKind { variant: Variant::Eight, probability: false, guarded: true };
        let mut studio = Core::with_guard(kind, Some(guard));
        assert_eq!(studio.active_variant(), Some(Variant::Eight));
        // A smooth tone: any hard switch between sources would be a step far
        // above the tone's own (0.1 * 2 pi * 220 / 48 000 = 0.003).
        let input: Vec<f32> = (0..30 * BLOCK)
            .map(|n| 0.1 * (2.0 * std::f32::consts::PI * 220.0 * n as f32 / 48_000.0).sin())
            .collect();
        let mut seen = Vec::new();
        let mut output = Vec::new();
        for chunk in input.chunks_exact(BLOCK).take(30) {
            let mut block: [f32; BLOCK] = chunk.try_into().unwrap();
            studio.process(&mut block);
            assert!(block.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
            output.extend_from_slice(&block);
            seen.push(studio.active_variant());
        }
        let limit = OVERRUN_LIMIT as usize;
        assert!(seen[..limit - 1].iter().all(|v| *v == Some(Variant::Eight)), "{seen:?}");
        assert_eq!(seen[limit - 1], None, "{seen:?}");
        let two = seen.iter().position(|v| *v == Some(Variant::Two)).expect("dpdfnet2 took over");
        assert_eq!(two, limit + WARM_UP_BLOCKS, "{seen:?}");
        assert!(seen[two..two + limit - 1].iter().all(|v| *v == Some(Variant::Two)), "{seen:?}");
        assert!(seen[two + limit - 1..].iter().all(|v| v.is_none()), "{seen:?}");
        assert!(sticky.eight_too_slow.load(Ordering::Acquire) && sticky.two_too_slow.load(Ordering::Acquire));
        let largest = (1..output.len())
            .map(|n| ((output[n] - output[n - 1]).abs(), n))
            .fold((0.0f32, 0), |a, b| if b.0 > a.0 { b } else { a });
        assert!(largest.0 <= 0.01, "step {} at sample {}", largest.0, largest.1);
        // The next suppressor sharing the decision starts on RNNoise.
        let next = Core::with_guard(kind, Some(Guard { budget: Duration::ZERO, run: 0, sticky }));
        assert_eq!((next.active_variant(), next.wanted), (None, None));
        assert!(!APP_STICKY.eight_too_slow.load(Ordering::Acquire), "the app's decision is its own");
    }

    /// Until a model is ready (or after both are given up), RNNoise is sent,
    /// delayed to the same 50 ms, so a switch lines up.
    #[test]
    fn the_stand_in_is_rnnoise_at_the_model_delay() {
        let sticky: &'static Sticky = Box::leak(Box::default());
        sticky.eight_too_slow.store(true, Ordering::Release);
        sticky.two_too_slow.store(true, Ordering::Release);
        let kind = StudioKind { variant: Variant::Eight, probability: true, guarded: true };
        let mut studio = Core::with_guard(kind, Some(Guard { budget: BLOCK_TIME, run: 0, sticky }));
        let mut rnnoise = RnNoise::new();
        let input = reference_input();
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        for chunk in input.chunks_exact(BLOCK) {
            let mut a: [f32; BLOCK] = chunk.try_into().unwrap();
            let mut b = a;
            studio.process(&mut a);
            rnnoise.process(&mut b);
            ours.extend_from_slice(&a);
            theirs.extend_from_slice(&b);
        }
        let late = FALLBACK_LINE_BLOCKS * BLOCK;
        assert!(ours[..late].iter().all(|&v| v == 0.0));
        assert_eq!(&ours[late..], &theirs[..theirs.len() - late]);
        assert_eq!(studio.delay_samples(), MODEL_DELAY_SAMPLES);
        assert!(studio.voice_probability().is_some());
    }

    #[test]
    fn embedded_models_are_the_vendored_files() {
        for (variant, bytes, sha) in [
            (Variant::Eight, 14_857_107, "7b3afbb260a08fe9af3d16e3bda992971be1e7e951d1dee7c2d235f5c43f5631"),
            (Variant::Two, 10_493_337, "7f0575a5cec0ba4ffd8f8bd657e06d007e4ccdd955d76faab922b9d3291dc14b"),
        ] {
            let digest = Sha256::digest(variant.bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!((variant.bytes().len(), hex.as_str()), (bytes, sha), "{variant:?}");
        }
    }

    /// The app's path, the model on its own thread, gives exactly the output
    /// the bench measures inline, at the same 60 ms, when the thread keeps up
    /// (this test waits for each result; the app never does).
    #[test]
    fn the_model_thread_gives_the_inline_output_at_the_same_delay() {
        let kind = StudioKind { variant: Variant::Eight, probability: true, guarded: false };
        let mut threaded = Studio::with_runner(kind, Studio::spawn(kind, true, None));
        let mut inline = Studio::new(kind);
        assert!(matches!(inline.runner, Runner::Inline { .. }));
        for chunk in reference_input().chunks_exact(BLOCK) {
            let mut a: [f32; BLOCK] = chunk.try_into().unwrap();
            let mut b = a;
            threaded.process(&mut a);
            inline.process(&mut b);
            assert_eq!(a, b);
            assert_eq!(threaded.voice_probability(), inline.voice_probability());
        }
        assert_eq!((threaded.passed_through, threaded.not_queued), (0, 0));
        assert_eq!(threaded.delay_samples(), 2_880);
    }

    /// A stuck model thread never holds the caller: every block comes back at
    /// once, as the microphone delayed to the same 60 ms, and dropping the
    /// suppressor does not wait for the thread either.
    #[test]
    fn a_stuck_model_thread_never_stalls_the_caller() {
        let kind = StudioKind { variant: Variant::Two, probability: false, guarded: false };
        let gate = Arc::new(Mutex::new(()));
        let stuck = gate.lock().unwrap();
        let mut studio = Studio::with_runner(kind, Studio::spawn(kind, false, Some(gate.clone())));
        let input = reference_input();
        let mut output = Vec::new();
        for chunk in input.chunks_exact(BLOCK) {
            let mut block: [f32; BLOCK] = chunk.try_into().unwrap();
            let started = Instant::now();
            studio.process(&mut block);
            assert!(started.elapsed() < Duration::from_millis(5), "{:?}", started.elapsed());
            output.extend_from_slice(&block);
        }
        let blocks = input.len() / BLOCK;
        assert_eq!(studio.passed_through, (blocks - STUDIO_DELAY_BLOCKS) as u64);
        // Block 0 is with the thread (or still queued) and QUEUE_BLOCKS wait.
        let not_queued = studio.not_queued as usize;
        assert!((blocks - 1 - QUEUE_BLOCKS..=blocks - QUEUE_BLOCKS).contains(&not_queued), "{not_queued}");
        assert!(output[..DELAY_SAMPLES].iter().all(|&v| v == 0.0));
        assert_eq!(&output[DELAY_SAMPLES..], &input[..input.len() - DELAY_SAMPLES]);
        let started = Instant::now();
        drop(studio);
        assert!(started.elapsed() < Duration::from_millis(5));
        drop(stuck);
    }

    /// Opt-in, release build: the app's path in real time, a block every
    /// 10 ms for 20 s with dpdfnet8 on its thread. Prints what the voice
    /// worker spends per block and how many blocks went out unprocessed:
    /// `cargo test --release -p relay-client -- --ignored --nocapture paced_hand_off`.
    #[test]
    #[ignore]
    fn paced_hand_off_in_real_time() {
        load_blocking(Variant::Eight).unwrap();
        let kind = StudioKind { variant: Variant::Eight, probability: false, guarded: false };
        let mut studio = Studio::with_runner(kind, Studio::spawn(kind, false, None));
        let input = reference_input();
        let blocks = 2_000;
        let mut times = Vec::with_capacity(blocks);
        let start = Instant::now();
        for index in 0..blocks {
            let due = start + BLOCK_TIME * index as u32;
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let at = (index * BLOCK) % (input.len() - BLOCK);
            let mut block: [f32; BLOCK] = input[at..at + BLOCK].try_into().unwrap();
            let started = Instant::now();
            studio.process(&mut block);
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "voice worker per block: p50 {:.4} ms, p99 {:.4} ms, max {:.4} ms; {} of {blocks} blocks sent unprocessed, {} not queued",
            times[blocks / 2],
            times[blocks * 99 / 100],
            times[blocks - 1],
            studio.passed_through,
            studio.not_queued
        );
    }
}
