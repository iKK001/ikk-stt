//! Capture processing chain: preamp → denoise → AGC. The one place that
//! defines the order. Denoise before AGC, otherwise AGC lifts the noise.
//! Pure DSP state over blocks; no I/O.
//!
//! The noise model also tells speech from background per block
//! ([`CaptureChain::speech_probability`]). With denoise off it still
//! listens, so the chunker finds the gaps between words in a loud room
//! while the audio itself goes out untouched.
//!
//! The gate uses that probability to turn the pauses down: a ventilator
//! or breathing between words is otherwise transcribed as words, and a
//! chunk ending in machine breath makes recognisers invent text. Unlike
//! denoising it never touches the speech itself — it opens the moment the
//! model hears a voice and closes [`GATE_HOLD`] after the last speech.

use std::borrow::Cow;

use crate::agc::{AgcConfig, AgcStage};
use crate::denoise::DenoiseStage;
use crate::dsp::{apply_gain, db_to_linear};
use crate::profile::NoiseProfile;
use crate::shape::{Biquad, Compressor, TrillFiller};

/// Below this speech probability a block counts as pause for the gate.
const GATE_SPEECH_PROB: f32 = 0.3;
/// Pause this long before the gate closes (keeps word endings).
const GATE_HOLD_MS: u32 = 300;
/// Level of a closed gate, dB.
const GATE_FLOOR_DB: f32 = -30.0;
/// The gate's gain moves this far per block at most (no clicks).
const GATE_STEP: f32 = 0.25;

#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// Software gain before any filter, dB (0 = off).
    pub preamp_gain_db: f32,
    pub denoise: bool,
    /// Keep the noise model listening for speech even with denoise off.
    pub listen: bool,
    /// Turn pauses down to [`GATE_FLOOR_DB`] (needs the model listening).
    pub gate: bool,
    /// Tone shaping before everything else, see [`crate::shape`].
    pub highpass_hz: f32,
    pub presence_db: f32,
    pub compressor: bool,
    /// Fill the closures of a rolled R, see [`crate::shape::TrillFiller`].
    pub trill_filler: bool,
    /// Subtract a learnt steady-noise profile, see [`crate::profile`].
    pub noise_profile: bool,
    /// Fixed path equaliser, 18 third-octave gains in dB (needs the
    /// profile stage, which hosts the FFT).
    pub path_eq_db: Vec<f32>,
    pub agc: bool,
    pub agc_target_dbfs: f32,
    pub sample_rate_hz: u32,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            preamp_gain_db: 0.0,
            denoise: false,
            listen: false,
            gate: false,
            highpass_hz: 0.0,
            presence_db: 0.0,
            compressor: false,
            trill_filler: false,
            noise_profile: false,
            path_eq_db: Vec::new(),
            agc: false,
            agc_target_dbfs: -20.0,
            sample_rate_hz: 16_000,
        }
    }
}

pub struct CaptureChain {
    preamp: f32,
    highpass: Option<Biquad>,
    presence: Option<Biquad>,
    compressor: Option<Compressor>,
    trill: Option<TrillFiller>,
    profile: Option<NoiseProfile>,
    denoise: Option<DenoiseStage>,
    listen: Option<DenoiseStage>,
    agc: Option<AgcStage>,
    gate: Option<Gate>,
    sample_rate_hz: u32,
}

/// Pause attenuator driven by the speech probability.
struct Gate {
    /// Current gain, 1.0 open … floor closed.
    gain: f32,
    /// Blocks of pause seen since the last speech.
    pause_ms: u32,
}

impl CaptureChain {
    pub fn new(cfg: ChainConfig) -> Self {
        let rate = cfg.sample_rate_hz.max(1);
        Self {
            preamp: db_to_linear(cfg.preamp_gain_db),
            highpass: (cfg.highpass_hz > 0.0).then(|| Biquad::highpass(rate, cfg.highpass_hz)),
            presence: (cfg.presence_db.abs() > 0.01).then(|| Biquad::peaking(rate, 3_000.0, 1.0, cfg.presence_db)),
            compressor: cfg.compressor.then(|| Compressor::new(rate)),
            trill: cfg.trill_filler.then(|| TrillFiller::new(rate)),
            profile: (cfg.noise_profile || !cfg.path_eq_db.is_empty()).then(|| {
                NoiseProfile::new()
                    .with_path_eq(&cfg.path_eq_db)
                    .subtracting(cfg.noise_profile)
            }),
            denoise: cfg.denoise.then(|| DenoiseStage::new(cfg.sample_rate_hz)),
            listen: ((cfg.listen || cfg.gate) && !cfg.denoise).then(|| DenoiseStage::new(cfg.sample_rate_hz)),
            gate: cfg.gate.then_some(Gate { gain: 1.0, pause_ms: 0 }),
            sample_rate_hz: cfg.sample_rate_hz.max(1),
            agc: cfg.agc.then(|| {
                AgcStage::new(AgcConfig {
                    target_dbfs: cfg.agc_target_dbfs,
                    ..Default::default()
                })
            }),
        }
    }

