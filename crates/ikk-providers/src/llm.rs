//! Text-model correctors: the dictated text goes to a language model with
//! one instruction — fix what the recogniser got wrong, judged from the
//! context, and the grammar; change nothing else; answer with the text
//! only. Two vendors over their raw HTTPS APIs (no SDKs): OpenAI chat
//! completions and Anthropic messages. Credential ids "openai" (the same
//! key as the OpenAI speech provider) and "anthropic".

use ikk_stt::{ErrorClass, SttError, TextCorrector};
use tracing::debug;

const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
const OPENAI_MODEL: &str = "gpt-4o-mini";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Safety-classifier declines are re-run on a fallback model inside the
/// same call (the "default" routing picks it by category).
const ANTHROPIC_BETA: &str = "server-side-fallback-2026-07-01";
const ANTHROPIC_MODEL: &str = "claude-opus-5-5";
/// Room for a text a little longer than the input.
const MAX_TOKENS: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    OpenAi,
    Anthropic,
}

pub struct AiCorrector {
    vendor: Vendor,
    /// Model override; empty = the vendor default above.
    model: std::sync::RwLock<String>,
    /// Ask for the gender colon in feminine plurals of persons.
    gender_colon: std::sync::atomic::AtomicBool,
    broken_words: std::sync::atomic::AtomicBool,
}

