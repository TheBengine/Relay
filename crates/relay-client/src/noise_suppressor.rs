//! Broadband noise suppressors for the microphone, behind one interface so
//! the send chain (`media::MicrophoneProcessor`) and the audio bench run the
//! same code. Every suppressor takes 10 ms blocks of 48 kHz mono PCM in
//! [-1, 1], works in place, and says how many samples late its output is.
//!
//! The choice between them was measured with the audio bench; see
//! `docs/audio-suppressor-comparison-20261028.md`.
use nnnoiseless::DenoiseState;

/// One 10 ms block at 48 kHz.
pub(crate) const BLOCK: usize = 480;

pub(crate) trait Suppressor: Send {
    fn process(&mut self, block: &mut [f32; BLOCK]);
    /// How many samples later the output follows the input. The app's delay
    /// budget still counts RNNoise's fixed 10 ms; the bench reads this.
    #[cfg_attr(not(any(test, feature = "audio-bench")), allow(dead_code))]
    fn delay_samples(&self) -> usize;
    /// For reports and test messages.
    #[cfg_attr(not(test), allow(dead_code))]
    fn name(&self) -> &'static str;
    /// Speech probability (0..1) of the block just processed, when the
    /// suppressor estimates one. Click suppression needs it to tell a click
    /// from a consonant.
    fn voice_probability(&self) -> Option<f32>;
    fn status(&self) -> crate::audio_model_lab::Status { crate::audio_model_lab::Status::Active }
}

/// WebRTC noise suppression strength: the most it takes off steady noise.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NsLevel {
    /// 6 dB
    Low = 0,
    /// 12 dB, WebRTC's default
    Moderate = 1,
    /// 18 dB
    High = 2,
    /// 21 dB
    VeryHigh = 3,
}

// Only the bench and tests choose the alternatives today.
#[cfg_attr(not(any(test, feature = "audio-bench")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SuppressorKind {
    Off,
    RnNoise,
    WebRtcNs(NsLevel),
    /// RNNoise first, then WebRTC NS on what is left.
    RnNoiseThenWebRtcNs(NsLevel),
    /// WebRTC NS first, then RNNoise.
    WebRtcNsThenRnNoise(NsLevel),
    /// DPDFNet (Studio): removes music and other voices, 60 ms.
    DpdfNet(crate::dpdfnet::StudioKind),
    Comparison(crate::audio_model_lab::Model),
}

/// Studio gives click suppression no voice probability: measured on the
/// bench, see `docs/audio-studio-level-20261030.md`.
pub(crate) const STUDIO_CLICKS: bool = false;

/// Studio holds back no keys struck while the user talks: every key-hold
/// candidate (before the model, after it, guided by what the model removed)
/// let as much or more of the keys through and most changed the voice; see
/// `docs/audio-keys-on-words-20261030.md`. They stay on the bench.
pub(crate) const STUDIO_KEYS: Option<crate::click_suppression::KeyHoldParams> = None;

/// The suppressor for the noise settings: RNNoise for Standard and for
/// every Enhanced strength, DPDFNet (dpdfnet8, with the overrun guard) for
/// Studio. The measured alternatives to RNNoise (WebRTC NS in front of it,
/// DeepFilterNet) each lost somewhere the current chain holds; see
/// docs/audio-suppressor-comparison-20261028.md before changing this.
pub(crate) fn for_setting(
    model: crate::audio_settings::NoiseModel,
    _strength: crate::click_suppression::NoiseStrength,
) -> SuppressorKind {
    if let Some(model) = model.comparison() { return SuppressorKind::Comparison(model); }
    match model {
        crate::audio_settings::NoiseModel::Enhanced | crate::audio_settings::NoiseModel::Standard => {
            SuppressorKind::RnNoise
        }
        crate::audio_settings::NoiseModel::Studio => SuppressorKind::DpdfNet(crate::dpdfnet::StudioKind {
            variant: crate::dpdfnet::Variant::Eight,
            probability: STUDIO_CLICKS,
            guarded: true,
            keys_before: STUDIO_KEYS,
            keys_after: STUDIO_KEYS,
        }),
        crate::audio_settings::NoiseModel::WebRtc => SuppressorKind::WebRtcNs(NsLevel::High),
        crate::audio_settings::NoiseModel::RnNoiseThenWebRtc => SuppressorKind::RnNoiseThenWebRtcNs(NsLevel::High),
        crate::audio_settings::NoiseModel::WebRtcThenRnNoise => SuppressorKind::WebRtcNsThenRnNoise(NsLevel::High),
        _ => unreachable!("comparison models handled above"),
    }
}

