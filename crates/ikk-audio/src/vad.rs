//! Energy-based voice activity detection with adaptive noise floor.
//!
//! Decides speech vs. silence per block, tracks the room's noise floor
//! during silence *and* during speech (so a session that starts mid-speech
//! still learns the floor), and reports speech start/end with sample-exact
//! timing independent of block size.

use serde::{Deserialize, Serialize};

use crate::dsp::rms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VadMode {
    /// Higher threshold, confident speech only (noisy rooms).
    Aggressive,
    #[default]
    Normal,
    /// Low threshold for very quiet microphones.
    Sensitive,
}

impl VadMode {
    fn energy_threshold(self) -> f32 {
        match self {
            Self::Aggressive => 0.015,
            Self::Normal => 0.003,
            Self::Sensitive => 0.0015,
        }
    }
    fn noise_multiplier(self) -> f32 {
        match self {
            Self::Aggressive => 4.0,
            Self::Normal => 2.5,
            Self::Sensitive => 1.8,
        }
    }
}

const NOISE_TRACK_WINDOW_MS: u32 = 1500;
const NOISE_TRACK_PEAK_RATIO: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    SpeechStarted,
    SpeechEnded,
}

pub struct VoiceActivityDetector {
    mode: VadMode,
    sample_rate: u32,
    noise_floor: f32,
    is_speaking: bool,
    speech_samples: usize,
    silence_samples: usize,
    min_speech_ms: u32,
    pause_duration_ms: u32,
    hangover_ms: u32,
    hangover_remaining: usize,
    rms_history: Vec<f32>,
    speech_peak_rms: f32,
    track_min_rms: f32,
    track_samples: usize,
}

impl VoiceActivityDetector {
    pub fn new(mode: VadMode, sample_rate: u32) -> Self {
        Self {
            mode,
            sample_rate: sample_rate.max(1),
            noise_floor: 0.001,
            is_speaking: false,
            speech_samples: 0,
            silence_samples: 0,
            min_speech_ms: 64,
            pause_duration_ms: 480,
            hangover_ms: 0,
            hangover_remaining: 0,
            rms_history: Vec::with_capacity(5),
            speech_peak_rms: 0.0,
            track_min_rms: f32::MAX,
            track_samples: 0,
        }
    }

    pub fn is_speaking(&self) -> bool {
        self.is_speaking
    }
    pub fn noise_floor(&self) -> f32 {
        self.noise_floor
    }
    pub fn set_mode(&mut self, mode: VadMode) {
        self.mode = mode;
        self.reset();
    }
    /// Silence length that ends a segment.
    pub fn set_pause_duration_ms(&mut self, ms: u32) {
        self.pause_duration_ms = ms.max(1);
    }
    /// Extra time speech is kept "on" after energy drops (slow speakers).
    pub fn set_hangover_ms(&mut self, ms: u32) {
        self.hangover_ms = ms;
    }

    fn ms_to_samples(&self, ms: u32) -> usize {
        (ms as u64 * self.sample_rate as u64 / 1000) as usize
    }

    /// Feed one block; returns a speech start/end event when the state flips.
    pub fn process(&mut self, samples: &[f32]) -> Option<VadEvent> {
        if samples.is_empty() {
            return None;
        }
        let block_rms = rms(samples);
        self.rms_history.push(block_rms);
        if self.rms_history.len() > 5 {
            self.rms_history.remove(0);
        }
        let avg_rms = self.rms_history.iter().sum::<f32>() / self.rms_history.len() as f32;
        let n = samples.len();

        if self.is_speaking {
            self.track_noise_during_speech(avg_rms, n);
        }

        let threshold = (self.noise_floor * self.mode.noise_multiplier()).max(self.mode.energy_threshold());
        let is_speech = match self.mode {
            VadMode::Sensitive => avg_rms > threshold || avg_rms > self.noise_floor * 1.5,
            _ => avg_rms > threshold,
        };

        if is_speech {
            self.speech_samples += n;
            self.silence_samples = 0;
            self.hangover_remaining = self.ms_to_samples(self.hangover_ms);
        } else if self.is_speaking && self.hangover_remaining > 0 {
            self.hangover_remaining = self.hangover_remaining.saturating_sub(n);
        } else {
            self.silence_samples += n;
            if !self.is_speaking {
                self.speech_samples = 0;
                // Adapt the floor: quickly up, slowly down.
                if block_rms > self.noise_floor {
                    self.noise_floor = self.noise_floor * 0.98 + block_rms * 0.02;
                } else {
                    self.noise_floor = self.noise_floor * 0.995 + block_rms * 0.005;
                }
            }
        }

        if !self.is_speaking && self.speech_samples >= self.ms_to_samples(self.min_speech_ms) {
            self.is_speaking = true;
            self.speech_peak_rms = 0.0;
            self.track_min_rms = f32::MAX;
            self.track_samples = 0;
            return Some(VadEvent::SpeechStarted);
        }
        if self.is_speaking && self.silence_samples >= self.ms_to_samples(self.pause_duration_ms) {
            self.is_speaking = false;
            self.speech_samples = 0;
            return Some(VadEvent::SpeechEnded);
        }
        None
    }

