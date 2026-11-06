//! Real streaming comparison models. No devices, networking or fallback models.
//!
//! Only the newer native RNNoise is built in by default. DeepFilterNet 3 and
//! GTCRN (about 45 MB of models) need the non-default `model-lab` feature;
//! without it `Stream::new` refuses them and `Model::is_built_in` says so.
#[cfg(feature = "model-lab")]
mod gtcrn;
mod native_rnnoise;
#[cfg(feature = "model-lab")]
mod resample;

#[cfg(feature = "model-lab")]
use deep_filter::tract::{DfParams, DfTract, RuntimeParams};
#[cfg(feature = "model-lab")]
use std::sync::{Mutex, OnceLock};

pub const BLOCK: usize = 480;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    DeepFilter3,
    DeepFilter3LowLatency,
    Gtcrn,
    RnNoiseNew,
}

impl Model {
    /// Whether this build carries the model (DeepFilterNet and GTCRN only
    /// with the `model-lab` feature).
    pub const fn is_built_in(self) -> bool {
        match self {
            Self::DeepFilter3 | Self::DeepFilter3LowLatency | Self::Gtcrn => cfg!(feature = "model-lab"),
            Self::RnNoiseNew => true,
        }
    }

    /// Algorithmic stream delay, excluding the app's worker handoff and devices.
    pub fn delay_samples(self) -> usize {
        match self {
            Self::DeepFilter3 => 1440,
            Self::DeepFilter3LowLatency => 480,
            Self::Gtcrn => 1664,
            Self::RnNoiseNew => 960,
        }
    }
}

enum Engine {
    #[cfg(feature = "model-lab")]
    DeepFilter(Box<DfTract>),
    #[cfg(feature = "model-lab")]
    Gtcrn(Box<gtcrn::Stream>),
    RnNoise(native_rnnoise::Stream),
}

pub struct Stream {
    engine: Engine,
}

#[cfg(feature = "model-lab")]
static DEEP_FILTER: [OnceLock<Result<Mutex<DfTract>, String>>; 2] =
    [OnceLock::new(), OnceLock::new()];
#[cfg(feature = "model-lab")]
const DF_STANDARD: &[u8] =
    include_bytes!("../../../vendor/models/deepfilter/DeepFilterNet3_onnx.tar.gz");
#[cfg(feature = "model-lab")]
const DF_LOW_LATENCY: &[u8] =
    include_bytes!("../../../vendor/models/deepfilter/DeepFilterNet3_ll_onnx.tar.gz");

impl Stream {
    /// Call on the model worker: graph construction must never block audio/UI.
    pub fn new(model: Model) -> Result<Self, String> {
        let engine = match model {
            #[cfg(not(feature = "model-lab"))]
            Model::DeepFilter3 | Model::DeepFilter3LowLatency | Model::Gtcrn => {
                return Err("This build does not include this comparison model".into());
            }
            #[cfg(feature = "model-lab")]
            Model::DeepFilter3 | Model::DeepFilter3LowLatency => {
                let index = usize::from(model == Model::DeepFilter3LowLatency);
                let template = DEEP_FILTER[index]
                    .get_or_init(|| {
                        let bytes = if index == 0 {
                            DF_STANDARD
                        } else {
                            DF_LOW_LATENCY
                        };
                        let params = DfParams::from_bytes(bytes)
                            .map_err(|e| format!("DeepFilterNet weights: {e:#}"))?;
                        let stream = DfTract::new(params, &RuntimeParams::default_with_ch(1))
                            .map_err(|e| format!("DeepFilterNet graph: {e:#}"))?;
                        if stream.sr != 48000
                            || stream.hop_size != BLOCK
                            || stream.fft_size - stream.hop_size + stream.lookahead * BLOCK
                                != model.delay_samples()
                        {
                            return Err(
                                "Unexpected DeepFilterNet sample rate, block size or delay".into(),
                            );
                        }
                        Ok(Mutex::new(stream))
                    })
                    .as_ref()
                    .map_err(Clone::clone)?;
                Engine::DeepFilter(Box::new(
                    template
                        .lock()
                        .map_err(|_| "DeepFilterNet template unavailable")?
                        .clone(),
                ))
            }
            #[cfg(feature = "model-lab")]
            Model::Gtcrn => Engine::Gtcrn(Box::new(gtcrn::Stream::new()?)),
            Model::RnNoiseNew => Engine::RnNoise(native_rnnoise::Stream::new()?),
        };
        Ok(Self { engine })
    }

