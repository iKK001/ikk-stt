//! The engine. Shells (Flutter via ikk-ffi, the CLI, the mobile hosts) send
//! [`EngineCommand`]s through an [`EngineHandle`] and receive
//! [`EngineEvent`]s. Nothing else is public API.
//!
//! The engine is an actor: one task owns the microphone capture, the STT
//! session, the VAD, the text pipeline and the output sink. Flow per
//! session:
//!
//! ```text
//! Start ─► consent? ─► probe target ─► open capture ─► open session ─► Listening
//! frames ─► VAD (pause ⇒ cut_segment) ─► session.push
//! session: Interim ─► event (preview)         Final ─► text pipeline ─► command? ─► sink/fallback
//! Stop ─► capture.stop ─► session.finish (grace for last finals) ─► Idle
//! ```
//!
//! Output has two modes. `Inject`: text goes straight to the focused field
//! of another app. `Fallback`: injection cannot work (see
//! [`TargetStatus`]) and the shell shows the dictation window; the engine
//! keeps the session text and can transfer it later. The engine decides
//! automatically — probe before start, verify while typing — so the window
//! only ever appears when it is needed (ARCHITECTURE §10).

pub mod config;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ikk_audio::vad::{VadEvent, VadMode, VoiceActivityDetector};
use ikk_audio::{Capture, CaptureConfig, PIPELINE_SAMPLE_RATE_HZ};
use ikk_commands::Intent;
use ikk_consent::{recipient_for, ConsentState, Decision, Recipient};
use ikk_output::live::LiveTyper;
use ikk_output::{OutputError, TargetStatus, TextSink, Verification};
use ikk_stt::{open_session, ProviderId, ProviderRegistry, Secret, SessionConfig, StreamingSession, TranscriptEvent};
use ikk_text::TextConfig;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub use ikk_output::AccessLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineState {
    Idle,
    AwaitingConsent,
    Listening,
    /// Stop requested, waiting for the last finals.
    Finishing,
    Error,
}

/// Where committed text currently goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputMode {
    /// Straight into the focused target.
    Inject,
    /// Into the engine's session text; shell shows the dictation window.
    Fallback,
}

/// Why the engine switched to the dictation window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FallbackReason {
    Elevated,
    NoEditableFocus,
    NoInjection(String),
    /// Typing was attempted and failed (retry limit, dropped keystrokes).
    InjectionFailed(String),
    /// Read-back showed the target did not receive the text.
    VerificationMismatch,
}

/// Everything a session needs; built from settings via [`config`].
#[derive(Debug, Clone)]
pub struct StartConfig {
    pub session: SessionConfig,
    pub capture: CaptureConfig,
    pub text: TextConfig,
    pub vad_mode: VadMode,
    /// Silence that closes a segment locally (cut_segment to the provider).
    pub pause: Duration,
    /// Locale for spoken commands ("de", "de-CH", "en"); empty = no commands.
    pub command_locale: String,
    /// Typing flow: ms per character when draining committed text into the
    /// target (even writing flow). 0 = type each final at once.
    pub flow_ms: u32,
}

