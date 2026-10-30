//! Repeatable measurements of the send chain: a deterministic noise corpus,
//! objective speech and noise metrics, and adapters that run the app's own
//! microphone processing (`media::MicrophoneProcessor`) one 10 ms block at a
//! time. Nothing here records from a device or opens a socket.
//!
//! Run: `cargo run -p relay-client --release --features audio-bench --bin audio-bench -j 8`
//! (see `bench-corpus/README.md`). It prints a Markdown table and writes
//! `target/audio-bench/<chain>.json`.
// Tests use the measurement parts; the command line exists only in the bench build.
#![cfg_attr(not(feature = "audio-bench"), allow(dead_code))]

use crate::audio_settings::{AudioSettings, NoiseModel};
use crate::media::{AudioControls, MicrophoneProcessor};
use crate::voice_test_signals::talker;
use serde::Serialize;
use std::f64::consts::TAU;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const RATE: usize = 48_000;
pub const BLOCK: usize = 480;
/// Length of every corpus item.
pub const SECONDS: f32 = 10.0;
/// Longest delay the bench looks for in a chain that declares none: 30 ms.
/// A chain that declares a delay is searched around it instead, however long.
pub const MAX_DELAY_SAMPLES: usize = 1_440;

// ---------------------------------------------------------------- chains

/// One send-side processing chain, fed 10 ms blocks of 48 kHz mono audio.
/// A block the chain would not transmit comes back as silence.
pub trait SendChain {
    fn name(&self) -> &'static str;
    fn process_block(&mut self, block: &mut [f32; BLOCK]);
    /// How many samples later the chain's output follows its input.
    fn algorithmic_delay_samples(&self) -> usize;
}

/// No processing at all: the noisy microphone signal as it arrives.
pub struct Passthrough;

impl SendChain for Passthrough {
    fn name(&self) -> &'static str {
        "none"
    }
    fn process_block(&mut self, _block: &mut [f32; BLOCK]) {}
    fn algorithmic_delay_samples(&self) -> usize {
        0
    }
}

/// The app's own microphone processing with given settings: gain, RNNoise,
/// Enhanced click suppression, automatic volume and the voice gate, exactly
/// as a call runs them (`MicrophoneProcessor::process`). Echo cancellation
/// stays off, as it is by default.
pub struct CurrentChain {
    name: &'static str,
    controls: AudioControls,
    processor: MicrophoneProcessor,
    delay: usize,
}

impl CurrentChain {
    pub fn new(name: &'static str, settings: &AudioSettings) -> Self {
        Self {
            name,
            controls: AudioControls::new(settings),
            processor: MicrophoneProcessor::new(),
            // RNNoise's overlap-add synthesis returns each 10 ms frame one
            // frame late; nothing else in the chain looks ahead.
            delay: if settings.noise_suppression {
                nnnoiseless::DenoiseState::FRAME_SIZE
            } else {
                0
            },
        }
    }

    /// RNNoise alone ("Standard"), everything else at the shipped defaults.
    pub fn rnnoise() -> Self {
        Self::new(
            "rnnoise",
            &AudioSettings {
                noise_model: NoiseModel::Standard,
                ..Default::default()
            },
        )
    }

    /// The shipped default settings: RNNoise plus Enhanced click suppression
    /// and voice isolation at Normal strength, automatic volume, voice gate
    /// with automatic sensitivity.
    pub fn rnnoise_enhanced() -> Self {
        Self::new("rnnoise+enhanced", &AudioSettings::default())
    }
}

impl SendChain for CurrentChain {
    fn name(&self) -> &'static str {
        self.name
    }
    fn process_block(&mut self, block: &mut [f32; BLOCK]) {
        if !self.processor.process(block, &self.controls) {
            block.fill(0.0);
        }
    }
    fn algorithmic_delay_samples(&self) -> usize {
        self.delay
    }
}

/// A chain the bench can build afresh for every corpus item.
pub struct ChainSpec {
    pub name: &'static str,
    pub make: Box<dyn Fn() -> Box<dyn SendChain>>,
}

impl ChainSpec {
    pub fn new(name: &'static str, make: impl Fn() -> Box<dyn SendChain> + 'static) -> Self {
        Self { name, make: Box::new(make) }
    }
}

/// Every chain the bench measures, in table order. Add candidates here.
pub fn chains() -> Vec<ChainSpec> {
    vec![
        ChainSpec::new("none", || Box::new(Passthrough)),
        ChainSpec::new("rnnoise", || Box::new(CurrentChain::rnnoise())),
        ChainSpec::new("rnnoise+enhanced", || Box::new(CurrentChain::rnnoise_enhanced())),
    ]
    .into_iter()
    .chain(crate::noise_suppressor::bench::chains())
    .collect()
}

pub struct ChainResult {
    pub output: Vec<f32>,
    /// Mean processing time per 10 ms block, on this thread.
    pub cpu_ms_per_block: f32,
    pub block_cpu_ms: Vec<f32>,
    /// The chain's declared delay.
    pub delay_ms: f32,
}

/// Runs `input` through `chain` in 10 ms blocks (the last one padded with
/// silence) and times each block. The output has the input's length.
pub fn run_chain(chain: &mut dyn SendChain, input: &[f32]) -> ChainResult {
    let blocks = input.len().div_ceil(BLOCK);
    let mut output = Vec::with_capacity(blocks * BLOCK);
    let mut block_cpu_ms = Vec::with_capacity(blocks);
    for index in 0..blocks {
        let start = index * BLOCK;
        let end = (start + BLOCK).min(input.len());
        let mut block = [0.0f32; BLOCK];
        block[..end - start].copy_from_slice(&input[start..end]);
        let started = Instant::now();
        chain.process_block(&mut block);
        block_cpu_ms.push(started.elapsed().as_secs_f32() * 1000.0);
        output.extend_from_slice(&block);
    }
    output.truncate(input.len());
    let cpu_ms_per_block = block_cpu_ms.iter().sum::<f32>() / blocks.max(1) as f32;
    ChainResult {
        output,
        cpu_ms_per_block,
        block_cpu_ms,
        delay_ms: chain.algorithmic_delay_samples() as f32 * 1000.0 / RATE as f32,
    }
}

