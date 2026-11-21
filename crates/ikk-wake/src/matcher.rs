//! Templates, thresholds and the decision: start word, stop word or neither.

use serde::{Deserialize, Serialize};

use crate::features::{Extractor, COEFFS};
use crate::WakeError;

pub const MIN_TAKES: usize = 3;
/// Shortest take that can be a word (0.15 s of sound once the quiet
/// edges are cut). It was 0.25 s, which a short word said quickly ("go",
/// "los", "stopp") does not reach: such takes were refused as too short
/// and, later, the word was never heard.
const MIN_FRAMES: usize = 15;
/// Accept up to this factor above the worst distance between the user's
/// own takes.
const THRESHOLD_FACTOR: f32 = 1.35;
/// Takes of this length (10 ms frames: one second) or more are a phrase
/// and accept up to PHRASE_FACTOR above their own spread.
const PHRASE_FRAMES: usize = 100;
const PHRASE_FACTOR: f32 = 1.7;
/// The winning word must be this much closer than the other one.
const AMBIGUITY_MARGIN: f32 = 0.85;
/// Very similar takes would give a limit so tight that the word, said a
/// little differently later, is missed. Real words of the same speaker
/// scored 17 to 22, other speech and noise 26 and more.
const MIN_THRESHOLD: f32 = 23.0;
/// One take much further from the others than they are from each other
/// (noise, a cough, a different word) makes the set unusable ...
const CONSISTENCY_RATIO: f32 = 3.0;
/// ... unless it is still close enough to be the same word: two nearly
/// identical takes must not turn a good third one into a reject.
/// (It was 20, below the limit a heard word is accepted at: three good
/// takes of a short word, which spread more, were refused again and again.)
const CONSISTENT_ENOUGH: f32 = 28.0;
/// Each time the user had to record a command again, the check gives this
/// much more room; from the third repeat on any three takes pass, so the
/// recording can always be finished.
const LENIENCY_STEP: f32 = 8.0;
const LENIENCY_FREE: u32 = 3;
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
        Self::train_lenient(start_takes, stop_takes, 0)
    }

    /// As [`Self::train`]; `leniency`: how often the user already had to
    /// record a command again (the takes may differ more each time).
    pub fn train_lenient(start_takes: &[Vec<f32>], stop_takes: &[Vec<f32>], leniency: u32) -> Result<Self, WakeError> {
        let ex = Extractor::new();
        let (start, _) = CommandModel::train_measured(&ex, start_takes, leniency)?;
        let (stop, _) = CommandModel::train_measured(&ex, stop_takes, leniency)?;
        // Told apart the way the spotter decides: a take of one command
        // must be closer to its own command (at worst `worst`) than to the
        // other one by the ambiguity margin. The acceptance thresholds do
        // not enter here — their floor (MIN_THRESHOLD) is about missing a
        // word said a little differently, and compared against it two
        // short, consistently spoken commands looked "too alike" although
        // the spotter would never confuse them. Phrases sharing a word
        // ("Diktat starten" / "Diktat stoppen") still fail this when the
        // differing word is too small a part of the phrase.
        //
        // Each take is tried the way a heard utterance is: against the
        // other takes of its own command and against the other command.
        // (Comparing the worst pair of one command with the best pair
        // across the two — two different takes — refused short words that
        // every single take tells apart: short words vary more from take
        // to take and lie closer to each other.)
        if !told_apart(&start.templates, &stop.templates) || !told_apart(&stop.templates, &start.templates) {
            return Err(WakeError::WordsTooSimilar);
        }
        Ok(Self { start, stop })
    }

    /// Check the takes of one word right after recording them, so a bad
    /// take can be repeated before the other word is recorded.
    pub fn check_word(takes: &[Vec<f32>]) -> Result<(), WakeError> {
        Self::check_word_lenient(takes, 0)
    }

    /// As [`Self::check_word`], with more room after each repeat.
    pub fn check_word_lenient(takes: &[Vec<f32>], leniency: u32) -> Result<(), WakeError> {
        CommandModel::train_measured(&Extractor::new(), takes, leniency).map(|_| ())
    }

    /// The smallest and the largest distance between the takes of one
    /// command, for a log line that explains a refusal.
    pub fn spread(takes: &[Vec<f32>]) -> Option<(f32, f32)> {
        let ex = Extractor::new();
        let t: Vec<Vec<Vec<f32>>> = takes
            .iter()
            .map(|t| ex.utterance(t).into_iter().map(|f| f.to_vec()).collect())
            .collect();
        let (mut best, mut worst) = (f32::INFINITY, 0f32);
        for i in 0..t.len() {
            for j in i + 1..t.len() {
                let d = dtw(&t[i], &t[j]);
                best = best.min(d);
                worst = worst.max(d);
            }
        }
        (worst.is_finite() && best.is_finite()).then_some((best, worst))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Result<Self, WakeError> {
        serde_json::from_str(json).map_err(|e| WakeError::Storage(e.to_string()))
    }
}

impl CommandModel {
    /// The model and the worst distance between the takes.
    fn train_measured(ex: &Extractor, takes: &[Vec<f32>], leniency: u32) -> Result<(Self, f32), WakeError> {
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
        let enough = CONSISTENT_ENOUGH + LENIENCY_STEP * leniency as f32;
        let odd_one_out = worst > best.max(1e-3) * CONSISTENCY_RATIO && worst > enough;
        if !worst.is_finite() || (odd_one_out && leniency < LENIENCY_FREE) {
            return Err(WakeError::TakesInconsistent);
        }
        let mean_frames = templates.iter().map(|t| t.len() as f32).sum::<f32>() / templates.len() as f32;
        Ok((
            Self {
                templates,
                threshold: (worst * THRESHOLD_FACTOR).max(MIN_THRESHOLD),
                mean_frames,
            },
            worst,
        ))
    }

    /// The acceptance limit, also for words trained before MIN_THRESHOLD.
    /// A phrase (a second of speech or more) gets more room than a single
    /// word: its takes are more alike than a day's repetitions of it, and
    /// other speech lands far away from a long pattern anyway (one
    /// speaker's phrase: hits 21–29, misses 35–44, noise and other speech
    /// 78 and more).
    pub fn limit(&self) -> f32 {
        let threshold = if self.mean_frames >= PHRASE_FRAMES as f32 {
            self.threshold * PHRASE_FACTOR / THRESHOLD_FACTOR
        } else {
            self.threshold
        };
        threshold.max(MIN_THRESHOLD)
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

/// Is every take of one command closer to another take of the same
/// command than to any take of the other command, by the margin the
/// spotter asks for?
fn told_apart(own: &[Vec<Vec<f32>>], other: &[Vec<Vec<f32>>]) -> bool {
    own.iter().enumerate().all(|(i, take)| {
        let same = own
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, t)| dtw(take, t))
            .fold(f32::INFINITY, f32::min);
        let cross = other.iter().map(|t| dtw(take, t)).fold(f32::INFINITY, f32::min);
        same <= cross * AMBIGUITY_MARGIN
    })
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
            start_threshold: self.commands.start.limit(),
            stop: None,
            stop_threshold: self.commands.stop.limit(),
        };
        if frames.len() < MIN_FRAMES {
            return j;
        }
        j.start = self.commands.start.distance(&frames);
        j.stop = self.commands.stop.distance(&frames);
        let accept = |d: Option<f32>, model: &CommandModel, other: Option<f32>| {
            d.is_some_and(|d| d <= model.limit() && other.is_none_or(|o| d <= o * AMBIGUITY_MARGIN))
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