pub(crate) fn make(kind: SuppressorKind) -> Box<dyn Suppressor> {
    match kind {
        SuppressorKind::Off => Box::new(Off),
        SuppressorKind::RnNoise => Box::new(RnNoise::new()),
        SuppressorKind::DpdfNet(kind) => Box::new(crate::dpdfnet::Studio::new(kind)),
        SuppressorKind::Comparison(model) => Box::new(crate::audio_model_lab::Comparison::new(crate::audio_model_lab::Service::new(), model, BLOCK)),
        #[cfg(feature = "native-rtc")]
        SuppressorKind::WebRtcNs(level) => Box::new(WebRtcNs::new(level)),
        #[cfg(feature = "native-rtc")]
        SuppressorKind::RnNoiseThenWebRtcNs(level) => Box::new(Chain {
            name: "rnnoise-then-webrtc-ns",
            first: Box::new(RnNoise::new()),
            second: Box::new(WebRtcNs::new(level)),
        }),
        #[cfg(feature = "native-rtc")]
        SuppressorKind::WebRtcNsThenRnNoise(level) => Box::new(Chain {
            name: "webrtc-ns-then-rnnoise",
            first: Box::new(WebRtcNs::new(level)),
            second: Box::new(RnNoise::new()),
        }),
        // A saved native-only choice is unavailable in a portable build.
        #[cfg(not(feature = "native-rtc"))]
        _ => Box::new(Unavailable),
    }
}

#[cfg(not(feature = "native-rtc"))]
struct Unavailable;
#[cfg(not(feature = "native-rtc"))]
impl Suppressor for Unavailable {
    fn process(&mut self, block: &mut [f32; BLOCK]) { block.fill(0.0); }
    fn delay_samples(&self) -> usize { 0 }
    fn name(&self) -> &'static str { "unavailable" }
    fn voice_probability(&self) -> Option<f32> { None }
    fn status(&self) -> crate::audio_model_lab::Status { crate::audio_model_lab::Status::Unavailable }
}

// ------------------------------------------------------------------- off

struct Off;

impl Suppressor for Off {
    fn process(&mut self, _block: &mut [f32; BLOCK]) {}
    fn delay_samples(&self) -> usize {
        0
    }
    fn name(&self) -> &'static str {
        "off"
    }
    fn voice_probability(&self) -> Option<f32> {
        None
    }
}

// --------------------------------------------------------------- RNNoise

/// RNNoise (nnnoiseless) with its built-in model. Its overlap-add synthesis
/// returns each frame one frame (10 ms) late; the very first frame, which has
/// no history, is sent as silence.
pub(crate) struct RnNoise {
    state: Box<DenoiseState<'static>>,
    first: bool,
    voice: f32,
}

impl RnNoise {
    pub(crate) fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            first: true,
            voice: 0.0,
        }
    }
}

