//! The corrector that needs nothing: no account, no key, no network.
//! Grammar and punctuation come from LanguageTool's rule set, run by the
//! pure-Rust `nlprule` engine; spelling from Hunspell dictionaries (Swiss
//! German without ß, US English) through `spellbook`. Everything is
//! embedded (gzipped) and unpacked on first use, so the first correction
//! takes a few hundred milliseconds and the next ones about a millisecond.
//!
//! It judges words, not meaning: a misheard but correctly spelled word
//! passes. Spelling fixes are deliberately careful — only lowercase words
//! of four letters or more whose best suggestion is one letter away, plus
//! case-only fixes ("haus" → "Haus"); capitalised words (names) are left
//! alone. Languages other than German and English pass through unchanged.

use std::io::Read;
use std::sync::OnceLock;

use ikk_stt::{SttError, TextCorrector};
use tracing::{debug, info};

struct Language {
    tokenizer: nlprule::Tokenizer,
    rules: nlprule::Rules,
    dict: spellbook::Dictionary,
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("embedded data is valid gzip");
    out
}

macro_rules! embedded {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name))
    };
}

fn load(name: &str, tokenizer: &[u8], rules: &[u8], aff: &[u8], dic: &[u8]) -> Option<Language> {
    let started = std::time::Instant::now();
    let tokenizer = nlprule::Tokenizer::from_reader(&mut gunzip(tokenizer).as_slice()).ok()?;
    let rules = nlprule::Rules::from_reader(&mut gunzip(rules).as_slice()).ok()?;
    let aff = String::from_utf8(gunzip(aff)).ok()?;
    let dic = String::from_utf8(gunzip(dic)).ok()?;
    let dict = spellbook::Dictionary::new(&aff, &dic).ok()?;
    info!(
        language = name,
        ms = started.elapsed().as_millis() as u64,
        "on-device corrector loaded"
    );
    Some(Language { tokenizer, rules, dict })
}

fn german() -> Option<&'static Language> {
    static L: OnceLock<Option<Language>> = OnceLock::new();
    L.get_or_init(|| {
        load(
            "de",
            embedded!("de_tokenizer.bin.gz"),
            embedded!("de_rules.bin.gz"),
            embedded!("de_CH_frami.aff.gz"),
            embedded!("de_CH_frami.dic.gz"),
        )
    })
    .as_ref()
}

fn english() -> Option<&'static Language> {
    static L: OnceLock<Option<Language>> = OnceLock::new();
    L.get_or_init(|| {
        load(
            "en",
            embedded!("en_tokenizer.bin.gz"),
            embedded!("en_rules.bin.gz"),
            embedded!("en_US.aff.gz"),
            embedded!("en_US.dic.gz"),
        )
    })
    .as_ref()
}

/// The language pack for a settings language code (ISO 639-3 or 639-1).
fn language(code: &str) -> Option<&'static Language> {
    match code.trim().to_ascii_lowercase().as_str() {
        "deu" | "gsw" | "de" | "de-ch" | "de-de" | "de-at" => german(),
        "eng" | "en" | "en-us" | "en-gb" => english(),
        _ => None,
    }
}

/// What the spelling dictionary says about the case of a word's first
/// letter: `Some(true)` when only the capitalised form is a word (a German
/// noun, a name), `Some(false)` when the lower-case form is one, `None`
/// when the word is unknown or the language is not covered.
pub fn capital_only(word: &str, language_code: &str) -> Option<bool> {
    let lang = language(language_code)?;
    let word: String = word.chars().filter(|c| c.is_alphabetic() || *c == '-').collect();
    let mut chars = word.chars();
    let first = chars.next()?;
    let rest: String = chars.collect();
    let lower: String = first.to_lowercase().chain(rest.chars()).collect();
    let upper: String = first.to_uppercase().chain(rest.chars()).collect();
    if lang.dict.check(&lower) {
        Some(false)
    } else if lang.dict.check(&upper) {
        Some(true)
    } else {
        None
    }
}

/// Whether the language is covered (for the app: say so in the settings).
pub fn supports(code: &str) -> bool {
    matches!(
        code.trim().to_ascii_lowercase().as_str(),
        "deu" | "gsw" | "de" | "de-ch" | "de-de" | "de-at" | "eng" | "en" | "en-us" | "en-gb"
    )
}

/// Levenshtein distance in characters.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// A spelling fix for one word, or `None` to leave it.
fn fix_word(word: &str, dict: &spellbook::Dictionary) -> Option<String> {
    let chars = word.chars().count();
    if chars < 4 || dict.check(word) {
        return None;
    }
    let capitalised = word.chars().next().is_some_and(char::is_uppercase);
    let mut suggestions = Vec::new();
    dict.suggest(word, &mut suggestions);
    let best = suggestions.first()?;
    if best.chars().count() != chars && capitalised {
        return None;
    }
    // Case-only ("haus" → "Haus"): always.
    if best.to_lowercase() == word.to_lowercase() {
        return Some(best.clone());
    }
    // Capitalised words are mostly names: never respelled.
    if capitalised {
        return None;
    }
    (distance(word, best) <= 1 && !best.contains(' ')).then(|| best.clone())
}

