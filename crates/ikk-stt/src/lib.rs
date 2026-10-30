//! Engine-independent speech-to-text model.
//!
//! This crate defines *what* a provider
//! is and *what* a session emits. It never knows a concrete provider and
//! never touches an audio device.

pub mod chunked;
pub mod lang;
pub mod merge;

use std::sync::Arc;
use std::time::Duration;

use ikk_audio::AudioFrame;
use serde::{Deserialize, Serialize};

/// Stable identifier of a provider, e.g. `"deepgram"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

/// An API key or token. Never serialised, never logged.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// What a provider can do. Drives routing and UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub batch: bool,
    pub streaming: bool,
    /// Streaming delivers provisional text before a segment is final.
    pub interim: bool,
    /// Streaming sessions may run a higher-quality batch pass on stop and
    /// append the tail the stream missed (Deepgram pattern).
    pub batch_replacement: bool,
    /// Sample rate the provider wants on the wire; `None` = pipeline rate.
    pub required_sample_rate_hz: Option<u32>,
    /// ISO-639-3 codes; empty = auto-detect only.
    pub languages: Vec<String>,
    /// Audio leaves the device. Always true for cloud providers.
    pub requires_network: bool,
    /// The provider records the microphone itself (system recognizers);
    /// the engine then opens no capture and pushes no frames.
    pub own_audio: bool,
}

/// Something the user must set up outside the app before the provider
/// works, e.g. a system privacy switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    /// What is missing and why, for the user.
    pub message: String,
    /// Where to change it (a settings URI the shell can open), or empty.
    pub settings_url: String,
}

/// Streaming vs chunked batch, when a provider supports both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TransferMode {
    /// Live socket, interim text, no wait at stop.
    #[default]
    Streaming,
    /// Chunked upload, higher accuracy, 1–3 s wait per chunk.
    Batch,
}

/// Per-session settings handed to a provider. The engine builds this from
/// the user's settings; providers ignore what they do not support.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub provider: ProviderId,
    pub credential: Option<Secret>,
    /// ISO-639-3 or empty for auto-detect.
    pub language: String,
    pub model: Option<String>,
    pub transfer: TransferMode,
    /// Bias terms with boost 1..=10 (Deepgram keywords, OpenAI prompt).
    pub keywords: Vec<(String, u8)>,
    /// Provider-side punctuation / number formatting.
    pub smart_format: bool,
    /// Silence that closes a segment server-side (Deepgram `endpointing`).
    pub endpointing: Duration,
    /// Provider-side utterance end signal (Deepgram `utterance_end_ms`).
    pub utterance_end: Duration,
    /// Batch mode: a chunk longer than this is cut at the next gap between
    /// words; chunks are transcribed in parallel and released in order.
    pub chunk_interval: Duration,
}

impl SessionConfig {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: ProviderId(provider.into()),
            credential: None,
            language: String::new(),
            model: None,
            transfer: TransferMode::Streaming,
            keywords: Vec::new(),
            smart_format: true,
            endpointing: Duration::from_millis(800),
            utterance_end: Duration::from_millis(1000),
            chunk_interval: Duration::from_millis(2000),
        }
    }
}

/// A complete recording for batch transcription.
#[derive(Debug, Clone)]
pub struct AudioClip {
    pub frames: Vec<AudioFrame>,
}

impl AudioClip {
    pub fn sample_rate_hz(&self) -> u32 {
        self.frames
            .first()
            .map(|f| f.sample_rate_hz)
            .unwrap_or(ikk_audio::PIPELINE_SAMPLE_RATE_HZ)
    }
    pub fn samples(&self) -> Vec<f32> {
        self.frames.iter().flat_map(|f| f.samples.iter().copied()).collect()
    }
    pub fn duration(&self) -> Duration {
        self.frames.iter().map(|f| f.duration()).sum()
    }
    /// 16-bit PCM at `target_hz` (resampled if needed), the wire format most
    /// providers accept as `linear16` / `audio/wav`.
    pub fn to_pcm_s16le(&self, target_hz: u32) -> Vec<u8> {
        let samples = self.samples();
        let samples = ikk_audio::dsp::resample_linear(&samples, self.sample_rate_hz(), target_hz);
        let mut out = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        out
    }
    /// WAV container around [`Self::to_pcm_s16le`].
    pub fn to_wav(&self, target_hz: u32) -> Vec<u8> {
        let pcm = self.to_pcm_s16le(target_hz);
        let mut w = Vec::with_capacity(44 + pcm.len());
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&target_hz.to_le_bytes());
        w.extend_from_slice(&(target_hz * 2).to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        w.extend_from_slice(&pcm);
        w
    }
}

/// Result of a batch transcription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f32>,
}

/// What a session emits over time: finals, interims, segment
/// refinements and batch replacements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptEvent {
    /// Provisional text; replaces the previous interim.
    Interim(String),
    /// Committed fragment; append.
    Final(String),
    /// Better text for the current rolling segment; replaces it.
    SegmentRefine(String),
    /// Full-audio batch pass; replaces everything emitted so far.
    Replace(String),
    /// Provider or transport failure.
    Error(ErrorClass),
    /// Provider closed the session.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorClass {
    /// Wrong or expired credential; do not retry, tell the user.
    Unauthorized,
    /// Quota or rate limit; back off, maybe fall back.
    Throttled,
    /// Network gone; retry with backoff.
    Transport,
    /// Provider rejected the audio or config.
    BadRequest(String),
    Other(String),
}