impl Suppressor for RnNoise {
    fn process(&mut self, block: &mut [f32; BLOCK]) {
        let mut source = [0.0; DenoiseState::FRAME_SIZE];
        let mut output = [0.0; DenoiseState::FRAME_SIZE];
        for (to, from) in source.iter_mut().zip(block.iter()) {
            *to = (*from * 32768.0).clamp(-32768.0, 32767.0);
        }
        self.voice = self.state.process_frame(&mut output, &source);
        if self.first {
            output.fill(0.0);
            self.first = false;
        }
        for (to, from) in block.iter_mut().zip(output) {
            *to = if from.is_finite() {
                (from / 32768.0).clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
    }
    fn delay_samples(&self) -> usize {
        DenoiseState::FRAME_SIZE
    }
    fn name(&self) -> &'static str {
        "rnnoise"
    }
    fn voice_probability(&self) -> Option<f32> {
        Some(self.voice)
    }
}

// ------------------------------------------------------------ WebRTC NS

/// WebRTC's noise suppressor from the already linked audio processing
/// module, with its high-pass filter, and nothing else of it (no echo
/// canceller, no gain control). Level set through Relay's patched binding.
#[cfg(feature = "native-rtc")]
pub(crate) struct WebRtcNs {
    apm: cxx::UniquePtr<webrtc_sys::apm::ffi::AudioProcessingModule>,
    failed: bool,
}

/// 7.0 ms: the three-band split at 48 kHz plus the suppressor's overlap.
/// Measured on clicks (`declared_delays_match_what_clicks_show`).
#[cfg(feature = "native-rtc")]
#[cfg_attr(not(any(test, feature = "audio-bench")), allow(dead_code))]
const WEBRTC_NS_DELAY_SAMPLES: usize = 335;

#[cfg(feature = "native-rtc")]
impl WebRtcNs {
    pub(crate) fn new(level: NsLevel) -> Self {
        let apm = webrtc_sys::apm::ffi::create_apm_with_ns_level(false, false, true, true, level as i32);
        Self {
            failed: apm.is_null(),
            apm,
        }
    }
}

#[cfg(feature = "native-rtc")]
impl Suppressor for WebRtcNs {
    fn process(&mut self, block: &mut [f32; BLOCK]) {
        if self.failed {
            block.fill(0.0);
            return;
        }
        let mut frame = [0i16; BLOCK];
        for (to, from) in frame.iter_mut().zip(block.iter()) {
            *to = if from.is_finite() {
                (from.clamp(-1.0, 1.0) * 32767.0).round() as i16
            } else {
                0
            };
        }
        // SAFETY: source and destination are the same live 480-sample
        // buffer, which is exactly one 10 ms mono frame at 48 kHz.
        let status = unsafe {
            self.apm.pin_mut().process_stream(
                frame.as_ptr(),
                BLOCK,
                frame.as_mut_ptr(),
                BLOCK,
                48_000,
                1,
            )
        };
        if status != 0 {
            // A module that failed once is not trusted again: pass the
            // microphone through rather than send garbage.
            self.failed = true;
            block.fill(0.0);
            return;
        }
        for (to, from) in block.iter_mut().zip(frame) {
            *to = from as f32 / 32768.0;
        }
    }
    fn delay_samples(&self) -> usize {
        WEBRTC_NS_DELAY_SAMPLES
    }
    fn name(&self) -> &'static str {
        "webrtc-ns"
    }
    fn status(&self) -> crate::audio_model_lab::Status {
        if self.failed { crate::audio_model_lab::Status::Unavailable } else { crate::audio_model_lab::Status::Active }
    }
    fn voice_probability(&self) -> Option<f32> {
        None
    }
}

// -------------------------------------------------------------- two-stage

/// Two suppressors in series. The voice probability comes from whichever
/// stage estimates one (RNNoise), so click suppression can follow.
struct Chain {
    name: &'static str,
    first: Box<dyn Suppressor>,
    second: Box<dyn Suppressor>,
}

impl Suppressor for Chain {
    fn status(&self) -> crate::audio_model_lab::Status {
        let first = self.first.status();
        if first != crate::audio_model_lab::Status::Active { first } else { self.second.status() }
    }
    fn process(&mut self, block: &mut [f32; BLOCK]) {
        self.first.process(block);
        self.second.process(block);
    }
    fn delay_samples(&self) -> usize {
        self.first.delay_samples() + self.second.delay_samples()
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn voice_probability(&self) -> Option<f32> {
        self.second
            .voice_probability()
            .or_else(|| self.first.voice_probability())
    }
}

/// Candidate chains for the audio bench: the app's whole microphone
/// processing (gain, suppressor, click suppression, automatic volume, voice
/// gate) with the suppressor fixed.
#[cfg(any(test, feature = "audio-bench"))]
pub(crate) mod bench {
    use super::{make, NsLevel, SuppressorKind, BLOCK};
    use crate::dpdfnet::{StudioKind, Variant};
    use crate::audio_bench::{ChainSpec, SendChain};
    use crate::audio_settings::{AudioSettings, NoiseModel};
    use crate::click_suppression::{KeyHoldParams, NoiseStrength};
    use crate::media::{AudioControls, MicrophoneProcessor};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};

    pub struct Candidate {
        name: &'static str,
        controls: AudioControls,
        processor: MicrophoneProcessor,
        delay: usize,
    }

    impl Candidate {
        /// `clicks`: also run Enhanced click suppression and voice isolation
        /// at that strength (only for suppressors with a voice probability).
        pub fn new(name: &'static str, kind: SuppressorKind, clicks: Option<NoiseStrength>) -> Self {
            let settings = AudioSettings {
                noise_model: if clicks.is_some() { NoiseModel::Enhanced } else { NoiseModel::Standard },
                noise_strength: clicks.unwrap_or_default(),
                ..Default::default()
            };
            Self {
                name,
                controls: AudioControls::new(&settings),
                processor: MicrophoneProcessor::with_suppressor(kind),
                delay: make(kind).delay_samples(),
            }
        }
    }

