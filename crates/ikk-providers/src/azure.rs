//! Microsoft Azure Speech: batch over HTTPS (short-audio REST, split into
//! pieces under a minute), live over WebSocket (the speech service's
//! message protocol: text messages with headers and a JSON body, binary
//! audio messages with a length-prefixed header block).
//!
//! Credential: `REGION:KEY`, e.g. `westeurope:0123…` (the region of the
//! Speech resource in the Azure portal). Swiss German (`gsw`) uses Azure's
//! `de-CH` model, which writes standard German from Swiss speech.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use ikk_audio::{AudioFrame, PIPELINE_SAMPLE_RATE_HZ};
use ikk_stt::lang::iso639_1;
use ikk_stt::*;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

const WIRE_RATE_HZ: u32 = PIPELINE_SAMPLE_RATE_HZ;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_GRACE: Duration = Duration::from_secs(4);
/// The short-audio REST endpoint takes at most 60 s per request.
const MAX_REST_SECONDS: usize = 55;

#[derive(Default)]
pub struct Azure;

/// Azure locale for a settings language (ISO-639-3).
pub fn locale(iso639_3: &str) -> &'static str {
    if iso639_3.trim() == "gsw" {
        return "de-CH";
    }
    match iso639_1(iso639_3) {
        Some("de") => "de-DE",
        Some("fr") => "fr-FR",
        Some("it") => "it-IT",
        Some("es") => "es-ES",
        Some("pt") => "pt-PT",
        Some("nl") => "nl-NL",
        _ => "en-US",
    }
}

/// `REGION:KEY` → (region, key).
pub fn split_credential(credential: &str) -> Result<(String, String), SttError> {
    const FORMAT: &str = "Azure needs region and key, e.g. switzerlandnorth:KEY";
    let (region, key) = credential
        .trim()
        .split_once(':')
        .ok_or(SttError::BadCredential(FORMAT))?;
    let region = region.trim().to_lowercase();
    let key = key.trim().to_string();
    if region.is_empty() || key.is_empty() || !region.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(SttError::BadCredential(FORMAT));
    }
    Ok((region, key))
}

fn endpoint(scheme: &str, region: &str, locale: &str) -> String {
    format!(
        "{scheme}://{region}.stt.speech.microsoft.com/speech/recognition/conversation/cognitiveservices/v1?language={locale}&format=simple"
    )
}

/// 32 hex digits, unique per call (request and connection ids).
fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let mix = |mut z: u64| {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:016x}{:016x}",
        mix(nanos ^ std::process::id() as u64),
        mix(nanos.wrapping_add(n))
    )
}

/// ISO 8601 UTC with milliseconds, e.g. `2026-10-25T13:56:30.104Z`.
fn timestamp() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// WAV header for an open-ended 16 kHz mono 16-bit stream.
fn wav_header() -> Vec<u8> {
    let mut w = Vec::with_capacity(44);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&0u32.to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&WIRE_RATE_HZ.to_le_bytes());
    w.extend_from_slice(&(WIRE_RATE_HZ * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&0u32.to_le_bytes());
    w
}

/// Binary audio message: 2-byte big-endian header length, headers, audio.
pub fn audio_message(request_id: &str, audio: &[u8]) -> Vec<u8> {
    let headers = format!(
        "Path: audio\r\nX-RequestId: {request_id}\r\nX-Timestamp: {}\r\nContent-Type: audio/x-wav\r\n",
        timestamp()
    );
    let mut out = Vec::with_capacity(2 + headers.len() + audio.len());
    out.extend_from_slice(&(headers.len() as u16).to_be_bytes());
    out.extend_from_slice(headers.as_bytes());
    out.extend_from_slice(audio);
    out
}

fn config_message(request_id: &str) -> String {
    let body = serde_json::json!({
        "context": {
            "system": { "version": "1.0.0" },
            "os": { "platform": std::env::consts::OS, "name": "ikk-stt", "version": env!("CARGO_PKG_VERSION") },
            "device": { "manufacturer": "", "model": "", "version": "" }
        }
    });
    format!(
        "Path: speech.config\r\nX-RequestId: {request_id}\r\nX-Timestamp: {}\r\nContent-Type: application/json\r\n\r\n{body}",
        timestamp()
    )
}

