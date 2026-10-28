//! Deterministic test signals at 48 kHz: speech-like talkers (glottal source,
//! moving formants, syllables and pauses) and keyboard/mouse clicks. Test-only.

pub(crate) const RATE: f32 = 48_000.0;

struct Random(u32);
impl Random {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / 16_777_215.0
    }
    fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.next()
    }
}

struct Resonator {
    a1: f32,
    a2: f32,
    gain: f32,
    y1: f32,
    y2: f32,
}
impl Resonator {
    fn new() -> Self {
        Self { a1: 0.0, a2: 0.0, gain: 0.0, y1: 0.0, y2: 0.0 }
    }
    fn tune(&mut self, frequency: f32, bandwidth: f32) {
        let r = (-std::f32::consts::PI * bandwidth / RATE).exp();
        self.a1 = -2.0 * r * (std::f32::consts::TAU * frequency / RATE).cos();
        self.a2 = r * r;
        self.gain = 1.0 - r;
    }
    fn run(&mut self, x: f32) -> f32 {
        let y = self.gain * x - self.a1 * self.y1 - self.a2 * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

const VOWELS: [[f32; 3]; 5] = [
    [800.0, 1200.0, 2500.0],
    [300.0, 2300.0, 3000.0],
    [350.0, 800.0, 2400.0],
    [500.0, 1800.0, 2500.0],
    [500.0, 900.0, 2400.0],
];

/// Speech-like talker: syllables of 120–260 ms with 40–140 ms gaps, a longer
/// pause every few syllables, a short consonant burst at each onset.
/// Returns samples scaled so the loudest syllables peak near `peak`.
pub(crate) fn talker(seconds: f32, f0: (f32, f32), seed: u32, peak: f32) -> Vec<f32> {
    let total = (seconds * RATE) as usize;
    let mut random = Random(seed);
    let mut out = vec![0.0f32; total];
    let mut formants = [Resonator::new(), Resonator::new(), Resonator::new()];
    let mut at = (random.range(0.02, 0.1) * RATE) as usize;
    let mut phase = 0.0f32;
    let mut syllable = 0;
    while at < total {
        let length = (random.range(0.12, 0.26) * RATE) as usize;
        let vowel = VOWELS[(random.next() * 5.0) as usize % 5];
        for (resonator, frequency) in formants.iter_mut().zip(vowel) {
            resonator.tune(frequency * random.range(0.9, 1.1), 90.0);
        }
        let start_f0 = random.range(f0.0, f0.1);
        let end_f0 = start_f0 * random.range(0.9, 1.1);
        let loudness = random.range(0.6, 1.0);
        let burst = (random.range(0.015, 0.03) * RATE) as usize;
        for index in 0..length.min(total - at) {
            let progress = index as f32 / length as f32;
            let f = start_f0 + (end_f0 - start_f0) * progress;
            phase += f / RATE;
            if phase >= 1.0 {
                phase -= 1.0;
            }
            let source = 2.0 * phase - 1.0;
            let voiced = formants[0].run(source) * 1.0
                + formants[1].run(source) * 0.6
                + formants[2].run(source) * 0.3;
            let attack = (index as f32 / (0.02 * RATE)).min(1.0);
            let release = ((length - index) as f32 / (0.04 * RATE)).min(1.0);
            let consonant = if index < burst {
                (random.next() - 0.5) * 0.4 * (1.0 - index as f32 / burst as f32)
            } else {
                0.0
            };
            out[at + index] = (voiced * attack * release + consonant) * loudness;
        }
        at += length + (random.range(0.04, 0.14) * RATE) as usize;
        syllable += 1;
        if syllable % 6 == 0 {
            at += (random.range(0.3, 0.7) * RATE) as usize;
        }
    }
    let loudest = out.iter().fold(0.0f32, |p, s| p.max(s.abs())).max(1e-9);
    out.iter_mut().for_each(|s| *s *= peak / loudest);
    out
}

/// Keyboard and mouse clicks: sharp broadband onsets decaying over 4–12 ms,
/// some with a second "release" click, at random times.
pub(crate) fn clicks(seconds: f32, per_second: f32, peak: f32, seed: u32) -> (Vec<f32>, Vec<usize>) {
    let total = (seconds * RATE) as usize;
    let mut random = Random(seed);
    let mut out = vec![0.0f32; total];
    let mut onsets = Vec::new();
    let mut at = (random.range(0.05, 0.3) * RATE) as usize;
    let mut resonator = Resonator::new();
    while at < total {
        onsets.push(at);
        resonator.tune(random.range(2_500.0, 6_000.0), 1_500.0);
        let decay = random.range(0.004, 0.012) * RATE;
        let amplitude = peak * random.range(0.5, 1.0);
        let length = (decay * 4.0) as usize;
        let second = (random.range(0.03, 0.09) * RATE) as usize;
        for (offset, scale) in [(0usize, 1.0f32), (second, 0.6)] {
            for index in 0..length {
                let Some(slot) = out.get_mut(at + offset + index) else { break };
                let noise = (random.next() - 0.5) * 2.0;
                let ring = resonator.run(noise) * 6.0;
                *slot += (noise * 0.5 + ring) * amplitude * scale * (-(index as f32) / decay).exp();
            }
        }
        at += (random.range(0.5, 1.5) / per_second * RATE) as usize;
    }
    (out, onsets)
}

pub(crate) fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

pub(crate) fn db(value: f32) -> f32 {
    20.0 * value.max(1e-9).log10()
}
