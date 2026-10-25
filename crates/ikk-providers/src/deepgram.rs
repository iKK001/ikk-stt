//! Deepgram adapter: batch over HTTPS, live over WebSocket.
//!
//! Streaming: 16 kHz linear16 PCM frames go up, `Results` JSON comes down
//! with interim and final transcripts. `Finalize` closes the current segment
//! on demand, `CloseStream` ends the session. A `KeepAlive` every 4 s keeps
//! the socket open through pauses. In `Batch` transfer mode the stream is
//! only a live preview: on finish the full audio is re-transcribed once and
//! the words the stream missed at the end are appended.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use ikk_audio::{AudioFrame, PIPELINE_SAMPLE_RATE_HZ};
use ikk_stt::lang::iso639_1;
use ikk_stt::merge::trailing_suffix;
use ikk_stt::*;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use url::Url;

const BATCH_URL: &str = "https://api.deepgram.com/v1/listen";
const STREAM_URL: &str = "wss://api.deepgram.com/v1/listen";
const WIRE_RATE_HZ: u32 = PIPELINE_SAMPLE_RATE_HZ;
const KEEPALIVE: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// Audio kept for the batch tail pass; beyond this the pass is skipped.
const MAX_TAIL_BUFFER_BYTES: usize = 55 * 1024 * 1024;

#[derive(Default)]
pub struct Deepgram;

impl Deepgram {
    fn params(cfg: &SessionConfig, streaming: bool) -> Vec<(String, String)> {
        let model = cfg.model.clone().unwrap_or_else(|| "nova-3".into());
        let mut p = vec![
            ("model".into(), model.clone()),
            ("encoding".into(), "linear16".into()),
            ("sample_rate".into(), WIRE_RATE_HZ.to_string()),
            ("channels".into(), "1".into()),
            ("smart_format".into(), cfg.smart_format.to_string()),
            ("punctuate".into(), "true".into()),
        ];
        if let Some(l) = iso639_1(&cfg.language) {
            p.push(("language".into(), l.into()));
        }
        if streaming {
            p.push(("interim_results".into(), "true".into()));
            p.push(("endpointing".into(), cfg.endpointing.as_millis().to_string()));
            p.push((
                "utterance_end_ms".into(),
                cfg.utterance_end.as_millis().max(1000).to_string(),
            ));
        }
        // Nova-3 replaced `keywords` with English-only `keyterm`; keep keywords for older models.
        if !model.to_lowercase().starts_with("nova-3") {
            for (word, boost) in &cfg.keywords {
                p.push(("keywords".into(), format!("{word}:{}", boost.clamp(&1, &10))));
            }
        }
        p
    }

    fn url(base: &str, cfg: &SessionConfig, streaming: bool) -> Url {
        let mut u = Url::parse(base).expect("static url");
        u.query_pairs_mut().extend_pairs(Self::params(cfg, streaming));
        u
    }

    fn credential(cfg: &SessionConfig) -> Result<&str, SttError> {
        cfg.credential
            .as_ref()
            .map(|s| s.expose())
            .ok_or(SttError::MissingCredential)
    }
}

/// Parse one Deepgram `Results` message. Heartbeats and metadata → `None`.
pub fn parse_message(json: &str) -> Option<TranscriptEvent> {
    let j: serde_json::Value = serde_json::from_str(json).ok()?;
    match j.get("type").and_then(|t| t.as_str()) {
        Some("Results") => {}
        Some("Error") => {
            let msg = j
                .get("message")
                .or(j.get("description"))
                .and_then(|m| m.as_str())
                .unwrap_or("deepgram error");
            return Some(TranscriptEvent::Error(ErrorClass::from_message(msg)));
        }
        _ => return None,
    }
    let text = j
        .pointer("/channel/alternatives/0/transcript")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let is_final = j.get("is_final").and_then(|v| v.as_bool()).unwrap_or(false);
    if is_final {
        Some(TranscriptEvent::Final(text.to_string()))
    } else if text.is_empty() {
        None
    } else {
        Some(TranscriptEvent::Interim(text.to_string()))
    }
}