/// The lag (0..=`max_lag` samples) at which `output` best matches `input`.
pub fn measure_delay_samples(input: &[f32], output: &[f32], max_lag: usize) -> usize {
    measure_delay_between(input, output, 0, max_lag)
}

/// Only lags within this distance of a chain's declared delay are searched.
/// The talker repeats every pitch period (7-10 ms), so a full search can lock
/// onto the neighbouring period for a chain whose delay is not a whole number
/// of 10 ms blocks (WebRTC NS: 7 ms, found 2.8 ms on speech).
pub const DELAY_SEARCH_SAMPLES: usize = 96;

/// The lag in `first..=last` at which `output` best matches `input`.
pub fn measure_delay_between(input: &[f32], output: &[f32], first: usize, last: usize) -> usize {
    let span = input.len().min(output.len()).saturating_sub(last);
    let mut best = (first, f64::MIN);
    for lag in first..=last {
        let (mut cross, mut power) = (0.0f64, 0.0f64);
        for n in 0..span {
            let out = output[n + lag] as f64;
            cross += input[n] as f64 * out;
            power += out * out;
        }
        let score = if power > 0.0 { cross / power.sqrt() } else { f64::MIN };
        if score > best.1 {
            best = (lag, score);
        }
    }
    best.0
}

// --------------------------------------------------------------- metrics

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Metrics {
    /// Output energy in noise-only stretches (no speech in the clean
    /// signal) against the noisy input there. 0 dB: nothing removed.
    pub residual_noise_db: f32,
    /// Mean segmental SNR of the output against the clean speech over
    /// speech frames, after one least-squares gain (clamped to -10..35 dB).
    pub speech_segsnr_db: f32,
    /// Mean log-spectral distance between clean and output speech frames,
    /// after the same gain. 0 dB: the voice's spectrum is unchanged.
    pub lsd_db: f32,
    /// How much quieter the output speech is than the clean speech.
    pub speech_attenuation_db: f32,
}

/// Speech frames are within 30 dB of the loudest clean frame.
const ACTIVE_BELOW_PEAK: f64 = 1e-3;
/// Noise-only frames are more than 60 dB below it (or the clean is silent).
const QUIET_BELOW_PEAK: f64 = 1e-6;
/// Input noise is never taken as quieter than -70 dBFS, a real microphone's
/// self-noise, so a perfect result on a noise-free input stays finite.
const NOISE_FLOOR_POWER: f64 = 1e-7;
/// Numerical floor for output energy: -140 dBFS.
const OUTPUT_FLOOR_POWER: f64 = 1e-14;
const SEGSNR_RANGE: (f64, f64) = (-10.0, 35.0);
/// Spectral bins more than 50 dB below the frame's strongest clean bin
/// count as that floor: inaudible differences do not dominate the LSD.
const LSD_FLOOR_BELOW_PEAK: f64 = 1e-5;
const FFT_SIZE: usize = 512;

fn energy(samples: &[f32]) -> f64 {
    samples.iter().map(|s| (*s as f64) * (*s as f64)).sum()
}

fn db10(ratio: f64) -> f32 {
    (10.0 * ratio.log10()) as f32
}

struct Frames {
    active: Vec<usize>,
    quiet: Vec<usize>,
}

/// Classifies the 10 ms frames of `clean` whose delayed output also exists.
fn classify(clean: &[f32], processed_len: usize, delay: usize) -> Frames {
    let usable = clean.len().min(processed_len.saturating_sub(delay)) / BLOCK;
    let energies: Vec<f64> = (0..usable)
        .map(|f| energy(&clean[f * BLOCK..(f + 1) * BLOCK]))
        .collect();
    let peak = energies.iter().fold(0.0f64, |p, e| p.max(*e));
    let mut frames = Frames {
        active: Vec::new(),
        quiet: Vec::new(),
    };
    for (index, e) in energies.iter().enumerate() {
        if peak > 0.0 && *e >= peak * ACTIVE_BELOW_PEAK {
            frames.active.push(index * BLOCK);
        } else if peak <= 0.0 || *e <= peak * QUIET_BELOW_PEAK {
            frames.quiet.push(index * BLOCK);
        }
    }
    frames
}

fn floored_ratio_db(output: f64, input: f64, samples: usize) -> f32 {
    if samples == 0 {
        return f32::NAN;
    }
    let n = samples as f64;
    db10(output.max(n * OUTPUT_FLOOR_POWER) / input.max(n * NOISE_FLOOR_POWER))
}

pub fn metrics(clean: &[f32], noisy: &[f32], processed: &[f32], delay_samples: usize) -> Metrics {
    let d = delay_samples;
    let frames = classify(clean, processed.len(), d);
    let out = |start: usize| &processed[start + d..start + d + BLOCK];

    let (mut out_quiet, mut in_quiet) = (0.0, 0.0);
    for &start in &frames.quiet {
        out_quiet += energy(out(start));
        in_quiet += energy(&noisy[start..start + BLOCK]);
    }
    let residual_noise_db = floored_ratio_db(out_quiet, in_quiet, frames.quiet.len() * BLOCK);

    if frames.active.is_empty() {
        return Metrics {
            residual_noise_db,
            speech_segsnr_db: f32::NAN,
            lsd_db: f32::NAN,
            speech_attenuation_db: f32::NAN,
        };
    }
    let (mut cross, mut out_power, mut clean_power) = (0.0f64, 0.0f64, 0.0f64);
    for &start in &frames.active {
        for (c, p) in clean[start..start + BLOCK].iter().zip(out(start)) {
            cross += *c as f64 * *p as f64;
            out_power += *p as f64 * *p as f64;
            clean_power += *c as f64 * *c as f64;
        }
    }
    let gain = if out_power > 0.0 { cross / out_power } else { 1.0 };
    let n = (frames.active.len() * BLOCK) as f64;
    let speech_attenuation_db = db10(clean_power / out_power.max(n * OUTPUT_FLOOR_POWER));

    let mut segsnr = 0.0f64;
    let mut lsd = 0.0f64;
    for &start in &frames.active {
        let c = &clean[start..start + BLOCK];
        let p = out(start);
        let signal = energy(c);
        let error: f64 = c
            .iter()
            .zip(p)
            .map(|(c, p)| {
                let e = *c as f64 - gain * *p as f64;
                e * e
            })
            .sum();
        let snr = if error <= signal * 1e-12 {
            SEGSNR_RANGE.1
        } else {
            10.0 * (signal / error).log10()
        };
        segsnr += snr.clamp(SEGSNR_RANGE.0, SEGSNR_RANGE.1);
        lsd += log_spectral_distance(c, p, gain);
    }
    let count = frames.active.len() as f64;
    Metrics {
        residual_noise_db,
        speech_segsnr_db: (segsnr / count) as f32,
        lsd_db: (lsd / count) as f32,
        speech_attenuation_db,
    }
}

