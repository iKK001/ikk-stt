//! Voiceitt adapter: speech recognition trained on non-standard speech.
//!
//! Credential: Voiceitt needs an *App ID* and an *API key*. The session
//! credential string is `APP_ID:API_KEY`. They are exchanged for a short
//! lived bearer token plus refresh token (`/v1/auth/login/user_id`,
//! `/v1/auth/refresh_token`), cached per provider instance.
//!
//! Batch: multipart `/v1/rec/transcribe` with an `options` JSON part and a
//! 16 kHz WAV. Streaming: Socket.IO on `web.voiceitt.com`; 16 kHz int16
//! PCM in 200 ms chunks via `stream_audio_samples`; `partial_recognition`
//! carries a stable delta plus unstable tail per segment, `recognition` is
//! the per-segment final. On finish the full audio is re-transcribed in
//! batch and the tail the stream missed is appended; Voiceitt sometimes
//! drops the last recognitions on disconnect, so batch is authoritative.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ikk_audio::AudioFrame;
use ikk_stt::merge::trailing_suffix;
use ikk_stt::*;
use reqwest::multipart::{Form, Part};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use crate::socketio::{SioClient, SioEvent};

const API: &str = "https://api2.voiceitt.com";
const STREAM: &str = "https://web.voiceitt.com";
const WIRE_RATE_HZ: u32 = 16_000;
/// 3200 samples @ 16 kHz = 200 ms, Voiceitt's recommended chunk.
const CHUNK_BYTES: usize = 3200 * 2;
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);
const TAIL_BUFFER_MAX: usize = 32_000 * 60 * 10; // ≈ 10 minutes

#[derive(Default)]
struct Auth {
    token: Option<String>,
    token_expires: Option<Instant>,
    refresh: Option<String>,
    refresh_expires: Option<Instant>,
}

#[derive(Default)]
pub struct Voiceitt {
    auth: Mutex<Auth>,
}

/// Per-session options Voiceitt accepts, carried in `SessionConfig.keywords`
/// style would be wrong; they are booleans, so we encode them as model
/// string flags: `"si;itn;cap;noprof"`. Defaults: ITN on, capitalisation on,
/// profanity filter off.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub run_itn: bool,
    pub run_capitalization: bool,
    pub filter_profanities: bool,
}

impl Options {
    pub fn from_model(model: Option<&str>) -> Self {
        let m = model.unwrap_or("");
        Self {
            run_itn: !m.contains("noitn"),
            run_capitalization: !m.contains("nocap"),
            filter_profanities: m.contains("prof"),
        }
    }
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "run_itn": self.run_itn,
            "run_capitalization": self.run_capitalization,
            "filter_profanities": self.filter_profanities,
        })
    }
}

fn split_credential(cfg: &SessionConfig) -> Result<(String, String), SttError> {
    let raw = cfg
        .credential
        .as_ref()
        .ok_or(SttError::MissingCredential)?
        .expose()
        .to_string();
    let (app_id, key) = raw.split_once(':').ok_or(SttError::MissingCredential)?;
    if app_id.is_empty() || key.is_empty() {
        return Err(SttError::MissingCredential);
    }
    Ok((app_id.to_string(), key.to_string()))
}

fn err(e: impl ToString) -> SttError {
    SttError::Provider(ErrorClass::from_message(&e.to_string()))
}