fn parse_batch(json: &serde_json::Value) -> Transcript {
    let alt = json.pointer("/results/channels/0/alternatives/0");
    Transcript {
        text: alt
            .and_then(|a| a.get("transcript"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        language: json
            .pointer("/results/channels/0/detected_language")
            .and_then(|l| l.as_str())
            .map(String::from),
        confidence: alt
            .and_then(|a| a.get("confidence"))
            .and_then(|c| c.as_f64())
            .map(|c| c as f32),
    }
}

async fn batch_request(key: &str, url: Url, pcm: Vec<u8>) -> Result<Transcript, SttError> {
    let resp = crate::http::client()
        .post(url)
        .header("Authorization", format!("Token {key}"))
        .header("Content-Type", "application/octet-stream")
        .body(pcm)
        .send()
        .await
        .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status != 200 {
        return Err(SttError::Provider(ErrorClass::from_status(status, &body)));
    }
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| SttError::Provider(ErrorClass::Other(e.to_string())))?;
    Ok(parse_batch(&json))
}

#[async_trait::async_trait]
impl Provider for Deepgram {
    fn id(&self) -> ProviderId {
        ProviderId("deepgram".into())
    }
    fn display_name(&self) -> &'static str {
        "Deepgram"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            batch: true,
            streaming: true,
            interim: true,
            batch_replacement: true,
            required_sample_rate_hz: Some(WIRE_RATE_HZ),
            languages: ["deu", "eng", "fra", "ita", "spa", "por", "nld"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            requires_network: true,
            own_audio: false,
        }
    }

    async fn transcribe(&self, audio: AudioClip, cfg: &SessionConfig) -> Result<Transcript, SttError> {
        let key = Self::credential(cfg)?;
        let pcm = audio.to_pcm_s16le(WIRE_RATE_HZ);
        if pcm.is_empty() {
            return Ok(Transcript {
                text: String::new(),
                language: None,
                confidence: None,
            });
        }
        batch_request(key, Self::url(BATCH_URL, cfg, false), pcm).await
    }

    async fn stream(&self, cfg: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        let key = Self::credential(cfg)?.to_string();
        let url = Self::url(STREAM_URL, cfg, true);
        let tls = tokio_tungstenite::Connector::Rustls(crate::http::tls_config());
        info!(model = ?cfg.model, language = %cfg.language, "deepgram ws connect");

        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| SttError::Provider(ErrorClass::Other(e.to_string())))?;
        req.headers_mut().insert(
            "Authorization",
            format!("Token {key}")
                .parse()
                .map_err(|_| SttError::Provider(ErrorClass::Other("auth header".into())))?,
        );
        let (ws, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(tls)),
        )
        .await
        .map_err(|_| SttError::Provider(ErrorClass::Transport))?
        .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))?;
        let (sink, mut source) = ws.split();
        let sink = Arc::new(Mutex::new(sink));

        let (event_tx, event_rx) = mpsc::channel(64);
        let committed = Arc::new(Mutex::new(String::new()));
        let last_interim = Arc::new(Mutex::new(String::new()));

        // Receiver task: JSON → events; tracks committed finals for the tail pass.
        let recv_task = {
            let event_tx = event_tx.clone();
            let committed = committed.clone();
            let last_interim = last_interim.clone();
            tokio::spawn(async move {
                while let Some(msg) = source.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            if let Some(ev) = parse_message(t.as_str()) {
                                match &ev {
                                    TranscriptEvent::Final(text) => {
                                        last_interim.lock().await.clear();
                                        if text.is_empty() {
                                            continue;
                                        }
                                        let mut c = committed.lock().await;
                                        if !c.is_empty() {
                                            c.push(' ');
                                        }
                                        c.push_str(text);
                                    }
                                    TranscriptEvent::Interim(text) => *last_interim.lock().await = text.clone(),
                                    _ => {}
                                }
                                if event_tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(e) => {
                            let _ = event_tx
                                .send(TranscriptEvent::Error(ErrorClass::from_message(&e.to_string())))
                                .await;
                            break;
                        }
                    }
                }
                debug!("deepgram ws receiver finished");
            })
        };

        // Keepalive task.
        let keepalive_task = {
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(KEEPALIVE);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if sink
                        .lock()
                        .await
                        .send(Message::text(r#"{"type":"KeepAlive"}"#))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };

        Ok(Box::new(DeepgramSession {
            sink,
            events: event_rx,
            event_tx,
            recv_task,
            keepalive_task,
            committed,
            last_interim,
            tail_pass: cfg.transfer == TransferMode::Batch,
            tail_buffer: Vec::new(),
            tail_overflow: false,
            cfg: cfg.clone(),
            key,
            started: Instant::now(),
        }))
    }
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

pub struct DeepgramSession {
    sink: Arc<Mutex<WsSink>>,
    events: mpsc::Receiver<TranscriptEvent>,
    event_tx: mpsc::Sender<TranscriptEvent>,
    recv_task: tokio::task::JoinHandle<()>,
    keepalive_task: tokio::task::JoinHandle<()>,
    committed: Arc<Mutex<String>>,
    last_interim: Arc<Mutex<String>>,
    tail_pass: bool,
    tail_buffer: Vec<u8>,
    tail_overflow: bool,
    cfg: SessionConfig,
    key: String,
    started: Instant,
}

#[async_trait::async_trait]
impl StreamingSession for DeepgramSession {
    async fn push(&mut self, frame: AudioFrame) -> Result<(), SttError> {
        let bytes = if frame.sample_rate_hz == WIRE_RATE_HZ {
            frame.to_pcm_s16le()
        } else {
            AudioClip { frames: vec![frame] }.to_pcm_s16le(WIRE_RATE_HZ)
        };
        if self.tail_pass && !self.tail_overflow {
            if self.tail_buffer.len() + bytes.len() > MAX_TAIL_BUFFER_BYTES {
                self.tail_overflow = true;
                self.tail_buffer.clear();
            } else {
                self.tail_buffer.extend_from_slice(&bytes);
            }
        }
        self.sink
            .lock()
            .await
            .send(Message::binary(bytes))
            .await
            .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))
    }

    async fn cut_segment(&mut self) -> Result<(), SttError> {
        self.sink
            .lock()
            .await
            .send(Message::text(r#"{"type":"Finalize"}"#))
            .await
            .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        self.keepalive_task.abort();
        {
            let mut sink = self.sink.lock().await;
            let _ = sink.send(Message::text(r#"{"type":"Finalize"}"#)).await;
            let _ = sink.send(Message::text(r#"{"type":"CloseStream"}"#)).await;
        }
        // Let the last finals arrive, then close.
        if tokio::time::timeout(CLOSE_GRACE, &mut self.recv_task).await.is_err() {
            warn!("deepgram did not close within grace period");
            self.recv_task.abort();
        }
        let _ = self.sink.lock().await.send(Message::Close(None)).await;

        // Unfinalised interim at the end counts as final.
        let dangling = std::mem::take(&mut *self.last_interim.lock().await);
        if !dangling.trim().is_empty() {
            let mut c = self.committed.lock().await;
            if !c.is_empty() {
                c.push(' ');
            }
            c.push_str(dangling.trim());
            let _ = self
                .event_tx
                .send(TranscriptEvent::Final(dangling.trim().to_string()))
                .await;
        }

        // Batch tail pass: append what the stream missed at the end.
        if self.tail_pass && !self.tail_overflow && !self.tail_buffer.is_empty() {
            let url = Deepgram::url(BATCH_URL, &self.cfg, false);
            match tokio::time::timeout(
                Duration::from_secs(60),
                batch_request(&self.key, url, std::mem::take(&mut self.tail_buffer)),
            )
            .await
            {
                Ok(Ok(t)) => {
                    let committed = self.committed.lock().await.clone();
                    let tail = trailing_suffix(&committed, &t.text);
                    if !tail.is_empty() {
                        let _ = self.event_tx.send(TranscriptEvent::Final(tail)).await;
                    }
                }
                Ok(Err(e)) => warn!(%e, "deepgram tail pass failed"),
                Err(_) => warn!("deepgram tail pass timed out"),
            }
        }
        let _ = self.event_tx.send(TranscriptEvent::Closed).await;
        info!(elapsed = ?self.started.elapsed(), "deepgram session finished");
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
    fn parses_interim_and_final() {
        let interim = r#"{"type":"Results","is_final":false,"channel":{"alternatives":[{"transcript":"hallo wel"}]}}"#;
        let fin = r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"Hallo Welt."}]}}"#;
        assert_eq!(
            parse_message(interim),
            Some(TranscriptEvent::Interim("hallo wel".into()))
        );
        assert_eq!(parse_message(fin), Some(TranscriptEvent::Final("Hallo Welt.".into())));
        assert_eq!(parse_message(r#"{"type":"Metadata"}"#), None);
        assert_eq!(
            parse_message(r#"{"type":"Results","is_final":false,"channel":{"alternatives":[{"transcript":""}]}}"#),
            None
        );
        assert!(matches!(
            parse_message(r#"{"type":"Error","message":"invalid api key"}"#),
            Some(TranscriptEvent::Error(ErrorClass::Unauthorized))
        ));
    }

    #[test]
    fn url_carries_settings_and_drops_keywords_for_nova3() {
        let mut cfg = SessionConfig::new("deepgram");
        cfg.language = "deu".into();
        cfg.keywords = vec![("Korner".into(), 7)];
        let u = Deepgram::url(STREAM_URL, &cfg, true).to_string();
        assert!(u.contains("model=nova-3") && u.contains("language=de") && u.contains("interim_results=true"));
        assert!(u.contains("endpointing=800") && u.contains("utterance_end_ms=1000"));
        assert!(!u.contains("keywords"));
        cfg.model = Some("nova-2".into());
        let u = Deepgram::url(BATCH_URL, &cfg, false).to_string();
        assert!(u.contains("keywords=Korner%3A7") && !u.contains("interim_results"));
    }

    #[test]
    fn parses_batch_body() {
        let j: serde_json::Value = serde_json::from_str(
            r#"{"results":{"channels":[{"detected_language":"de","alternatives":[{"transcript":" Guten Tag. ","confidence":0.97}]}]}}"#,
        )
        .unwrap();
        let t = parse_batch(&j);
        assert_eq!(t.text, "Guten Tag.");
        assert_eq!(t.language.as_deref(), Some("de"));
        assert!(t.confidence.unwrap() > 0.9);
    }
}
