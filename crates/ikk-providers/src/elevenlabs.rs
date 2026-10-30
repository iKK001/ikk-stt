//! ElevenLabs Scribe adapter: batch over `/v1/speech-to-text`.
//! Live dictation uses the generic chunked batch session. Language is sent
//! as ISO-639-3 (`language_code`), which Scribe accepts directly.

use ikk_stt::*;
use reqwest::multipart::{Form, Part};

const URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";
/// Pipeline rate: speech models work at 16 kHz internally, and a 48 kHz
/// upload is three times the data per chunk, which on a slow line delays
/// every result (tried 2026-10-30).
const RATE_HZ: u32 = 16_000;
// Scribe v1 is deprecated; v2 is its replacement (same endpoint).
const DEFAULT_MODEL: &str = "scribe_v2";

/// Scribe's `language_code` for a settings language (ISO-639-3). Scribe has
/// no code for Swiss German; its German model writes Swiss speech as
/// Standard German, like the other providers do. Empty = let Scribe detect.
fn scribe_language(iso639_3: &str) -> Option<String> {
    match iso639_3.trim() {
        "" => None,
        "gsw" => Some("deu".into()),
        other => Some(other.to_string()),
    }
}

pub struct ElevenLabs;

fn err(e: impl ToString) -> SttError {
    SttError::Provider(ErrorClass::from_message(&e.to_string()))
}

#[async_trait::async_trait]
impl Provider for ElevenLabs {
    fn id(&self) -> ProviderId {
        ProviderId("elevenlabs".into())
    }
    fn display_name(&self) -> &'static str {
        "ElevenLabs"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            batch: true,
            requires_network: true,
            own_audio: false,
            ..Default::default()
        }
    }

    async fn transcribe(&self, audio: AudioClip, cfg: &SessionConfig) -> Result<Transcript, SttError> {
        if audio.frames.is_empty() {
            return Ok(Transcript {
                text: String::new(),
                language: None,
                confidence: None,
            });
        }
        let key = cfg
            .credential
            .as_ref()
            .map(|s| s.expose())
            .ok_or(SttError::MissingCredential)?;
        let mut form = Form::new()
            .text("model_id", cfg.model.clone().unwrap_or_else(|| DEFAULT_MODEL.into()))
            .text("tag_audio_events", "false")
            .part(
                "file",
                Part::bytes(audio.to_wav(RATE_HZ))
                    .file_name("audio.wav")
                    .mime_str("audio/wav")
                    .map_err(err)?,
            );
        if let Some(code) = scribe_language(&cfg.language) {
            form = form.text("language_code", code);
        }
        let r = crate::http::client()
            .post(URL)
            .header("xi-api-key", key)
            .multipart(form)
            .send()
            .await
            .map_err(err)?;
        let status = r.status().as_u16();
        let body = r.text().await.unwrap_or_default();
        if status != 200 {
            return Err(SttError::Provider(ErrorClass::from_status(status, &body)));
        }
        let j: serde_json::Value = serde_json::from_str(&body).map_err(err)?;
        Ok(Transcript {
            text: j.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string(),
            language: j.get("language_code").and_then(|l| l.as_str()).map(String::from),
            confidence: j.get("language_probability").and_then(|c| c.as_f64()).map(|c| c as f32),
        })
    }

    async fn stream(&self, _cfg: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        Err(SttError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swiss_german_goes_to_the_german_model() {
        assert_eq!(scribe_language("gsw").as_deref(), Some("deu"));
        assert_eq!(scribe_language("deu").as_deref(), Some("deu"));
        assert_eq!(scribe_language(" "), None);
    }
}
