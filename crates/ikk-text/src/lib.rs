//! Post-processing pipeline between STT and output.
//!
//! Deterministic stages (processing, vocabulary) run always; the LLM suggest/normalize stage is opt-in and
//! consent-gated.

pub mod dictionary;
pub mod learned;

use serde::{Deserialize, Serialize};

pub use dictionary::{all_words, is_word};
pub use learned::{apply_text, find_swaps, similar_hints, LearnedWord, Swap};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextConfig {
    pub auto_punctuation: bool,
    pub auto_casing: bool,
    /// Word → replacement, applied after STT (names, domain terms).
    pub vocabulary: Vec<(String, String)>,
    /// Trained words (training mode): heard forms swapped for the word.
    /// Here only the certain swaps apply (several words, or `always`);
    /// the agreed-words session judges single-word forms by its readings.
    #[serde(default)]
    pub learned: Vec<LearnedWord>,
}

/// One deterministic pass over a committed fragment.
pub fn process(cfg: &TextConfig, input: &str) -> String {
    let mut out = input.trim().to_string();
    for (from, to) in &cfg.vocabulary {
        out = out.replace(from, to);
    }
    out = apply_text(&cfg.learned, &out, true);
    if cfg.auto_casing {
        let mut chars = out.chars();
        if let Some(first) = chars.next() {
            out = first.to_uppercase().collect::<String>() + chars.as_str();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_and_casing() {
        let cfg = TextConfig {
            auto_casing: true,
            vocabulary: vec![("ikk".into(), "iKK".into())],
            ..Default::default()
        };
        assert_eq!(process(&cfg, " ikk stt "), "IKK stt");
    }
}