/// Output energy during speech that comes from the noise: the chain's output
/// for the noisy input minus its output for the clean input, against the
/// noise that went in. Catches clicks and music that pass while someone talks.
/// NaN when the row has no noise above -70 dBFS during speech.
pub fn speech_noise_leak_db(
    clean: &[f32],
    noisy: &[f32],
    processed_noisy: &[f32],
    processed_clean: &[f32],
    delay_samples: usize,
) -> f32 {
    let d = delay_samples;
    let len = processed_noisy.len().min(processed_clean.len());
    let frames = classify(clean, len, d);
    let (mut leaked, mut noise) = (0.0f64, 0.0f64);
    for &start in &frames.active {
        for offset in start..start + BLOCK {
            let difference = processed_noisy[offset + d] as f64 - processed_clean[offset + d] as f64;
            let input = noisy[offset] as f64 - clean[offset] as f64;
            leaked += difference * difference;
            noise += input * input;
        }
    }
    let samples = frames.active.len() * BLOCK;
    if noise < samples as f64 * NOISE_FLOOR_POWER {
        return f32::NAN;
    }
    floored_ratio_db(leaked, noise, samples)
}

fn power_spectrum(frame: &[f32], gain: f64) -> Vec<f64> {
    let mut re = vec![0.0f64; FFT_SIZE];
    let mut im = vec![0.0f64; FFT_SIZE];
    for (index, sample) in frame.iter().enumerate() {
        let window = 0.5 - 0.5 * (TAU * index as f64 / frame.len() as f64).cos();
        re[index] = *sample as f64 * gain * window;
    }
    fft(&mut re, &mut im);
    (0..=FFT_SIZE / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect()
}

fn log_spectral_distance(clean: &[f32], processed: &[f32], gain: f64) -> f64 {
    let reference = power_spectrum(clean, 1.0);
    let output = power_spectrum(processed, gain);
    let floor = reference.iter().fold(0.0f64, |p, v| p.max(*v)) * LSD_FLOOR_BELOW_PEAK + 1e-20;
    let sum: f64 = reference
        .iter()
        .zip(&output)
        .map(|(r, o)| {
            let difference = 10.0 * ((r.max(floor)) / (o.max(floor))).log10();
            difference * difference
        })
        .sum();
    (sum / reference.len() as f64).sqrt()
}

/// In-place iterative radix-2 FFT; the length must be a power of two.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut length = 2;
    while length <= n {
        let half = length / 2;
        let angle = -TAU / length as f64;
        for start in (0..n).step_by(length) {
            for k in 0..half {
                let (sin, cos) = (angle * k as f64).sin_cos();
                let (a, b) = (start + k, start + k + half);
                let real = re[b] * cos - im[b] * sin;
                let imaginary = re[b] * sin + im[b] * cos;
                re[b] = re[a] - real;
                im[b] = im[a] - imaginary;
                re[a] += real;
                im[a] += imaginary;
            }
        }
        length <<= 1;
    }
}

// ---------------------------------------------------------------- corpus

#[derive(Clone, Debug, PartialEq)]
pub struct CorpusItem {
    pub name: &'static str,
    pub clean: Vec<f32>,
    /// Already scaled so that `clean + noise` is at `snr_db`.
    pub noise: Vec<f32>,
    /// Active speech level over active noise level; NaN without speech.
    pub snr_db: f32,
}

impl CorpusItem {
    pub fn noisy(&self) -> Vec<f32> {
        self.clean.iter().zip(&self.noise).map(|(c, n)| c + n).collect()
    }
}

/// The eleven bench rows, synthesized deterministically (fixed seeds), all
/// 10 s of 48 kHz mono (or the length of `VOICE_TEST_WAV`). See
/// `bench-corpus/README.md`.
pub fn corpus() -> Vec<CorpusItem> {
    // The talker and seed of `enhanced_noise_suppression_removes_the_clicks_rnnoise_lets_through`.
    let synthetic = talker(SECONDS, (105.0, 135.0), 13, 0.3);
    // A recorded voice replaces it, at its own length (every row follows).
    let speech = match std::env::var_os("VOICE_TEST_WAV") {
        Some(path) => recorded_talker(Path::new(&path), active_power(&synthetic)),
        None => synthetic,
    };
    let len = speech.len();
    let fan_noise: Vec<f32> = {
        let fan = fan(len, 41);
        let mouse = mouse_train(len, (0.6, 1.1), 42);
        fan.iter().zip(&mouse).map(|(f, m)| f + m * 8.0).collect()
    };
    let laptop = laptop_fan(len, 61);
    let laptop_rms = (energy(&laptop) / len as f64).sqrt() as f32;
    // -45 dBFS RMS: a laptop fan heard by its built-in microphone.
    let floor_level = 10f32.powf(-45.0 / 20.0);
    vec![
        mix("keyboard_between_words", &speech, keyboard(&speech, false, 21).0, 10.0),
        mix("keyboard_during_speech", &speech, keyboard(&speech, true, 22).0, 10.0),
        mix("mouse_clicks", &speech, mouse_train(len, (0.23, 0.27), 31), 15.0),
        mix("fan_plus_mouse", &speech, fan_noise.clone(), 5.0),
        // The same fan and mouse, quieter: a fan further from the microphone.
        mix("fan_plus_mouse_15", &speech, fan_noise.clone(), 15.0),
        mix("fan_plus_mouse_10", &speech, fan_noise, 10.0),
        mix("music_bed", &speech, music_bed(len, 51), 5.0),
        // The same music, quieter: background music in a real room.
        mix("music_bed_12", &speech, music_bed(len, 51), 12.0),
        mix("second_talker", &speech, second_talker(len), 0.0),
        CorpusItem {
            name: "silence_floor",
            clean: vec![0.0; len],
            noise: laptop.iter().map(|s| s * floor_level / laptop_rms).collect(),
            snr_db: f32::NAN,
        },
        // No noise at all: does the chain leave a voice alone?
        CorpusItem {
            name: "clean_voice",
            noise: vec![0.0; len],
            clean: speech,
            snr_db: f32::NAN,
        },
    ]
}

