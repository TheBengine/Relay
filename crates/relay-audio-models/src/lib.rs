//! Real streaming comparison models. No devices, networking or fallback models.
mod gtcrn;
mod native_rnnoise;
mod resample;

use deep_filter::tract::{DfParams, DfTract, RuntimeParams};
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
    DeepFilter(Box<DfTract>),
    Gtcrn(Box<gtcrn::Stream>),
    RnNoise(native_rnnoise::Stream),
}

pub struct Stream {
    engine: Engine,
}

static DEEP_FILTER: [OnceLock<Result<Mutex<DfTract>, String>>; 2] =
    [OnceLock::new(), OnceLock::new()];
const DF_STANDARD: &[u8] =
    include_bytes!("../../../vendor/models/deepfilter/DeepFilterNet3_onnx.tar.gz");
const DF_LOW_LATENCY: &[u8] =
    include_bytes!("../../../vendor/models/deepfilter/DeepFilterNet3_ll_onnx.tar.gz");

impl Stream {
    /// Call on the model worker: graph construction must never block audio/UI.
    pub fn new(model: Model) -> Result<Self, String> {
        let engine = match model {
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
        ] {
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
        ] {
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