impl Voiceitt {
    async fn absorb(&self, body: &str) -> Result<String, SttError> {
        let j: serde_json::Value = serde_json::from_str(body).map_err(err)?;
        let token = j
            .get("token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| err("voiceitt auth: no token"))?
            .to_string();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let to_instant =
            |ms: Option<i64>| ms.map(|m| Instant::now() + Duration::from_millis((m - now_ms).max(0) as u64));
        let mut a = self.auth.lock().await;
        a.token = Some(token.clone());
        a.token_expires = to_instant(j.get("token_expires_at").and_then(|v| v.as_i64()));
        if let Some(r) = j.get("refresh_token").and_then(|v| v.as_str()) {
            a.refresh = Some(r.to_string());
        }
        a.refresh_expires = to_instant(j.get("refresh_token_expires_at").and_then(|v| v.as_i64()));
        Ok(token)
    }

    async fn post_json(url: &str, body: serde_json::Value) -> Result<(u16, String), SttError> {
        let r = crate::http::client().post(url).json(&body).send().await.map_err(err)?;
        let status = r.status().as_u16();
        Ok((status, r.text().await.unwrap_or_default()))
    }

    async fn token(&self, cfg: &SessionConfig) -> Result<String, SttError> {
        let (app_id, key) = split_credential(cfg)?;
        {
            let a = self.auth.lock().await;
            if let (Some(t), Some(exp)) = (&a.token, a.token_expires) {
                if exp > Instant::now() + EXPIRY_MARGIN {
                    return Ok(t.clone());
                }
            }
        }
        let refresh = {
            let a = self.auth.lock().await;
            match (&a.refresh, a.refresh_expires) {
                (Some(r), Some(exp)) if exp > Instant::now() + EXPIRY_MARGIN => Some(r.clone()),
                _ => None,
            }
        };
        if let Some(r) = refresh {
            if let Ok((200, body)) = Self::post_json(
                &format!("{API}/v1/auth/refresh_token"),
                serde_json::json!({"refresh_token": r}),
            )
            .await
            {
                if let Ok(t) = self.absorb(&body).await {
                    return Ok(t);
                }
            }
        }
        let (status, body) = Self::post_json(
            &format!("{API}/v1/auth/login/user_id"),
            serde_json::json!({"app_id": app_id, "api_key": key}),
        )
        .await?;
        if status != 200 {
            return Err(SttError::Provider(ErrorClass::from_status(status, &body)));
        }
        self.absorb(&body).await
    }

    async fn invalidate(&self) {
        *self.auth.lock().await = Auth::default();
    }

    async fn refresh_token(&self) -> Option<String> {
        self.auth.lock().await.refresh.clone()
    }

    async fn batch(&self, cfg: &SessionConfig, wav: Vec<u8>) -> Result<Transcript, SttError> {
        let opts = Options::from_model(cfg.model.as_deref());
        for attempt in 0..2 {
            let token = self.token(cfg).await?;
            let form = Form::new()
                .part(
                    "options",
                    Part::text(opts.json().to_string())
                        .file_name("options.json")
                        .mime_str("application/json")
                        .map_err(err)?,
                )
                .part(
                    "audio",
                    Part::bytes(wav.clone())
                        .file_name("audio.wav")
                        .mime_str("audio/wav")
                        .map_err(err)?,
                );
            let r = crate::http::client()
                .post(format!("{API}/v1/rec/transcribe"))
                .bearer_auth(&token)
                .multipart(form)
                .send()
                .await
                .map_err(err)?;
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            if status == 401 && attempt == 0 {
                self.invalidate().await;
                continue;
            }
            if status != 200 {
                return Err(SttError::Provider(ErrorClass::from_status(status, &body)));
            }
            let j: serde_json::Value = serde_json::from_str(&body).map_err(err)?;
            return Ok(Transcript {
                text: j.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string(),
                language: Some("eng".into()),
                confidence: None,
            });
        }
        Err(SttError::Provider(ErrorClass::Unauthorized))
    }
}

#[async_trait::async_trait]
impl Provider for Voiceitt {
    fn id(&self) -> ProviderId {
        ProviderId("voiceitt".into())
    }
    fn display_name(&self) -> &'static str {
        "Voiceitt"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            batch: true,
            streaming: true,
            interim: true,
            batch_replacement: true,
            required_sample_rate_hz: Some(WIRE_RATE_HZ),
            languages: vec!["eng".into()],
            requires_network: true,
            own_audio: false,
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
        self.batch(cfg, audio.to_wav(WIRE_RATE_HZ)).await
    }

    async fn stream(&self, cfg: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        let token = self.token(cfg).await?;
        let mut auth = serde_json::json!({"token": token});
        if let Some(r) = self.refresh_token().await {
            auth["refresh_token"] = serde_json::Value::String(r);
        }
        let client = SioClient::connect(
            STREAM,
            auth,
            tokio_tungstenite::Connector::Rustls(crate::http::tls_config()),
        )
        .await
        .map_err(err)?;
        info!("voiceitt socket.io connected");
        let opts = Options::from_model(cfg.model.as_deref());
        // Clone-free: the provider's auth cache is not needed by the session; batch
        // tail uses a fresh provider call via the cfg credential.
        Ok(Box::new(VoiceittSession {
            client: Some(client),
            opts,
            ready: false,
            pending: Vec::new(),
            tail: Vec::new(),
            state: SegmentState::default(),
            events_tx_buf: Vec::new(),
            cfg: cfg.clone(),
            events: mpsc::channel(64),
        }))
    }
}

/// Text assembly per Voiceitt's segment model.
#[derive(Default)]
struct SegmentState {
    history: Vec<String>,
    pending_segments: BTreeMap<i64, String>,
    current_id: Option<i64>,
    current_stable: String,
    current_unstable: String,
    longest_interim: String,
}