/// What a service text message means for the session.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Event(TranscriptEvent),
    /// The service ended the current turn (after a long pause, or after
    /// the empty audio message of `finish`).
    TurnEnd,
    Ignore,
}

/// Parse one text message of the speech service.
pub fn parse_message(msg: &str) -> Incoming {
    let (head, body) = msg.split_once("\r\n\r\n").unwrap_or((msg, ""));
    let path = head
        .lines()
        .find_map(|l| l.strip_prefix("Path:").or_else(|| l.strip_prefix("path:")))
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase();
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    match path.as_str() {
        "speech.hypothesis" | "speech.fragment" => match json.get("Text").and_then(|t| t.as_str()) {
            Some(t) if !t.trim().is_empty() => Incoming::Event(TranscriptEvent::Interim(t.trim().to_string())),
            _ => Incoming::Ignore,
        },
        "speech.phrase" => {
            let status = json.get("RecognitionStatus").and_then(|s| s.as_str()).unwrap_or("");
            match status {
                "Success" => {
                    let text = json.get("DisplayText").and_then(|t| t.as_str()).unwrap_or("").trim();
                    Incoming::Event(TranscriptEvent::Final(text.to_string()))
                }
                "NoMatch" | "InitialSilenceTimeout" | "BabbleTimeout" | "EndOfDictation" => Incoming::Ignore,
                "" => Incoming::Ignore,
                other => Incoming::Event(TranscriptEvent::Error(ErrorClass::from_message(other))),
            }
        }
        "turn.end" => Incoming::TurnEnd,
        _ => Incoming::Ignore,
    }
}

async fn rest_request(region: &str, key: &str, locale: &str, wav: Vec<u8>) -> Result<String, SttError> {
    let resp = crate::http::client()
        .post(endpoint("https", region, locale))
        .header("Ocp-Apim-Subscription-Key", key)
        .header("Content-Type", "audio/wav; codecs=audio/pcm; samplerate=16000")
        .header("Accept", "application/json")
        .body(wav)
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
    match json.get("RecognitionStatus").and_then(|s| s.as_str()) {
        Some("Success") => Ok(json
            .get("DisplayText")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string()),
        Some("NoMatch") | Some("InitialSilenceTimeout") | Some("BabbleTimeout") => Ok(String::new()),
        other => Err(SttError::Provider(ErrorClass::Other(format!(
            "azure: {}",
            other.unwrap_or("no status")
        )))),
    }
}

fn credential(cfg: &SessionConfig) -> Result<(String, String), SttError> {
    let c = cfg
        .credential
        .as_ref()
        .map(|s| s.expose())
        .ok_or(SttError::MissingCredential)?;
    split_credential(c)
}

