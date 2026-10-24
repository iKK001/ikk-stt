//! Post-processing pipeline between STT and output.
//!
//! Deterministic stages (processing, vocabulary) run always; the LLM suggest/normalize stage is opt-in and
//! consent-gated.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextConfig {
    pub auto_punctuation: bool,
    pub auto_casing: bool,
    /// Word → replacement, applied after STT (names, domain terms).
    pub vocabulary: Vec<(String, String)>,
}

/// One deterministic pass over a committed fragment.
pub fn process(cfg: &TextConfig, input: &str) -> String {
    let mut out = input.trim().to_string();
    for (from, to) in &cfg.vocabulary {
        out = out.replace(from, to);
    }
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