    pub fn process(&mut self, block: &mut [f32; BLOCK]) -> Result<(), String> {
        for sample in block.iter_mut() {
            *sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        match &mut self.engine {
            #[cfg(feature = "model-lab")]
            Engine::DeepFilter(stream) => {
                let mut out = [0.0; BLOCK];
                stream
                    .process(
                        ndarray::ArrayView2::from_shape((1, BLOCK), block)
                            .map_err(|e| e.to_string())?,
                        ndarray::ArrayViewMut2::from_shape((1, BLOCK), &mut out)
                            .map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| format!("DeepFilterNet inference: {e:#}"))?;
                *block = out;
            }
            #[cfg(feature = "model-lab")]
            Engine::Gtcrn(stream) => stream.process(block)?,
            Engine::RnNoise(stream) => stream.process(block),
        }
        if block.iter().any(|x| !x.is_finite()) {
            return Err("Model produced non-finite audio".into());
        }
        for sample in block {
            *sample = sample.clamp(-1.0, 1.0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_model_runs_real_streaming_inference_and_restarts_cleanly() {
        for model in [
            Model::DeepFilter3,
            Model::DeepFilter3LowLatency,
            Model::Gtcrn,
            Model::RnNoiseNew,
        ]
        .into_iter()
        .filter(|model| model.is_built_in())
        {
            let mut first = Stream::new(model).unwrap_or_else(|e| panic!("{model:?}: {e}"));
            let mut second = Stream::new(model).unwrap();
            let mut energy = 0.0;
            for frame in 0..80 {
                let input: [f32; BLOCK] = std::array::from_fn(|n| {
                    let phase = (frame * BLOCK + n) as f32 / 48000.0;
                    (std::f32::consts::TAU * 220.0 * phase).sin() * 0.25
                        + (std::f32::consts::TAU * 440.0 * phase).sin() * 0.1
                });
                let (mut a, mut b) = (input, input);
                first.process(&mut a).unwrap();
                second.process(&mut b).unwrap();
                assert_eq!(a, b, "{model:?}: shared state across streams");
                energy += a.iter().map(|x| x * x).sum::<f32>();
            }
            assert!(energy > 0.001, "{model:?}: no processed output");
            eprintln!("{model:?}: streaming inference passed, output energy={energy}");
        }
    }

    /// Releases carry only the newer RNNoise; the lab models need `model-lab`.
    #[test]
    fn only_the_lab_feature_builds_deepfilternet_and_gtcrn() {
        assert!(Model::RnNoiseNew.is_built_in());
        for model in [Model::DeepFilter3, Model::DeepFilter3LowLatency, Model::Gtcrn] {
            assert_eq!(model.is_built_in(), cfg!(feature = "model-lab"), "{model:?}");
            assert_eq!(Stream::new(model).is_ok(), model.is_built_in(), "{model:?}");
        }
    }
}

#[cfg(test)]
mod reference_tests {
    use super::*;
    fn pcm(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect()
    }
    fn floats(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect()
    }
    #[test]
    fn streaming_models_match_independent_reference_runtimes() {
        let input = pcm(include_bytes!("fixtures/input.pcm16"));
        for (model, reference) in [
            (
                Model::DeepFilter3,
                Some(floats(include_bytes!("fixtures/deepfilter3.f32"))),
            ),
            (
                Model::DeepFilter3LowLatency,
                Some(floats(include_bytes!("fixtures/deepfilter3-ll.f32"))),
            ),
            (
                Model::Gtcrn,
                Some(
                    include_bytes!("fixtures/gtcrn.f32")
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                        .collect(),
                ),
            ),
            (Model::RnNoiseNew, None),
        ]
        .into_iter()
        .filter(|(model, _)| model.is_built_in())
        {
            let mut stream = Stream::new(model).unwrap();
            let mut output = Vec::new();
            for chunk in input.chunks_exact(BLOCK) {
                let mut block: [f32; BLOCK] = chunk.try_into().unwrap();
                stream.process(&mut block).unwrap();
                output.extend(block);
            }
            if let Some(reference) = reference {
                assert_eq!(output.len(), reference.len());
                let rms = (output
                    .iter()
                    .zip(reference)
                    .map(|(a, b)| (*a as f64 - b as f64).powi(2))
                    .sum::<f64>()
                    / output.len() as f64)
                    .sqrt();
                eprintln!("{model:?} reference RMS error {rms}");
                assert!(rms < 0.00003, "{model:?}: reference mismatch {rms}");
            }
        }
    }
}
