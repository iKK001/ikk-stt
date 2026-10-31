//! Steady-noise removal with a learnt profile: a ventilator, a fan, a
//! fridge. The noise spectrum is learnt by minimum statistics — in every
//! frequency bin the quietest level of the last ~1.5 s is the noise,
//! whatever the speech does on top — and subtracted from every frame's
//! magnitude (phase untouched, floor at −20 dB so nothing goes to digital
//! silence). Unlike RNNoise it never models speech, so soft consonants
//! survive; unlike a gate it also cleans the audio under the words.
//!
//! STFT 32 ms frames, 16 ms hop, square-root Hann in and out (perfect
//! reconstruction). Output lags the input by one hop. A profile can be
//! seeded from a standalone recording of the noise ([`NoiseProfile::learn`]).

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

const FRAME: usize = 512;
const HOP: usize = FRAME / 2;
const BINS: usize = FRAME / 2 + 1;
/// Frames of history the minimum is taken over (≈ 1.5 s at 16 kHz).
const MIN_WINDOW: usize = 96;
/// Minimum statistics underestimate the noise; scale it up.
const BIAS: f32 = 1.6;
/// Over-subtraction and spectral floor (defaults; `IKK_PROFILE_ALPHA` and
/// `IKK_PROFILE_FLOOR` override them for experiments).
const ALPHA: f32 = 1.8;
const FLOOR: f32 = 0.1;