#[derive(Debug)]
pub enum EngineCommand {
    Start(Box<StartConfig>),
    Stop,
    CutSegment,
    GrantConsent(String),
    DenyConsent(String),
    /// Shell loaded the persisted consent decisions at startup.
    LoadConsent(ConsentState),
    /// Shell loaded a key from the keychain; the engine injects it into
    /// sessions for that provider. Keys never leave the engine.
    SetCredential(ProviderId, Secret),
    /// Fallback mode: user focused a target and pressed "insert"; the engine
    /// re-probes and types the session text if it now can.
    TransferToTarget,
    /// Fallback mode: shell took the text (copy/share); clear it.
    ClearSessionText,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineEvent {
    /// Input level in dBFS for a meter (≤ 10 per second).
    Level(f32),
    State(EngineState),
    Speech(bool),
    Transcript(TranscriptEvent),
    ConsentRequired(Recipient),
    /// A consent decision changed; the shell persists the state.
    ConsentChanged(ConsentState),
    /// Shell must ask the OS permission (macOS Accessibility, Android IME).
    PermissionRequired,
    /// Shell shows the dictation window with the text dictated so far.
    FallbackRequired {
        reason: FallbackReason,
        carried_text: String,
    },
    /// Fallback text was typed into a target; shell may hide the window.
    Transferred(usize),
    /// Characters handed to the sink.
    Typed(usize),
    /// Mobile: the host (keyboard extension / IME) must insert this text.
    HostText(String),
    /// A spoken command was recognised and executed.
    Command(Intent),
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("unknown provider {0:?}")]
    UnknownProvider(ProviderId),
    #[error("engine is busy")]
    Busy,
    #[error("refusing to type into a password field")]
    PasswordField,
    #[error("target not injectable: {0:?}")]
    NotInjectable(TargetStatus),
    #[error("audio: {0}")]
    Audio(#[from] ikk_audio::AudioError),
    #[error("stt: {0}")]
    Stt(#[from] ikk_stt::SttError),
}

/// Opens a microphone capture; injected so tests run without hardware.
pub type CaptureOpener = Box<dyn Fn(&CaptureConfig) -> Result<Box<dyn Capture>, ikk_audio::AudioError> + Send>;

/// What shells hold: send commands, receive events.
pub struct EngineHandle {
    pub commands: mpsc::Sender<EngineCommand>,
    pub events: mpsc::Receiver<EngineEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl EngineHandle {
    pub async fn send(&self, cmd: EngineCommand) -> bool {
        self.commands.send(cmd).await.is_ok()
    }
    pub async fn shutdown(self) {
        let _ = self.commands.send(EngineCommand::Shutdown).await;
        let _ = self.task.await;
    }
}

const LEVEL_INTERVAL: Duration = Duration::from_millis(100);
const FINISH_GRACE: Duration = Duration::from_secs(8);

pub struct Engine {
    registry: ProviderRegistry,
    consent: ConsentState,
    sink: Box<dyn TextSink>,
    open_capture: CaptureOpener,
    credentials: HashMap<ProviderId, Secret>,
    state: EngineState,
    output_mode: OutputMode,
    session_text: String,
    events: mpsc::Sender<EngineEvent>,
    // Live session parts.
    capture: Option<Box<dyn Capture>>,
    session: Option<Box<dyn StreamingSession>>,
    vad: Option<VoiceActivityDetector>,
    text_cfg: TextConfig,
    command_locale: String,
    last_level: Instant,
    /// Owner of the physical target text while injecting.
    live: LiveTyper,
}

impl Engine {
    /// Spawns the engine task with the real microphone backend.
    pub fn spawn(registry: ProviderRegistry, sink: Box<dyn TextSink>) -> EngineHandle {
        Self::spawn_with(registry, sink, Box::new(ikk_audio::open_capture))
    }

    pub fn spawn_with(
        registry: ProviderRegistry,
        sink: Box<dyn TextSink>,
        open_capture: CaptureOpener,
    ) -> EngineHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (ev_tx, ev_rx) = mpsc::channel(256);
        let engine = Engine {
            registry,
            consent: ConsentState::default(),
            sink,
            open_capture,
            credentials: HashMap::new(),
            state: EngineState::Idle,
            output_mode: OutputMode::Inject,
            session_text: String::new(),
            events: ev_tx,
            capture: None,
            session: None,
            vad: None,
            text_cfg: TextConfig::default(),
            command_locale: String::new(),
            last_level: Instant::now(),
            live: LiveTyper::default(),
        };
        let task = tokio::spawn(engine.run(cmd_rx));
        EngineHandle {
            commands: cmd_tx,
            events: ev_rx,
            task,
        }
    }

    pub fn state(&self) -> EngineState {
        self.state
    }
    pub fn output_mode(&self) -> OutputMode {
        self.output_mode
    }
    pub fn session_text(&self) -> &str {
        &self.session_text
    }

    async fn run(mut self, mut commands: mpsc::Receiver<EngineCommand>) {
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    None | Some(EngineCommand::Shutdown) => {
                        self.stop().await;
                        break;
                    }
                    Some(cmd) => {
                        if let Err(e) = self.handle(cmd).await {
                            warn!(%e, "engine command failed");
                            let _ = self.events.send(EngineEvent::Error(e.to_string())).await;
                        }
                    }
                },
                frame = async { self.capture.as_mut().unwrap().frames().recv().await }, if self.capture.is_some() => match frame {
                    Some(frame) => self.on_frame(frame).await,
                    None => {
                        warn!("capture ended unexpectedly");
                        self.stop().await;
                    }
                },
                ev = async { self.session.as_mut().unwrap().events().recv().await }, if self.session.is_some() => match ev {
                    Some(ev) => self.on_transcript(ev).await,
                    None => self.stop().await,
                },
                _ = tokio::time::sleep(Duration::from_millis(self.live.flow_rate_ms().max(1) as u64)), if self.live.flow_has_pending() => {
                    self.flow_tick().await;
                }
            }
        }
    }

    async fn handle(&mut self, cmd: EngineCommand) -> Result<(), EngineError> {
        match cmd {
            EngineCommand::Start(cfg) => self.start(*cfg).await,
            EngineCommand::Stop => {
                self.stop().await;
                Ok(())
            }
            EngineCommand::CutSegment => {
                if let Some(s) = self.session.as_mut() {
                    s.cut_segment().await?;
                }
                Ok(())
            }
            EngineCommand::GrantConsent(id) => {
                let r = self.recipient(&id);
                self.consent.allow(r);
                let _ = self
                    .events
                    .send(EngineEvent::ConsentChanged(self.consent.clone()))
                    .await;
                if self.state == EngineState::AwaitingConsent {
                    self.set_state(EngineState::Idle).await;
                }
                Ok(())
            }
            EngineCommand::DenyConsent(id) => {
                let r = self.recipient(&id);
                self.consent.deny(r);
                let _ = self
                    .events
                    .send(EngineEvent::ConsentChanged(self.consent.clone()))
                    .await;
                self.set_state(EngineState::Idle).await;
                Ok(())
            }
            EngineCommand::LoadConsent(state) => {
                self.consent = state;
                Ok(())
            }
            EngineCommand::SetCredential(p, s) => {
                self.credentials.insert(p, s);
                Ok(())
            }
            EngineCommand::TransferToTarget => self.transfer().await,
            EngineCommand::ClearSessionText => {
                self.session_text.clear();
                Ok(())
            }
            EngineCommand::Shutdown => Ok(()),
        }
    }

    async fn start(&mut self, mut cfg: StartConfig) -> Result<(), EngineError> {
        if self.session.is_some() {
            return Err(EngineError::Busy);
        }
        let provider = self
            .registry
            .get(&cfg.session.provider)
            .ok_or_else(|| EngineError::UnknownProvider(cfg.session.provider.clone()))?;
        let caps = provider.capabilities();
        if caps.requires_network && self.consent.decision(&cfg.session.provider.0) != Decision::Allowed {
            self.set_state(EngineState::AwaitingConsent).await;
            let r = recipient_for(&cfg.session.provider.0, provider.display_name());
            let _ = self.events.send(EngineEvent::ConsentRequired(r)).await;
            return Ok(());
        }
        if cfg.session.credential.is_none() {
            cfg.session.credential = self.credentials.get(&cfg.session.provider).cloned();
        }
        self.session_text.clear();
        self.live = LiveTyper::new(Box::new(ikk_output::guard::AllowAllGuard), cfg.flow_ms);
        self.decide_output_mode().await?;

        let capture = (self.open_capture)(&cfg.capture)?;
        let session = match open_session(provider, &cfg.session).await {
            Ok(s) => s,
            Err(e) => {
                capture.stop();
                return Err(e.into());
            }
        };
        let mut vad = VoiceActivityDetector::new(cfg.vad_mode, PIPELINE_SAMPLE_RATE_HZ);
        vad.set_pause_duration_ms(cfg.pause.as_millis().max(100) as u32);
        self.capture = Some(capture);
        self.session = Some(session);
        self.vad = Some(vad);
        self.text_cfg = cfg.text;
        self.command_locale = cfg.command_locale;
        self.set_state(EngineState::Listening).await;
        info!(provider = %cfg.session.provider.0, mode = ?self.output_mode, "session started");
        Ok(())
    }

    async fn on_frame(&mut self, frame: ikk_audio::AudioFrame) {
        if self.last_level.elapsed() >= LEVEL_INTERVAL {
            self.last_level = Instant::now();
            let _ = self.events.try_send(EngineEvent::Level(frame.level_dbfs()));
        }
        let ended = match self.vad.as_mut().map(|v| v.process(&frame.samples)) {
            Some(Some(VadEvent::SpeechStarted)) => {
                let _ = self.events.try_send(EngineEvent::Speech(true));
                false
            }
            Some(Some(VadEvent::SpeechEnded)) => {
                let _ = self.events.try_send(EngineEvent::Speech(false));
                true
            }
            _ => false,
        };
        if let Some(s) = self.session.as_mut() {
            if let Err(e) = s.push(frame).await {
                warn!(%e, "session push failed");
                let _ = self.events.send(EngineEvent::Error(e.to_string())).await;
                self.stop().await;
                return;
            }
            if ended {
                let _ = s.cut_segment().await;
            }
        }
    }

    async fn on_transcript(&mut self, ev: TranscriptEvent) {
        match ev {
            TranscriptEvent::Final(text) => {
                let processed = ikk_text::process(&self.text_cfg, &text);
                if processed.is_empty() {
                    return;
                }
                if !self.command_locale.is_empty() {
                    if let Some(intent) = ikk_commands::match_command(&processed, &self.command_locale) {
                        let _ = self.events.send(EngineEvent::Command(intent.clone())).await;
                        self.execute(intent).await;
                        return;
                    }
                }
                let with_space = if self.session_text.is_empty() {
                    processed
                } else {
                    format!(" {processed}")
                };
                let _ = self.emit_text(&with_space).await;
                let _ = self
                    .events
                    .send(EngineEvent::Transcript(TranscriptEvent::Final(
                        with_space.trim().to_string(),
                    )))
                    .await;
            }
            TranscriptEvent::Error(class) => {
                let retryable = class.is_retryable();
                let _ = self
                    .events
                    .send(EngineEvent::Transcript(TranscriptEvent::Error(class)))
                    .await;
                if !retryable {
                    self.stop().await;
                    self.set_state(EngineState::Error).await;
                }
            }
            TranscriptEvent::Closed => {
                if self.state == EngineState::Listening {
                    self.stop().await;
                }
            }
            TranscriptEvent::Replace(text) => {
                let processed = ikk_text::process(&self.text_cfg, &text);
                self.replace_all(&processed).await;
                let _ = self
                    .events
                    .send(EngineEvent::Transcript(TranscriptEvent::Replace(processed)))
                    .await;
            }
            other => {
                let _ = self.events.send(EngineEvent::Transcript(other)).await;
            }
        }
    }

    /// Drain one step of the typing queue into the target.
    async fn flow_tick(&mut self) {
        if self.output_mode != OutputMode::Inject {
            return;
        }
        match self.live.flow_tick(1, &mut *self.sink) {
            Ok(n) if n > 0 => {
                let _ = self.events.try_send(EngineEvent::Typed(n));
            }
            Ok(_) => {}
            Err(e) => {
                self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                    .await
            }
        }
    }

    /// Full-text replacement (batch pass). Fallback mode just swaps the text.
    async fn replace_all(&mut self, text: &str) {
        self.session_text = text.to_string();
        if self.output_mode != OutputMode::Inject {
            return;
        }
        if let Err(e) = self.live.replace_all(text, &mut *self.sink) {
            self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                .await;
        }
    }

    async fn execute(&mut self, intent: Intent) {
        match intent {
            Intent::StopDictation => self.stop().await,
            Intent::NewLine => {
                let _ = self.emit_text("\n").await;
            }
            Intent::NewParagraph => {
                let _ = self.emit_text("\n\n").await;
            }
            Intent::DeleteLastWord => {
                let n = self
                    .session_text
                    .trim_end()
                    .rsplit(' ')
                    .next()
                    .map(|w| w.chars().count())
                    .unwrap_or(0);
                if n > 0 && self.output_mode == OutputMode::Inject {
                    let _ = self.sink.backspace(n + 1);
                }
                let cut = self.session_text.trim_end().rfind(' ').unwrap_or(0);
                self.session_text.truncate(cut);
            }
            Intent::DeleteLastSentence | Intent::Undo => {
                warn!(?intent, "command not implemented in v1");
            }
        }
    }

    async fn stop(&mut self) {
        if self.capture.is_none() && self.session.is_none() {
            if self.state != EngineState::Idle && self.state != EngineState::Error {
                self.set_state(EngineState::Idle).await;
            }
            return;
        }
        self.set_state(EngineState::Finishing).await;
        if let Some(c) = self.capture.take() {
            c.stop();
        }
        if self.output_mode == OutputMode::Inject {
            if let Err(e) = self.live.flush(&mut *self.sink) {
                self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                    .await;
            }
        }
        self.vad = None;
        if let Some(mut session) = self.session.take() {
            let _ = tokio::time::timeout(FINISH_GRACE, session.finish()).await;
            // Drain what finish produced (last finals, batch tail) until Closed.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            let mut finals = Vec::new();
            while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, session.events().recv()).await {
                match ev {
                    TranscriptEvent::Final(t) => finals.push(ikk_text::process(&self.text_cfg, &t)),
                    TranscriptEvent::Closed => break,
                    other => {
                        let _ = self.events.send(EngineEvent::Transcript(other)).await;
                    }
                }
            }
            for t in finals {
                if !t.is_empty() {
                    let with_space = if self.session_text.is_empty() {
                        t
                    } else {
                        format!(" {t}")
                    };
                    let _ = self.emit_text(&with_space).await;
                    let _ = self
                        .events
                        .send(EngineEvent::Transcript(TranscriptEvent::Final(
                            with_space.trim().to_string(),
                        )))
                        .await;
                }
            }
        }
        self.set_state(EngineState::Idle).await;
    }

    async fn transfer(&mut self) -> Result<(), EngineError> {
        match self.sink.probe() {
            TargetStatus::Injectable(_) | TargetStatus::Unknown => {}
            TargetStatus::PasswordField => return Err(EngineError::PasswordField),
            other => return Err(EngineError::NotInjectable(other)),
        }
        let text = std::mem::take(&mut self.session_text);
        match self.sink.type_text(&text) {
            Ok(()) => {
                let _ = self.events.send(EngineEvent::Transferred(text.chars().count())).await;
                Ok(())
            }
            Err(e) => {
                self.session_text = text;
                Err(EngineError::NotInjectable(TargetStatus::NoInjection(e.to_string())))
            }
        }
    }

    /// Probe the target and pick the output mode for this session.
    async fn decide_output_mode(&mut self) -> Result<(), EngineError> {
        let reason = match self.sink.probe() {
            TargetStatus::Injectable(_) | TargetStatus::Unknown => {
                self.output_mode = OutputMode::Inject;
                return Ok(());
            }
            TargetStatus::PasswordField => return Err(EngineError::PasswordField),
            TargetStatus::PermissionMissing => {
                let _ = self.events.send(EngineEvent::PermissionRequired).await;
                return Err(EngineError::NotInjectable(TargetStatus::PermissionMissing));
            }
            TargetStatus::Elevated => FallbackReason::Elevated,
            TargetStatus::NoEditableFocus => FallbackReason::NoEditableFocus,
            TargetStatus::NoInjection(why) => FallbackReason::NoInjection(why),
        };
        self.switch_to_fallback(reason).await;
        Ok(())
    }

    async fn switch_to_fallback(&mut self, reason: FallbackReason) {
        self.output_mode = OutputMode::Fallback;
        let _ = self
            .events
            .send(EngineEvent::FallbackRequired {
                reason,
                carried_text: self.session_text.clone(),
            })
            .await;
    }

    /// Hands a committed fragment to the current output. In `Inject` mode the
    /// live typer appends it (immediately or via the typing queue); a failure
    /// or a verification mismatch switches to fallback and keeps the text.
    async fn emit_text(&mut self, text: &str) -> Result<usize, OutputError> {
        self.session_text.push_str(text);
        let n = text.chars().count();
        if self.output_mode == OutputMode::Fallback {
            return Ok(n);
        }
        let segment = text.trim_start();
        if self.sink.access_level() == AccessLevel::Host {
            let _ = self.events.send(EngineEvent::HostText(text.to_string())).await;
        }
        match self.live.append(segment, &mut *self.sink) {
            Ok(_) => {
                if self.live.flow_rate_ms() == 0 {
                    if self.sink.verify(segment) == Verification::Mismatch {
                        self.switch_to_fallback(FallbackReason::VerificationMismatch).await;
                        return Ok(n);
                    }
                    let _ = self.events.send(EngineEvent::Typed(n)).await;
                }
                Ok(n)
            }
            Err(e) => {
                self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                    .await;
                Ok(n)
            }
        }
    }

    fn recipient(&self, provider_id: &str) -> Recipient {
        let name = self
            .registry
            .get(&ProviderId(provider_id.to_string()))
            .map(|p| p.display_name())
            .unwrap_or(provider_id);
        recipient_for(provider_id, name)
    }

    async fn set_state(&mut self, s: EngineState) {
        if self.state != s {
            self.state = s;
            let _ = self.events.send(EngineEvent::State(s)).await;
        }
    }
}

#[cfg(test)]
mod tests;
