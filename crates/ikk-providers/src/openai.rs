//! OpenAI adapter: batch via `/v1/audio/transcriptions`, live via the
//! Realtime API in transcription mode.
//!
//! Batch model default `gpt-4o-transcribe`; `whisper-1` and
//! `gpt-4o-mini-transcribe` work with the same endpoint. Keywords become the
//! `prompt` bias. Realtime: PCM16 is sent base64-encoded as
//! `input_audio_buffer.append`; the client commits at speech pauses
//! (`cut_segment`) because without server turn detection the API never
//! closes an item on its own (finding 2026-09). Deltas are
//! accumulated into a growing interim; `…transcription.completed` is final.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use ikk_audio::AudioFrame;
use ikk_stt::lang::iso639_1;
use ikk_stt::*;
use reqwest::multipart::{Form, Part};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::info;

const BATCH_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
const REALTIME_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
const BATCH_RATE_HZ: u32 = 16_000;
/// Realtime expects 24 kHz PCM16 mono.
const REALTIME_RATE_HZ: u32 = 24_000;
/// whisper-1: deterministic, never truncates, and with the user's phrases
/// as prompt it read both impaired-speech samples exactly; gpt-4o-transcribe
/// stops after the first sentence or paraphrases on the same audio.
/// Whisper's subtitle-credit hallucination on noise-only audio is kept out
/// by not uploading speechless audio.
const DEFAULT_BATCH_MODEL: &str = "whisper-1";
const DEFAULT_REALTIME_MODEL: &str = "gpt-4o-transcribe";
/// Committing less than this errors with `input_audio_buffer_commit_empty`.
const MIN_COMMIT_BYTES: usize = (REALTIME_RATE_HZ as usize / 10) * 2; // 100 ms

pub struct OpenAi;

fn err(e: impl ToString) -> SttError {
    SttError::Provider(ErrorClass::from_message(&e.to_string()))
}

fn key(cfg: &SessionConfig) -> Result<&str, SttError> {
    cfg.credential
        .as_ref()
        .map(|s| s.expose())
        .ok_or(SttError::MissingCredential)
}

/// Session configuration message for the Realtime transcription session.
pub fn session_update(model: &str, language: Option<&str>, prompt: &str) -> serde_json::Value {
    let mut transcription = serde_json::json!({ "model": model });
    if let Some(l) = language {
        transcription["language"] = serde_json::Value::String(l.into());
    }
    if !prompt.is_empty() {
        transcription["prompt"] = serde_json::Value::String(prompt.into());
    }
    serde_json::json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": REALTIME_RATE_HZ },
                    "noise_reduction": { "type": "near_field" },
                    "transcription": transcription
                }
            }
        }
    })
}

/// Parse one Realtime server event.
pub fn parse_event(json: &str) -> Option<TranscriptEvent> {
    let j: serde_json::Value = serde_json::from_str(json).ok()?;
    match j.get("type").and_then(|t| t.as_str())? {
        "conversation.item.input_audio_transcription.delta" => {
            let d = j.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            (!d.is_empty()).then(|| TranscriptEvent::Interim(d.to_string()))
        }
        "conversation.item.input_audio_transcription.completed" => Some(TranscriptEvent::Final(
            j.get("transcript")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string(),
        )),
        "error" => {
            let msg = j
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("openai realtime error");
            Some(TranscriptEvent::Error(ErrorClass::from_message(msg)))
        }
        _ => None,
    }
}

/// whisper's timed words come bare; the text has them with punctuation and
/// in the same order. The next text token that begins with the word is the
/// word as written; tokens that are not it are passed over.
fn with_punctuation<'a>(bare: &str, tokens: &mut impl Iterator<Item = &'a str>) -> String {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let want = norm(bare);
    let mut look = tokens.by_ref().peekable();
    for _ in 0..3 {
        match look.peek() {
            Some(t) if want.is_empty() || norm(t).starts_with(&want) || want.starts_with(&norm(t)) => {
                return look.next().unwrap_or(bare).to_string();
            }
            Some(_) => {
                look.next();
            }
            None => break,
        }
    }
    bare.to_string()
}

