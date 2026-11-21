//! Post-processing pipeline between STT and output.
//!
//! Deterministic stages (processing, vocabulary) run always; the LLM suggest/normalize stage is opt-in and
//! consent-gated.

pub mod dictionary;
pub mod learned;
pub mod parts;

use serde::{Deserialize, Serialize};

pub use dictionary::{all_words, is_word};
pub use learned::{apply_text, find_swaps, similar_hints, LearnedWord, Swap};
pub use parts::{join_parts, join_word_parts};

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
    /// Word stems whose feminine plural is written with the gender colon
    /// ("Autor" → "Autor:innen").
    #[serde(default)]
    pub gender_colon: Vec<String>,
    /// German: every feminine plural of a person noun gets the colon when
    /// the dictionary knows the masculine form and the feminine singular
    /// ("Lehrerinnen" → "Lehrer:innen", "Ärztinnen" → "Ärzt:innen").
    #[serde(default)]
    pub gender_colon_auto: bool,
    /// The speaker is on a ventilator and words may come back in parts:
    /// a text model that corrects is told so.
    #[serde(default)]
    pub broken_words: bool,
}

/// One deterministic pass over a committed fragment.
pub fn process(cfg: &TextConfig, input: &str) -> String {
    let mut out = input.trim().to_string();
    for (from, to) in &cfg.vocabulary {
        out = out.replace(from, to);
    }
    out = apply_text(&cfg.learned, &out, true);
    out = gender_colon(&cfg.gender_colon, &out);
    if cfg.gender_colon_auto {
        out = gender_colon_auto(&out);
    }
    if cfg.auto_casing {
        let mut chars = out.chars();
        if let Some(first) = chars.next() {
            out = first.to_uppercase().collect::<String>() + chars.as_str();
        }
    }
    out
}