impl AiCorrector {
    pub fn openai() -> Self {
        Self::new(Vendor::OpenAi)
    }
    pub fn anthropic() -> Self {
        Self::new(Vendor::Anthropic)
    }
    fn new(vendor: Vendor) -> Self {
        Self {
            vendor,
            model: std::sync::RwLock::new(String::new()),
            gender_colon: std::sync::atomic::AtomicBool::new(false),
            broken_words: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn model(&self) -> String {
        let chosen = self.model.read().map(|m| m.clone()).unwrap_or_default();
        if !chosen.is_empty() {
            return chosen;
        }
        match self.vendor {
            Vendor::OpenAi => OPENAI_MODEL.into(),
            Vendor::Anthropic => ANTHROPIC_MODEL.into(),
        }
    }
}

/// The instruction, with the language when known. Dictated text comes in
/// pieces and without formatting, so the model is told to keep it plain.
pub fn instruction(language: &str, gender_colon: bool, broken_words: bool) -> String {
    let lang = match language.trim() {
        "" => String::new(),
        "deu" | "gsw" => " The text is German (Swiss spelling, no ß).".into(),
        "eng" => " The text is English.".into(),
        "fra" => " The text is French.".into(),
        "ita" => " The text is Italian.".into(),
        "spa" => " The text is Spanish.".into(),
        "por" => " The text is Portuguese.".into(),
        "nld" => " The text is Dutch.".into(),
        other => format!(" The text's language code is {other}."),
    };
    format!(
        "You correct text that was dictated by a person with impaired speech and written down by a speech recogniser.{lang} \
         Fix words the recogniser misheard, judged from the context, and fix grammar, spelling and punctuation. \
         Keep the meaning, the language, the person's wording and the sentence order; do not shorten, expand, \
         rephrase or comment. Keep line breaks. Answer with the corrected text only, nothing else.{}{}",
        if broken_words {
            " The person speaks on a ventilator and has to stop for breath in the middle of words: \
             where the parts of one word stand apart or a word is cut off and continued, write the whole word."
        } else {
            ""
        },
        if gender_colon && matches!(language.trim(), "deu" | "gsw") {
            " Write feminine plural forms of persons with the gender colon: Autor:innen, Lehrer:innen, Kolleg:innen. \
             Only these plurals get the colon: a singular stays as it is (Autorin, Lehrerin, never Autor:in), \
             and a masculine plural stays as it is (die Lehrer stays die Lehrer)."
        } else {
            ""
        }
    )
}

/// The gender colon is for plurals only: a singular colon form the model
/// wrote on its own ("Lehrer:in", "Autor*in") goes back to the plain word
/// ("Lehrerin"). A form the person wrote that way in `original` stays.
fn singular_without_colon(original: &str, answer: &str) -> String {
    if !answer.contains("in") {
        return answer.to_string();
    }
    let chars: Vec<char> = answer.chars().collect();
    let mut out = String::with_capacity(answer.len());
    let mut i = 0;
    while i < chars.len() {
        let joiner = matches!(chars[i], ':' | '*' | '_' | '·');
        let after_letter = i > 0 && chars[i - 1].is_alphabetic();
        let suffix = i + 2 < chars.len() && chars[i + 1] == 'i' && chars[i + 2] == 'n';
        let ends = i + 3 >= chars.len() || !chars[i + 3].is_alphanumeric();
        if joiner && after_letter && suffix && ends {
            let mut start = i;
            while start > 0 && chars[start - 1].is_alphabetic() {
                start -= 1;
            }
            let form: String = chars[start..i + 3].iter().collect();
            if !original.contains(&form) {
                // Drop the joiner; "in" follows as ordinary letters.
                i += 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `None` when the answer equals the input (nothing to change).
fn changed(original: &str, answer: &str) -> Option<String> {
    let answer = answer.trim_end_matches('\n');
    (answer != original.trim_end_matches('\n') && !answer.trim().is_empty()).then(|| answer.to_string())
}

impl AiCorrector {
    /// One question to the text model: `system` says what to do with `text`.
    async fn ask(&self, system: &str, text: &str, key: String) -> Result<String, SttError> {
        let err = |e: reqwest::Error| SttError::Provider(ErrorClass::from_message(&e.to_string()));
        let model = self.model();
        let (request, headers): (serde_json::Value, Vec<(&str, String)>) = match self.vendor {
            Vendor::OpenAi => (
                serde_json::json!({
                    "model": model,
                    "temperature": 0,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": text},
                    ],
                }),
                vec![("Authorization", format!("Bearer {key}"))],
            ),
            Vendor::Anthropic => (
                serde_json::json!({
                    "model": model,
                    "max_tokens": MAX_TOKENS,
                    "system": system,
                    "output_config": {"effort": "low"},
                    "fallbacks": "default",
                    "messages": [{"role": "user", "content": text}],
                }),
                vec![
                    ("x-api-key", key),
                    ("anthropic-version", ANTHROPIC_VERSION.into()),
                    ("anthropic-beta", ANTHROPIC_BETA.into()),
                ],
            ),
        };
        let url = match self.vendor {
            Vendor::OpenAi => OPENAI_URL,
            Vendor::Anthropic => ANTHROPIC_URL,
        };
        let mut req = crate::http::client().post(url).json(&request);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        let r = req.send().await.map_err(err)?;
        let status = r.status().as_u16();
        let body = r.text().await.unwrap_or_default();
        if status != 200 {
            return Err(SttError::Provider(ErrorClass::from_status(status, &body)));
        }
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| SttError::Provider(ErrorClass::Other(e.to_string())))?;
        let answer = match self.vendor {
            Vendor::OpenAi => json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            Vendor::Anthropic => {
                if json["stop_reason"].as_str() == Some("refusal") {
                    return Err(SttError::Provider(ErrorClass::Other(
                        "the model declined to process this text".into(),
                    )));
                }
                json["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default()
            }
        };
        Ok(answer)
    }
}

/// What the model is asked for other words in place of a marked one.
pub fn suggest_instruction(language: &str) -> String {
    let lang = match language.trim() {
        "" => String::new(),
        "deu" | "gsw" => " The text is German (Swiss spelling, no ß).".into(),
        "eng" => " The text is English.".into(),
        "fra" => " The text is French.".into(),
        "ita" => " The text is Italian.".into(),
        other => format!(" The text's language code is {other}."),
    };
    format!(
        "A person with impaired speech dictated a text and a speech recogniser wrote it down; the person marked \
         a part the recogniser may have got wrong.{lang} You get the marked part after MARKED: and, when known, the \
         text around it after CONTEXT: (where the marked part stands there between ⟦ and ⟧, that is its place). Answer with five different things the person may really have said \
         in place of the marked part: words or short phrases that sound similar to it and make sense in the \
         context, the most likely first, written as they would stand in the text. Always give five, also when \
         the marked part looks right to you or is a name: then give the words that sound most like it. Only \
         the replacements for the marked part, never the sentence around it. One per line, nothing else: \
         no numbering, no quotation marks, no brackets, no comments. Do not repeat the marked part itself."
    )
}

/// The lines of a suggestion answer as candidates: trimmed, without list
/// marks, without the marked part itself, no doubles, five at most.
fn candidates(marked: &str, answer: &str) -> Vec<String> {
    // One per line as asked; a model that put them all on one line
    // separated them with commas or semicolons.
    let lines: Vec<&str> = answer.lines().filter(|l| !l.trim().is_empty()).collect();
    let pieces: Vec<&str> = if lines.len() == 1 && lines[0].contains([',', ';']) {
        lines[0].split([',', ';']).collect()
    } else {
        lines
    };
    let marks = |c: char| matches!(c, '"' | '«' | '»' | '“' | '”' | '„' | '⟦' | '⟧' | '[' | ']' | '`' | '*');
    let mut out: Vec<String> = Vec::new();
    for piece in pieces {
        let piece = piece
            .trim()
            .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '–' | '•' | '*'))
            .trim()
            .trim_matches(marks)
            .trim();
        if piece.is_empty() || piece.chars().count() > 80 {
            continue;
        }
        if piece.to_lowercase() == marked.trim().to_lowercase()
            || out.iter().any(|o| o.to_lowercase() == piece.to_lowercase())
        {
            continue;
        }
        out.push(piece.to_string());
        if out.len() == 5 {
            break;
        }
    }
    out
}

#[async_trait::async_trait]
impl TextCorrector for AiCorrector {
    fn id(&self) -> &'static str {
        match self.vendor {
            Vendor::OpenAi => "openai",
            Vendor::Anthropic => "anthropic",
        }
    }
    fn display_name(&self) -> &'static str {
        match self.vendor {
            Vendor::OpenAi => "OpenAI",
            Vendor::Anthropic => "Anthropic",
        }
    }
    /// The model to ask; empty restores the default.
    fn set_model(&self, model: &str) {
        if let Ok(mut m) = self.model.write() {
            *m = model.trim().to_string();
        }
    }
    fn set_gender_colon(&self, on: bool) {
        self.gender_colon.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    fn set_broken_words(&self, on: bool) {
        self.broken_words.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    async fn suggest(
        &self,
        marked: &str,
        context: &str,
        language: &str,
        credential: Option<&str>,
    ) -> Result<Vec<String>, SttError> {
        let key = credential.ok_or(SttError::MissingCredential)?.trim().to_string();
        if key.is_empty() {
            return Err(SttError::MissingCredential);
        }
        let question = if context.trim().is_empty() {
            format!("MARKED: {}", marked.trim())
        } else {
            format!("MARKED: {}\nCONTEXT: {}", marked.trim(), context.trim())
        };
        let answer = self.ask(&suggest_instruction(language), &question, key).await?;
        let found = candidates(marked, &answer);
        if found.is_empty() {
            // Why nothing can be offered: what the model wrote instead.
            let shown: String = answer.chars().take(300).collect();
            tracing::info!(vendor = ?self.vendor, answer = %shown, "text model gave no usable suggestion");
        }
        debug!(vendor = ?self.vendor, found = found.len(), "text model suggested");
        Ok(found)
    }
    async fn correct(&self, text: &str, language: &str, credential: Option<&str>) -> Result<Option<String>, SttError> {
        let key = credential.ok_or(SttError::MissingCredential)?.trim().to_string();
        if key.is_empty() {
            return Err(SttError::MissingCredential);
        }
        let system = instruction(
            language,
            self.gender_colon.load(std::sync::atomic::Ordering::Relaxed),
            self.broken_words.load(std::sync::atomic::Ordering::Relaxed),
        );
        let answer = self.ask(&system, text, key).await?;
        let model = self.model();
        let corrected = changed(text, &singular_without_colon(text, &answer));
        debug!(vendor = ?self.vendor, model = %model, changed = corrected.is_some(), "text model answered");
        Ok(corrected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_text_is_none() {
        assert_eq!(changed("Hallo Welt.", "Hallo Welt.\n"), None);
        assert_eq!(changed("Hallo Welt.", "Hallo, Welt."), Some("Hallo, Welt.".into()));
        assert_eq!(changed("Hallo", "   "), None);
    }

    #[test]
    fn default_models_and_overrides() {
        let a = AiCorrector::anthropic();
        assert_eq!(a.model(), ANTHROPIC_MODEL);
        a.set_model(" claude-sonnet-5-5 ");
        assert_eq!(a.model(), "claude-sonnet-5-5");
        a.set_model("");
        assert_eq!(a.model(), ANTHROPIC_MODEL);
        assert_eq!(AiCorrector::openai().id(), "openai");
    }

    #[test]
    fn instruction_names_the_language() {
        assert!(instruction("deu", false, false).contains("German"));
        assert_eq!(
            candidates(
                "Kranken Haus",
                "1. Krankenhaus\n- \"Krankenkasse\"\nKranken Haus\nkrankenhaus\n\nKrankenhauses"
            ),
            vec!["Krankenhaus", "Krankenkasse", "Krankenhauses"]
        );
        assert_eq!(
            candidates("Haus", "⟦Maus⟧, Laus; [Haus], raus"),
            vec!["Maus", "Laus", "raus"]
        );
        assert!(instruction("deu", false, true).contains("ventilator"));
        assert!(!instruction("deu", false, false).contains("ventilator"));
        assert!(!instruction("", false, false).contains("language code"));
        assert!(instruction("deu", true, false).contains("Autor:innen"));
        // The colon is German writing: no other language is told about it.
        assert!(!instruction("eng", true, false).contains("Autor:innen"));
        assert!(!instruction("", true, false).contains("Autor:innen"));
    }

    #[test]
    fn the_colon_is_for_plurals_only() {
        let f = singular_without_colon;
        assert_eq!(f("die lehrerin kam", "Die Lehrer:in kam."), "Die Lehrerin kam.");
        assert_eq!(f("eine autorin", "Eine Autor*in"), "Eine Autorin");
        assert_eq!(f("die lehrerinnen", "Die Lehrer:innen"), "Die Lehrer:innen");
        // Written that way by the person: kept.
        assert_eq!(f("Die Lehrer:in kam", "Die Lehrer:in kam."), "Die Lehrer:in kam.");
        // A colon that is punctuation is not touched.
        assert_eq!(f("merke in der tat", "Merke: in der Tat"), "Merke: in der Tat");
        assert_eq!(f("Ärzt:in", "Ärzt:in"), "Ärzt:in");
    }
}
