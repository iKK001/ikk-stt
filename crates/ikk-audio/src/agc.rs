//! Automatic Gain Control.
//!
//! Levels the microphone signal to a target RMS so quiet speakers do not
//! get lost in STT and loud passages do not clip. Pure DSP, streaming,
//! state kept across blocks. Separate attack (fast down) and release (slow
//! up) avoid pumping and noise lift; a soft-knee limiter guards clipping.

use crate::dsp::rms;

#[derive(Debug, Clone, Copy)]
pub struct AgcConfig {
    /// Target RMS in dBFS (negative, e.g. −20).
    pub target_dbfs: f32,
    pub max_gain: f32,
    pub min_gain: f32,
    /// Smoothing when gain goes *down* (0 < a < 1, larger = faster).
    pub attack: f32,
    /// Smoothing when gain goes *up* (smaller = gentler).
    pub release: f32,
    /// Below this RMS nothing is amplified (silence / noise).
    pub noise_gate_rms: f32,
    /// Soft-knee limiter threshold (linear, < 1).
    pub limiter_threshold: f32,
}

impl Default for AgcConfig {
    fn default() -> Self {
        Self {
            target_dbfs: -20.0,
            max_gain: 8.0,
            min_gain: 0.1,
            attack: 0.25,
            release: 0.02,
            noise_gate_rms: 0.0008,
            limiter_threshold: 0.95,
        }
    }
}

impl AgcConfig {
    fn target_linear(&self) -> f32 {
        10.0f32.powf(self.target_dbfs / 20.0)
    }
}

pub struct AgcStage {
    config: AgcConfig,
    gain: f32,
}

impl AgcStage {
    pub fn new(config: AgcConfig) -> Self {
        Self { config, gain: 1.0 }
    }

    pub fn current_gain(&self) -> f32 {
        self.gain
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        let level = rms(input);
        if level < self.config.noise_gate_rms {
            // Silence: let gain drift back towards unity, never pump noise up.
            self.gain += (1.0 - self.gain) * self.config.release;
            return input
                .iter()
                .map(|&s| limit(s * self.gain, self.config.limiter_threshold))
                .collect();
        }
        let desired = (self.config.target_linear() / level).clamp(self.config.min_gain, self.config.max_gain);
        let coeff = if desired < self.gain {
            self.config.attack
        } else {
            self.config.release
        };
        self.gain += (desired - self.gain) * coeff;
        self.gain = self.gain.clamp(self.config.min_gain, self.config.max_gain);
        input
            .iter()
            .map(|&s| limit(s * self.gain, self.config.limiter_threshold))
            .collect()
    }
}

/// Soft-knee limiter: linear below `thr`, compressed towards ±1 above.
fn limit(x: f32, thr: f32) -> f32 {
    let a = x.abs();
    if a <= thr {
        return x;
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let over = a - thr;
    let headroom = 1.0 - thr;
    let compressed = thr + headroom * (over / (over + headroom)).min(1.0);
    sign * compressed.min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(s: &[f32]) -> f32 {
        s.iter().map(|x| x.abs()).fold(0.0f32, f32::max)
    }

    #[test]
    fn quiet_signal_gets_boosted() {
        let mut agc = AgcStage::new(AgcConfig::default());
        let quiet = vec![0.01f32; 1600];
        let mut last = Vec::new();
        for _ in 0..50 {
            last = agc.process(&quiet);
        }
        assert!(rms(&last) > rms(&quiet) * 2.0);
        assert!(agc.current_gain() > 1.0);
    }

    #[test]
    fn loud_signal_not_clipped() {
        let mut agc = AgcStage::new(AgcConfig::default());
        let loud: Vec<f32> = (0..1600).map(|i| if i % 2 == 0 { 0.98 } else { -0.98 }).collect();
        for _ in 0..50 {
            assert!(peak(&agc.process(&loud)) <= 1.0);
        }
        assert!(agc.current_gain() < 1.0);
    }

    #[test]
    fn limiter_is_monotonic_and_bounded() {
        let mut prev = -1.0;
        let mut x = -2.0;
        while x <= 2.0 {
            let y = limit(x, 0.95);
            assert!(y.abs() <= 1.0);
            assert!(y >= prev - 1e-6);
            prev = y;
            x += 0.01;
        }
    }

    #[test]
    fn silence_does_not_explode_gain() {
        let mut agc = AgcStage::new(AgcConfig::default());
        for _ in 0..100 {
            agc.process(&[0.0f32; 1600]);
        }
        assert!(agc.current_gain() <= 1.0 + 1e-3);
    }
}