#[async_trait::async_trait]
impl Provider for Azure {
    fn id(&self) -> ProviderId {
        ProviderId("azure".into())
    }
    fn display_name(&self) -> &'static str {
        "Microsoft Azure"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            batch: true,
            streaming: true,
            interim: true,
            batch_replacement: false,
            required_sample_rate_hz: Some(WIRE_RATE_HZ),
            languages: ["deu", "gsw", "eng", "fra", "ita", "spa", "por", "nld"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            requires_network: true,
            own_audio: false,
        }
    }

    async fn transcribe(&self, audio: AudioClip, cfg: &SessionConfig) -> Result<Transcript, SttError> {
        let (region, key) = credential(cfg)?;
        let locale = locale(&cfg.language);
        let pcm = audio.to_pcm_s16le(WIRE_RATE_HZ);
        // Under a minute per request.
        let piece = MAX_REST_SECONDS * WIRE_RATE_HZ as usize * 2;
        let mut texts = Vec::new();
        for chunk in pcm.chunks(piece) {
            let mut wav = wav_header();
            wav[4..8].copy_from_slice(&(36 + chunk.len() as u32).to_le_bytes());
            wav[40..44].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            wav.extend_from_slice(chunk);
            let t = rest_request(&region, &key, locale, wav).await?;
            if !t.is_empty() {
                texts.push(t);
            }
        }
        Ok(Transcript {
            text: texts.join(" "),
            language: Some(locale.to_string()),
            confidence: None,
        })
    }

    async fn stream(&self, cfg: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        let (region, key) = credential(cfg)?;
        let locale = locale(&cfg.language);
        info!(%region, %locale, "azure ws connect");
        let mut req = endpoint("wss", &region, locale)
            .into_client_request()
            .map_err(|e| SttError::Provider(ErrorClass::Other(e.to_string())))?;
        let bad = |_| SttError::Provider(ErrorClass::Other("azure header".into()));
        req.headers_mut()
            .insert("Ocp-Apim-Subscription-Key", key.parse().map_err(bad)?);
        req.headers_mut()
            .insert("X-ConnectionId", new_id().parse().map_err(bad)?);
        let tls = tokio_tungstenite::Connector::Rustls(crate::http::tls_config());
        let (ws, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(tls)),
        )
        .await
        .map_err(|_| SttError::Provider(ErrorClass::Transport))?
        .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))?;
        let (mut sink, mut source) = ws.split();

        let request_id = new_id();
        sink.send(Message::text(config_message(&request_id)))
            .await
            .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))?;
        sink.send(Message::binary(audio_message(&request_id, &wav_header())))
            .await
            .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))?;

        let sink = Arc::new(Mutex::new(sink));
        let request_id = Arc::new(std::sync::Mutex::new(request_id));
        let finishing = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel(64);
        let recv_task = {
            let event_tx = event_tx.clone();
            let sink = sink.clone();
            let request_id = request_id.clone();
            let finishing = finishing.clone();
            tokio::spawn(async move {
                let mut failed = false;
                while let Some(msg) = source.next().await {
                    match msg {
                        Ok(Message::Text(t)) => match parse_message(t.as_str()) {
                            Incoming::Event(ev) => {
                                if event_tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            Incoming::TurnEnd if finishing.load(Ordering::SeqCst) => break,
                            // Azure ends a turn after a long pause (or ~15 s
                            // of silence at the start) but keeps the
                            // connection: open the next turn and go on.
                            Incoming::TurnEnd => {
                                let id = new_id();
                                if let Ok(mut r) = request_id.lock() {
                                    *r = id.clone();
                                }
                                debug!("azure turn ended; next turn");
                                let sent = sink
                                    .lock()
                                    .await
                                    .send(Message::binary(audio_message(&id, &wav_header())))
                                    .await;
                                if let Err(e) = sent {
                                    let _ = event_tx
                                        .send(TranscriptEvent::Error(ErrorClass::from_message(&e.to_string())))
                                        .await;
                                    failed = true;
                                    break;
                                }
                            }
                            Incoming::Ignore => {}
                        },
                        Ok(Message::Close(frame)) => {
                            if let Some(f) = frame {
                                if f.code != tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal
                                {
                                    let _ = event_tx
                                        .send(TranscriptEvent::Error(ErrorClass::from_message(f.reason.as_str())))
                                        .await;
                                    failed = true;
                                }
                            }
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = event_tx
                                .send(TranscriptEvent::Error(ErrorClass::from_message(&e.to_string())))
                                .await;
                            failed = true;
                            break;
                        }
                    }
                }
                // Closed by the service while still dictating: say so, or
                // the app keeps showing "listening" with nothing arriving.
                if !failed && !finishing.load(Ordering::SeqCst) {
                    warn!("azure closed the connection during dictation");
                    let _ = event_tx.send(TranscriptEvent::Closed).await;
                }
                debug!("azure ws receiver finished");
            })
        };

        Ok(Box::new(AzureSession {
            sink,
            events: event_rx,
            event_tx,
            recv_task,
            request_id,
            finishing,
        }))
    }
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

pub struct AzureSession {
    sink: Arc<Mutex<WsSink>>,
    events: mpsc::Receiver<TranscriptEvent>,
    event_tx: mpsc::Sender<TranscriptEvent>,
    recv_task: tokio::task::JoinHandle<()>,
    /// Changes with every turn.
    request_id: Arc<std::sync::Mutex<String>>,
    finishing: Arc<AtomicBool>,
}