impl SegmentState {
    fn compose(&mut self) -> String {
        let mut parts: Vec<&str> = self
            .history
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        parts.extend(
            self.pending_segments
                .values()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty()),
        );
        if !self.current_stable.trim().is_empty() {
            parts.push(self.current_stable.trim());
        }
        if !self.current_unstable.trim().is_empty() {
            parts.push(self.current_unstable.trim());
        }
        let composed = parts.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        if composed.len() > self.longest_interim.len() {
            self.longest_interim = composed.clone();
        }
        composed
    }

    fn partial(&mut self, seg_id: Option<i64>, stable_delta: &str, unstable: &str) -> String {
        if let (Some(id), Some(cur)) = (seg_id, self.current_id) {
            if id != cur {
                let prev = self.current_stable.trim().to_string();
                if !prev.is_empty() {
                    self.pending_segments.insert(cur, prev);
                }
                self.current_stable.clear();
                self.current_unstable.clear();
            }
        }
        if seg_id.is_some() {
            self.current_id = seg_id;
        }
        let d = stable_delta.trim();
        if !d.is_empty() {
            if !self.current_stable.is_empty() {
                self.current_stable.push(' ');
            }
            self.current_stable.push_str(d);
        }
        self.current_unstable = unstable.trim().to_string();
        self.compose()
    }

    fn fin(&mut self, seg_id: Option<i64>, text: &str) -> Option<String> {
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        let id = seg_id.or(self.current_id);
        if let Some(id) = id {
            self.pending_segments.remove(&id);
            if Some(id) == self.current_id {
                self.current_stable.clear();
                self.current_unstable.clear();
                self.current_id = None;
            }
        }
        self.history.push(t.to_string());
        Some(self.compose())
    }

    fn best_text(&mut self) -> String {
        let composed = self.compose();
        if !self.longest_interim.trim().is_empty() {
            self.longest_interim.trim().to_string()
        } else {
            composed.trim().to_string()
        }
    }
}

pub struct VoiceittSession {
    client: Option<SioClient>,
    opts: Options,
    ready: bool,
    pending: Vec<u8>,
    tail: Vec<u8>,
    state: SegmentState,
    events_tx_buf: Vec<TranscriptEvent>,
    cfg: SessionConfig,
    events: (mpsc::Sender<TranscriptEvent>, mpsc::Receiver<TranscriptEvent>),
}

impl VoiceittSession {
    fn read_i64(v: &serde_json::Value, keys: &[&str]) -> Option<i64> {
        keys.iter()
            .find_map(|k| v.get(k))
            .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
    }
    fn read_str<'a>(v: &'a serde_json::Value, keys: &[&str]) -> &'a str {
        keys.iter()
            .find_map(|k| v.get(k))
            .and_then(|x| x.as_str())
            .unwrap_or("")
    }

    /// Pull socket events without blocking and turn them into transcript events.
    async fn pump(&mut self) {
        let mut queued = Vec::new();
        if let Some(client) = self.client.as_mut() {
            while let Ok(ev) = client.events.try_recv() {
                queued.push(ev);
            }
        }
        for ev in queued {
            match ev {
                SioEvent::Connected => {}
                SioEvent::Event { name, args } => {
                    let arg = args.first().cloned().unwrap_or(serde_json::Value::Null);
                    match name.as_str() {
                        "connection_ready" | "model_ready" => self.mark_ready().await,
                        "ready_changed" => {
                            let ready = arg == serde_json::Value::Bool(true)
                                || arg.get("isReady") == Some(&serde_json::Value::Bool(true));
                            if ready {
                                self.mark_ready().await
                            } else if arg == serde_json::Value::Bool(false) {
                                self.ready = false;
                            }
                        }
                        "partial_recognition" => {
                            let text = self.state.partial(
                                Self::read_i64(&arg, &["segment_id", "segmentId"]),
                                Self::read_str(&arg, &["text"]),
                                Self::read_str(&arg, &["unstable_text", "unstableText"]),
                            );
                            self.events_tx_buf.push(TranscriptEvent::Interim(text));
                        }
                        "recognition" => {
                            if let Some(text) = self.state.fin(
                                Self::read_i64(&arg, &["segment_id", "segmentId"]),
                                Self::read_str(&arg, &["text"]),
                            ) {
                                self.events_tx_buf.push(TranscriptEvent::Interim(text));
                            }
                        }
                        "error" => warn!(?arg, "voiceitt socket error"),
                        _ => {}
                    }
                }
                SioEvent::ConnectError(e) => self
                    .events_tx_buf
                    .push(TranscriptEvent::Error(ErrorClass::from_message(&e))),
                SioEvent::Transport(e) => self
                    .events_tx_buf
                    .push(TranscriptEvent::Error(ErrorClass::from_message(&e))),
                SioEvent::Disconnected => self.ready = false,
            }
        }
        for ev in self.events_tx_buf.drain(..) {
            let _ = self.events.0.send(ev).await;
        }
    }

    async fn mark_ready(&mut self) {
        if self.ready {
            return;
        }
        self.ready = true;
        if let Some(c) = self.client.as_ref() {
            let _ = c.emit("set_options", &[self.opts.json()]).await;
        }
        self.flush().await;
    }

    async fn flush(&mut self) {
        if !self.ready {
            return;
        }
        let Some(client) = self.client.as_ref() else { return };
        while self.pending.len() >= CHUNK_BYTES {
            let chunk: Vec<u8> = self.pending.drain(..CHUNK_BYTES).collect();
            if let Err(e) = client
                .emit_binary("stream_audio_samples", chunk, &[serde_json::json!("int16")])
                .await
            {
                warn!(%e, "voiceitt emit failed");
                self.ready = false;
                return;
            }
        }
    }
}