/// A recorded 48 kHz mono 16-bit WAV, cut to whole blocks and scaled to
/// `active` so that the gain, gate and SNR figures start from the same speech
/// level as the synthetic talker. Its pauses must already be digital silence
/// (see `docs/audio-deepfilter-decision-20261029.md`), as the talker's are:
/// the keyboard row and the pause metrics look for them.
fn recorded_talker(path: &Path, active: f64) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("VOICE_TEST_WAV {}: {e}", path.display()));
    let field = |at: usize, width: usize| bytes[at..at + width].iter().rev().fold(0usize, |v, b| v << 8 | *b as usize);
    assert!(&bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE", "VOICE_TEST_WAV must be a WAV file");
    let (mut at, mut format, mut samples) = (12, None, None);
    while at + 8 <= bytes.len() {
        let size = field(at + 4, 4);
        match &bytes[at..at + 4] {
            b"fmt " => format = Some((field(at + 8, 2), field(at + 10, 2), field(at + 12, 4), field(at + 22, 2))),
            b"data" => samples = Some(&bytes[at + 8..(at + 8 + size).min(bytes.len())]),
            _ => {}
        }
        at += 8 + size + size % 2;
    }
    assert_eq!(format, Some((1, 1, RATE, 16)), "VOICE_TEST_WAV must be 48 kHz mono 16-bit PCM");
    let mut speech: Vec<f32> = samples
        .expect("VOICE_TEST_WAV has no data chunk")
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32_768.0)
        .collect();
    speech.truncate(speech.len() / BLOCK * BLOCK);
    let scale = (active / active_power(&speech)).sqrt() as f32;
    speech.iter_mut().for_each(|s| *s *= scale);
    speech
}

fn samples_for(seconds: f32) -> usize {
    (seconds * RATE as f32) as usize
}

/// Mean power of the 10 ms frames within 30 dB of the loudest one: the
/// active level of speech, a click train or a steady noise alike.
pub fn active_power(samples: &[f32]) -> f64 {
    let energies: Vec<f64> = samples.chunks_exact(BLOCK).map(energy).collect();
    let peak = energies.iter().fold(0.0f64, |p, e| p.max(*e));
    let active: Vec<f64> = energies
        .into_iter()
        .filter(|e| peak > 0.0 && *e >= peak * ACTIVE_BELOW_PEAK)
        .collect();
    active.iter().sum::<f64>() / (active.len().max(1) * BLOCK) as f64
}

fn mix(name: &'static str, clean: &[f32], noise: Vec<f32>, snr_db: f32) -> CorpusItem {
    let scale = (active_power(clean) / (active_power(&noise) * 10f64.powf(snr_db as f64 / 10.0))).sqrt() as f32;
    CorpusItem {
        name,
        clean: clean.to_vec(),
        noise: noise.into_iter().map(|s| s * scale).collect(),
        snr_db,
    }
}

struct Random(u32);

impl Random {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / 16_777_215.0
    }
    fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.next()
    }
    fn bipolar(&mut self) -> f32 {
        self.next() * 2.0 - 1.0
    }
}

/// Two-pole band-pass resonator.
struct Resonator {
    a1: f32,
    a2: f32,
    gain: f32,
    y1: f32,
    y2: f32,
}

impl Resonator {
    fn tuned(frequency: f32, bandwidth: f32) -> Self {
        let rate = RATE as f32;
        let r = (-std::f32::consts::PI * bandwidth / rate).exp();
        Self {
            a1: -2.0 * r * (std::f32::consts::TAU * frequency / rate).cos(),
            a2: r * r,
            gain: 1.0 - r,
            y1: 0.0,
            y2: 0.0,
        }
    }
    fn run(&mut self, x: f32) -> f32 {
        let y = self.gain * x - self.a1 * self.y1 - self.a2 * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

struct ClickShape {
    /// Resonance range (Hz) and bandwidth.
    tone: (f32, f32),
    bandwidth: f32,
    /// Exponential decay range in seconds.
    decay: (f32, f32),
    /// The key or button coming back up: delay range (s) and relative level.
    release: (f32, f32),
    release_level: f32,
}

const KEY: ClickShape = ClickShape {
    tone: (2_500.0, 6_000.0),
    bandwidth: 1_500.0,
    decay: (0.004, 0.010),
    release: (0.030, 0.060),
    release_level: 0.6,
};

const MOUSE: ClickShape = ClickShape {
    tone: (1_500.0, 4_000.0),
    bandwidth: 800.0,
    decay: (0.0015, 0.003),
    release: (0.070, 0.110),
    release_level: 0.7,
};

/// Adds one press-and-release click at `at` and returns where it ends.
fn add_click(out: &mut [f32], at: usize, random: &mut Random, shape: &ClickShape) -> usize {
    let mut resonator = Resonator::tuned(random.range(shape.tone.0, shape.tone.1), shape.bandwidth);
    let decay = random.range(shape.decay.0, shape.decay.1) * RATE as f32;
    let amplitude = random.range(0.5, 1.0);
    let length = (decay * 5.0) as usize;
    let release = (random.range(shape.release.0, shape.release.1) * RATE as f32) as usize;
    for (offset, level) in [(0, 1.0), (release, shape.release_level)] {
        for index in 0..length {
            let Some(slot) = out.get_mut(at + offset + index) else {
                break;
            };
            let noise = random.bipolar();
            let ring = resonator.run(noise) * 6.0;
            *slot += (noise * 0.5 + ring) * amplitude * level * (-(index as f32) / decay).exp();
        }
    }
    at + release + length
}

fn rms(samples: &[f32]) -> f32 {
    (energy(samples) / samples.len().max(1) as f64).sqrt() as f32
}

/// Mechanical typing, 4-8 keys a second: only inside pauses of `speech`,
/// or only on top of its words. Returns the noise and each key press's onset.
fn keyboard(speech: &[f32], during_words: bool, seed: u32) -> (Vec<f32>, Vec<usize>) {
    let mut random = Random(seed);
    let mut out = vec![0.0f32; speech.len()];
    let mut onsets = Vec::new();
    let loud = active_power(speech).sqrt() as f32;
    let mut at = samples_for(0.2);
    while at + samples_for(0.15) < speech.len() {
        let fits = if during_words {
            rms(&speech[at..at + samples_for(0.06)]) > 0.3 * loud
        } else {
            // The whole press and release must fall in the pause.
            rms(&speech[at - BLOCK..at + samples_for(0.12)]) < 1e-4
        };
        if fits {
            add_click(&mut out, at, &mut random, &KEY);
            onsets.push(at);
            at += samples_for(random.range(0.12, 0.25));
        } else {
            at += BLOCK;
        }
    }
    (out, onsets)
}

/// Mouse clicks at a steady pace (`interval` in seconds), peak about 1.
fn mouse_train(len: usize, interval: (f32, f32), seed: u32) -> Vec<f32> {
    let mut random = Random(seed);
    let mut out = vec![0.0f32; len];
    let mut at = samples_for(0.1);
    while at < len {
        add_click(&mut out, at, &mut random, &MOUSE);
        at += samples_for(random.range(interval.0, interval.1));
    }
    let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs())).max(1e-9);
    out.iter_mut().for_each(|s| *s /= peak);
    out
}

