//! ElevenLabs Scribe adapter: batch over `/v1/speech-to-text`.
//! Live dictation uses the generic chunked batch session. Language is sent
//! as ISO-639-3 (`language_code`), which Scribe accepts directly.

use ikk_stt::*;
use reqwest::multipart::{Form, Part};

const URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const RATE_HZ: u32 = 16_000;
const DEFAULT_MODEL: &str = "scribe_v1";

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
        if !cfg.language.trim().is_empty() {
            form = form.text("language_code", cfg.language.trim().to_string());
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
