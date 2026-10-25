//! Templates, thresholds and the decision: start word, stop word or neither.

use serde::{Deserialize, Serialize};

use crate::features::{Extractor, COEFFS};
use crate::WakeError;

pub const MIN_TAKES: usize = 3;
/// Shortest take that can be a word (0.25 s).
const MIN_FRAMES: usize = 25;
/// Accept up to this factor above the worst distance between the user's
/// own takes.
const THRESHOLD_FACTOR: f32 = 1.35;
/// The winning word must be this much closer than the other one.
const AMBIGUITY_MARGIN: f32 = 0.85;
/// Utterances much shorter or longer than the takes are not the word.
const DURATION_RANGE: (f32, f32) = (0.5, 2.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Start,
    Stop,
}

/// One trained word: the takes' features and the acceptance threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandModel {
    pub templates: Vec<Vec<Vec<f32>>>,
    pub threshold: f32,
    pub mean_frames: f32,
}

/// Both words, as stored on the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceCommands {
    pub start: CommandModel,
    pub stop: CommandModel,
}

impl VoiceCommands {
    /// Train both words from their takes (16 kHz mono each).
    pub fn train(start_takes: &[Vec<f32>], stop_takes: &[Vec<f32>]) -> Result<Self, WakeError> {
        let ex = Extractor::new();
        let start = CommandModel::train(&ex, start_takes)?;
        let stop = CommandModel::train(&ex, stop_takes)?;
        let cross = start
            .templates
            .iter()
            .flat_map(|a| stop.templates.iter().map(move |b| dtw(a, b)))
            .fold(f32::INFINITY, f32::min);
        if cross <= start.threshold.max(stop.threshold) {
            return Err(WakeError::WordsTooSimilar);
        }
        Ok(Self { start, stop })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Result<Self, WakeError> {
        serde_json::from_str(json).map_err(|e| WakeError::Storage(e.to_string()))
    }
}

impl CommandModel {
    fn train(ex: &Extractor, takes: &[Vec<f32>]) -> Result<Self, WakeError> {
        if takes.len() < MIN_TAKES {
            return Err(WakeError::TooFewTakes(MIN_TAKES));
        }
        let mut templates = Vec::new();
        for (i, t) in takes.iter().enumerate() {
            let f = ex.utterance(t);
            if f.len() < MIN_FRAMES {
                return Err(WakeError::TakeTooShort(i + 1));
            }
            templates.push(f.into_iter().map(|fr| fr.to_vec()).collect::<Vec<_>>());
        }
        let mut worst = 0f32;
        for i in 0..templates.len() {
            for j in i + 1..templates.len() {
                worst = worst.max(dtw(&templates[i], &templates[j]));
            }
        }
        if !worst.is_finite() {
            return Err(WakeError::TakesInconsistent);
        }
        let mean_frames = templates.iter().map(|t| t.len() as f32).sum::<f32>() / templates.len() as f32;
        Ok(Self {
            templates,
            threshold: worst * THRESHOLD_FACTOR,
            mean_frames,
        })
    }

    /// Smallest distance to any take, or None if the length rules it out.
    fn distance(&self, frames: &[Vec<f32>]) -> Option<f32> {
        let ratio = frames.len() as f32 / self.mean_frames.max(1.0);
        if ratio < DURATION_RANGE.0 || ratio > DURATION_RANGE.1 {
            return None;
        }
        Some(
            self.templates
                .iter()
                .map(|t| dtw(t, frames))
                .fold(f32::INFINITY, f32::min),
        )
    }
}

/// Decides for one utterance at a time.
pub struct Spotter {
    commands: VoiceCommands,
    extractor: Extractor,
}

impl Spotter {
    pub fn new(commands: VoiceCommands) -> Self {
        Self {
            commands,
            extractor: Extractor::new(),
        }
    }

    pub fn detect(&self, utterance: &[f32]) -> Option<Command> {
        let frames: Vec<Vec<f32>> = self
            .extractor
            .utterance(utterance)
            .into_iter()
            .map(|f| f.to_vec())
            .collect();
        if frames.len() < MIN_FRAMES {
            return None;
        }
        let start = self.commands.start.distance(&frames);
        let stop = self.commands.stop.distance(&frames);
        let accept = |d: Option<f32>, model: &CommandModel, other: Option<f32>| {
            d.is_some_and(|d| d <= model.threshold && other.is_none_or(|o| d <= o * AMBIGUITY_MARGIN))
        };
        if accept(start, &self.commands.start, stop) {
            Some(Command::Start)
        } else if accept(stop, &self.commands.stop, start) {
            Some(Command::Stop)
        } else {
            None
        }
    }
}

/// Dynamic time warping, Euclidean frame distance, normalised by path
/// length so short and long words compare fairly.
pub fn dtw(a: &[Vec<f32>], b: &[Vec<f32>]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return f32::INFINITY;
    }
    let (n, m) = (a.len(), b.len());
    let mut prev = vec![(f32::INFINITY, 0usize); m + 1];
    let mut cur = vec![(f32::INFINITY, 0usize); m + 1];
    prev[0] = (0.0, 0);
    for i in 1..=n {
        cur[0] = (f32::INFINITY, 0);
        for j in 1..=m {
            let d = frame_distance(&a[i - 1], &b[j - 1]);
            let best = [prev[j - 1], prev[j], cur[j - 1]]
                .into_iter()
                .min_by(|x, y| x.0.total_cmp(&y.0))
                .unwrap_or((f32::INFINITY, 0));
            cur[j] = (best.0 + d, best.1 + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let (cost, steps) = prev[m];
    cost / steps.max(1) as f32
}

fn frame_distance(x: &[f32], y: &[f32]) -> f32 {
    x.iter()
        .zip(y)
        .take(COEFFS)
        .map(|(a, b)| (a - b) * (a - b))
        .sum::<f32>()
        .sqrt()
}