fn knob(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub struct NoiseProfile {
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    overlap: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<f32>,
    /// Smoothed magnitude per bin and its recent minima.
    smooth: Vec<f32>,
    history: Vec<Vec<f32>>,
    pos: usize,
    noise: Vec<f32>,
    frozen: bool,
    alpha: f32,
    floor: f32,
    /// Late-reverberation suppression (experiment knob `IKK_DEREVERB`,
    /// 0 = off): the magnitude a few frames back, scaled by this, is
    /// subtracted too — the room's tail, not the voice.
    dereverb: f32,
    past: Vec<Vec<f32>>,
    /// Adaptive equaliser (knob `IKK_LTAS_EQ`): band energies of speech
    /// frames so far, and the per-bin gain derived from them.
    ltas_eq: bool,
    band_energy: [f64; 18],
    speech_frames: u32,
    eq_gain: Vec<f32>,
    /// A fixed equaliser (18 third-octave gains in dB, 100 Hz–6.3 kHz):
    /// the inverse of a measured microphone path. Set once by calibration,
    /// applied always; empty = none.
    path_eq: bool,
}

/// Frames of delay for the reverb estimate (3 × 16 ms ≈ 50 ms).
const REVERB_DELAY: usize = 3;

/// Long-term spectrum of clean close-microphone speech, 1/3-octave bands
/// from 100 Hz to 6.3 kHz, dB relative to their mean (measured on the
/// reference recording). The adaptive equaliser bends the incoming
/// speech's long-term spectrum towards it — undoing what a room, a
/// loudspeaker or a phone across the table did to the voice (measured:
/// +14 dB boom at 220 Hz and a 7–9 dB hole at 1–2 kHz on such a path).
const SPEECH_LTAS_DB: [f32; 18] = [
    -1.5, 4.2, 7.6, 12.9, 17.4, 14.7, 3.3, 6.8, 3.7, 2.2, -5.2, -4.3, -3.8, -7.7, -11.3, -10.9, -12.5, -15.7,
];
/// Largest correction per band.
const LTAS_MAX_DB: f32 = 9.0;
/// Speech frames needed before the correction is applied (≈ 3 s).
const LTAS_MIN_FRAMES: u32 = 180;

impl NoiseProfile {
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FRAME);
        let ifft = planner.plan_fft_inverse(FRAME);
        let window = (0..FRAME)
            .map(|i| (std::f32::consts::PI * (i as f32 + 0.5) / FRAME as f32).sin())
            .collect();
        Self {
            fft,
            ifft,
            window,
            input: Vec::with_capacity(FRAME * 2),
            overlap: vec![0.0; FRAME],
            spectrum: vec![Complex::new(0.0, 0.0); BINS],
            scratch: vec![0.0; FRAME],
            smooth: vec![0.0; BINS],
            history: vec![vec![f32::MAX; BINS]; MIN_WINDOW / 8],
            pos: 0,
            noise: vec![0.0; BINS],
            frozen: false,
            alpha: knob("IKK_PROFILE_ALPHA", ALPHA),
            floor: knob("IKK_PROFILE_FLOOR", FLOOR),
            dereverb: knob("IKK_DEREVERB", 0.0),
            past: vec![vec![0.0; BINS]; REVERB_DELAY],
            ltas_eq: knob("IKK_LTAS_EQ", 0.0) > 0.0,
            band_energy: [0.0; 18],
            speech_frames: 0,
            eq_gain: vec![1.0; BINS],
            path_eq: false,
        }
    }

    /// With `false` the stage only equalises (no noise subtraction).
    pub fn subtracting(mut self, on: bool) -> Self {
        if !on {
            self.alpha = 0.0;
        }
        self
    }

    /// Use a fixed equaliser: `gains_db` per third-octave band from 100 Hz
    /// (18 bands); fewer values leave the upper bands at 0 dB.
    pub fn with_path_eq(mut self, gains_db: &[f32]) -> Self {
        if gains_db.iter().any(|g| g.abs() > 0.01) {
            let mut g = [0.0f32; 18];
            for (dst, src) in g.iter_mut().zip(gains_db) {
                *dst = src.clamp(-12.0, 12.0);
            }
            self.set_gains(&g);
            self.path_eq = true;
        }
        self
    }

    /// Learn the profile from a recording of the noise alone and keep it
    /// fixed (no adaptation during speech).
    pub fn learn(&mut self, noise: &[f32]) {
        let mut acc = vec![0.0f32; BINS];
        let mut n = 0;
        for frame in noise.chunks_exact(HOP).collect::<Vec<_>>().windows(2) {
            let mut buf: Vec<f32> = frame[0]
                .iter()
                .chain(frame[1].iter())
                .zip(&self.window)
                .map(|(s, w)| s * w)
                .collect();
            if self.fft.process(&mut buf, &mut self.spectrum).is_ok() {
                for (a, c) in acc.iter_mut().zip(&self.spectrum) {
                    *a += c.norm();
                }
                n += 1;
            }
        }
        if n > 0 {
            for (k, a) in acc.iter().enumerate() {
                self.noise[k] = a / n as f32;
            }
            self.frozen = true;
        }
    }

    pub fn reset(&mut self) {
        self.input.clear();
        self.overlap.iter_mut().for_each(|v| *v = 0.0);
        if !self.frozen {
            self.noise.iter_mut().for_each(|v| *v = 0.0);
            self.history
                .iter_mut()
                .for_each(|h| h.iter_mut().for_each(|v| *v = f32::MAX));
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        self.input.extend_from_slice(input);
        let mut out = Vec::with_capacity(input.len());
        while self.input.len() >= FRAME {
            for (i, s) in self.scratch.iter_mut().enumerate() {
                *s = self.input[i] * self.window[i];
            }
            if self.fft.process(&mut self.scratch, &mut self.spectrum).is_err() {
                break;
            }
            self.update_noise();
            if self.ltas_eq {
                self.update_eq();
            }
            let mags: Vec<f32> = self.spectrum.iter().map(|c| c.norm()).collect();
            for (k, c) in self.spectrum.iter_mut().enumerate() {
                let mag = mags[k];
                if mag > 0.0 {
                    let reverb = self.dereverb * self.past[0][k];
                    let cleaned = (mag - self.alpha * self.noise[k] - reverb).max(self.floor * mag);
                    *c *= cleaned / mag;
                }
            }
            if self.dereverb > 0.0 {
                self.past.rotate_left(1);
                self.past[REVERB_DELAY - 1].copy_from_slice(&mags);
            }
            // Colour correction after the cleaning, so boosted bands do not
            // boost the noise estimate.
            if self.ltas_eq || self.path_eq {
                for (c, g) in self.spectrum.iter_mut().zip(&self.eq_gain) {
                    *c *= *g;
                }
            }
            if self.ifft.process(&mut self.spectrum, &mut self.scratch).is_err() {
                break;
            }
            let norm = 1.0 / FRAME as f32;
            for i in 0..FRAME {
                self.overlap[i] += self.scratch[i] * norm * self.window[i];
            }
            out.extend_from_slice(&self.overlap[..HOP]);
            self.overlap.copy_within(HOP.., 0);
            self.overlap[HOP..].iter_mut().for_each(|v| *v = 0.0);
            self.input.drain(..HOP);
        }
        out
    }

    /// Minimum statistics over sub-windows of 8 frames: the smoothed
    /// magnitude's minimum over the last MIN_WINDOW frames, times BIAS.
    fn update_noise(&mut self) {
        if self.frozen {
            return;
        }
        let sub = self.pos / 8 % self.history.len();
        if self.pos % 8 == 0 {
            self.history[sub].iter_mut().for_each(|v| *v = f32::MAX);
        }
        for k in 0..BINS {
            let mag = self.spectrum[k].norm();
            self.smooth[k] = 0.7 * self.smooth[k] + 0.3 * mag;
            if self.smooth[k] < self.history[sub][k] {
                self.history[sub][k] = self.smooth[k];
            }
            let min = self.history.iter().map(|h| h[k]).fold(f32::MAX, f32::min);
            if min < f32::MAX {
                self.noise[k] = BIAS * min;
            }
        }
        self.pos += 1;
    }
}