/// "Autorinnen", "Autor innen", "AutorInnen", "Autor*innen", "Autor_innen"
/// → "Autor:innen" for every stem in `stems`, keeping the stem's case as
/// written. Works on characters, so umlauts and ß keep their place.
pub fn gender_colon(stems: &[String], text: &str) -> String {
    if stems.is_empty() || !text.to_lowercase().contains("innen") {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len() + 8);
    let mut i = 0;
    'outer: while i < chars.len() {
        let at_boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if at_boundary {
            for stem in stems {
                let stem_l: Vec<char> = stem.trim().to_lowercase().chars().collect();
                if stem_l.is_empty() || i + stem_l.len() > lower.len() || lower[i..i + stem_l.len()] != stem_l[..] {
                    continue;
                }
                let mut j = i + stem_l.len();
                // An optional joiner between stem and "innen".
                if j < lower.len() && matches!(lower[j], ' ' | '*' | '_' | ':' | '·' | '-') {
                    j += 1;
                }
                let suffix: [char; 5] = ['i', 'n', 'n', 'e', 'n'];
                if j + 5 <= lower.len()
                    && lower[j..j + 5] == suffix
                    && (j + 5 == lower.len() || !chars[j + 5].is_alphanumeric())
                {
                    out.extend_from_slice(&chars[i..i + stem_l.len()]);
                    out.extend([':', 'i', 'n', 'n', 'e', 'n']);
                    i = j + 5;
                    continue 'outer;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out.into_iter().collect()
}

/// The general German rule: a word "X" + "innen" becomes "X:innen" when
/// "X" + "in" is a word of the list (a feminine singular of a person) and
/// "X" itself or "X" + "e" is one too (the masculine). The length guard
/// keeps "Spinnen", "Zinnen", "beginnen" and the like out: their stems are
/// no nouns of persons. Split forms a recogniser produces — "Autor innen",
/// "AutorInnen", "Autor*innen", "Autor_innen", "Autor-innen" — are joined
/// first, so they fall under the same rule.
pub fn gender_colon_auto(text: &str) -> String {
    if !text.contains("innen") && !text.contains("Innen") {
        return text.to_string();
    }
    let text = &join_split_forms(text);
    let mut out = String::with_capacity(text.len() + 8);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            out.push_str(&colon_form(word).unwrap_or_else(|| word.clone()));
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// "Autor innen" / "Autor*innen" / "Autor_innen" / "Autor-innen" →
/// "Autor:innen" when "Autor" names a person; anything else is left.
fn join_split_forms(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if at_boundary && chars[i].is_uppercase() {
            let mut j = i;
            while j < chars.len() && chars[j].is_alphabetic() {
                j += 1;
            }
            let stem: String = chars[i..j].iter().collect();
            let joiner = j < chars.len() && matches!(chars[j], ' ' | '*' | '_' | '-' | '·');
            let suffix_at = j + 1;
            let suffix_ok = joiner
                && suffix_at + 5 <= chars.len()
                && chars[suffix_at..suffix_at + 5]
                    .iter()
                    .collect::<String>()
                    .to_lowercase()
                    == "innen"
                && (suffix_at + 5 == chars.len() || !chars[suffix_at + 5].is_alphanumeric());
            if suffix_ok && stem.chars().count() >= 4 && person_noun(&stem.to_lowercase()) {
                out.extend(stem.chars());
                out.extend([':', 'i', 'n', 'n', 'e', 'n']);
                i = suffix_at + 5;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out.into_iter().collect()
}

/// "Lehrerinnen" → Some("Lehrer:innen"), else None.
fn colon_form(word: &str) -> Option<String> {
    let lower = word.to_lowercase();
    let stem_len = lower.strip_suffix("innen")?.chars().count();
    if stem_len < 4 {
        return None;
    }
    let stem: String = word.chars().take(stem_len).collect();
    let stem_l = stem.to_lowercase();
    // "Innen", "drinnen", a word that already carries the colon.
    if !stem.chars().next().is_some_and(char::is_uppercase) {
        return None;
    }
    if person_noun(&stem_l) {
        Some(format!("{stem}:innen"))
    } else {
        None
    }
}

/// `stem` (lower case) names a person: the list has its feminine form
/// ("lehrerin") and its masculine form ("lehrer", or "kollege" for
/// "kolleg"). Compounds count by their head: "fachjournalist" through
/// "journalist", "chefärzt" through "ärzt", whatever stands before it.
fn person_noun(stem: &str) -> bool {
    let known = |s: &str| {
        s.chars().count() >= 4
            && dictionary::is_word(&format!("{s}in"))
            && (dictionary::is_word(s) || dictionary::is_word(&format!("{s}e")))
    };
    if known(stem) {
        return true;
    }
    let chars: Vec<char> = stem.chars().collect();
    // The head must leave at least two letters of a first part.
    (2..chars.len().saturating_sub(3)).any(|i| known(&chars[i..].iter().collect::<String>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_forms_fall_under_the_automatic_rule() {
        assert_eq!(
            gender_colon_auto("Liebe Autor innen und Lektor*innen"),
            "Liebe Autor:innen und Lektor:innen"
        );
        assert_eq!(gender_colon_auto("die LeserInnen"), "die Leser:innen");
        assert_eq!(gender_colon_auto("Lehrer-innen kamen"), "Lehrer:innen kamen");
        // Not persons: untouched.
        assert_eq!(gender_colon_auto("Haus innen und aussen"), "Haus innen und aussen");
        assert_eq!(gender_colon_auto("Garten innen"), "Garten innen");
    }

    #[test]
    fn vocabulary_and_casing() {
        let cfg = TextConfig {
            auto_casing: true,
            vocabulary: vec![("ikk".into(), "iKK".into())],
            ..Default::default()
        };
        assert_eq!(process(&cfg, " ikk stt "), "IKK stt");
    }

    #[test]
    fn gender_colon_for_feminine_plurals() {
        let stems = ["Autor", "Lektor", "Leser"].map(String::from).to_vec();
        assert_eq!(
            gender_colon(&stems, "Die Autorinnen und Leser innen lesen."),
            "Die Autor:innen und Leser:innen lesen."
        );
        assert_eq!(
            gender_colon(&stems, "LektorInnen, Autor*innen"),
            "Lektor:innen, Autor:innen"
        );
        assert_eq!(gender_colon(&stems, "Autor:innen bleibt"), "Autor:innen bleibt");
        assert_eq!(gender_colon(&stems, "Die Leserin kam"), "Die Leserin kam");
        assert_eq!(gender_colon(&stems, "Koautorinnen"), "Koautorinnen");
    }

    #[test]
    fn gender_colon_auto_for_german_person_nouns() {
        assert_eq!(
            gender_colon_auto("Die Lehrerinnen und Ärztinnen kamen."),
            "Die Lehrer:innen und Ärzt:innen kamen."
        );
        assert_eq!(
            gender_colon_auto("Kolleginnen, Freundinnen"),
            "Kolleg:innen, Freund:innen"
        );
        // Not persons, not plurals of persons.
        assert_eq!(
            gender_colon_auto("Spinnen beginnen drinnen zu gewinnen"),
            "Spinnen beginnen drinnen zu gewinnen"
        );
        assert_eq!(gender_colon_auto("die Zinnen der Burg"), "die Zinnen der Burg");
        assert_eq!(gender_colon_auto("Lehrer:innen"), "Lehrer:innen");
        // Compounds by their head.
        assert_eq!(
            gender_colon_auto("Fachjournalistinnen und Chefärztinnen"),
            "Fachjournalist:innen und Chefärzt:innen"
        );
        assert_eq!(gender_colon_auto("Hauptschullehrerinnen"), "Hauptschullehrer:innen");
    }
}