    impl SendChain for Candidate {
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

    /// A suppressor that cannot be linked into the client (DeepFilterNet,
    /// DPDFNet), run beforehand by an outside tool on the corpus WAVs that
    /// `--write-wav` writes. This chain plays that tool's output for
    /// whichever corpus signal it is fed and runs the rest of the app's chain
    /// (automatic volume, gate) on it. The directory holds
    /// `<row>.noisy.out.wav` and `<row>.clean.out.wav` (16-bit, the length of
    /// the input, not shifted back) and `delay.txt` (the tool's delay in
    /// samples). An optional `then.txt` reading `rnnoise+enhanced` also runs
    /// RNNoise with Enhanced click suppression at Normal after it.
    pub struct Replay {
        name: &'static str,
        controls: AudioControls,
        processor: MicrophoneProcessor,
        recording: Arc<Recording>,
        alive: Vec<usize>,
        at: usize,
        delay: usize,
    }

    pub struct Recording {
        /// (input the tool was given, what it returned)
        pairs: Vec<(Vec<f32>, Vec<f32>)>,
        delay: usize,
        then_rnnoise: bool,
    }

    /// Loaded on first use, then shared by every chain built from it.
    type Shared = Arc<OnceLock<Arc<Recording>>>;

    fn read_wav(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut at = 12;
        while &bytes[at..at + 4] != b"data" {
            at += 8 + u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        }
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        bytes[at + 8..at + 8 + size]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect()
    }

    /// `output`: the tool's directory; `input`: the corpus WAVs it was given.
    fn load(output: &Path, input: &Path) -> Recording {
        let mut pairs = Vec::new();
        let mut names: Vec<String> = std::fs::read_dir(output)
            .unwrap_or_else(|e| panic!("replay directory {}: {e}", output.display()))
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            if let Some(stem) = name.strip_suffix(".out.wav") {
                pairs.push((read_wav(&input.join(format!("{stem}.wav"))), read_wav(&output.join(&name))));
            }
        }
        assert!(!pairs.is_empty(), "{}: no <row>.<clean|noisy>.out.wav", output.display());
        let delay = std::fs::read_to_string(output.join("delay.txt"))
            .unwrap_or_else(|e| panic!("{}: delay.txt: {e}", output.display()))
            .trim()
            .parse()
            .expect("delay in samples");
        let then_rnnoise = match std::fs::read_to_string(output.join("then.txt")) {
            Ok(text) if text.trim() == "rnnoise+enhanced" => true,
            Ok(text) => panic!("{}: then.txt must read rnnoise+enhanced, not {text:?}", output.display()),
            Err(_) => false,
        };
        Recording { pairs, delay, then_rnnoise }
    }

    impl Replay {
        pub fn new(name: &'static str, recording: Arc<Recording>) -> Self {
            let then_rnnoise = recording.then_rnnoise;
            let settings = AudioSettings {
                noise_model: if then_rnnoise { NoiseModel::Enhanced } else { NoiseModel::Standard },
                ..Default::default()
            };
            let kind = if then_rnnoise { SuppressorKind::RnNoise } else { SuppressorKind::Off };
            Self {
                name,
                controls: AudioControls::new(&settings),
                processor: MicrophoneProcessor::with_suppressor(kind),
                alive: (0..recording.pairs.len()).collect(),
                at: 0,
                delay: recording.delay + make(kind).delay_samples(),
                recording,
            }
        }
    }

