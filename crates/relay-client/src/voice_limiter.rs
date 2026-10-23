//! Linked output protection after voice, shared audio, volume and notification mixing.
//! No lookahead, device access, allocation or retained audio samples.
pub(crate) struct OutputLimiter {
    gain: f32,
    release: f32,
}

impl OutputLimiter {
    pub(crate) fn new(rate: u32) -> Self {
        Self {
            gain: 1.0,
            release: 1.0 - (-1.0 / (rate.max(1) as f32 * 0.080)).exp(),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.gain = 1.0;
    }

    pub(crate) fn frame(&mut self, input: [f32; 2]) -> [f32; 2] {
        let input = input.map(|v| if v.is_finite() { v } else { 0.0 });
        let peak = input.iter().fold(1.0_f32, |peak, v| peak.max(v.abs()));
        // Immediate protection, gradual recovery. One shared gain preserves
        // stereo position and avoids normalizing every 2.5 ms mixer block.
        self.gain = (self.gain + (1.0 - self.gain) * self.release).min(1.0 / peak);
        input.map(|v| (v * self.gain).clamp(-1.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_audio_is_unchanged_and_overload_preserves_stereo_balance() {
        let mut limiter = OutputLimiter::new(48000);
        assert_eq!(limiter.frame([0.8, -0.4]), [0.8, -0.4]);
        assert_eq!(limiter.frame([2.0, -1.0]), [1.0, -0.5]);
        let next = limiter.frame([0.8, -0.4]);
        assert!(
            (0.4..0.401).contains(&next[0]),
            "abrupt gain release: {next:?}"
        );
        assert_eq!(next[0], -next[1] * 2.0);
        for _ in 0..48000 {
            limiter.frame([0.0; 2]);
        }
        assert!(limiter.frame([0.8, -0.4])[0] > 0.799);
        limiter.reset();
        assert_eq!(limiter.frame([0.8, -0.4]), [0.8, -0.4]);
    }

    #[test]
    fn mixed_overload_has_fewer_flat_tops_than_hard_clipping_at_common_device_rates() {
        for rate in [8000, 44100, 48000, 96000] {
            let mut limiter = OutputLimiter::new(rate);
            let (mut hard_flat, mut limited_flat) = (0, 0);
            for sample in 0..rate {
                let v = (sample as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 1.6;
                hard_flat += usize::from(v.abs() >= 1.0);
                let out = limiter.frame([v, v * 0.5]);
                assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
                assert_eq!(out[0], out[1] * 2.0);
                limited_flat += usize::from(out[0].abs() >= 0.9999);
            }
            assert!(
                limited_flat * 4 < hard_flat,
                "{rate}: {limited_flat} versus {hard_flat}"
            );
        }
        let mut limiter = OutputLimiter::new(48000);
        assert_eq!(limiter.frame([f32::NAN, f32::INFINITY]), [0.0; 2]);
    }
}
