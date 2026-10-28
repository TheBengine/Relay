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
}

/// The suppressor for the noise settings: RNNoise for Standard and for
/// every Enhanced strength. The measured alternatives (WebRTC NS in front of
/// RNNoise, DeepFilterNet) each lost somewhere the current chain holds; see
/// docs/audio-suppressor-comparison-20261028.md before changing this.
pub(crate) fn for_setting(
    _enhanced: bool,
    _strength: crate::click_suppression::NoiseStrength,
) -> SuppressorKind {
    SuppressorKind::RnNoise
}

pub(crate) fn make(kind: SuppressorKind) -> Box<dyn Suppressor> {
    match kind {
        SuppressorKind::Off => Box::new(Off),
        SuppressorKind::RnNoise => Box::new(RnNoise::new()),
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
        // A kind this build lacks falls back to RNNoise.
        #[allow(unreachable_patterns)]
        _ => Box::new(RnNoise::new()),
    }
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
    use crate::audio_bench::{ChainSpec, SendChain};
    use crate::audio_settings::{AudioSettings, NoiseModel};
    use crate::click_suppression::NoiseStrength;
    use crate::media::{AudioControls, MicrophoneProcessor};

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

    /// A suppressor that cannot be linked into the client (DeepFilterNet:
    /// its `tract` pins `time` below what the client needs), run beforehand
    /// by an outside tool on the corpus WAVs that `--write-wav` writes. This
    /// chain plays that tool's output for whichever corpus signal it is fed
    /// and runs the rest of the app's chain (automatic volume, gate) on it.
    /// The directory holds `<row>.noisy.out.wav`, `<row>.clean.out.wav` and
    /// `delay.txt`, and is named by the environment variable `variable`.
    pub struct Replay {
        name: &'static str,
        controls: AudioControls,
        processor: MicrophoneProcessor,
        pairs: &'static [(Vec<f32>, Vec<f32>)],
        alive: Vec<usize>,
        at: usize,
        delay: usize,
    }

    type Recording = (Vec<(Vec<f32>, Vec<f32>)>, usize);

    fn read_wav(path: &std::path::Path) -> Vec<f32> {
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

    fn load(variable: &str) -> Recording {
        let output = std::path::PathBuf::from(std::env::var_os(variable).expect("replay directory"));
        let input = crate::audio_bench::output_directory().join("wav");
        let mut pairs = Vec::new();
        for entry in std::fs::read_dir(&output).expect("replay directory") {
            let name = entry.expect("entry").file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".out.wav") {
                pairs.push((read_wav(&input.join(format!("{stem}.wav"))), read_wav(&output.join(&name))));
            }
        }
        let delay = std::fs::read_to_string(output.join("delay.txt"))
            .expect("delay.txt")
            .trim()
            .parse()
            .expect("delay in samples");
        (pairs, delay)
    }

    impl Replay {
        pub fn new(name: &'static str, recording: &'static Recording) -> Self {
            let settings = AudioSettings {
                noise_model: NoiseModel::Standard,
                ..Default::default()
            };
            Self {
                name,
                controls: AudioControls::new(&settings),
                processor: MicrophoneProcessor::with_suppressor(SuppressorKind::Off),
                pairs: &recording.0,
                alive: (0..recording.0.len()).collect(),
                at: 0,
                delay: recording.1,
            }
        }
    }

    impl SendChain for Replay {
        fn name(&self) -> &'static str {
            self.name
        }
        fn process_block(&mut self, block: &mut [f32; BLOCK]) {
            // The WAVs are 16-bit: match within two steps of quantization.
            let (at, pairs) = (self.at, self.pairs);
            self.alive.retain(|&index| {
                let input = &pairs[index].0;
                block.iter().enumerate().all(|(n, sample)| {
                    input.get(at + n).map_or(*sample == 0.0, |v| (v - sample).abs() <= 2.0 / 32768.0)
                })
            });
            let &index = self.alive.first().expect("no replayed recording matches this input");
            for (n, sample) in block.iter_mut().enumerate() {
                *sample = self.pairs[index].1.get(at + n).copied().unwrap_or(0.0);
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

    static DFN: std::sync::OnceLock<Recording> = std::sync::OnceLock::new();
    static DFN_LL: std::sync::OnceLock<Recording> = std::sync::OnceLock::new();

    macro_rules! candidate {
        ($name:literal, $kind:expr, $clicks:expr) => {
            ChainSpec {
                name: $name,
                make: || Box::new(Candidate::new($name, $kind, $clicks)),
            }
        };
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
        ];
        if std::env::var_os("RELAY_REPLAY_DFN").is_some() {
            chains.push(ChainSpec {
                name: "deepfilternet (replayed)",
                make: || Box::new(Replay::new("deepfilternet (replayed)", DFN.get_or_init(|| load("RELAY_REPLAY_DFN")))),
            });
        }
        if std::env::var_os("RELAY_REPLAY_DFN_LL").is_some() {
            chains.push(ChainSpec {
                name: "deepfilternet-ll (replayed)",
                make: || Box::new(Replay::new("deepfilternet-ll (replayed)", DFN_LL.get_or_init(|| load("RELAY_REPLAY_DFN_LL")))),
            });
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
    fn every_setting_keeps_rnnoise() {
        use crate::click_suppression::NoiseStrength::{High, Low, Normal};
        for enhanced in [false, true] {
            for strength in [Low, Normal, High] {
                assert_eq!(for_setting(enhanced, strength), SuppressorKind::RnNoise);
            }
        }
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
}