    /// During speech the quietest 1.5 s window (capped at 5 % of the speech
    /// peak) raises the floor, so a session that never saw silence still
    /// learns the room.
    fn track_noise_during_speech(&mut self, avg_rms: f32, n: usize) {
        self.speech_peak_rms = self.speech_peak_rms.max(avg_rms);
        self.track_min_rms = self.track_min_rms.min(avg_rms);
        self.track_samples += n;
        if self.track_samples < self.ms_to_samples(NOISE_TRACK_WINDOW_MS) {
            return;
        }
        let candidate = self.track_min_rms.min(self.speech_peak_rms * NOISE_TRACK_PEAK_RATIO);
        if candidate > self.noise_floor {
            self.noise_floor = candidate;
        }
        self.track_min_rms = f32::MAX;
        self.track_samples = 0;
    }

    /// Forget everything including the learned floor.
    pub fn reset(&mut self) {
        let mode = self.mode;
        let rate = self.sample_rate;
        let (pause, hang) = (self.pause_duration_ms, self.hangover_ms);
        *self = Self::new(mode, rate);
        self.pause_duration_ms = pause;
        self.hangover_ms = hang;
    }

    /// New utterance, keep the learned floor.
    pub fn reset_utterance(&mut self) {
        let floor = self.noise_floor;
        self.reset();
        self.noise_floor = floor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(vad: &mut VoiceActivityDetector, level: f32, blocks: usize) -> Vec<VadEvent> {
        let block = vec![level; 160];
        (0..blocks).filter_map(|_| vad.process(&block)).collect()
    }

    #[test]
    fn modes_are_ordered() {
        assert!(VadMode::Aggressive.energy_threshold() > VadMode::Normal.energy_threshold());
        assert!(VadMode::Normal.energy_threshold() > VadMode::Sensitive.energy_threshold());
    }

    #[test]
    fn speech_then_pause_emits_events() {
        let mut vad = VoiceActivityDetector::new(VadMode::Normal, 16_000);
        vad.set_pause_duration_ms(500);
        let ev = feed(&mut vad, 0.2, 100);
        assert_eq!(ev, vec![VadEvent::SpeechStarted]);
        assert!(vad.is_speaking());
        let ev = feed(&mut vad, 0.005, 300);
        assert_eq!(ev, vec![VadEvent::SpeechEnded]);
        assert!(!vad.is_speaking());
    }

    #[test]
    fn pause_timing_is_block_size_independent() {
        for block in [160usize, 512] {
            let mut vad = VoiceActivityDetector::new(VadMode::Normal, 16_000);
            vad.set_pause_duration_ms(800);
            let speech = vec![0.3f32; block];
            for _ in 0..(16_000 / block) {
                vad.process(&speech);
            }
            assert!(vad.is_speaking());
            let silence = vec![0.0008f32; block];
            let blocks_400ms = 6400 / block;
            for _ in 0..blocks_400ms {
                vad.process(&silence);
            }
            assert!(vad.is_speaking(), "ended too early at block {block}");
            for _ in 0..blocks_400ms * 2 {
                vad.process(&silence);
            }
            assert!(!vad.is_speaking(), "did not end at block {block}");
        }
    }

    #[test]
    fn continuous_speech_is_not_cut() {
        let mut vad = VoiceActivityDetector::new(VadMode::Normal, 16_000);
        vad.set_pause_duration_ms(500);
        feed(&mut vad, 0.2, 600);
        assert!(vad.is_speaking());
        feed(&mut vad, 0.05, 200);
        assert!(vad.is_speaking(), "quiet passage treated as silence");
    }

    #[test]
    fn reset_utterance_keeps_noise_floor() {
        let mut vad = VoiceActivityDetector::new(VadMode::Normal, 16_000);
        feed(&mut vad, 0.2, 100);
        feed(&mut vad, 0.005, 300);
        let learned = vad.noise_floor();
        assert!(learned > 0.001);
        vad.reset_utterance();
        assert_eq!(vad.noise_floor(), learned);
        assert!(
            feed(&mut vad, 0.005, 100).is_empty(),
            "phantom speech start from room noise"
        );
        vad.reset();
        assert_eq!(vad.noise_floor(), 0.001);
    }
}