    pub fn is_active(&self) -> bool {
        (self.preamp - 1.0).abs() > 1e-6
            || self.denoise.is_some()
            || self.agc.is_some()
            || self.gate.is_some()
            || self.highpass.is_some()
            || self.presence.is_some()
            || self.compressor.is_some()
            || self.trill.is_some()
            || self.profile.is_some()
    }

    /// Whether the gate is currently turned down (for meters and logs).
    pub fn gate_closed(&self) -> bool {
        self.gate.as_ref().is_some_and(|g| g.gain < 0.5)
    }

    /// Speech probability (0..1) of the last processed block, when the
    /// noise model ran on it.
    pub fn speech_probability(&self) -> Option<f32> {
        self.denoise
            .as_ref()
            .or(self.listen.as_ref())
            .filter(|d| d.supports_rate())
            .map(|d| d.last_voice_probability())
    }

    /// Zero-copy when nothing changes the audio.
    pub fn process<'a>(&mut self, input: &'a [f32]) -> Cow<'a, [f32]> {
        if input.is_empty() {
            return Cow::Borrowed(input);
        }
        if !self.is_active() {
            if let Some(l) = self.listen.as_mut() {
                l.analyse(input);
            }
            return Cow::Borrowed(input);
        }
        let mut owned: Vec<f32> = input.to_vec();
        apply_gain(&mut owned, self.preamp);
        if let Some(f) = self.highpass.as_mut() {
            f.process(&mut owned);
        }
        if let Some(f) = self.presence.as_mut() {
            f.process(&mut owned);
        }
        if let Some(c) = self.compressor.as_mut() {
            c.process(&mut owned);
        }
        if let Some(t) = self.trill.as_mut() {
            t.process(&mut owned);
        }
        if let Some(p) = self.profile.as_mut() {
            owned = p.process(&owned);
            if owned.is_empty() {
                return Cow::Owned(owned);
            }
        }
        if let Some(d) = self.denoise.as_mut() {
            owned = d.process(&owned);
        } else if let Some(l) = self.listen.as_mut() {
            l.analyse(&owned);
        }
        if let Some(a) = self.agc.as_mut() {
            owned = a.process(&owned);
        }
        if let Some(g) = self.gate.as_mut() {
            let block_ms = (owned.len() as u64 * 1000 / self.sample_rate_hz as u64) as u32;
            let speech = self
                .denoise
                .as_ref()
                .or(self.listen.as_ref())
                .map(|d| d.last_voice_probability())
                .unwrap_or(1.0);
            if speech >= GATE_SPEECH_PROB {
                g.pause_ms = 0;
            } else {
                g.pause_ms = g.pause_ms.saturating_add(block_ms);
            }
            let target = if g.pause_ms > GATE_HOLD_MS {
                db_to_linear(GATE_FLOOR_DB)
            } else {
                1.0
            };
            // Open instantly on speech, close gradually.
            g.gain = if target > g.gain {
                target
            } else {
                (g.gain - GATE_STEP).max(target)
            };
            if g.gain < 1.0 {
                apply_gain(&mut owned, g.gain);
            }
        }
        Cow::Owned(owned)
    }

    pub fn reset(&mut self) {
        if let Some(f) = self.highpass.as_mut() {
            f.reset();
        }
        if let Some(f) = self.presence.as_mut() {
            f.reset();
        }
        if let Some(c) = self.compressor.as_mut() {
            c.reset();
        }
        if let Some(t) = self.trill.as_mut() {
            t.reset();
        }
        if let Some(p) = self.profile.as_mut() {
            p.reset();
        }
        if let Some(d) = self.denoise.as_mut() {
            d.reset();
        }
        if let Some(l) = self.listen.as_mut() {
            l.reset();
        }
        if let Some(a) = self.agc.as_mut() {
            a.reset();
        }
        if let Some(g) = self.gate.as_mut() {
            g.gain = 1.0;
            g.pause_ms = 0;
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
    fn listening_leaves_audio_alone_but_reports_speech() {
        let mut chain = CaptureChain::new(ChainConfig {
            listen: true,
            ..Default::default()
        });
        assert!(!chain.is_active());
        assert_eq!(chain.speech_probability(), None.or(Some(0.0)));
        let input = vec![0.0f32; 3200];
        assert!(matches!(chain.process(&input), Cow::Borrowed(_)));
        let p = chain.speech_probability().unwrap();
        assert!(p < 0.5, "silence is not speech: {p}");
    }

    #[test]
    fn gate_turns_long_pauses_down_but_not_speech() {
        let mut chain = CaptureChain::new(ChainConfig {
            gate: true,
            ..Default::default()
        });
        // Silence long enough to close the gate (hold 300 ms + ramp).
        let quiet = vec![0.001f32; 320];
        let mut out = Vec::new();
        for _ in 0..40 {
            out = chain.process(&quiet).into_owned();
        }
        assert!(chain.gate_closed());
        assert!(rms(&out) < rms(&quiet) * 0.1, "pause turned down: {}", rms(&out));
        // A voiced tone-like burst opens it again at once.
        let voice: Vec<f32> = (0..320).map(|i| (i as f32 * 0.08).sin() * 0.3).collect();
        let _ = chain.process(&voice);
        let out = chain.process(&voice).into_owned();
        assert!(!chain.gate_closed() || rms(&out) > rms(&voice) * 0.5, "speech passes");
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
