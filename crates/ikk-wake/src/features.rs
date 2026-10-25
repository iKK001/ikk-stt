//! MFCC features: 25 ms windows every 10 ms, 26 mel bands, 13 cepstral
//! coefficients, mean-normalised per utterance (independent of loudness and
//! microphone colour).

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::SAMPLE_RATE_HZ;

pub const WINDOW: usize = 400; // 25 ms
pub const HOP: usize = 160; // 10 ms
const FFT_SIZE: usize = 512;
const MEL_BANDS: usize = 26;
pub const COEFFS: usize = 13;

/// One feature vector per 10 ms.
pub type Frame = [f32; COEFFS];

pub struct Extractor {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    filters: Vec<Vec<(usize, f32)>>,
}

impl Default for Extractor {
    fn default() -> Self {
        Self::new()
    }
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10f32.powf(mel / 2595.0) - 1.0)
}

impl Extractor {
    pub fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window = (0..WINDOW)
            .map(|i| 0.54 - 0.46 * (2.0 * PI * i as f32 / (WINDOW - 1) as f32).cos())
            .collect();
        // Triangular mel filters between 100 Hz and 7.6 kHz.
        let (lo, hi) = (hz_to_mel(100.0), hz_to_mel(7600.0));
        let points: Vec<f32> = (0..MEL_BANDS + 2)
            .map(|i| mel_to_hz(lo + (hi - lo) * i as f32 / (MEL_BANDS + 1) as f32))
            .collect();
        let bin = |hz: f32| hz * FFT_SIZE as f32 / SAMPLE_RATE_HZ as f32;
        let filters = (0..MEL_BANDS)
            .map(|m| {
                let (l, c, r) = (bin(points[m]), bin(points[m + 1]), bin(points[m + 2]));
                (l.floor() as usize..=r.ceil() as usize)
                    .filter(|&k| k <= FFT_SIZE / 2)
                    .filter_map(|k| {
                        let k_f = k as f32;
                        let w = if k_f < c {
                            (k_f - l) / (c - l)
                        } else {
                            (r - k_f) / (r - c)
                        };
                        (w > 0.0).then_some((k, w))
                    })
                    .collect()
            })
            .collect();
        Self { fft, window, filters }
    }

    /// Features of a whole utterance (16 kHz mono), quiet edges trimmed,
    /// mean-normalised.
    pub fn utterance(&self, samples: &[f32]) -> Vec<Frame> {
        let samples = trim_quiet_edges(samples);
        if samples.len() < WINDOW {
            return Vec::new();
        }
        let mut frames: Vec<Frame> = (0..=(samples.len() - WINDOW) / HOP)
            .map(|i| self.frame(&samples[i * HOP..i * HOP + WINDOW]))
            .collect();
        let n = frames.len() as f32;
        let mut mean = [0f32; COEFFS];
        for f in &frames {
            for (m, v) in mean.iter_mut().zip(f) {
                *m += v / n;
            }
        }
        for f in &mut frames {
            for (v, m) in f.iter_mut().zip(&mean) {
                *v -= m;
            }
        }
        frames
    }

    fn frame(&self, chunk: &[f32]) -> Frame {
        let mut buf = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        // Pre-emphasis lifts consonants, which carry most of the identity.
        let mut prev = chunk[0];
        for (i, (&s, w)) in chunk.iter().zip(&self.window).enumerate() {
            let emph = if i == 0 { s } else { s - 0.97 * prev };
            prev = s;
            buf[i].re = emph * w;
        }
        self.fft.process(&mut buf);
        let power: Vec<f32> = buf[..=FFT_SIZE / 2].iter().map(|c| c.norm_sqr()).collect();
        let log_mel: Vec<f32> = self
            .filters
            .iter()
            .map(|f| (f.iter().map(|&(k, w)| power[k] * w).sum::<f32>() + 1e-10).ln())
            .collect();
        // DCT-II of the log mel energies.
        let mut out = [0f32; COEFFS];
        for (c, o) in out.iter_mut().enumerate() {
            *o = log_mel
                .iter()
                .enumerate()
                .map(|(m, &e)| e * (PI * c as f32 * (m as f32 + 0.5) / MEL_BANDS as f32).cos())
                .sum();
        }
        out
    }
}

/// Drop leading and trailing parts more than 30 dB below the loudest 10 ms,
/// so lead-in and pause around a word do not count as part of it.
pub fn trim_quiet_edges(samples: &[f32]) -> &[f32] {
    let energy: Vec<f32> = samples
        .chunks(HOP)
        .map(|c| c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32)
        .collect();
    let peak = energy.iter().copied().fold(0f32, f32::max);
    if peak <= 0.0 {
        return &samples[..0];
    }
    let floor = peak * 1e-3; // -30 dB
    let first = energy.iter().position(|&e| e > floor).unwrap_or(0);
    let last = energy.iter().rposition(|&e| e > floor).unwrap_or(0);
    let start = first * HOP;
    let end = ((last + 1) * HOP).min(samples.len());
    &samples[start..end]
}
