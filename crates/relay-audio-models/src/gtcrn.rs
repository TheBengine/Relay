//! Streaming GTCRN with the upstream 512-point sqrt-Hann STFT and recurrent
//! tensor layout (Xiaobin-Rong/gtcrn, MIT; vendor/models/gtcrn/LICENSE).
//! Relay supplies bounded 48/16 kHz resampling and fixed-delay 10 ms delivery.
use crate::{resample::SincFilter, BLOCK};
use realfft::{num_complex::Complex32, ComplexToReal, RealFftPlanner, RealToComplex};
use std::{
    collections::VecDeque,
    sync::{Arc, OnceLock},
};
use tract_onnx::prelude::*;

const HOP: usize = 256;
const FFT: usize = 512;
const BINS: usize = 257;
const BYTES: &[u8] = include_bytes!("../../../vendor/models/gtcrn/gtcrn.onnx");
static PLAN: OnceLock<Result<Arc<TypedSimplePlan>, String>> = OnceLock::new();

pub(crate) struct Stream {
    #[cfg(test)]
    bypass_model: bool,
    plan: Arc<TypedSimplePlan>,
    state: [Tensor; 3],
    down: SincFilter,
    up: SincFilter,
    pending: VecDeque<f32>,
    output: VecDeque<f32>,
    analysis: [f32; FFT],
    overlap: [f32; HOP],
    window: [f32; FFT],
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    time: Vec<f32>,
    spectrum: Vec<Complex32>,
    forward_scratch: Vec<Complex32>,
    inverse_scratch: Vec<Complex32>,
}

impl Stream {
    pub(crate) fn new() -> Result<Self, String> {
        let plan = PLAN
            .get_or_init(|| {
                tract_onnx::onnx()
                    .model_for_read(&mut &BYTES[..])
                    .and_then(|m| m.with_input_fact(0, f32::fact([1, BINS, 1, 2]).into()))
                    .and_then(|m| m.with_input_fact(1, f32::fact([2, 1, 16, 16, 33]).into()))
                    .and_then(|m| m.with_input_fact(2, f32::fact([2, 3, 1, 1, 16]).into()))
                    .and_then(|m| m.with_input_fact(3, f32::fact([2, 1, 33, 16]).into()))
                    .and_then(|m| m.into_optimized())
                    .and_then(|m| m.into_runnable())
                    .map_err(|e| format!("GTCRN graph: {e:#}"))
            })
            .clone()?;
        let state = [
            Tensor::zero::<f32>(&[2, 1, 16, 16, 33]),
            Tensor::zero::<f32>(&[2, 3, 1, 1, 16]),
            Tensor::zero::<f32>(&[2, 1, 33, 16]),
        ];
        let [a, b, c] = state;
        let state = [
            a.map_err(|e| e.to_string())?,
            b.map_err(|e| e.to_string())?,
            c.map_err(|e| e.to_string())?,
        ];
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(FFT);
        let inverse = planner.plan_fft_inverse(FFT);
        Ok(Self {
            #[cfg(test)]
            bypass_model: false,
            plan,
            state,
            down: SincFilter::new(1.0 / 3.0),
            up: SincFilter::new(1.0),
            pending: VecDeque::with_capacity(HOP + 160),
            // One hop absorbs the 10 ms/16 ms block mismatch without periodically
            // inserting gaps. STFT overlap contributes another 16 ms.
            output: VecDeque::from(vec![0.0; HOP]),
            analysis: [0.0; FFT],
            overlap: [0.0; HOP],
            window: std::array::from_fn(|i| {
                (0.5 * (1.0 - (std::f32::consts::TAU * i as f32 / FFT as f32).cos())).sqrt()
            }),
            time: forward.make_input_vec(),
            spectrum: forward.make_output_vec(),
            forward_scratch: forward.make_scratch_vec(),
            inverse_scratch: inverse.make_scratch_vec(),
            forward,
            inverse,
        })
    }