    impl SendChain for Replay {
        fn name(&self) -> &'static str {
            self.name
        }
        fn process_block(&mut self, block: &mut [f32; BLOCK]) {
            // The WAVs are 16-bit: match within two steps of quantization.
            let (at, pairs) = (self.at, &self.recording.pairs);
            self.alive.retain(|&index| {
                let input = &pairs[index].0;
                block.iter().enumerate().all(|(n, sample)| {
                    input.get(at + n).map_or(*sample == 0.0, |v| (v - sample).abs() <= 2.0 / 32768.0)
                })
            });
            let &index = self.alive.first().expect("no replayed recording matches this input");
            for (n, sample) in block.iter_mut().enumerate() {
                *sample = pairs[index].1.get(at + n).copied().unwrap_or(0.0);
            }
            self.at += BLOCK;
            if !self.processor.process(block, &self.controls) {
                block.fill(0.0);
            }
        }
        fn algorithmic_delay_samples(&self) -> usize {
            self.delay
        }
    }

    fn replay_chain(name: &'static str, output: PathBuf, input: PathBuf) -> ChainSpec {
        let shared: Shared = Arc::default();
        ChainSpec::new(name, move || {
            let recording = shared.get_or_init(|| Arc::new(load(&output, &input))).clone();
            Box::new(Replay::new(name, recording))
        })
    }

    /// One chain `replay:<label>` for every subfolder `<label>` of `root`
    /// (`RELAY_REPLAY_DIR`), each laid out as `Replay` describes; `input`
    /// holds the corpus WAVs the tool was given.
    pub fn replay_chains(root: &Path, input: &Path) -> Vec<ChainSpec> {
        let mut labels: Vec<PathBuf> = std::fs::read_dir(root)
            .unwrap_or_else(|e| panic!("RELAY_REPLAY_DIR {}: {e}", root.display()))
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.is_dir())
            .collect();
        labels.sort();
        labels
            .into_iter()
            .map(|dir| {
                let label = dir.file_name().expect("label").to_string_lossy();
                // Chain names are 'static; the bench builds them once per run.
                let name: &'static str = Box::leak(format!("replay:{label}").into_boxed_str());
                replay_chain(name, dir, input.to_path_buf())
            })
            .collect()
    }

    fn studio(variant: Variant, probability: bool) -> SuppressorKind {
        studio_keys(variant, probability, None, None)
    }

    fn studio_keys(
        variant: Variant,
        probability: bool,
        keys_before: Option<KeyHoldParams>,
        keys_after: Option<KeyHoldParams>,
    ) -> SuppressorKind {
        SuppressorKind::DpdfNet(StudioKind { variant, probability, guarded: false, keys_before, keys_after })
    }

    /// The plan's candidate (b): a 12 dB jump within 2 ms against the 50 ms
    /// median, the band above 1 kHz held for at most 8 ms, after the model.
    const KEYS_AFTER: KeyHoldParams = KeyHoldParams { split_hz: 1_000, onset_db: 12, hold_db: 0, max_ms: 8, removed_db: 0 };
    /// The same detector on the model's input, before the model sees the key.
    const KEYS_BEFORE: KeyHoldParams = KeyHoldParams { split_hz: 1_000, onset_db: 12, hold_db: 0, max_ms: 40, removed_db: 0 };
    /// After the model, looking for the jump in the model's input and only
    /// where the model removed at least 6 dB of it: spares consonants.
    pub(crate) const KEYS_GUIDED: KeyHoldParams =
        KeyHoldParams { split_hz: 1_000, onset_db: 12, hold_db: 0, max_ms: 8, removed_db: 6 };

    /// `RELAY_KEYS_CHAINS=name:before=HZ/DB/HOLD_DB/MS,after=HZ/DB/HOLD_DB/MS/REMOVED_DB;name2:...`
    /// adds a dpdfnet8 chain per entry with those key-hold settings (either
    /// part may be left out), for parameter sweeps without a rebuild.
    fn keys_chains_from_env() -> Vec<ChainSpec> {
        let Ok(text) = std::env::var("RELAY_KEYS_CHAINS") else {
            return Vec::new();
        };
        let parse = |value: &str| -> KeyHoldParams {
            let parts: Vec<i32> = value
                .split('/')
                .map(|p| p.trim().parse().unwrap_or_else(|_| panic!("RELAY_KEYS_CHAINS: bad number in {value:?}")))
                .collect();
            assert!(matches!(parts.len(), 4 | 5), "RELAY_KEYS_CHAINS: HZ/DB/HOLD_DB/MS[/REMOVED_DB], not {value:?}");
            KeyHoldParams {
                split_hz: parts[0] as u16,
                onset_db: parts[1] as u8,
                hold_db: parts[2] as i8,
                max_ms: parts[3] as u8,
                removed_db: parts.get(4).copied().unwrap_or(0) as u8,
            }
        };
        text.split(';')
            .filter(|entry| !entry.trim().is_empty())
            .map(|entry| {
                let (name, settings) = entry.split_once(':').expect("RELAY_KEYS_CHAINS: name:settings");
                let (mut before, mut after) = (None, None);
                for part in settings.split(',') {
                    match part.split_once('=') {
                        Some(("before", value)) => before = Some(parse(value)),
                        Some(("after", value)) => after = Some(parse(value)),
                        _ => panic!("RELAY_KEYS_CHAINS: before=... or after=..., not {part:?}"),
                    }
                }
                let name: &'static str = Box::leak(format!("dpdfnet8+{}", name.trim()).into_boxed_str());
                let kind = studio_keys(Variant::Eight, false, before, after);
                ChainSpec::new(name, move || Box::new(Candidate::new(name, kind, None)))
            })
            .collect()
    }

    macro_rules! candidate {
        ($name:literal, $kind:expr, $clicks:expr) => {
            ChainSpec::new($name, || Box::new(Candidate::new($name, $kind, $clicks)))
        };
    }

    /// CPU of one key-hold stage per 10 ms block on `signal` (and, guided,
    /// the same signal as its reference): (p50, p99, max) in ms.
    pub fn key_hold_cpu(signal: &[f32], guided: bool) -> (f32, f32, f32) {
        let params = if guided { KEYS_GUIDED } else { KEYS_AFTER };
        let mut stage = crate::click_suppression::KeyHold::new(params);
        let mut times = Vec::new();
        for _ in 0..5 {
            for chunk in signal.chunks_exact(BLOCK) {
                let reference: [f32; BLOCK] = chunk.try_into().expect("whole blocks");
                let mut block = reference;
                let started = std::time::Instant::now();
                stage.process_with(&mut block, guided.then_some(&reference));
                times.push(started.elapsed().as_secs_f32() * 1000.0);
                std::hint::black_box(&block);
            }
        }
        times.sort_by(f32::total_cmp);
        let at = |f: f32| times[((times.len() - 1) as f32 * f).round() as usize];
        (at(0.5), at(0.99), at(1.0))
    }

    pub fn chains() -> Vec<ChainSpec> {
        use NoiseStrength::{High, Normal};
        use SuppressorKind::*;
        let mut chains = vec![
            candidate!("rnnoise+enhanced-high", RnNoise, Some(High)),
            candidate!("webrtc-ns", WebRtcNs(NsLevel::Moderate), None),
            candidate!("webrtc-ns-veryhigh", WebRtcNs(NsLevel::VeryHigh), None),
            candidate!("rnnoise-then-webrtc-ns+enhanced", RnNoiseThenWebRtcNs(NsLevel::Moderate), Some(Normal)),
            candidate!("rnnoise-then-webrtc-ns-veryhigh+enhanced", RnNoiseThenWebRtcNs(NsLevel::VeryHigh), Some(Normal)),
            candidate!("webrtc-ns-then-rnnoise+enhanced", WebRtcNsThenRnNoise(NsLevel::Moderate), Some(Normal)),
            candidate!("webrtc-ns-high-then-rnnoise+enhanced", WebRtcNsThenRnNoise(NsLevel::High), Some(Normal)),
            candidate!("webrtc-ns-veryhigh-then-rnnoise+enhanced", WebRtcNsThenRnNoise(NsLevel::VeryHigh), Some(Normal)),
            candidate!("webrtc-ns-veryhigh-then-rnnoise+enhanced-high", WebRtcNsThenRnNoise(NsLevel::VeryHigh), Some(High)),
            // Studio, in process: the app's code with the guard off, so a
            // busy bench machine cannot swap the model mid-row.
            candidate!("dpdfnet8", studio(Variant::Eight, false), None),
            candidate!("dpdfnet2", studio(Variant::Two, false), None),
            // Click suppression fed the model's output-to-input energy ratio.
            candidate!("dpdfnet8+ratio-clicks", studio(Variant::Eight, true), Some(Normal)),
            candidate!("dpdfnet2+ratio-clicks", studio(Variant::Two, true), Some(Normal)),
            // Keys struck while talking (docs/audio-keys-on-words-20261030.md).
            candidate!("dpdfnet8+keys-after", studio_keys(Variant::Eight, false, None, Some(KEYS_AFTER)), None),
            candidate!(
                "dpdfnet8+ratio-clicks+keys-after",
                studio_keys(Variant::Eight, true, None, Some(KEYS_AFTER)),
                Some(Normal)
            ),
            candidate!("dpdfnet8+keys-before", studio_keys(Variant::Eight, false, Some(KEYS_BEFORE), None), None),
            candidate!("dpdfnet8+keys-guided", studio_keys(Variant::Eight, false, None, Some(KEYS_GUIDED)), None),
        ];
        chains.extend(keys_chains_from_env());
        // Worker E's DeepFilterNet routes (docs/audio-deepfilter-decision-20261029.md),
        // kept for its reruns; RELAY_REPLAY_DIR does the same for any tool.
        let input = crate::audio_bench::output_directory().join("wav");
        let path = |variable: &str| std::env::var_os(variable).map(PathBuf::from);
        if let Some(dir) = path("RELAY_REPLAY_DFN") {
            chains.push(replay_chain("deepfilternet (replayed)", dir, input.clone()));
        }
        if let Some(dir) = path("RELAY_REPLAY_DFN_LL") {
            chains.push(replay_chain("deepfilternet-ll (replayed)", dir.clone(), input.clone()));
            // The same output through RNNoise with click suppression.
            let shared: Shared = Arc::default();
            let input = input.clone();
            chains.push(ChainSpec::new("deepfilternet-ll-then-rnnoise+enhanced (replayed)", move || {
                let recording = shared.get_or_init(|| {
                    let mut recording = load(&dir, &input);
                    recording.then_rnnoise = true;
                    Arc::new(recording)
                });
                Box::new(Replay::new("deepfilternet-ll-then-rnnoise+enhanced (replayed)", recording.clone()))
            }));
        }
        if let Some(root) = path("RELAY_REPLAY_DIR") {
            chains.extend(replay_chains(&root, &input));
        }
        chains
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_test_signals::talker;

    fn every_kind() -> Vec<SuppressorKind> {
        let mut kinds = vec![SuppressorKind::Off, SuppressorKind::RnNoise];
        for level in [NsLevel::Low, NsLevel::Moderate, NsLevel::High, NsLevel::VeryHigh] {
            kinds.push(SuppressorKind::WebRtcNs(level));
            kinds.push(SuppressorKind::RnNoiseThenWebRtcNs(level));
            kinds.push(SuppressorKind::WebRtcNsThenRnNoise(level));
        }
        kinds
    }

    #[test]
    fn every_suppressor_is_block_exact_and_reports_delay() {
        let speech = talker(1.0, (105.0, 135.0), 13, 0.3);
        for kind in every_kind() {
            let mut suppressor = make(kind);
            let mut out = Vec::new();
            for index in 0..100 {
                let mut block = [0.0f32; BLOCK];
                let start = (index * BLOCK) % (speech.len() - BLOCK);
                block.copy_from_slice(&speech[start..start + BLOCK]);
                suppressor.process(&mut block);
                out.extend_from_slice(&block);
            }
            assert_eq!(out.len(), 100 * BLOCK, "{kind:?}");
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "{kind:?}");
            assert!(suppressor.delay_samples() <= 960, "{kind:?}");
        }
    }

    #[test]
    fn enhanced_and_standard_keep_rnnoise_and_studio_runs_guarded_dpdfnet8() {
        use crate::audio_settings::NoiseModel;
        use crate::click_suppression::NoiseStrength::{High, Low, Normal};
        for strength in [Low, Normal, High] {
            for model in [NoiseModel::Enhanced, NoiseModel::Standard] {
                assert_eq!(for_setting(model, strength), SuppressorKind::RnNoise);
            }
            assert_eq!(
                for_setting(NoiseModel::Studio, strength),
                SuppressorKind::DpdfNet(crate::dpdfnet::StudioKind {
                    variant: crate::dpdfnet::Variant::Eight,
                    probability: STUDIO_CLICKS,
                    guarded: true,
                    keys_before: None,
                    keys_after: None,
                })
            );
        }
    }

    /// Keys on words (docs/audio-keys-on-words-20261030.md): no key hold
    /// shipped, so Studio's delay stays 60 ms and its output is the model's.
    #[test]
    fn studio_holds_back_no_keys_and_keeps_its_delay() {
        use crate::audio_settings::NoiseModel;
        use crate::click_suppression::NoiseStrength::Normal;
        assert_eq!(STUDIO_KEYS, None);
        let SuppressorKind::DpdfNet(kind) = for_setting(NoiseModel::Studio, Normal) else {
            panic!("Studio is DPDFNet");
        };
        assert_eq!((kind.keys_before, kind.keys_after), (None, None));
        assert_eq!(crate::dpdfnet::DELAY_SAMPLES, 2_880);
    }

    /// The one candidate that meets the plan's fan target, pinned so the
    /// comparison document stays true: WebRTC NS at its strongest level in
    /// front of RNNoise, with click suppression at High, takes the bench's
    /// fan plus mouse clicks down at least 25 dB between words, within the
    /// 20 ms delay budget. (Not shipped: it removes fewer clicks.)
    #[cfg(feature = "native-rtc")]
    #[test]
    fn webrtc_ns_before_rnnoise_meets_the_fan_target_within_the_delay_budget() {
        use crate::audio_bench::{corpus, measure_delay_between, metrics, run_chain, SendChain};
        use crate::click_suppression::NoiseStrength::High;
        let item = corpus().into_iter().find(|item| item.name == "fan_plus_mouse").unwrap();
        let noisy = item.noisy();
        let kind = SuppressorKind::WebRtcNsThenRnNoise(NsLevel::VeryHigh);
        let make_chain = || bench::Candidate::new("candidate", kind, Some(High));
        let declared = make_chain().algorithmic_delay_samples();
        assert!(declared <= 20 * 48, "{declared} samples");
        let reference = run_chain(&mut make_chain(), &item.clean).output;
        let delay = measure_delay_between(&item.clean, &reference, declared - 96, declared + 96);
        let result = run_chain(&mut make_chain(), &noisy);
        let m = metrics(&item.clean, &noisy, &result.output, delay);
        println!("WebRTC NS very high, RNNoise, High on fan_plus_mouse: {:.1} dB in pauses, delay {delay}", m.residual_noise_db);
        assert!(m.residual_noise_db <= -25.0, "{:.1} dB", m.residual_noise_db);
    }

    /// Each suppressor's declared delay is where its output lines up with
    /// its input, within 0.5 ms. Measured on clicks alone: they are
    /// aperiodic, so the match cannot slip by a pitch period as it can on
    /// the talker.
    #[test]
    fn declared_delays_match_what_clicks_show() {
        let (input, _) = crate::voice_test_signals::clicks(3.0, 4.0, 0.3, 31);
        for kind in every_kind() {
            let mut suppressor = make(kind);
            let mut out = Vec::with_capacity(input.len());
            for chunk in input.chunks_exact(BLOCK) {
                let mut block: [f32; BLOCK] = chunk.try_into().unwrap();
                suppressor.process(&mut block);
                out.extend_from_slice(&block);
            }
            let measured = crate::audio_bench::measure_delay_samples(&input, &out, 1_440);
            println!("{}, {kind:?}: declared {}, measured {measured}", suppressor.name(), suppressor.delay_samples());
            assert!(
                measured.abs_diff(suppressor.delay_samples()) <= 24,
                "{kind:?}: declared {}, measured {measured}",
                suppressor.delay_samples()
            );
        }
    }

    /// RELAY_REPLAY_DIR: every label folder becomes a chain that plays the
    /// tool's output back for the matching input, at the tool's declared
    /// delay (plus RNNoise's when `then.txt` asks for it), even past the 30 ms
    /// the bench searches without a declared delay.
    #[test]
    fn replay_dir_plays_each_label_back_at_its_delay() {
        use crate::audio_bench::{measure_delay_between, run_chain, write_wav};
        let root = std::env::temp_dir().join(format!("relay-replay-test-{}", std::process::id()));
        let (input_dir, replay) = (root.join("wav"), root.join("replay"));
        let speech = talker(2.0, (105.0, 135.0), 13, 0.3);
        let late = 2_400;
        let mut delayed = vec![0.0f32; late];
        delayed.extend_from_slice(&speech[..speech.len() - late]);
        std::fs::create_dir_all(&input_dir).unwrap();
        for kind in ["clean", "noisy"] {
            write_wav(&input_dir.join(format!("row.{kind}.wav")), &speech).unwrap();
        }
        for (label, then) in [("late", false), ("late-then-clicks", true)] {
            let dir = replay.join(label);
            std::fs::create_dir_all(&dir).unwrap();
            for kind in ["clean", "noisy"] {
                write_wav(&dir.join(format!("row.{kind}.out.wav")), &delayed).unwrap();
            }
            std::fs::write(dir.join("delay.txt"), format!("{late}\n")).unwrap();
            if then {
                std::fs::write(dir.join("then.txt"), "rnnoise+enhanced\n").unwrap();
            }
        }
        let chains = bench::replay_chains(&replay, &input_dir);
        let names: Vec<&str> = chains.iter().map(|spec| spec.name).collect();
        assert_eq!(names, ["replay:late", "replay:late-then-clicks"]);
        for (spec, declared) in chains.iter().zip([late, late + BLOCK]) {
            let mut chain = (spec.make)();
            assert_eq!((chain.name(), chain.algorithmic_delay_samples()), (spec.name, declared));
            let output = run_chain(&mut *chain, &speech).output;
            let measured = measure_delay_between(&speech, &output, declared - 96, declared + 96);
            assert!(measured.abs_diff(declared) <= 24, "{}: declared {declared}, measured {measured}", spec.name);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
