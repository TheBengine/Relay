//! Band-limited device-rate conversion shared by capture and voice playback.
use std::sync::{Arc, OnceLock};
pub(crate) const TAPS: usize = 64;
const PHASES: usize = 1024;

fn kernels(cutoff: f64) -> Arc<[[f32; TAPS]]> {
    fn generate(cutoff: f64) -> Arc<[[f32; TAPS]]> {
        (0..=PHASES)
            .map(|phase| {
                let fraction = phase as f64 / PHASES as f64;
                let mut row = [0.0_f64; TAPS];
                for (lag, coefficient) in row.iter_mut().enumerate() {
                    let distance = lag as f64 - (TAPS / 2) as f64 + fraction;
                    let x = distance / (TAPS / 2) as f64;
                    if x.abs() < 1.0 {
                        let window = 0.42
                            + 0.5 * (std::f64::consts::PI * x).cos()
                            + 0.08 * (std::f64::consts::TAU * x).cos();
                        let angle = std::f64::consts::PI * distance;
                        let sinc = if angle.abs() < 1e-12 {
                            cutoff
                        } else {
                            (cutoff * angle).sin() / angle
                        };
                        *coefficient = window * sinc;
                    }
                }
                let sum = row.iter().sum::<f64>();
                row.map(|value| (value / sum) as f32)
            })
            .collect::<Vec<_>>()
            .into()
    }
    static FULL_BAND: OnceLock<Arc<[[f32; TAPS]]>> = OnceLock::new();
    if cutoff == 1.0 {
        FULL_BAND.get_or_init(|| generate(1.0)).clone()
    } else {
        generate(cutoff)
    }
}

pub(crate) struct SincFilter {
    history: [f32; TAPS],
    head: usize,
    table: Arc<[[f32; TAPS]]>,
}
impl SincFilter {
    /// Coefficients are prepared before callbacks start; rendering allocates nothing.
    pub(crate) fn new(cutoff: f64) -> Self {
        assert!(cutoff > 0.0 && cutoff <= 1.0);
        Self {
            history: [0.0; TAPS],
            head: 0,
            table: kernels(cutoff),
        }
    }
    pub(crate) fn reset(&mut self) {
        self.history.fill(0.0);
        self.head = 0;
    }
    pub(crate) fn push(&mut self, value: f32) {
        self.history[self.head] = value;
        self.head = (self.head + 1) & (TAPS - 1);
    }
    pub(crate) fn at(&self, phase: f64) -> f32 {
        debug_assert!((0.0..1.0).contains(&phase));
        let position = phase * PHASES as f64;
        let row = (position.floor() as usize).min(PHASES - 1);
        let blend = (position - row as f64) as f32;
        let mut value = 0.0_f32;
        for lag in 0..TAPS {
            let coefficient =
                self.table[row][lag] + (self.table[row + 1][lag] - self.table[row][lag]) * blend;
            value += self.history[(self.head + TAPS - 1 - lag) & (TAPS - 1)] * coefficient;
        }
        value
    }

    /// Preserve exact DC without turning floating-point coefficient roundoff
    /// into a peak change. Other content uses the unmodified filter response.
    pub(crate) fn at_exact_dc(&self, phase: f64) -> f32 {
        self.constant_value().unwrap_or_else(|| self.at(phase))
    }
    pub(crate) fn constant_value(&self) -> Option<f32> {
        let first = self.history[0];
        self.history
            .iter()
            .all(|sample| *sample == first)
            .then_some(first)
    }
}
