//! Cuts single utterances out of the pipeline with the voice activity
//! detector, keeping a short lead-in so the first sound is not lost.

use std::collections::VecDeque;

use ikk_audio::vad::{VadEvent, VadMode, VoiceActivityDetector};

use crate::SAMPLE_RATE_HZ;

const PREROLL_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 4; // 250 ms
/// Commands are short; longer speech is dictation, not a command.
const MAX_SAMPLES: usize = SAMPLE_RATE_HZ as usize * 3;
/// Pause that ends a command utterance.
const PAUSE_MS: u32 = 400;

pub struct Segmenter {
    vad: VoiceActivityDetector,
    preroll: VecDeque<f32>,
    current: Option<Vec<f32>>,
    too_long: bool,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter {
    pub fn new() -> Self {
        let mut vad = VoiceActivityDetector::new(VadMode::default(), SAMPLE_RATE_HZ);
        vad.set_pause_duration_ms(PAUSE_MS);
        Self {
            vad,
            preroll: VecDeque::with_capacity(PREROLL_SAMPLES),
            current: None,
            too_long: false,
        }
    }

    /// Feed 16 kHz mono samples; returns a finished utterance of command
    /// length, if one just ended.
    pub fn push(&mut self, samples: &[f32]) -> Option<Vec<f32>> {
        let event = self.vad.process(samples);
        if let Some(buf) = self.current.as_mut() {
            buf.extend_from_slice(samples);
            if buf.len() > MAX_SAMPLES {
                self.too_long = true;
                buf.clear();
            }
        }
        match event {
            Some(VadEvent::SpeechStarted) => {
                let mut buf: Vec<f32> = self.preroll.iter().copied().collect();
                buf.extend_from_slice(samples);
                self.current = Some(buf);
                self.too_long = false;
            }
            Some(VadEvent::SpeechEnded) => {
                let done = self.current.take();
                let too_long = std::mem::take(&mut self.too_long);
                self.remember(samples);
                return done.filter(|_| !too_long);
            }
            None => {}
        }
        self.remember(samples);
        None
    }

    /// Forget an utterance in progress (it began before the caller was
    /// ready); what the detector learned about the room stays.
    pub fn discard_current(&mut self) {
        self.current = None;
        self.too_long = false;
    }

    fn remember(&mut self, samples: &[f32]) {
        for &s in samples {
            if self.preroll.len() == PREROLL_SAMPLES {
                self.preroll.pop_front();
            }
            self.preroll.push_back(s);
        }
    }
}