/// Pink noise (Paul Kellet's filter) at unit RMS.
fn pink(len: usize, seed: u32) -> Vec<f32> {
    let mut random = Random(seed);
    let mut b = [0.0f32; 7];
    let mut out: Vec<f32> = (0..len)
        .map(|_| {
            let white = random.bipolar();
            b[0] = 0.99886 * b[0] + white * 0.055_517_9;
            b[1] = 0.99332 * b[1] + white * 0.075_075_9;
            b[2] = 0.969 * b[2] + white * 0.153_852;
            b[3] = 0.8665 * b[3] + white * 0.310_485_6;
            b[4] = 0.55 * b[4] + white * 0.532_952_2;
            b[5] = -0.7616 * b[5] - white * 0.016_898;
            let sample = b.iter().sum::<f32>() + white * 0.5362;
            b[6] = white * 0.115_926;
            sample
        })
        .collect();
    normalize(&mut out);
    out
}

fn normalize(samples: &mut [f32]) {
    let level = rms(samples).max(1e-9);
    samples.iter_mut().for_each(|s| *s /= level);
}

/// A desk or room fan: low-passed pink noise plus a wavering blade hum, unit RMS.
fn fan(len: usize, seed: u32) -> Vec<f32> {
    let mut out = pink(len, seed);
    let mut low = 0.0f32;
    for (index, sample) in out.iter_mut().enumerate() {
        low += (*sample - low) * 0.18;
        let t = index as f64 / RATE as f64;
        let wobble = 1.0 + 0.1 * (TAU * 0.4 * t).sin();
        let hum: f64 = (1..=4).map(|k| (TAU * 87.0 * k as f64 * t).sin() * 0.3 / k as f64).sum();
        *sample = low + (hum * wobble) as f32;
    }
    normalize(&mut out);
    out
}

/// A laptop fan: pink noise with the rumble removed and a faint whine, unit RMS.
fn laptop_fan(len: usize, seed: u32) -> Vec<f32> {
    let mut out = pink(len, seed);
    let mut low = 0.0f32;
    for (index, sample) in out.iter_mut().enumerate() {
        low += (*sample - low) * 0.013;
        let t = index as f64 / RATE as f64;
        let whine = 0.05 * (1.0 + 0.3 * (TAU * 0.7 * t).sin()) * (TAU * 1_870.0 * t).sin();
        *sample = *sample - low + whine as f32;
    }
    normalize(&mut out);
    out
}

fn midi_hz(note: u8) -> f64 {
    440.0 * 2f64.powf((note as f64 - 69.0) / 12.0)
}

/// Harmonic tone: fundamental plus `harmonics` overtones falling as 1/k.
fn tone(frequency: f64, t: f64, harmonics: u32) -> f64 {
    (1..=harmonics)
        .map(|k| (TAU * frequency * k as f64 * t).sin() / k as f64)
        .sum()
}

/// Pop-like music at 100 BPM: a C-G-Am-F pad, plucked eighth-note
/// arpeggios, a bass on every beat, a kick drum and off-beat hi-hats.
fn music_bed(len: usize, seed: u32) -> Vec<f32> {
    const CHORDS: [([u8; 3], u8); 4] = [
        ([60, 64, 67], 36),
        ([55, 59, 62], 43),
        ([57, 60, 64], 45),
        ([53, 57, 60], 41),
    ];
    let beat = 0.6;
    let mut random = Random(seed);
    let mut out = vec![0.0f32; len];
    for (index, sample) in out.iter_mut().enumerate() {
        let t = index as f64 / RATE as f64;
        let beat_index = (t / beat) as usize;
        let in_beat = t - beat_index as f64 * beat;
        let (chord, root) = CHORDS[(beat_index / 4) % CHORDS.len()];
        let chord_start = (beat_index / 4) as f64 * 4.0 * beat;
        let swell = ((t - chord_start) / 0.05).min(1.0);
        let pad: f64 = chord.iter().map(|n| tone(midi_hz(*n), t, 6)).sum::<f64>() * 0.12 * swell;
        let eighth = (t / (beat / 2.0)) as usize;
        let in_eighth = t - eighth as f64 * beat / 2.0;
        let pluck_note = chord[eighth % 3] + 12;
        let pluck = tone(midi_hz(pluck_note), t, 5) * (-in_eighth / 0.12).exp() * 0.35;
        let bass = tone(midi_hz(root), t, 4) * (-in_beat / 0.25).exp() * 0.45;
        let kick_phase = TAU * (45.0 * in_beat + 65.0 * 0.03 * (1.0 - (-in_beat / 0.03).exp()));
        let kick = kick_phase.sin() * (-in_beat / 0.12).exp() * 0.8;
        let off_beat = in_beat - beat / 2.0;
        let hat = if off_beat >= 0.0 {
            random.bipolar() as f64 * (-off_beat / 0.03).exp() * 0.15
        } else {
            0.0
        };
        *sample = (pad + pluck + bass + kick + hat) as f32;
    }
    out
}

