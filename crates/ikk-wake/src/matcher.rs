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
/// One take much further from the others than they are from each other
/// (noise, a cough, a different word) makes the set unusable.
const CONSISTENCY_RATIO: f32 = 2.2;
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

    /// Check the takes of one word right after recording them, so a bad
    /// take can be repeated before the other word is recorded.
    pub fn check_word(takes: &[Vec<f32>]) -> Result<(), WakeError> {
        CommandModel::train(&Extractor::new(), takes).map(|_| ())
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
        let mut best = f32::INFINITY;
        for i in 0..templates.len() {
            for j in i + 1..templates.len() {
                let d = dtw(&templates[i], &templates[j]);
                worst = worst.max(d);
                best = best.min(d);
            }
        }
        if !worst.is_finite() || worst > best.max(1e-3) * CONSISTENCY_RATIO {
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
        self.judge(utterance).command
    }

    /// The decision with its numbers, for diagnosing words that are never
    /// (or too often) recognised.
    pub fn judge(&self, utterance: &[f32]) -> Judgement {
        let frames: Vec<Vec<f32>> = self
            .extractor
            .utterance(utterance)
            .into_iter()
            .map(|f| f.to_vec())
            .collect();
        let rms = (utterance.iter().map(|x| x * x).sum::<f32>() / utterance.len().max(1) as f32).sqrt();
        let mut j = Judgement {
            command: None,
            secs: utterance.len() as f32 / crate::SAMPLE_RATE_HZ as f32,
            level_dbfs: 20.0 * rms.max(1e-9).log10(),
            frames: frames.len(),
            start: None,
            start_threshold: self.commands.start.threshold,
            stop: None,
            stop_threshold: self.commands.stop.threshold,
        };
        if frames.len() < MIN_FRAMES {
            return j;
        }
        j.start = self.commands.start.distance(&frames);
        j.stop = self.commands.stop.distance(&frames);
        let accept = |d: Option<f32>, model: &CommandModel, other: Option<f32>| {
            d.is_some_and(|d| d <= model.threshold && other.is_none_or(|o| d <= o * AMBIGUITY_MARGIN))
        };
        j.command = if accept(j.start, &self.commands.start, j.stop) {
            Some(Command::Start)
        } else if accept(j.stop, &self.commands.stop, j.start) {
            Some(Command::Stop)
        } else {
            None
        };
        j
    }
}

/// What the spotter made of one utterance. A distance is None when the
/// utterance's length rules that word out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Judgement {
    pub command: Option<Command>,
    pub secs: f32,
    pub level_dbfs: f32,
    pub frames: usize,
    pub start: Option<f32>,
    pub start_threshold: f32,
    pub stop: Option<f32>,
    pub stop_threshold: f32,
}

impl Judgement {
    /// One line for a log or a status text.
    pub fn summary(&self) -> String {
        let d = |v: Option<f32>| v.map_or("length off".to_string(), |v| format!("{v:.1}"));
        format!(
            "{:.2} s, {:.0} dBFS, start {} (limit {:.1}), stop {} (limit {:.1}) -> {}",
            self.secs,
            self.level_dbfs,
            d(self.start),
            self.start_threshold,
            d(self.stop),
            self.stop_threshold,
            match self.command {
                Some(Command::Start) => "START",
                Some(Command::Stop) => "STOP",
                None => "nothing",
            }
        )
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
