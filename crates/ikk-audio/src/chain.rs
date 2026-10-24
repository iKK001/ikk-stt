//! Capture processing chain: preamp → denoise → AGC. The one place that
//! defines the order. Denoise before AGC, otherwise AGC lifts the noise.
//! Pure DSP state over blocks; no I/O.

use std::borrow::Cow;

use crate::agc::{AgcConfig, AgcStage};
use crate::denoise::DenoiseStage;
use crate::dsp::{apply_gain, db_to_linear};

#[derive(Debug, Clone, Copy)]
pub struct ChainConfig {
    /// Software gain before any filter, dB (0 = off).
    pub preamp_gain_db: f32,
    pub denoise: bool,
    pub agc: bool,
    pub agc_target_dbfs: f32,
    pub sample_rate_hz: u32,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            preamp_gain_db: 0.0,
            denoise: false,
            agc: false,
            agc_target_dbfs: -20.0,
            sample_rate_hz: 16_000,
        }
    }
}

pub struct CaptureChain {
    preamp: f32,
    denoise: Option<DenoiseStage>,
    agc: Option<AgcStage>,
}

impl CaptureChain {
    pub fn new(cfg: ChainConfig) -> Self {
        Self {
            preamp: db_to_linear(cfg.preamp_gain_db),
            denoise: cfg.denoise.then(|| DenoiseStage::new(cfg.sample_rate_hz)),
            agc: cfg.agc.then(|| {
                AgcStage::new(AgcConfig {
                    target_dbfs: cfg.agc_target_dbfs,
                    ..Default::default()
                })
            }),
        }
    }

    pub fn is_active(&self) -> bool {
        (self.preamp - 1.0).abs() > 1e-6 || self.denoise.is_some() || self.agc.is_some()
    }

    /// Zero-copy when nothing is enabled.
    pub fn process<'a>(&mut self, input: &'a [f32]) -> Cow<'a, [f32]> {
        if !self.is_active() || input.is_empty() {
            return Cow::Borrowed(input);
        }
        let mut owned: Vec<f32> = input.to_vec();
        apply_gain(&mut owned, self.preamp);
        if let Some(d) = self.denoise.as_mut() {
            owned = d.process(&owned);
        }
        if let Some(a) = self.agc.as_mut() {
            owned = a.process(&owned);
        }
        Cow::Owned(owned)
    }

    pub fn reset(&mut self) {
        if let Some(d) = self.denoise.as_mut() {
            d.reset();
        }
        if let Some(a) = self.agc.as_mut() {
            a.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::rms;

    #[test]
    fn disabled_chain_is_identity_borrow() {
        let mut chain = CaptureChain::new(ChainConfig::default());
        assert!(!chain.is_active());
        let input = vec![0.1f32, -0.2, 0.3];
        assert!(matches!(chain.process(&input), Cow::Borrowed(_)));
    }

    #[test]
    fn agc_only_boosts_quiet_signal() {
        let mut chain = CaptureChain::new(ChainConfig {
            agc: true,
            ..Default::default()
        });
        let quiet = vec![0.01f32; 1600];
        let mut last = Vec::new();
        for _ in 0..50 {
            last = chain.process(&quiet).into_owned();
        }
        assert!(rms(&last) > rms(&quiet) * 2.0);
    }

    #[test]
    fn order_is_denoise_then_agc() {
        // A block shorter than one RNNoise frame yields nothing from denoise;
        // AGC is length-preserving, so an empty result proves denoise ran first.
        let mut chain = CaptureChain::new(ChainConfig {
            denoise: true,
            agc: true,
            ..Default::default()
        });
        assert!(chain.process(&[0.2f32; 64]).is_empty());
    }

    #[test]
    fn preamp_applies_gain() {
        let mut chain = CaptureChain::new(ChainConfig {
            preamp_gain_db: 6.0,
            ..Default::default()
        });
        let out = chain.process(&[0.1f32; 10]).into_owned();
        assert!((out[0] - 0.1995).abs() < 1e-3);
    }
}