#[async_trait::async_trait]
impl StreamingSession for VoiceittSession {
    async fn push(&mut self, frame: AudioFrame) -> Result<(), SttError> {
        let bytes = AudioClip { frames: vec![frame] }.to_pcm_s16le(WIRE_RATE_HZ);
        if self.tail.len() < TAIL_BUFFER_MAX {
            self.tail.extend_from_slice(&bytes);
        }
        self.pending.extend_from_slice(&bytes);
        self.pump().await;
        self.flush().await;
        Ok(())
    }

    /// Voiceitt segments server-side; nothing to cut.
    async fn cut_segment(&mut self) -> Result<(), SttError> {
        self.pump().await;
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        self.pump().await;
        if let Some(client) = self.client.as_ref() {
            if self.ready && !self.pending.is_empty() {
                let rest = std::mem::take(&mut self.pending);
                let _ = client
                    .emit_binary("stream_audio_samples", rest, &[serde_json::json!("int16")])
                    .await;
            }
            let _ = client.emit("disconnect_request", &[]).await;
        }
        // Give the server a moment to deliver the last recognitions.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.pump().await;
            if !self.ready {
                break;
            }
        }
        let streamed = self.state.best_text();
        let tx = self.events.0.clone();
        let cfg = self.cfg.clone();
        let tail = std::mem::take(&mut self.tail);
        if let Some(client) = self.client.take() {
            client.disconnect().await;
        }

        if !streamed.is_empty() {
            let _ = tx.send(TranscriptEvent::Final(streamed.clone())).await;
        }
        // Batch pass over the full audio: authoritative tail (Voiceitt may drop
        // the last recognitions on disconnect).
        if !tail.is_empty() {
            let provider = Voiceitt::default();
            let wav = AudioClip {
                frames: vec![AudioFrame {
                    samples: pcm_to_f32(&tail),
                    sample_rate_hz: WIRE_RATE_HZ,
                    offset: Duration::ZERO,
                }],
            }
            .to_wav(WIRE_RATE_HZ);
            match tokio::time::timeout(Duration::from_secs(60), provider.batch(&cfg, wav)).await {
                Ok(Ok(t)) => {
                    let add = if streamed.is_empty() {
                        t.text.trim().to_string()
                    } else {
                        trailing_suffix(&streamed, &t.text)
                    };
                    if !add.is_empty() {
                        let _ = tx.send(TranscriptEvent::Final(add)).await;
                    }
                }
                Ok(Err(e)) => warn!(%e, "voiceitt tail batch failed"),
                Err(_) => warn!("voiceitt tail batch timed out"),
            }
        }
        let _ = tx.send(TranscriptEvent::Closed).await;
        Ok(())
    }

    fn events(&mut self) -> &mut mpsc::Receiver<TranscriptEvent> {
        &mut self.events.1
    }
}

fn pcm_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_format() {
        let mut cfg = SessionConfig::new("voiceitt");
        assert!(split_credential(&cfg).is_err());
        cfg.credential = Some(Secret::new("app123:key456"));
        assert_eq!(split_credential(&cfg).unwrap(), ("app123".into(), "key456".into()));
    }

    #[test]
    fn options_from_model_flags() {
        let o = Options::from_model(None);
        assert!(o.run_itn && o.run_capitalization && !o.filter_profanities);
        let o = Options::from_model(Some("si;noitn;prof"));
        assert!(!o.run_itn && o.run_capitalization && o.filter_profanities);
    }

    #[test]
    fn segment_assembly_composes_partials_and_finals() {
        let mut s = SegmentState::default();
        assert_eq!(s.partial(Some(1), "hello", "wor"), "hello wor");
        assert_eq!(s.partial(Some(1), "world", ""), "hello world");
        // New segment keeps the previous one pending.
        assert_eq!(s.partial(Some(2), "how", "are"), "hello world how are");
        assert_eq!(s.fin(Some(1), "Hello world."), Some("Hello world. how are".into()));
        assert_eq!(s.fin(Some(2), "How are you?"), Some("Hello world. How are you?".into()));
        assert_eq!(s.best_text(), "Hello world. How are you?");
    }
}
