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
pub fn instruction(language: &str, gender_colon: bool) -> String {
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
         rephrase or comment. Keep line breaks. Answer with the corrected text only, nothing else.{}",
        if gender_colon {
            " Write feminine plural forms of persons with the gender colon: Autor:innen, Lehrer:innen, Kolleg:innen."
        } else {
            ""
        }
    )
}

/// `None` when the answer equals the input (nothing to change).
fn changed(original: &str, answer: &str) -> Option<String> {
    let answer = answer.trim_end_matches('\n');
    (answer != original.trim_end_matches('\n') && !answer.trim().is_empty()).then(|| answer.to_string())
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
    async fn correct(&self, text: &str, language: &str, credential: Option<&str>) -> Result<Option<String>, SttError> {
        let key = credential.ok_or(SttError::MissingCredential)?.trim().to_string();
        if key.is_empty() {
            return Err(SttError::MissingCredential);
        }
        let err = |e: reqwest::Error| SttError::Provider(ErrorClass::from_message(&e.to_string()));
        let model = self.model();
        let system = instruction(language, self.gender_colon.load(std::sync::atomic::Ordering::Relaxed));
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
        let corrected = changed(text, &answer);
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
        assert!(instruction("deu", false).contains("German"));
        assert!(!instruction("", false).contains("language code"));
        assert!(instruction("deu", true).contains("Autor:innen"));
    }
}