impl ErrorClass {
    /// Map an HTTP status to a class.
    pub fn from_status(status: u16, body: &str) -> Self {
        match status {
            401 | 403 => Self::Unauthorized,
            429 | 402 => Self::Throttled,
            400 | 422 => Self::BadRequest(body.chars().take(200).collect()),
            s if s >= 500 => Self::Transport,
            _ => Self::Other(format!("HTTP {status}: {}", body.chars().take(200).collect::<String>())),
        }
    }

    /// Classify a free-text transport/provider error message.
    pub fn from_message(msg: &str) -> Self {
        let m = msg.to_lowercase();
        if [
            "401",
            "403",
            "unauthorized",
            "forbidden",
            "invalid api key",
            "invalid_api_key",
        ]
        .iter()
        .any(|k| m.contains(k))
        {
            Self::Unauthorized
        } else if m.contains("429") || m.contains("rate limit") || m.contains("quota") {
            Self::Throttled
        } else if [
            "no longer supported",
            "deprecated",
            "invalid value",
            "invalid_request",
            "unknown parameter",
            "invalid model",
            "model_not_found",
        ]
        .iter()
        .any(|k| m.contains(k))
        {
            Self::BadRequest(msg.chars().take(200).collect())
        } else {
            Self::Transport
        }
    }

    /// Whether retrying the same request can succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Throttled | Self::Transport)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("provider error: {0:?}")]
    Provider(ErrorClass),
    #[error("provider does not support this mode")]
    Unsupported,
    #[error("no credential for provider")]
    MissingCredential,
    /// A credential is set but not in the provider's format.
    #[error("credential not usable: {0}")]
    BadCredential(&'static str),
}

/// A live session. Audio goes in, events come out. Implemented by
/// true streaming adapters and by [`chunked::ChunkedBatchSession`].
#[async_trait::async_trait]
pub trait StreamingSession: Send {
    async fn push(&mut self, frame: AudioFrame) -> Result<(), SttError>;
    /// Close the segment spoken so far and deliver it as `Final` now.
    async fn cut_segment(&mut self) -> Result<(), SttError>;
    /// Flush and close. Remaining events (last finals, batch tail, `Closed`)
    /// are still delivered through [`Self::events`] afterwards; callers drain
    /// until `Closed`.
    async fn finish(&mut self) -> Result<(), SttError>;
    fn events(&mut self) -> &mut tokio::sync::mpsc::Receiver<TranscriptEvent>;
}

/// A speech-to-text provider. Implemented in `ikk-providers`,
/// `ikk-stt-local` and any additional engine crate.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;

    async fn transcribe(&self, audio: AudioClip, cfg: &SessionConfig) -> Result<Transcript, SttError>;

    /// A true streaming session. Providers without streaming return
    /// `Unsupported`; use [`open_session`] to get chunked batch instead.
    async fn stream(&self, cfg: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError>;

    /// What the user still has to set up before this provider works, if
    /// anything. Checked after consent, before a session opens.
    fn setup_needed(&self) -> Option<Setup> {
        None
    }
}

/// Opens the best session for the config: streaming if requested and
/// supported, otherwise chunked batch over `transcribe`.
pub async fn open_session(
    provider: Arc<dyn Provider>,
    cfg: &SessionConfig,
) -> Result<Box<dyn StreamingSession>, SttError> {
    let caps = provider.capabilities();
    if caps.streaming && (cfg.transfer == TransferMode::Streaming || !caps.batch) {
        return provider.stream(cfg).await;
    }
    if caps.batch {
        return Ok(Box::new(chunked::ChunkedBatchSession::new(provider, cfg.clone())));
    }
    Err(SttError::Unsupported)
}

/// Routing decision for a session: which provider, and what to fall back to.
#[derive(Debug, Clone)]
pub struct RoutingPolicy {
    pub primary: ProviderId,
    pub fallback: Option<ProviderId>,
}

/// Registry of available providers; the engine selects from it.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, p: Box<dyn Provider>) {
        self.providers.push(Arc::from(p));
    }
    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| &p.id() == id).cloned()
    }
    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers.iter().map(|p| p.id()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_prints() {
        let s = Secret::new("sk-123");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(s.expose(), "sk-123");
    }

    #[test]
    fn error_classes() {
        assert_eq!(ErrorClass::from_status(401, ""), ErrorClass::Unauthorized);
        assert_eq!(ErrorClass::from_status(429, ""), ErrorClass::Throttled);
        assert!(matches!(ErrorClass::from_status(400, "bad"), ErrorClass::BadRequest(_)));
        assert_eq!(ErrorClass::from_status(503, ""), ErrorClass::Transport);
        assert_eq!(
            ErrorClass::from_message("ws: 401 Unauthorized"),
            ErrorClass::Unauthorized
        );
        assert!(ErrorClass::from_message("connection reset").is_retryable());
        assert!(!ErrorClass::from_message("invalid api key").is_retryable());
    }

    #[test]
    fn clip_wav_header() {
        let clip = AudioClip {
            frames: vec![AudioFrame {
                samples: vec![0.0; 160],
                sample_rate_hz: 16_000,
                offset: Duration::ZERO,
            }],
        };
        let wav = clip.to_wav(16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 320);
        assert_eq!(clip.to_pcm_s16le(48_000).len(), 960);
    }
}
