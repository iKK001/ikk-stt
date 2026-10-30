//! Audio pipeline: capture → downmix → resample 16 kHz → preamp → denoise →
//! AGC → frames (+ VAD and calibration helpers for the consumer).
//!
//! Platform-neutral. Capture uses `cpal`, which runs on Windows, macOS,
//! Linux, iOS and Android. Quality stages are off by default.

pub mod agc;
pub mod calibration;
pub mod capture;
pub mod chain;
pub mod denoise;
pub mod dsp;
pub mod mmcss;
pub mod vad;

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use capture::{list_input_devices, open_capture, pick_device, DevicePick};

/// Sample rate every provider path receives. Providers that need a different
/// rate (e.g. Voiceitt at 16 kHz) get it from here; capture may run higher.
pub const PIPELINE_SAMPLE_RATE_HZ: u32 = 16_000;

/// One block of mono PCM samples, normalised to `[-1.0, 1.0]`.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub sample_rate_hz: u32,
    /// Position of the first sample relative to session start.
    pub offset: Duration,
}

impl AudioFrame {
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / self.sample_rate_hz as f64)
    }

    /// RMS level in dBFS, for level meters and energy VAD.
    pub fn level_dbfs(&self) -> f32 {
        if self.samples.is_empty() {
            return -120.0;
        }
        dsp::rms_to_dbfs(dsp::rms(&self.samples))
    }

    /// 16-bit little-endian PCM, what most provider APIs want on the wire.
    pub fn to_pcm_s16le(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.samples.len() * 2);
        for s in &self.samples {
            out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        out
    }
}

/// An input device as the platform backend reports it. Enumerated with
/// `cpal` on every target, including Windows (WASAPI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// iOS audio session mode (ignored elsewhere):
/// `Measurement` = raw, `Default` = mild iOS AGC, `VoiceChat` = aggressive
/// voice processing (often best for impaired speech in field use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IosSessionMode {
    Measurement,
    #[default]
    Default,
    VoiceChat,
}

/// Capture stage configuration. Quality stages default to off.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    /// `None` = system default device.
    pub device_id: Option<String>,
    /// Software gain before any filter, 0..=24 dB.
    pub preamp_gain_db: f32,
    pub denoise: bool,
    pub agc: bool,
    pub ios_mode: IosSessionMode,
    /// Rate the frames leave the capture at. The pipeline's 16 kHz unless
    /// a provider wants the microphone's full bandwidth (48 kHz).
    #[serde(default = "pipeline_rate")]
    pub sample_rate_hz: u32,
}

fn pipeline_rate() -> u32 {
    PIPELINE_SAMPLE_RATE_HZ
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            preamp_gain_db: 0.0,
            denoise: false,
            agc: false,
            ios_mode: IosSessionMode::Default,
            sample_rate_hz: PIPELINE_SAMPLE_RATE_HZ,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no input device available")]
    NoDevice,
    #[error("microphone permission denied")]
    PermissionDenied,
    #[error("audio backend error: {0}")]
    Backend(String),
}

/// A running microphone capture delivering 16 kHz mono frames on a channel.
pub trait Capture: Send {
    fn frames(&mut self) -> &mut tokio::sync::mpsc::Receiver<AudioFrame>;
    /// Last processed block level in dBFS (for meters).
    fn level_dbfs(&self) -> f32;
    fn stop(self: Box<Self>);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_very_quiet() {
        let f = AudioFrame {
            samples: vec![0.0; 160],
            sample_rate_hz: 16_000,
            offset: Duration::ZERO,
        };
        assert!(f.level_dbfs() <= -99.0);
        assert_eq!(f.duration(), Duration::from_millis(10));
    }

    #[test]
    fn full_scale_is_zero_dbfs() {
        let f = AudioFrame {
            samples: vec![1.0; 16],
            sample_rate_hz: 16_000,
            offset: Duration::ZERO,
        };
        assert!(f.level_dbfs().abs() < 1e-3);
        assert_eq!(f.to_pcm_s16le()[..2], [0xFF, 0x7F]);
    }
}
