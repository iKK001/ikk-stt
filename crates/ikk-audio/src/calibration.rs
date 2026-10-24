//! Guided microphone check: from a silence measurement (noise floor RMS)
//! and a speech sample (peak / average RMS) derive a recommendation for
//! preamp/AGC, denoise and VAD mode. Pure arithmetic, no I/O.

use serde::{Deserialize, Serialize};

use crate::dsp::rms_to_dbfs;
use crate::vad::VadMode;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CalibrationInput {
    pub noise_rms: f32,
    pub speech_peak_rms: f32,
    pub speech_avg_rms: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationRecommendation {
    pub noise_dbfs: f32,
    pub speech_peak_dbfs: f32,
    pub speech_avg_dbfs: f32,
    pub snr_db: f32,
    pub denoise: bool,
    pub agc: bool,
    pub agc_target_dbfs: f32,
    pub vad_mode: VadMode,
    /// Machine-readable verdict; the shell translates it.
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Excellent,
    Good,
    Usable,
    TooNoisy,
}

pub fn recommend(input: CalibrationInput) -> CalibrationRecommendation {
    let noise_dbfs = rms_to_dbfs(input.noise_rms);
    let speech_peak_dbfs = rms_to_dbfs(input.speech_peak_rms);
    let speech_avg_dbfs = rms_to_dbfs(input.speech_avg_rms);
    let snr_db = speech_avg_dbfs - noise_dbfs;
    let denoise = noise_dbfs > -55.0 || snr_db < 20.0;
    let agc = speech_peak_dbfs < -24.0;
    let agc_target_dbfs = if speech_peak_dbfs < -30.0 {
        -16.0
    } else if speech_peak_dbfs > -8.0 {
        -20.0
    } else {
        -18.0
    };
    let vad_mode = if noise_dbfs > -45.0 {
        VadMode::Aggressive
    } else if noise_dbfs > -60.0 {
        VadMode::Normal
    } else {
        VadMode::Sensitive
    };
    let verdict = if snr_db >= 30.0 {
        Verdict::Excellent
    } else if snr_db >= 20.0 {
        Verdict::Good
    } else if snr_db >= 12.0 {
        Verdict::Usable
    } else {
        Verdict::TooNoisy
    };
    CalibrationRecommendation {
        noise_dbfs,
        speech_peak_dbfs,
        speech_avg_dbfs,
        snr_db,
        denoise,
        agc,
        agc_target_dbfs,
        vad_mode,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_room_quiet_mic_recommends_agc_not_denoise() {
        let r = recommend(CalibrationInput {
            noise_rms: 0.0008,
            speech_peak_rms: 0.02,
            speech_avg_rms: 0.01,
        });
        assert!(r.agc);
        assert!(!r.denoise);
        assert_eq!(r.vad_mode, VadMode::Sensitive);
    }

    #[test]
    fn noisy_room_recommends_denoise_and_aggressive_vad() {
        let r = recommend(CalibrationInput {
            noise_rms: 0.02,
            speech_peak_rms: 0.3,
            speech_avg_rms: 0.15,
        });
        assert!(r.denoise);
        assert_eq!(r.vad_mode, VadMode::Aggressive);
        assert_eq!(r.verdict, Verdict::Usable);
    }

    #[test]
    fn well_leveled_quiet_needs_nothing() {
        let r = recommend(CalibrationInput {
            noise_rms: 0.0006,
            speech_peak_rms: 0.2,
            speech_avg_rms: 0.09,
        });
        assert!(!r.agc && !r.denoise);
        assert_eq!(r.verdict, Verdict::Excellent);
    }
}