/// Spelling pass over the words of `text` (letter runs; words next to
/// digits, colons or apostrophes are left alone).
fn spell(text: &str, dict: &spellbook::Dictionary) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_alphabetic() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let before = start.checked_sub(1).map(|k| chars[k]);
            let after = chars.get(i).copied();
            let glued = |c: Option<char>| {
                c.is_some_and(|c| c.is_ascii_digit() || matches!(c, ':' | '\'' | '’' | '-' | '_' | '@' | '/' | '.'))
            };
            let fixed = if glued(before) || glued(after) {
                None
            } else {
                fix_word(&word, dict)
            };
            match fixed {
                Some(f) => {
                    debug!(%word, fix = %f, "spelling");
                    out.push_str(&f);
                }
                None => out.push_str(&word),
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn quotation_mark(c: char) -> bool {
    matches!(c, '"' | '„' | '“' | '”' | '«' | '»' | '‹' | '›' | '‚' | '‘')
}

/// The grammar rules over one line. A suggestion that touches a quotation
/// mark (or the character right beside one) is dropped: the marks are set
/// by the person ("Zitat", "Zitat Ende"), often with the quotation still
/// open, and the rules would push a full stop in beside them
/// ("Er sagte «" → "Er sagte.«", "kommt gut“" → "kommt gut.“").
fn grammar(line: &str, lang: &Language) -> String {
    let mut suggestions = lang.rules.suggest(line, &lang.tokenizer);
    if line.chars().any(quotation_mark) {
        let chars: Vec<char> = line.chars().collect();
        suggestions.retain(|s| {
            let span = s.span().char();
            let from = span.start.saturating_sub(1);
            let to = (span.end + 1).min(chars.len());
            let touches = chars[from.min(to)..to].iter().copied().any(quotation_mark);
            if touches {
                debug!(rule = s.source(), "grammar: suggestion beside a quotation mark dropped");
            }
            !touches
        });
    }
    if suggestions.is_empty() {
        return line.to_string();
    }
    nlprule::rules::apply_suggestions(line, &suggestions)
}

/// Grammar rules, then spelling, line by line (line breaks are kept).
fn correct_text(text: &str, lang: &Language) -> String {
    text.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                return line.to_string();
            }
            let grammar = grammar(line, lang);
            spell(&grammar, &lang.dict)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The corrector itself; `Default` is the only constructor needed.
#[derive(Default)]
pub struct OnDevice;

#[async_trait::async_trait]
impl TextCorrector for OnDevice {
    fn id(&self) -> &'static str {
        "ondevice"
    }
    fn display_name(&self) -> &'static str {
        "On this device"
    }
    fn local(&self) -> bool {
        true
    }
    async fn correct(
        &self,
        text: &str,
        language_code: &str,
        _credential: Option<&str>,
    ) -> Result<Option<String>, SttError> {
        let Some(lang) = language(language_code) else {
            debug!(language = language_code, "on-device corrector: language not covered");
            return Ok(None);
        };
        let corrected = correct_text(text, lang);
        Ok((corrected != text).then_some(corrected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn german_grammar_and_spelling() {
        let lang = german().expect("German pack");
        assert_eq!(
            correct_text("Wir treffen uns am Montag den 3. März im Büro", lang),
            "Wir treffen uns am Montag, dem 3. März im Büro"
        );
        // Case-only and one-letter fixes on lowercase words; names stay.
        assert_eq!(
            correct_text("das haus von Korner ist gros", lang),
            "das Haus von Korner ist gross"
        );
        // Gender colon forms and numbers are not touched.
        assert_eq!(
            correct_text("12 Lehrer:innen kamen um 9.30 Uhr", lang),
            "12 Lehrer:innen kamen um 9.30 Uhr"
        );
        assert_eq!(
            correct_text("Zeile eins\n\nZeile zwei", lang),
            "Zeile eins\n\nZeile zwei"
        );
    }

    #[test]
    fn english_grammar() {
        let lang = english().expect("English pack");
        assert_eq!(
            correct_text("She was not been here since Monday.", lang),
            "She was not here since Monday."
        );
    }

    #[tokio::test]
    async fn trait_answers_none_for_unchanged_or_uncovered() {
        let c = OnDevice;
        assert_eq!(c.correct("Das Haus ist gross.", "deu", None).await.unwrap(), None);
        assert_eq!(c.correct("Bonjour tout le monde", "fra", None).await.unwrap(), None);
        assert!(c.local());
    }

    #[test]
    fn distance_counts_edits() {
        assert_eq!(distance("gros", "gross"), 1);
        assert_eq!(distance("Haus", "Maus"), 1);
        assert_eq!(distance("abc", "xyz"), 3);
    }

    #[test]
    fn quotation_marks_are_left_alone() {
        let lang = german().expect("German pack");
        // Zitat / Zitat Ende set these marks; no sign is pushed in beside
        // them, whether the quotation is still open or closed.
        for t in [
            "Er sagte «",
            "Er sagte «kommt gut",
            "Er sagte «kommt gut»",
            "Er sagte «kommt gut» und ging",
            "Er sagte „",
            "Er sagte „kommt gut“",
            "Er sagte „kommt gut“ und ging",
            "Er sagte \"kommt gut\" und ging",
        ] {
            assert_eq!(correct_text(t, lang), t);
        }
        // The words inside and around are still corrected.
        assert_eq!(
            correct_text("er sagte «ich komme morgen» und ging nach hause", lang),
            "er sagte «ich komme morgen» und ging nach Hause"
        );
    }
}