/// The user's words as the prompt. Nothing else: an instruction-like
/// sentence ("Diktat auf Deutsch.") or `temperature 0` made
/// gpt-4o-transcribe stop after the first sentence of a 28 s recording,
/// and the sentence came back typed as if spoken.
fn prompt_from_keywords(cfg: &SessionConfig) -> String {
    cfg.keywords
        .iter()
        .map(|(w, _)| w.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whisper's stock answers to audio without speech: subtitle credits and
/// sign-offs from its training data. Never dictation.
const STOCK_HALLUCINATIONS: [&str; 10] = [
    "amara.org",
    "untertitel der",
    "untertitel von",
    "untertitelung",
    "subtitles by",
    "vielen dank fürs zuschauen",
    "danke fürs zuschauen",
    "bis zum nächsten mal",
    "thanks for watching",
    "copyright",
];

fn is_stock_hallucination(text: &str) -> bool {
    let t = text.to_lowercase();
    STOCK_HALLUCINATIONS.iter().any(|h| t.contains(h))
}

/// A result that only repeats the prompt (or one of its words) is the
/// model echoing the prompt over audio it could not read, not speech.
fn is_prompt_echo(text: &str, cfg: &SessionConfig) -> bool {
    let norm = |t: &str| {
        t.chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let got = norm(text);
    if got.is_empty() {
        return false;
    }
    got == norm(&prompt_from_keywords(cfg)) || cfg.keywords.iter().any(|(w, _)| norm(w) == got)
}

#[async_trait::async_trait]
impl Provider for OpenAi {
    fn id(&self) -> ProviderId {
        ProviderId("openai".into())
    }
    fn display_name(&self) -> &'static str {
        "OpenAI"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            batch: true,
            streaming: true,
            interim: true,
            batch_replacement: false,
            required_sample_rate_hz: None,
            languages: ["deu", "eng", "fra", "ita", "spa", "por", "nld"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            requires_network: true,
            max_in_flight: None,
            own_audio: false,
            timed_words: false,
        }
    }

    async fn transcribe(&self, audio: AudioClip, cfg: &SessionConfig) -> Result<Transcript, SttError> {
        if audio.frames.is_empty() {
            return Ok(Transcript {
                text: String::new(),
                language: None,
                confidence: None,
                words: Vec::new(),
            });
        }
        let k = key(cfg)?;
        let model = cfg.model.clone().unwrap_or_else(|| DEFAULT_BATCH_MODEL.into());
        // Deterministic decoding: the same audio gives the same text.
        // whisper-1 can time every word; the agreed-words mode uses that.
        let timed = model.starts_with("whisper");
        let mut form = Form::new()
            .text("model", model.clone())
            .text("response_format", if timed { "verbose_json" } else { "json" })
            .part(
                "file",
                Part::bytes(audio.to_wav(BATCH_RATE_HZ))
                    .file_name("audio.wav")
                    .mime_str("audio/wav")
                    .map_err(err)?,
            );
        if timed {
            form = form.text("timestamp_granularities[]", "word");
        }
        if let Some(l) = iso639_1(&cfg.language) {
            form = form.text("language", l.to_string());
        }
        let prompt = prompt_from_keywords(cfg);
        if !prompt.is_empty() {
            form = form.text("prompt", prompt);
        }
        // Experiment knob: decoding temperature (whisper-1 only; gpt-4o
        // truncates with it). Not a setting.
        if let Ok(t) = std::env::var("IKK_OPENAI_TEMPERATURE") {
            form = form.text("temperature", t);
        }
        let r = crate::http::client()
            .post(BATCH_URL)
            .bearer_auth(k)
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
        let mut text = j.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        if is_prompt_echo(&text, cfg) {
            tracing::debug!(%text, "prompt echoed over unreadable audio; dropped");
            text.clear();
        }
        if is_stock_hallucination(&text) {
            tracing::info!(%text, "stock hallucination over speechless audio; dropped");
            text.clear();
        }
        let words = if text.is_empty() {
            Vec::new()
        } else {
            let mut tokens = text.split_whitespace();
            j.get("words")
                .and_then(|w| w.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|w| {
                            let start = w.get("start")?.as_f64()?.max(0.0);
                            let bare = w.get("word")?.as_str()?.trim();
                            Some(ikk_stt::Word {
                                text: with_punctuation(bare, &mut tokens),
                                start: std::time::Duration::from_secs_f64(start),
                                end: std::time::Duration::from_secs_f64(w.get("end")?.as_f64()?.max(start)),
                            })
                        })
                        .filter(|w| !w.text.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(Transcript {
            text,
            language: j.get("language").and_then(|l| l.as_str()).map(String::from),
            confidence: None,
            words,
        })
    }

    async fn stream(&self, cfg: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        let k = key(cfg)?.to_string();
        let model = cfg.model.clone().unwrap_or_else(|| DEFAULT_REALTIME_MODEL.into());
        let mut req = REALTIME_URL.into_client_request().map_err(err)?;
        req.headers_mut()
            .insert("Authorization", format!("Bearer {k}").parse().map_err(err)?);
        let tls = tokio_tungstenite::Connector::Rustls(crate::http::tls_config());
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(tls)),
        )
        .await
        .map_err(|_| SttError::Provider(ErrorClass::Transport))?
        .map_err(err)?;
        let (sink, mut source) = ws.split();
        let sink = Arc::new(Mutex::new(sink));
        sink.lock()
            .await
            .send(Message::text(
                session_update(&model, iso639_1(&cfg.language), &prompt_from_keywords(cfg)).to_string(),
            ))
            .await
            .map_err(err)?;
        info!(%model, "openai realtime session opened");

        let (tx, rx) = mpsc::channel(64);
        let final_seen = Arc::new(tokio::sync::Notify::new());
        let recv_task = {
            let tx = tx.clone();
            let final_seen = final_seen.clone();
            tokio::spawn(async move {
                let mut acc = String::new();
                while let Some(msg) = source.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            if let Some(ev) = parse_event(t.as_str()) {
                                let ev = match ev {
                                    TranscriptEvent::Interim(d) => {
                                        acc.push_str(&d);
                                        TranscriptEvent::Interim(acc.clone())
                                    }
                                    other => {
                                        acc.clear();
                                        if matches!(other, TranscriptEvent::Final(_)) {
                                            final_seen.notify_one();
                                        }
                                        other
                                    }
                                };
                                if tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(e) => {
                            let _ = tx
                                .send(TranscriptEvent::Error(ErrorClass::from_message(&e.to_string())))
                                .await;
                            break;
                        }
                    }
                }
            })
        };
        Ok(Box::new(OpenAiSession {
            sink,
            events: rx,
            tx,
            recv_task,
            final_seen,
            uncommitted: 0,
        }))
    }
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

pub struct OpenAiSession {
    sink: Arc<Mutex<WsSink>>,
    events: mpsc::Receiver<TranscriptEvent>,
    tx: mpsc::Sender<TranscriptEvent>,
    recv_task: tokio::task::JoinHandle<()>,
    /// Signalled by the receiver on every `Final` (used by `finish`).
    final_seen: Arc<tokio::sync::Notify>,
    uncommitted: usize,
}

impl OpenAiSession {
    async fn send(&self, v: serde_json::Value) -> Result<(), SttError> {
        self.sink
            .lock()
            .await
            .send(Message::text(v.to_string()))
            .await
            .map_err(err)
    }
    async fn commit(&mut self) -> Result<(), SttError> {
        if self.uncommitted < MIN_COMMIT_BYTES {
            return Ok(());
        }
        self.uncommitted = 0;
        self.send(serde_json::json!({"type": "input_audio_buffer.commit"}))
            .await
    }
}

#[async_trait::async_trait]
impl StreamingSession for OpenAiSession {
    async fn push(&mut self, frame: AudioFrame) -> Result<(), SttError> {
        let pcm = AudioClip { frames: vec![frame] }.to_pcm_s16le(REALTIME_RATE_HZ);
        self.uncommitted += pcm.len();
        let b64 = base64::engine::general_purpose::STANDARD.encode(pcm);
        self.send(serde_json::json!({"type": "input_audio_buffer.append", "audio": b64}))
            .await
    }

    async fn cut_segment(&mut self) -> Result<(), SttError> {
        self.commit().await
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        let _ = self.commit().await;
        // Give the server time to deliver the final of the last commit; the
        // receiver task forwards it into `events`, which the caller drains.
        if self.uncommitted == 0 {
            let _ = tokio::time::timeout(Duration::from_secs(5), self.final_seen.notified()).await;
        }
        let _ = self.sink.lock().await.send(Message::Close(None)).await;
        self.recv_task.abort();
        let _ = self.tx.send(TranscriptEvent::Closed).await;
        Ok(())
    }

    fn events(&mut self) -> &mut mpsc::Receiver<TranscriptEvent> {
        &mut self.events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_realtime_events() {
        assert_eq!(
            parse_event(r#"{"type":"conversation.item.input_audio_transcription.delta","delta":"Hal"}"#),
            Some(TranscriptEvent::Interim("Hal".into()))
        );
        assert_eq!(
            parse_event(
                r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":" Hallo Welt. "}"#
            ),
            Some(TranscriptEvent::Final("Hallo Welt.".into()))
        );
        assert!(matches!(
            parse_event(r#"{"type":"error","error":{"message":"invalid_api_key"}}"#),
            Some(TranscriptEvent::Error(ErrorClass::Unauthorized))
        ));
        assert_eq!(parse_event(r#"{"type":"session.created"}"#), None);
    }

    #[test]
    fn stock_hallucinations_are_recognised() {
        assert!(is_stock_hallucination("Untertitel der Amara.org-Community"));
        assert!(is_stock_hallucination("Untertitel von Stephanie Geiges"));
        assert!(!is_stock_hallucination("Ich habe so viel zu schreiben."));
    }

    #[test]
    fn prompt_echoes_are_recognised() {
        let mut cfg = SessionConfig::new("openai");
        cfg.keywords = vec![("Bildschirmtastatur".into(), 5), ("Korner".into(), 5)];
        assert!(is_prompt_echo("Bildschirmtastatur.", &cfg));
        assert!(is_prompt_echo("Bildschirmtastatur, Korner", &cfg));
        assert!(!is_prompt_echo("meine Bildschirmtastatur", &cfg));
        assert!(!is_prompt_echo("", &cfg));
    }

    #[test]
    fn timed_words_get_their_punctuation_back() {
        let text = "Hallo, ich hoffe, eine Lösung.";
        let mut t = text.split_whitespace();
        let got: Vec<String> = ["Hallo", "ich", "hoffe", "eine", "Lösung"]
            .iter()
            .map(|w| with_punctuation(w, &mut t))
            .collect();
        assert_eq!(got, ["Hallo,", "ich", "hoffe,", "eine", "Lösung."]);
    }

    #[test]
    fn prompt_is_the_words_only() {
        let mut cfg = SessionConfig::new("openai");
        cfg.language = "deu".into();
        assert_eq!(prompt_from_keywords(&cfg), "");
        cfg.keywords = vec![("Bildschirmtastatur".into(), 5), ("Korner".into(), 5)];
        assert_eq!(prompt_from_keywords(&cfg), "Bildschirmtastatur, Korner");
    }

    #[test]
    fn session_update_shape() {
        let v = session_update("gpt-4o-transcribe", Some("de"), "Korner, iKK");
        assert_eq!(v["session"]["type"], "transcription");
        assert_eq!(v["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(v["session"]["audio"]["input"]["transcription"]["language"], "de");
        assert_eq!(v["session"]["audio"]["input"]["transcription"]["prompt"], "Korner, iKK");
        assert!(v["session"].get("turn_detection").is_none());
    }
}