    fn step(&mut self) -> Result<(), String> {
        self.analysis.copy_within(HOP.., 0);
        for sample in &mut self.analysis[HOP..] {
            *sample = self.pending.pop_front().ok_or("GTCRN partial frame")?;
        }
        for ((sample, input), window) in self.time.iter_mut().zip(self.analysis).zip(self.window) {
            *sample = input * window;
        }
        self.forward
            .process_with_scratch(
                &mut self.time,
                &mut self.spectrum,
                &mut self.forward_scratch,
            )
            .map_err(|e| e.to_string())?;
        #[cfg(test)]
        if !self.bypass_model {
            self.enhance()?;
        }
        #[cfg(not(test))]
        self.enhance()?;
        self.spectrum[0].im = 0.0;
        self.spectrum[BINS - 1].im = 0.0;
        self.inverse
            .process_with_scratch(
                &mut self.spectrum,
                &mut self.time,
                &mut self.inverse_scratch,
            )
            .map_err(|e| e.to_string())?;
        for (sample, window) in self.time.iter_mut().zip(self.window) {
            *sample *= window / FFT as f32;
        }
        for i in 0..HOP {
            self.output.push_back(self.overlap[i] + self.time[i]);
            self.overlap[i] = self.time[i + HOP];
        }
        Ok(())
    }

    fn enhance(&mut self) -> Result<(), String> {
        let packed: Vec<f32> = self.spectrum.iter().flat_map(|x| [x.re, x.im]).collect();
        let result = self
            .plan
            .run(tvec![
                Tensor::from_shape(&[1, BINS, 1, 2], &packed)
                    .map_err(|e| e.to_string())?
                    .into(),
                self.state[0].clone().into(),
                self.state[1].clone().into(),
                self.state[2].clone().into(),
            ])
            .map_err(|e| format!("GTCRN inference: {e:#}"))?;
        if result.len() != 4 {
            return Err("GTCRN returned the wrong output count".into());
        }
        let view = result[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| e.to_string())?;
        let values = view.as_slice().ok_or("GTCRN spectrum is not contiguous")?;
        if values.len() != BINS * 2 {
            return Err("GTCRN returned the wrong spectrum size".into());
        }
        for (bin, values) in self.spectrum.iter_mut().zip(values.chunks_exact(2)) {
            *bin = Complex32::new(values[0], values[1]);
        }
        for (state, result) in self.state.iter_mut().zip(&result[1..]) {
            if state.shape() != result.shape() {
                return Err("GTCRN state shape changed".into());
            }
            *state = result.clone().into_tensor();
        }
        Ok(())
    }

    pub(crate) fn process(&mut self, block: &mut [f32; BLOCK]) -> Result<(), String> {
        for (i, sample) in block.iter().enumerate() {
            self.down.push(*sample);
            if i % 3 == 2 {
                self.pending.push_back(self.down.at(0.0));
            }
        }
        while self.pending.len() >= HOP {
            self.step()?;
        }
        for chunk in block.chunks_exact_mut(3) {
            self.up.push(
                self.output
                    .pop_front()
                    .ok_or("GTCRN output timing underrun")?,
            );
            for (i, sample) in chunk.iter_mut().enumerate() {
                *sample = self.up.at(i as f64 / 3.0);
            }
        }
        debug_assert!(self.output.len() <= HOP * 2 && self.pending.len() < HOP);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stft_resampling_and_block_adapter_keep_the_declared_delay() {
        let mut stream = Stream::new().unwrap();
        stream.bypass_model = true;
        let impulse = 4800;
        let mut output = Vec::new();
        for frame in 0..30 {
            let mut block = [0.0; BLOCK];
            if frame * BLOCK == impulse {
                block[0] = 0.5;
            }
            stream.process(&mut block).unwrap();
            output.extend(block);
        }
        let peak = output
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .unwrap()
            .0;
        let delay = peak - impulse;
        eprintln!("GTCRN adapter measured buffering delay {delay} samples");
        // Downsampling phase picks sample 2; display rounds the conservative
        // full filter/hop budget upward, a difference below 0.05 ms.
        assert!(delay.abs_diff(crate::Model::Gtcrn.delay_samples()) <= 2);
    }
}