/// A second person in the room: the synthetic talker with a different voice,
/// generated at double speed and stretched to half speed (and half pitch).
fn second_talker(len: usize) -> Vec<f32> {
    let fast = talker(len as f32 / RATE as f32 / 2.0, (260.0, 340.0), 77, 0.3);
    (0..len)
        .map(|index| {
            let position = index as f32 * 0.5;
            let at = position as usize;
            let next = (at + 1).min(fast.len() - 1);
            let fraction = position - at as f32;
            fast[at.min(fast.len() - 1)] * (1.0 - fraction) + fast[next] * fraction
        })
        .collect()
}

// ------------------------------------------------------------------ CLI

#[derive(Serialize)]
struct RowReport {
    item: &'static str,
    snr_db: Option<f32>,
    residual_noise_db: Option<f32>,
    speech_noise_leak_db: Option<f32>,
    speech_segsnr_db: Option<f32>,
    lsd_db: Option<f32>,
    speech_attenuation_db: Option<f32>,
    cpu_ms_per_block: f32,
}

#[derive(Serialize)]
struct ChainReport {
    chain: &'static str,
    generated_at: String,
    build: &'static str,
    machine: String,
    declared_delay_ms: f32,
    measured_delay_ms: f32,
    cpu_ms_per_block_mean: f32,
    cpu_ms_per_block_p50: f32,
    cpu_ms_per_block_p99: f32,
    cpu_ms_per_block_max: f32,
    rows: Vec<RowReport>,
}

fn finite(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

fn cell(value: Option<f32>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.1}"))
}

fn percentile(values: &mut [f32], fraction: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    values[((values.len() - 1) as f32 * fraction).round() as usize]
}

pub(crate) fn output_directory() -> PathBuf {
    // target/<profile>/audio-bench.exe -> target/audio-bench
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("audio-bench")
}

/// A chain name as part of a file name (`replay:x` -> `replay_x`).
pub(crate) fn file_stem(chain: &str) -> String {
    chain.replace(|c: char| r#"<>:"/\|?*"#.contains(c), "_")
}

pub(crate) fn write_wav(path: &Path, samples: &[f32]) -> std::io::Result<()> {
    let data = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&(RATE as u32).to_le_bytes());
    bytes.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

const USAGE: &str = "audio-bench [--chain NAME]... [--write-wav]
Runs the noise corpus through each send chain and prints a Markdown table.
  --chain NAME   only this chain (repeatable); default: all
  --write-wav    also write the corpus and every chain's output as WAV
  --input-16bit  feed every chain the 16-bit samples the corpus WAVs hold,
                 as an outside tool behind a replay chain was fed
Environment:
  VOICE_TEST_WAV=<48 kHz mono 16-bit WAV>  a recorded talker for every row
  RELAY_REPLAY_DIR=<folder>  one chain `replay:<label>` per subfolder <label>
                 (see noise_suppressor::bench::Replay)
Results: target/audio-bench/<chain>.json";

/// Entry point of the `audio-bench` binary. Returns the process exit code.
pub fn run_cli() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    let mut wanted = Vec::new();
    let mut write_wavs = false;
    let mut input_16bit = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--chain" => match rest.next() {
                Some(name) => wanted.push(name.clone()),
                None => {
                    eprintln!("--chain needs a name\n{USAGE}");
                    return 2;
                }
            },
            "--write-wav" => write_wavs = true,
            "--input-16bit" => input_16bit = true,
            other => {
                eprintln!("Unknown argument {other}\n{USAGE}");
                return 2;
            }
        }
    }
    let specs: Vec<ChainSpec> = chains()
        .into_iter()
        .filter(|spec| wanted.is_empty() || wanted.iter().any(|w| w == spec.name))
        .collect();
    if specs.is_empty() {
        let names: Vec<&str> = chains().iter().map(|s| s.name).collect();
        eprintln!("No such chain. Chains: {}", names.join(", "));
        return 2;
    }
    let directory = output_directory();
    let wav_directory = directory.join("wav");
    if let Err(error) = std::fs::create_dir_all(if write_wavs { &wav_directory } else { &directory }) {
        eprintln!("Cannot create {}: {error}", directory.display());
        return 1;
    }
    let corpus = corpus();
    let noisy: Vec<Vec<f32>> = corpus.iter().map(CorpusItem::noisy).collect();
    // What each chain is fed: the corpus, or the corpus as `write_wav` stores
    // it. Metrics always compare with the exact clean speech.
    let as_fed = |samples: &[f32]| -> Vec<f32> {
        if input_16bit {
            samples.iter().map(|v| (v.clamp(-1.0, 1.0) * 32767.0).round() / 32768.0).collect()
        } else {
            samples.to_vec()
        }
    };
    let fed_noisy: Vec<Vec<f32>> = noisy.iter().map(|v| as_fed(v)).collect();
    let fed_clean: Vec<Vec<f32>> = corpus.iter().map(|item| as_fed(&item.clean)).collect();
    if write_wavs {
        for (item, noisy) in corpus.iter().zip(&noisy) {
            for (suffix, samples) in [("clean", &item.clean), ("noise", &item.noise), ("noisy", noisy)] {
                let path = wav_directory.join(format!("{}.{suffix}.wav", item.name));
                if let Err(error) = write_wav(&path, samples) {
                    eprintln!("Cannot write {}: {error}", path.display());
                    return 1;
                }
            }
        }
    }
    let build = if cfg!(debug_assertions) { "debug" } else { "release" };
    let machine = format!(
        "{} ({} logical processors)",
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown CPU".to_owned()),
        std::thread::available_parallelism().map_or(0, |n| n.get()),
    );
    let generated_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut reports = Vec::new();
    for spec in &specs {
        // Delay as measured on the clean speech of the first row; every
        // speech row uses the same talker.
        let reference = &corpus[0].clean;
        let mut chain = (spec.make)();
        assert_eq!(chain.name(), spec.name, "chain list and chain disagree");
        let declared = run_chain(&mut *chain, &fed_clean[0]);
        let declared_samples = chain.algorithmic_delay_samples();
        let measured = if declared_samples == 0 {
            measure_delay_samples(reference, &declared.output, MAX_DELAY_SAMPLES)
        } else {
            measure_delay_between(
                reference,
                &declared.output,
                declared_samples.saturating_sub(DELAY_SEARCH_SAMPLES),
                declared_samples + DELAY_SEARCH_SAMPLES,
            )
        };
        let mut rows = Vec::new();
        let mut all_blocks = Vec::new();
        for (index, (item, noisy)) in corpus.iter().zip(&noisy).enumerate() {
            let processed = run_chain(&mut *(spec.make)(), &fed_noisy[index]);
            let processed_clean = run_chain(&mut *(spec.make)(), &fed_clean[index]);
            let m = metrics(&item.clean, noisy, &processed.output, measured);
            let leak = speech_noise_leak_db(&item.clean, noisy, &processed.output, &processed_clean.output, measured);
            if write_wavs {
                let path = wav_directory.join(format!("{}.{}.wav", item.name, file_stem(spec.name)));
                if let Err(error) = write_wav(&path, &processed.output) {
                    eprintln!("Cannot write {}: {error}", path.display());
                    return 1;
                }
            }
            all_blocks.extend_from_slice(&processed.block_cpu_ms);
            rows.push(RowReport {
                item: item.name,
                snr_db: finite(item.snr_db),
                residual_noise_db: finite(m.residual_noise_db),
                speech_noise_leak_db: finite(leak),
                speech_segsnr_db: finite(m.speech_segsnr_db),
                lsd_db: finite(m.lsd_db),
                speech_attenuation_db: finite(m.speech_attenuation_db),
                cpu_ms_per_block: processed.cpu_ms_per_block,
            });
        }
        let mean = all_blocks.iter().sum::<f32>() / all_blocks.len().max(1) as f32;
        let max = all_blocks.iter().fold(0.0f32, |m, v| m.max(*v));
        let p50 = percentile(&mut all_blocks, 0.5);
        let p99 = percentile(&mut all_blocks, 0.99);
        let report = ChainReport {
            chain: spec.name,
            generated_at: generated_at.clone(),
            build,
            machine: machine.clone(),
            declared_delay_ms: declared.delay_ms,
            measured_delay_ms: measured as f32 * 1000.0 / RATE as f32,
            cpu_ms_per_block_mean: mean,
            cpu_ms_per_block_p50: p50,
            cpu_ms_per_block_p99: p99,
            cpu_ms_per_block_max: max,
            rows,
        };
        let path = directory.join(format!("{}.json", file_stem(spec.name)));
        let json = serde_json::to_string_pretty(&report).expect("bench report serializes");
        if let Err(error) = std::fs::write(&path, json) {
            eprintln!("Cannot write {}: {error}", path.display());
            return 1;
        }
        reports.push(report);
    }
    print_tables(&reports);
    println!("\nJSON: {}", directory.display());
    0
}