impl NoiseProfile {
    fn band_of(k: usize, rate_hz: f32) -> Option<usize> {
        let f = k as f32 * rate_hz / FRAME as f32;
        if f < 100.0 {
            return None;
        }
        let b = (3.0 * (f / 100.0).log2()).floor() as usize;
        (b < 18).then_some(b)
    }

    /// Accumulate the speech spectrum (frames well above the noise) and
    /// derive the gain that moves it towards [`SPEECH_LTAS_DB`].
    fn update_eq(&mut self) {
        let rate = 16_000.0;
        let energy: f32 = self.spectrum.iter().map(|c| c.norm_sqr()).sum();
        let noise: f32 = self.noise.iter().map(|n| n * n).sum::<f32>().max(1e-9);
        if energy < 8.0 * noise {
            return;
        }
        for (k, c) in self.spectrum.iter().enumerate() {
            if let Some(b) = Self::band_of(k, rate) {
                self.band_energy[b] += c.norm_sqr() as f64;
            }
        }
        self.speech_frames += 1;
        if self.speech_frames < LTAS_MIN_FRAMES || self.speech_frames % 16 != 0 {
            return;
        }
        let db: Vec<f32> = self
            .band_energy
            .iter()
            .map(|e| 10.0 * (e / self.speech_frames as f64 + 1e-12).log10() as f32)
            .collect();
        let mean = db.iter().sum::<f32>() / 18.0;
        let mut gains_db = [0.0f32; 18];
        for (b, g) in gains_db.iter_mut().enumerate() {
            *g = (SPEECH_LTAS_DB[b] - (db[b] - mean)).clamp(-LTAS_MAX_DB, LTAS_MAX_DB);
        }
        self.set_gains(&gains_db);
    }

    /// Per-bin gains from 18 band gains: interpolated between band
    /// centres on a log scale, so no bin jumps against its neighbour.
    fn set_gains(&mut self, gains_db: &[f32; 18]) {
        let rate = 16_000.0;
        let centre = |b: usize| 100.0 * 2f32.powf((b as f32 + 0.5) / 3.0);
        for k in 0..BINS {
            let f = k as f32 * rate / FRAME as f32;
            let g = if f <= centre(0) {
                gains_db[0] * (f / centre(0)).max(0.0)
            } else if f >= centre(17) {
                gains_db[17]
            } else {
                let x = 3.0 * (f / 100.0).log2() - 0.5;
                let b = x.floor() as usize;
                let t = x - b as f32;
                gains_db[b] * (1.0 - t) + gains_db[b + 1] * t
            };
            self.eq_gain[k] = 10f32.powf(g / 20.0);
        }
    }
}

impl Default for NoiseProfile {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rms;
    use std::f32::consts::PI;

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
    fn steady_noise_is_removed_and_a_tone_on_top_survives() {
        let rate = 16_000;
        let n: Vec<f32> = noise(rate * 4, 0.02, 7);
        let mut p = NoiseProfile::new();
        // Two seconds of noise alone to learn, then noise plus a tone.
        let mut learnt = p.process(&n[..rate * 2]);
        let before_noise = rms(&n[..rate * 2]);
        let after_noise = rms(&learnt.split_off(learnt.len() / 2));
        assert!(
            after_noise < before_noise * 0.4,
            "noise {before_noise:.4} → {after_noise:.4}"
        );
        // A 0.6 s tone on top (a word, not steady noise) comes through.
        let mixed: Vec<f32> = n[rate * 2..rate * 2 + 9_600]
            .iter()
            .enumerate()
            .map(|(i, v)| v + 0.2 * (2.0 * PI * 700.0 * i as f32 / rate as f32).sin())
            .collect();
        let out = p.process(&mixed);
        let tail = &out[out.len() / 2..];
        assert!(rms(tail) > 0.11, "tone kept: {}", rms(tail));
    }

    #[test]
    fn a_learnt_profile_from_a_standalone_recording_is_used() {
        let rate = 16_000;
        let n: Vec<f32> = noise(rate, 0.02, 3);
        let mut p = NoiseProfile::new();
        p.learn(&n);
        let out = p.process(&noise(rate, 0.02, 3));
        assert!(rms(&out[out.len() / 2..]) < rms(&n) * 0.4);
    }
}
