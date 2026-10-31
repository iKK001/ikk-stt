//! Portable tone shaping for the capture chain, all platforms: a high-pass
//! against rumble and ventilator hum, a presence lift around 3 kHz that
//! sharpens soft consonants, and a fast compressor that flattens the
//! 25–30 Hz flutter of a rolled R so it reads more like a plain German R.
//! Plain biquads and a peak follower; no allocation per block.

/// Direct-form-I biquad.
#[derive(Debug, Clone)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Second-order Butterworth high-pass.
    pub fn highpass(rate_hz: u32, cutoff_hz: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * cutoff_hz / rate_hz as f32;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self::new(
            (1.0 + cos) / 2.0 / a0,
            -(1.0 + cos) / a0,
            (1.0 + cos) / 2.0 / a0,
            -2.0 * cos / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// Peaking EQ: `gain_db` at `center_hz`, width `q`.
    pub fn peaking(rate_hz: u32, center_hz: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * center_hz / rate_hz as f32;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self::new(
            (1.0 + alpha * a) / a0,
            -2.0 * cos / a0,
            (1.0 - alpha * a) / a0,
            -2.0 * cos / a0,
            (1.0 - alpha / a) / a0,
        )
    }

    fn new(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            let x = *s;
            let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = y;
            *s = y;
        }
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Fast peak compressor: threshold −30 dBFS, ratio 8, attack 1 ms,
/// release 8 ms (short enough to follow the 25–30 Hz flutter of a rolled
/// R), makeup +4 dB. Envelope per sample, gain in linear terms.
#[derive(Debug, Clone)]
pub struct Compressor {
    threshold: f32,
    ratio: f32,
    attack: f32,
    release: f32,
    makeup: f32,
    envelope: f32,
}

impl Compressor {
    pub fn new(rate_hz: u32) -> Self {
        let coef = |ms: f32| (-1.0 / (rate_hz as f32 * ms / 1000.0)).exp();
        Self {
            threshold: 10f32.powf(-30.0 / 20.0),
            ratio: 8.0,
            attack: coef(1.0),
            release: coef(8.0),
            makeup: 10f32.powf(4.0 / 20.0),
            envelope: 0.0,
        }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            let level = s.abs();
            let coef = if level > self.envelope {
                self.attack
            } else {
                self.release
            };
            self.envelope = coef * self.envelope + (1.0 - coef) * level;
            let gain = if self.envelope > self.threshold {
                // Above the threshold only 1/ratio of the excess gets through.
                let over = self.envelope / self.threshold;
                over.powf(1.0 / self.ratio - 1.0)
            } else {
                1.0
            };
            *s = (*s * gain * self.makeup).clamp(-1.0, 1.0);
        }
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
    }
}

/// Trill filler: a rolled R is a run of short closures (25–30 Hz) in an
/// otherwise voiced sound. Two envelope followers — one that drops into
/// the closures, one that holds the level across them — give the gain
/// that lifts the closures back to the surrounding level, so the engine
/// hears a steady R-like sound. Syllable gaps (100 ms and more) are left
/// alone because the slow follower has decayed by then.
#[derive(Debug, Clone)]
pub struct TrillFiller {
    attack: f32,
    fast_release: f32,
    slow_release: f32,
    fast: f32,
    slow: f32,
}

/// Loudest lift applied inside a closure, linear (16 dB).
const TRILL_MAX_LIFT: f32 = 6.0;
/// Below this level nothing is lifted (silence stays silence).
const TRILL_FLOOR: f32 = 0.003;

impl TrillFiller {
    pub fn new(rate_hz: u32) -> Self {
        let coef = |ms: f32| (-1.0 / (rate_hz as f32 * ms / 1000.0)).exp();
        Self {
            attack: coef(0.3),
            fast_release: coef(2.0),
            slow_release: coef(45.0),
            fast: 0.0,
            slow: 0.0,
        }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            let level = s.abs();
            let cf = if level > self.fast {
                self.attack
            } else {
                self.fast_release
            };
            self.fast = cf * self.fast + (1.0 - cf) * level;
            let cs = if level > self.slow {
                self.attack
            } else {
                self.slow_release
            };
            self.slow = cs * self.slow + (1.0 - cs) * level;
            if self.fast > TRILL_FLOOR && self.slow > self.fast {
                let lift = (self.slow / self.fast).min(TRILL_MAX_LIFT);
                *s = (*s * lift).clamp(-1.0, 1.0);
            }
        }
    }

    pub fn reset(&mut self) {
        self.fast = 0.0;
        self.slow = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rms;
    use std::f32::consts::PI;

    #[test]
    fn trill_filler_lifts_the_closures_but_not_syllable_gaps() {
        let rate = 16_000;
        // 1 kHz with 28 Hz closures (0.3 → 0.05), then a 150 ms gap.
        let mut x: Vec<f32> = (0..16_000)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let env = if (t * 28.0).fract() < 0.35 { 0.05 } else { 0.3 };
                env * (2.0 * PI * 1000.0 * t).sin()
            })
            .collect();
        let gap_start = x.len();
        x.extend(std::iter::repeat_n(0.0f32, 2_400));
        x.extend((0..3_200).map(|i| 0.3 * (2.0 * PI * 1000.0 * i as f32 / rate as f32).sin()));
        let depth = |v: &[f32]| {
            let blocks: Vec<f32> = v[8_000..16_000].chunks(80).map(rms).collect();
            blocks.iter().cloned().fold(0.0, f32::max) / blocks.iter().cloned().fold(f32::MAX, f32::min)
        };
        let before = depth(&x);
        TrillFiller::new(rate).process(&mut x);
        let after = depth(&x);
        assert!(after < before * 0.5, "closures filled: {before:.1} → {after:.1}");
        assert!(rms(&x[gap_start..gap_start + 2_400]) < 1e-6, "silence stays silence");
    }

    fn tone(freq: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / rate as f32).sin() * 0.3)
            .collect()
    }

    #[test]
    fn highpass_removes_hum_and_keeps_speech_band() {
        let mut hp = Biquad::highpass(16_000, 150.0);
        let mut hum = tone(50.0, 16_000, 16_000);
        let mut voice = tone(1_000.0, 16_000, 16_000);
        let (h0, v0) = (rms(&hum), rms(&voice));
        hp.process(&mut hum);
        hp.reset();
        hp.process(&mut voice);
        assert!(rms(&hum[8_000..]) < h0 * 0.2, "hum {}", rms(&hum[8_000..]) / h0);
        assert!((rms(&voice[8_000..]) - v0).abs() < v0 * 0.1);
    }

    #[test]
    fn presence_lifts_three_khz_only() {
        let mut eq = Biquad::peaking(16_000, 3_000.0, 1.0, 6.0);
        let mut mid = tone(3_000.0, 16_000, 16_000);
        let mut low = tone(300.0, 16_000, 16_000);
        let (m0, l0) = (rms(&mid), rms(&low));
        eq.process(&mut mid);
        eq.reset();
        eq.process(&mut low);
        assert!(rms(&mid[8_000..]) > m0 * 1.7, "{}", rms(&mid[8_000..]) / m0);
        assert!((rms(&low[8_000..]) - l0).abs() < l0 * 0.15);
    }

    #[test]
    fn compressor_flattens_a_trill_like_flutter() {
        // A 1 kHz tone with 28 Hz amplitude flutter between 0.05 and 0.3.
        let rate = 16_000;
        let mut x: Vec<f32> = (0..rate as usize)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let env = 0.175 + 0.125 * (2.0 * PI * 28.0 * t).sin();
                env * (2.0 * PI * 1000.0 * t).sin()
            })
            .collect();
        let depth = |v: &[f32]| {
            let blocks: Vec<f32> = v[8_000..].chunks(160).map(rms).collect();
            let max = blocks.iter().cloned().fold(0.0, f32::max);
            let min = blocks.iter().cloned().fold(f32::MAX, f32::min);
            max / min
        };
        let before = depth(&x);
        Compressor::new(rate).process(&mut x);
        let after = depth(&x);
        assert!(after < before * 0.6, "flutter {before:.2} → {after:.2}");
    }
}