fn print_tables(reports: &[ChainReport]) {
    let first = &reports[0];
    println!("Audio bench, {} build, {}, {}\n", first.build, first.machine, first.generated_at);
    println!("| chain | item | SNR in (dB) | residual noise in pauses (dB) | noise leak during speech (dB) | speech segSNR (dB) | LSD (dB) | speech attenuation (dB) | CPU (ms / 10 ms block) |");
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for report in reports {
        for row in &report.rows {
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {:.3} |",
                report.chain,
                row.item,
                cell(row.snr_db),
                cell(row.residual_noise_db),
                cell(row.speech_noise_leak_db),
                cell(row.speech_segsnr_db),
                row.lsd_db.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.2}")),
                cell(row.speech_attenuation_db),
                row.cpu_ms_per_block,
            );
        }
    }
    println!("\n| chain | declared delay (ms) | measured delay (ms) | CPU mean (ms / 10 ms block) | CPU p50 | CPU p99 | CPU max |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
    for report in reports {
        println!(
            "| {} | {:.2} | {:.2} | {:.3} | {:.3} | {:.3} | {:.3} |",
            report.chain,
            report.declared_delay_ms,
            report.measured_delay_ms,
            report.cpu_ms_per_block_mean,
            report.cpu_ms_per_block_p50,
            report.cpu_ms_per_block_p99,
            report.cpu_ms_per_block_max,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech(seconds: f32) -> Vec<f32> {
        talker(seconds, (105.0, 135.0), 13, 0.3)
    }

    #[test]
    fn metrics_of_identical_signals_are_perfect() {
        let clean = speech(2.0);
        let m = metrics(&clean, &clean, &clean, 0);
        assert!(m.residual_noise_db < -60.0 && m.lsd_db < 0.1, "{m:?}");
        assert!(m.speech_segsnr_db >= 34.9 && m.speech_attenuation_db.abs() < 0.01, "{m:?}");
    }

    #[test]
    fn metrics_align_for_a_known_delay() {
        let clean = speech(2.0);
        let mut processed = vec![0.0f32; BLOCK];
        processed.extend_from_slice(&clean[..clean.len() - BLOCK]);
        let aligned = metrics(&clean, &clean, &processed, BLOCK);
        assert!(aligned.lsd_db < 0.1 && aligned.speech_segsnr_db >= 34.9, "{aligned:?}");
        let misaligned = metrics(&clean, &clean, &processed, 0);
        assert!(misaligned.lsd_db > 1.0 && misaligned.speech_segsnr_db < 10.0, "{misaligned:?}");
        assert_eq!(measure_delay_samples(&clean, &processed, 960), BLOCK);
    }

    #[test]
    fn metrics_see_noise_in_pauses_and_a_quieter_voice() {
        let item = &corpus()[2];
        let noisy = item.noisy();
        let untouched = metrics(&item.clean, &noisy, &noisy, 0);
        assert!(untouched.residual_noise_db.abs() < 0.01, "{untouched:?}");
        let half: Vec<f32> = item.clean.iter().map(|s| s * 0.5).collect();
        let quieter = metrics(&item.clean, &noisy, &half, 0);
        assert!((quieter.speech_attenuation_db - 6.02).abs() < 0.01, "{quieter:?}");
        // One gain for the whole file: a uniformly quieter voice is not distortion.
        assert!(quieter.lsd_db < 0.1 && quieter.speech_segsnr_db >= 34.9, "{quieter:?}");
        assert!(quieter.residual_noise_db < -60.0, "{quieter:?}");
        let leak = speech_noise_leak_db(&item.clean, &noisy, &noisy, &item.clean, 0);
        assert!(leak.abs() < 0.01, "untouched noise leaks fully: {leak}");
        let none = speech_noise_leak_db(&item.clean, &item.clean, &item.clean, &item.clean, 0);
        assert!(none.is_nan(), "no noise during speech: nothing to leak");
    }

    #[test]
    fn passthrough_chain_reports_zero_delay_and_unchanged_audio() {
        let input: Vec<f32> = speech(1.0).into_iter().take(RATE - 100).collect();
        let result = run_chain(&mut Passthrough, &input);
        assert_eq!(result.output, input);
        assert_eq!(result.delay_ms, 0.0);
        assert_eq!(result.block_cpu_ms.len(), input.len().div_ceil(BLOCK));
    }

    #[test]
    fn corpus_items_are_deterministic_and_48k() {
        let first = corpus();
        let second = corpus();
        let names: Vec<&str> = first.iter().map(|item| item.name).collect();
        assert_eq!(
            names,
            [
                "keyboard_between_words",
                "keyboard_during_speech",
                "mouse_clicks",
                "fan_plus_mouse",
                "fan_plus_mouse_15",
                "fan_plus_mouse_10",
                "music_bed",
                "music_bed_12",
                "second_talker",
                "silence_floor",
                "clean_voice"
            ]
        );
        for (a, b) in first.iter().zip(&second) {
            let bits = |v: &[f32]| v.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&a.clean), bits(&b.clean), "{}", a.name);
            assert_eq!(bits(&a.noise), bits(&b.noise), "{}", a.name);
            assert_eq!(a.clean.len(), a.noise.len(), "{}", a.name);
            assert!((8 * RATE..=15 * RATE).contains(&a.clean.len()), "{}", a.name);
            let silent = a.name == "clean_voice";
            assert!(a.noise.iter().all(|s| s.is_finite()) && (rms(&a.noise) > 0.0) != silent, "{}", a.name);
            let peak = a.noisy().iter().fold(0.0f32, |p, s| p.max(s.abs()));
            assert!(peak < 1.0, "{} clips: {peak}", a.name);
            if a.snr_db.is_finite() {
                let snr = 10.0 * (active_power(&a.clean) / active_power(&a.noise)).log10();
                assert!((snr as f32 - a.snr_db).abs() < 0.01, "{}: {snr}", a.name);
            }
        }
    }

    #[test]
    fn music_bed_12_is_the_same_music_7_db_quieter_and_clean_voice_is_the_voice_alone() {
        let corpus = corpus();
        let row = |name: &str| corpus.iter().find(|item| item.name == name).unwrap();
        // The quieter fan rows: the same fan and mouse, 10 and 5 dB down.
        for (name, down) in [("fan_plus_mouse_15", 10.0f32), ("fan_plus_mouse_10", 5.0)] {
            let ratio = 10f32.powf(-down / 20.0);
            for (a, b) in row("fan_plus_mouse").noise.iter().zip(&row(name).noise) {
                assert!((a * ratio - b).abs() <= 1e-6, "{name}: {a} {b}");
            }
        }
        let (loud, quiet) = (row("music_bed"), row("music_bed_12"));
        let ratio = 10f32.powf(-7.0 / 20.0);
        assert_eq!(loud.clean, quiet.clean);
        for (a, b) in loud.noise.iter().zip(&quiet.noise) {
            assert!((a * ratio - b).abs() <= 1e-6, "{a} {b}");
        }
        let clean = row("clean_voice");
        assert_eq!(clean.clean, loud.clean);
        assert!(clean.snr_db.is_nan() && clean.noisy() == clean.clean);
        // Metrics against itself: perfect, and no leak to measure.
        let m = metrics(&clean.clean, &clean.clean, &clean.clean, 0);
        assert!(m.lsd_db < 0.1 && m.speech_attenuation_db.abs() < 0.01, "{m:?}");
        assert!(speech_noise_leak_db(&clean.clean, &clean.clean, &clean.clean, &clean.clean, 0).is_nan());
    }

    #[test]
    fn chain_names_become_file_names() {
        assert_eq!(file_stem("replay:dpdfnet8-lim24"), "replay_dpdfnet8-lim24");
        assert_eq!(file_stem("rnnoise+enhanced"), "rnnoise+enhanced");
    }

    #[test]
    fn keyboard_rows_put_clicks_in_pauses_or_on_words() {
        let clean = talker(SECONDS, (105.0, 135.0), 13, 0.3);
        assert_eq!(clean, corpus()[0].clean, "the rows use this talker");
        let loud = active_power(&clean).sqrt() as f32;
        let (between_noise, between) = keyboard(&clean, false, 21);
        let (_, during) = keyboard(&clean, true, 22);
        assert!(between.len() >= 8 && during.len() >= 20, "{} / {}", between.len(), during.len());
        // A press, its ring and its release all fall inside the pause.
        for &at in &between {
            assert!(rms(&clean[at - BLOCK..at + samples_for(0.12)]) < 1e-4, "{at}");
            assert!(between_noise[at..at + BLOCK].iter().any(|s| s.abs() > 1e-3), "{at}");
        }
        for &at in &during {
            assert!(rms(&clean[at..at + samples_for(0.06)]) > 0.3 * loud, "{at}");
        }
    }

    #[test]
    fn current_chains_declare_the_delay_they_measure() {
        let clean = speech(3.0);
        for make in [CurrentChain::rnnoise, CurrentChain::rnnoise_enhanced] {
            let mut chain = make();
            let result = run_chain(&mut chain, &clean);
            let measured = measure_delay_samples(&clean, &result.output, 960);
            // RNNoise holds one 480-sample frame; its filters move the best
            // match a few samples (0.06 ms measured) but never by 1 ms.
            let declared = chain.algorithmic_delay_samples();
            assert!(measured.abs_diff(declared) < RATE / 1000, "{}: {measured} vs {declared}", chain.name());
            assert_eq!(result.delay_ms, 10.0, "{}", chain.name());
        }
    }
}