impl AzureSession {
    fn request_id(&self) -> String {
        self.request_id.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl StreamingSession for AzureSession {
    async fn push(&mut self, frame: AudioFrame) -> Result<(), SttError> {
        let pcm = if frame.sample_rate_hz == WIRE_RATE_HZ {
            frame.to_pcm_s16le()
        } else {
            AudioClip { frames: vec![frame] }.to_pcm_s16le(WIRE_RATE_HZ)
        };
        self.sink
            .lock()
            .await
            .send(Message::binary(audio_message(&self.request_id(), &pcm)))
            .await
            .map_err(|e| SttError::Provider(ErrorClass::from_message(&e.to_string())))
    }

    async fn cut_segment(&mut self) -> Result<(), SttError> {
        Ok(()) // Azure closes phrases at its own speech pauses.
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        // An empty audio message ends the stream; the last phrase and
        // `turn.end` follow.
        self.finishing.store(true, Ordering::SeqCst);
        let _ = self
            .sink
            .lock()
            .await
            .send(Message::binary(audio_message(&self.request_id(), &[])))
            .await;
        if tokio::time::timeout(CLOSE_GRACE, &mut self.recv_task).await.is_err() {
            warn!("azure did not end the turn within the grace period");
            self.recv_task.abort();
        }
        let _ = self.sink.lock().await.send(Message::Close(None)).await;
        let _ = self.event_tx.send(TranscriptEvent::Closed).await;
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
    fn locales_include_swiss_german() {
        assert_eq!(locale("gsw"), "de-CH");
        assert_eq!(locale("deu"), "de-DE");
        assert_eq!(locale("fra"), "fr-FR");
        assert_eq!(locale(""), "en-US");
    }

    #[test]
    fn credential_is_region_and_key() {
        assert_eq!(
            split_credential(" WestEurope : abc123 ").unwrap(),
            ("westeurope".to_string(), "abc123".to_string())
        );
        assert!(split_credential("abc123").is_err());
        assert!(split_credential("west europe:abc").is_err());
        assert!(split_credential(":abc").is_err());
    }

    #[test]
    fn parses_service_messages() {
        let hyp = "X-RequestId: 1\r\nPath: speech.hypothesis\r\nContent-Type: application/json\r\n\r\n{\"Text\":\"hallo wel\",\"Offset\":0}";
        assert_eq!(
            parse_message(hyp),
            Incoming::Event(TranscriptEvent::Interim("hallo wel".into()))
        );
        let phrase = "Path: speech.phrase\r\n\r\n{\"RecognitionStatus\":\"Success\",\"DisplayText\":\"Hallo Welt.\"}";
        assert_eq!(
            parse_message(phrase),
            Incoming::Event(TranscriptEvent::Final("Hallo Welt.".into()))
        );
        let nomatch = "Path: speech.phrase\r\n\r\n{\"RecognitionStatus\":\"NoMatch\"}";
        assert_eq!(parse_message(nomatch), Incoming::Ignore);
        assert_eq!(parse_message("Path: turn.end\r\n\r\n{}"), Incoming::TurnEnd);
        assert_eq!(parse_message("Path: turn.start\r\n\r\n{}"), Incoming::Ignore);
    }

    #[test]
    fn audio_message_layout() {
        let m = audio_message("ab", &[1, 2, 3]);
        let len = u16::from_be_bytes([m[0], m[1]]) as usize;
        let headers = std::str::from_utf8(&m[2..2 + len]).unwrap();
        assert!(headers.starts_with("Path: audio\r\nX-RequestId: ab\r\n"));
        assert!(headers.ends_with("Content-Type: audio/x-wav\r\n"));
        assert_eq!(&m[2 + len..], &[1, 2, 3]);
    }

    #[test]
    fn ids_and_timestamps_are_well_formed() {
        let (a, b) = (new_id(), new_id());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        let t = timestamp();
        assert_eq!(t.len(), 24, "{t}");
        assert!(t.ends_with('Z') && &t[4..5] == "-" && &t[10..11] == "T");
    }
}
