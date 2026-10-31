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
    /// Most batch requests the provider accepts at once (`None`: the
    /// session's own limit). Azure's fast transcription allows one.
    pub max_in_flight: Option<usize>,
    /// The provider records the microphone itself (system recognizers);
    /// the engine then opens no capture and pushes no frames.
    pub own_audio: bool,
    /// Batch answers carry word times that hold still from one reading of
    /// the same speech to the next (within tens of milliseconds). The
    /// agreed-words mode then votes per moment instead of per text.
    pub timed_words: bool,
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
    /// Batch mode: earliest cut at a gap between words (pauses cut as
    /// well, a timer at three times this length is the safety net); chunks
    /// are transcribed in parallel and released in order.
    pub chunk_interval: Duration,
    /// Batch mode: when a pause ends a thought that took several chunks,
    /// transcribe the whole thought again in one piece and replace the
    /// chunk texts (`Refined`). Quick first text, whole-recording quality
    /// a moment after the speaker stops.
    pub refine: bool,
    /// With `refine`: show the quick chunk texts only as `Interim` and type
    /// each thought once, when the second pass is in (or, if that fails,
    /// the chunk texts then). Off: type the chunks and replace them later.
    pub refine_preview: bool,
    /// Batch mode, agreed words only: the thought so far is transcribed
    /// every `step`; words that the last `windows` results agree on are
    /// typed and never changed, the rest at the pause. Replaces the
    /// quick chunks and the second pass.
    pub agreement: Option<Agreement>,
    /// Trained words (training mode, see [`ikk_text::learned`]): their
    /// heard forms are swapped in every result, and the ones resembling
    /// the thought so far go along as hints where the provider takes any.
    pub learned: Vec<ikk_text::LearnedWord>,
}

/// See [`SessionConfig::agreement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agreement {
    pub step: Duration,
    pub windows: usize,
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
            refine: false,
            refine_preview: false,
            agreement: None,
            learned: Vec::new(),
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

/// One recognised word with its position in the clip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start: Duration,
    pub end: Duration,
}

/// Result of a batch transcription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f32>,
    /// Word timestamps when the provider gives them (ElevenLabs, whisper-1);
    /// the agreed-words mode votes per moment with them.
    #[serde(default)]
    pub words: Vec<Word>,
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
    /// The whole thought transcribed in one piece once a pause ended it:
    /// replaces the last `segments` finals, which were its quick chunks.
    Refined { segments: usize, text: String },
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
    /// Map an HTTP status to a class. The provider's reason is logged
    /// (shortened, never the request), because the class alone cannot
    /// tell a wrong key from a key without the needed permission.
    pub fn from_status(status: u16, body: &str) -> Self {
        if status != 200 {
            let reason: String = body.chars().filter(|c| !c.is_control()).take(240).collect();
            tracing::warn!(status, reason, "provider refused the request");
        }
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

/// Whether `text` is written in the script of `language` (ISO-639-3).
/// Recognisers given a few seconds of unclear speech sometimes answer in
/// Japanese, Arabic or Georgian; such text is noise for a German
/// dictation. Languages this does not know about accept everything.
pub fn text_fits_language(text: &str, language: &str) -> bool {
    let latin = matches!(
        language,
        "deu"
            | "gsw"
            | "eng"
            | "fra"
            | "ita"
            | "spa"
            | "por"
            | "nld"
            | "swe"
            | "dan"
            | "nor"
            | "fin"
            | "pol"
            | "ces"
            | "hun"
            | "ron"
            | "tur"
    );
    if !latin {
        return true;
    }
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return true;
    }
    let foreign = letters
        .iter()
        .filter(|c| !matches!(**c, 'A'..='Z' | 'a'..='z' | '\u{00C0}'..='\u{024F}' | '\u{1E00}'..='\u{1EFF}'))
        .count();
    foreign * 2 < letters.len()
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
    corrector: Option<Arc<dyn TextCorrector>>,
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
    /// The grammar and spelling checker for live correction, if any.
    pub fn set_corrector(&mut self, c: Arc<dyn TextCorrector>) {
        self.corrector = Some(c);
    }
    pub fn corrector(&self) -> Option<Arc<dyn TextCorrector>> {
        self.corrector.clone()
    }
}

/// Corrects grammar and spelling of dictated text (LanguageTool and the
/// like). Text leaves the device, so it needs the user's consent like a
/// speech provider.
#[async_trait::async_trait]
pub trait TextCorrector: Send + Sync {
    /// Consent id, e.g. "languagetool".
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    /// `language`: the settings language (ISO-639-3) or empty for auto.
    /// `credential`: the user's account with the checker, if any (higher
    /// limits); without it the anonymous service is used.
    /// Returns the corrected text, or `None` when nothing needs changing.
    async fn correct(&self, text: &str, language: &str, credential: Option<&str>) -> Result<Option<String>, SttError>;
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
    fn foreign_script_does_not_fit_a_latin_language() {
        assert!(text_fits_language("Hallo, ich möchte schreiben.", "deu"));
        assert!(text_fits_language("Zürich – Genève, 12 %", "gsw"));
        assert!(!text_fits_language("なめるな。", "deu"));
        assert!(!text_fits_language("იეცილივისუს, და ბირხემფასასუ.", "deu"));
        assert!(!text_fits_language("عليك.", "eng"));
        assert!(text_fits_language("なめるな。", "jpn"));
        assert!(text_fits_language("123", "deu"));
    }

    #[test]
    fn clip_wav_header() {
        let clip = AudioClip {
            frames: vec![AudioFrame {
                samples: vec![0.0; 160],
                sample_rate_hz: 16_000,
                offset: Duration::ZERO,
                speech: None,
            }],
        };
        let wav = clip.to_wav(16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 320);
        assert_eq!(clip.to_pcm_s16le(48_000).len(), 960);
    }
}
