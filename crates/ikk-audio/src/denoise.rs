//! RNNoise denoise stage.
//!
//! `nnnoiseless` works only on 48 kHz frames of 480 samples and expects
//! i16-range amplitudes. The pipeline runs at 16 kHz in [-1, 1], so this
//! stage upsamples ×3, denoises, downsamples /3 and rescales. Forgetting the
//! i16 scaling yields silence — a classic integration bug avoided here.

use nnnoiseless::DenoiseState;

const RNNOISE_FRAME: usize = nnnoiseless::FRAME_SIZE;
const RNNOISE_RATE: u32 = 48_000;
const I16_SCALE: f32 = i16::MAX as f32;

pub struct DenoiseStage {
    state: Box<DenoiseState<'static>>,
    input_rate: u32,
    residual_48k: Vec<f32>,
    last_voice_prob: f32,
}

impl DenoiseStage {
    pub fn new(input_rate: u32) -> Self {
        Self {
            state: DenoiseState::new(),
            input_rate,
            residual_48k: Vec::with_capacity(RNNOISE_FRAME * 2),
            last_voice_prob: 0.0,
        }
    }

    /// RNNoise's own speech probability for the last frame (0..1).
    pub fn last_voice_probability(&self) -> f32 {
        self.last_voice_prob
    }

    fn factor(&self) -> Option<usize> {
        if self.input_rate == 0 || RNNOISE_RATE % self.input_rate != 0 {
            return None;
        }
        Some((RNNOISE_RATE / self.input_rate) as usize)
    }

    pub fn reset(&mut self) {
        self.state = DenoiseState::new();
        self.residual_48k.clear();
        self.last_voice_prob = 0.0;
    }

    /// Denoise a block. Output may lag by up to one 10 ms frame (buffering).
    /// Unsupported input rates pass through unchanged.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        match self.factor() {
            Some(1) => self.denoise_48k(input),
            Some(f) => {
                let up = upsample_linear(input, f);
                let den = self.denoise_48k(&up);
                downsample_avg(&den, f)
            }
            None => input.to_vec(),
        }
    }

    fn denoise_48k(&mut self, input_48k: &[f32]) -> Vec<f32> {
        self.residual_48k.extend_from_slice(input_48k);
        let full = self.residual_48k.len() / RNNOISE_FRAME;
        if full == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(full * RNNOISE_FRAME);
        let mut in_frame = [0.0f32; RNNOISE_FRAME];
        let mut out_frame = [0.0f32; RNNOISE_FRAME];
        for f in 0..full {
            let start = f * RNNOISE_FRAME;
            for (dst, &src) in in_frame
                .iter_mut()
                .zip(&self.residual_48k[start..start + RNNOISE_FRAME])
            {
                *dst = src * I16_SCALE;
            }
            self.last_voice_prob = self.state.process_frame(&mut out_frame, &in_frame);
            out.extend(out_frame.iter().map(|&s| (s / I16_SCALE).clamp(-1.0, 1.0)));
        }
        self.residual_48k.drain(0..full * RNNOISE_FRAME);
        out
    }
}

fn upsample_linear(input: &[f32], factor: usize) -> Vec<f32> {
    if factor <= 1 || input.is_empty() {
        return input.to_vec();
    }
    let mut out = Vec::with_capacity(input.len() * factor);
    for i in 0..input.len() {
        let a = input[i];
        let b = if i + 1 < input.len() { input[i + 1] } else { a };
        for k in 0..factor {
            out.push(a + (b - a) * (k as f32 / factor as f32));
        }
    }
    out
}

fn downsample_avg(input: &[f32], factor: usize) -> Vec<f32> {
    if factor <= 1 || input.is_empty() {
        return input.to_vec();
    }
    input
        .chunks_exact(factor)
        .map(|c| c.iter().sum::<f32>() / factor as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rms;
    use std::f32::consts::PI;

    fn tone(freq: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / rate as f32).sin() * 0.3)
            .collect()
    }

    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut x = seed.wrapping_add(0x9E3779B97F4A7C15);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x >> 40) as f32 / 16_777_216.0 - 0.5) * 2.0 * amp
            })
            .collect()
    }

    #[test]
    fn resample_roundtrip_preserves_dc() {
        let dc = vec![0.5f32; 600];
        for v in downsample_avg(&upsample_linear(&dc, 3), 3) {
            assert!((v - 0.5).abs() < 1e-4);
        }
    }

    #[test]
    fn unsupported_rate_is_passthrough() {
        let mut s = DenoiseStage::new(44_100);
        let input = tone(220.0, 44_100, 1024);
        assert_eq!(s.process(&input), input);
    }

    #[test]
    fn output_length_tracks_input_within_one_frame() {
        let mut s = DenoiseStage::new(16_000);
        let input = tone(180.0, 16_000, 8000);
        let produced: usize = input.chunks(512).map(|b| s.process(b).len()).sum();
        let diff = (input.len() as i64 - produced as i64).abs();
        assert!(diff < (RNNOISE_FRAME / 3) as i64 + 2, "drift {diff}");
    }

    #[test]
    fn denoise_reduces_energy_without_collapsing() {
        let n = 16_000;
        let clean = tone(200.0, 16_000, n);
        let nz = noise(n, 0.15, 42);
        let noisy: Vec<f32> = clean.iter().zip(&nz).map(|(c, x)| c + x).collect();
        let mut s = DenoiseStage::new(16_000);
        let _ = s.process(&noisy);
        let out = s.process(&noisy);
        assert!(!out.is_empty());
        assert!(rms(&out) < rms(&noisy));
        assert!(rms(&out) > 1e-4, "collapsed to silence: scaling bug");
        assert!((0.0..=1.0).contains(&s.last_voice_probability()));
    }
}
