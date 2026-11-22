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

mod commands;
pub mod config;
pub mod training;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ikk_audio::vad::{VadEvent, VadMode, VoiceActivityDetector};
use ikk_audio::{Capture, CaptureConfig, PIPELINE_SAMPLE_RATE_HZ};
use ikk_commands::Intent;
use ikk_consent::{recipient_for, ConsentState, Decision, Recipient};
use ikk_output::live::LiveTyper;
use ikk_output::{OutputError, TargetStatus, TextSink, Verification};
use ikk_stt::{
    open_session, ProviderId, ProviderRegistry, Secret, SessionConfig, StreamingSession, TextCorrector, TranscriptEvent,
};
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

/// Counters for the periodic diagnosis line (where time is lost: audio,
/// provider or typing).
#[derive(Debug)]
struct Diag {
    since: Instant,
    frames: u32,
    audio_ms: u32,
    interims: u32,
    finals: u32,
    final_chars: usize,
    typed: usize,
    /// Intervals in a row with nothing to report.
    quiet: u32,
}

impl Default for Diag {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            audio_ms: 0,
            interims: 0,
            finals: 0,
            final_chars: 0,
            typed: 0,
            quiet: 0,
        }
    }
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
    /// Grammar and spelling corrected live by the chosen corrector.
    pub correction: bool,
    /// Who corrects (settings `text.corrector`); a corrector without a key
    /// is replaced by the on-device one.
    pub corrector: String,
    /// With `correction`: write the corrected text over the typed one.
    /// Otherwise it is only reported ([`EngineEvent::Suggestion`]).
    pub correction_rewrites: bool,
}

/// The correction button's provider call gets this long; dictated words
/// wait meanwhile, so it is short.
const RANGE_CORRECTION_TIMEOUT: Duration = Duration::from_secs(25);

/// What the correction button corrects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CorrectionRange {
    /// From the caret to the end of the text.
    CursorToEnd,
    WholeText,
    /// From one click position to the next. The shell captures the two
    /// clicks and makes the selection (see `ikk_overlay::capture_clicks`),
    /// then asks for [`CorrectionRange::Selection`].
    ClickToClick,
    /// The word under one click. The shell captures the click, selects the
    /// word with a double-click and asks for [`CorrectionRange::Selection`].
    Word,
    /// The text is selected already: copy it as it is.
    Selection,
}

impl CorrectionRange {
    /// From the settings value (`text.correction_range`).
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "whole_text" => CorrectionRange::WholeText,
            "click_to_click" => CorrectionRange::ClickToClick,
            // Word suggestions name their word by one click as well.
            "single_word" | "word_suggestions" => CorrectionRange::Word,
            _ => CorrectionRange::CursorToEnd,
        }
    }
}

/// Word suggestions: how much of the text in front of the marked part and
/// after it goes to the model, in characters.
const AROUND_BEFORE: usize = 400;
const AROUND_AFTER: usize = 200;

/// An on-demand correction (the correction button next to the microphone).
#[derive(Debug, Clone)]
pub struct CorrectRequest {
    /// The host (a phone's accessibility service, a keyboard) already
    /// selected the text and hands it over; the answer goes back as
    /// [`EngineEvent::HostReplace`] instead of a paste.
    pub host_text: Option<String>,
    /// Word suggestions through a host: the text in front of the marked
    /// part and after it, as far as the host can read it.
    pub host_around: Option<(String, String)>,
    pub range: CorrectionRange,
    /// Corrector id ("languagetool", "openai", "anthropic").
    pub corrector: String,
    /// Model override for text-model correctors; empty = default.
    pub model: String,
    /// Settings language (ISO-639-3), empty = auto.
    pub language: String,
}

/// Live correction of a session: one request in flight at a time, the
/// answer applied to the newest segments it covered.
struct Correction {
    /// Settings language (ISO-639-3), empty = auto.
    language: String,
    /// Answers from an earlier session are dropped.
    generation: u64,
    in_flight: bool,
    /// New text arrived while a request was out.
    dirty: bool,
    /// Text already checked; no second request for the same text.
    checked: String,
    /// Write corrections over the typed text; otherwise only suggest.
    rewrites: bool,
    asked_consent: bool,
    /// After a 429, corrections rest until here.
    paused_until: Option<Instant>,
    last_request: Option<Instant>,
    /// A delayed retry is already on its way.
    tick_pending: bool,
}

/// Replace by voice: the words spoken for a selection.
struct ReplaceByVoice {
    original: String,
    /// The range begins a sentence (`None`: cannot be told).
    at_start: Option<bool>,
    prior_clipboard: Option<String>,
    collected: String,
    /// The engine opened the session for this and closes it afterwards.
    own_session: bool,
    started: Instant,
    last_final: Option<Instant>,
    /// The voice detector hears speech right now.
    speech: bool,
    /// When the voice detector last heard the speech end: the reading that
    /// covers the end of the thought arrives after this moment.
    speech_ended_at: Option<Instant>,
}

/// Replace by voice: the thought counts as finished this long after the
/// last words came back with no speech going on (batch answers for the
/// tail of a thought take a moment to arrive).
const REPLACE_SETTLE: Duration = Duration::from_millis(2000);
/// Nothing heard for this long: the selection is left as it was.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(20);
/// After the speech ended, the reading that covers its end is waited for
/// this long at most; then what came goes in.
const REPLACE_LAST_READING: Duration = Duration::from_secs(8);

/// Rest after the checker said "too many requests".
const CORRECTION_PAUSE: Duration = Duration::from_secs(60);
/// The checker's limits are per minute: at most one request every few
/// seconds.
const CORRECTION_MIN_GAP: Duration = Duration::from_secs(4);
/// Only the newest segments are checked; earlier text had its turn.
const CORRECTION_WINDOW: usize = 3;

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
    /// Offline voice commands on (trained words + what to start with) or off.
    SetVoiceCommands(Option<Box<VoiceSetup>>),
    /// The corrector answered for the segments `from` ..= `through`.
    ApplyCorrection {
        generation: u64,
        from: ikk_output::live::SegmentId,
        through: ikk_output::live::SegmentId,
        result: Result<Option<String>, String>,
        throttled: bool,
    },
    /// The minimum gap between correction requests has passed.
    CorrectionTick(u64),
    /// The correction button: correct the chosen range in the focused app.
    CorrectRange(CorrectRequest),
    /// Replace by voice: select `range` in the app in front (or take the
    /// selection as it is), listen for one thought and paste it over the
    /// selection. `start`: how to open a session when none is running.
    ReplaceByVoice {
        range: CorrectionRange,
        start: Option<Box<StartConfig>>,
        /// See [`CorrectRequest::host_text`].
        host_text: Option<String>,
    },
    /// Internal: the spoken replacement has settled (or timed out).
    ReplaceTick(u64),
    /// Word suggestions: read the selection in the app in front and ask
    /// the text model what else may have been said there.
    SuggestWords(CorrectRequest),
    /// Internal: the text model answered with candidates.
    WordChoicesReady {
        original: String,
        bounds: Option<(f64, f64, f64, f64)>,
        result: Result<Vec<String>, String>,
    },
    /// The user picked a candidate (see [`EngineEvent::WordChoices`]): it
    /// goes over the selection, which is still the marked part.
    ApplyChoice(String),
    /// The corrector answered for a range correction.
    ApplyRangeCorrection {
        original: String,
        prior_clipboard: Option<String>,
        result: Result<Option<String>, String>,
    },
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
    /// Live correction without rewriting: how the checker would write the
    /// text so far. Shown by the shell, never typed.
    Suggestion(String),
    /// The correction button finished: the range was replaced (`changed`)
    /// or needed no change; `chars` of text were looked at.
    /// A host that selected the text itself: put `text` over its selection
    /// (the answer of a correction or a replacement by voice).
    HostReplace {
        text: String,
    },
    /// Replace by voice: the selection is read and the engine listens.
    ReplaceListening {
        original: String,
    },
    /// Replace by voice is over: `to` was pasted over `from` (`changed`),
    /// or nothing was heard and the selection was left as it was.
    Replaced {
        from: String,
        to: String,
        changed: bool,
    },
    Corrected {
        changed: bool,
        chars: usize,
    },
    /// Word suggestions: what else may have been said in place of the
    /// marked `word`, the most likely first; empty when the model found
    /// nothing. The marked part is still selected: the shell offers the
    /// candidates and answers with [`EngineCommand::ApplyChoice`].
    WordChoices {
        word: String,
        choices: Vec<String>,
        /// Where the marked part stands on the screen (x, y, width,
        /// height; origin top-left), when the program in front tells.
        #[serde(default)]
        bounds: Option<(f64, f64, f64, f64)>,
    },
    /// A voice command was understood but not carried out (why).
    CommandRefused(String),
    /// The phrase a voice command was recognised from, for the shell's
    /// confirmation.
    CommandHeard(String),
    /// Mobile: the host (keyboard extension / IME) must insert this text.
    /// Text for a host that inserts it itself (Android service, iOS
    /// keyboard). `rhythm`: type it character by character (streaming);
    /// otherwise insert it at once (batch chunks, dictation window).
    HostText {
        text: String,
        rhythm: bool,
        /// With `rhythm`: the pause after each character, in ms, from the
        /// shared typing rhythm, so every platform types alike.
        pauses_ms: Vec<u16>,
    },
    /// The provider needs a setting outside the app first (after consent).
    /// `settings_url` is a page the shell can open, or empty.
    SetupRequired {
        provider: String,
        message: String,
        settings_url: String,
    },
    /// An offline voice command was recognised.
    VoiceCommand(ikk_wake::Command),
    /// Listening locally for the start word (true) or not (false).
    VoiceStandby(bool),
    /// Diagnosis: what the voice command listener made of one utterance
    /// (`standby`: heard while waiting for the start word).
    VoiceHeard {
        standby: bool,
        judgement: ikk_wake::Judgement,
    },
    /// Diagnosis: loudest input level of the start-word listener over the
    /// last few seconds, in dBFS (shows whether it gets real sound).
    VoiceStandbyLevel(f32),
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

/// Trained voice commands and the session they start.
#[derive(Debug)]
pub struct VoiceSetup {
    pub commands: ikk_wake::VoiceCommands,
    pub start: StartConfig,
}

/// Offline voice commands while switched on.
struct Voice {
    spotter: ikk_wake::Spotter,
    segmenter: ikk_wake::Segmenter,
    start: StartConfig,
    /// Local microphone stream while no session runs (start word only).
    standby: Option<Box<dyn Capture>>,
    /// Opening the standby stream failed; retried after the next command.
    standby_failed: bool,
    /// Loudest standby frame since the last VoiceStandbyLevel event.
    standby_peak: f32,
    standby_level_at: Instant,
}

const LEVEL_INTERVAL: Duration = Duration::from_millis(100);
/// One diagnosis line (debug level) per this interval while a session
/// runs and something happened; quiet stretches every tenth one.
const SUMMARY_INTERVAL: Duration = Duration::from_secs(2);
const QUIET_SUMMARIES: u32 = 10;
const STANDBY_LEVEL_INTERVAL: Duration = Duration::from_secs(3);
/// How long `finish` may take: it waits for the chunks in flight and the
/// second pass over the whole thought.
const FINISH_GRACE: Duration = Duration::from_secs(20);
/// After `finish`, how long late results are waited for: the last chunks
/// and the second pass over the whole thought (a 30 s thought takes a
/// recogniser a few seconds).
const LATE_RESULTS: Duration = Duration::from_secs(15);

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
    /// The chosen corrector's id for live correction (from the start config).
    corrector_id: String,
    command_locale: String,
    last_level: Instant,
    /// Diagnosis: what happened since the last summary line.
    diag: Diag,
    /// Owner of the physical target text while injecting.
    live: LiveTyper,
    /// How text goes into the target in this session.
    insert_style: ikk_output::InsertStyle,
    /// Typing rhythm for hosts that type the text themselves (Android).
    host_rhythm: ikk_output::rhythm::Rhythm,
    /// To hand asynchronous results (corrections) back to this loop.
    self_tx: mpsc::Sender<EngineCommand>,
    correction: Option<Correction>,
    correction_generation: u64,
    /// A correction-button request waiting for the user's consent.
    pending_correction: Option<CorrectRequest>,
    /// A correction is working in the target: finals arriving meanwhile
    /// are held here and typed afterwards, and nothing else touches the
    /// field (no flow ticks, no refinements, no live correction).
    hold: Option<String>,
    /// The range of the correction in progress.
    correcting: Option<CorrectionRange>,
    /// What the voice commands know about the dictation.
    dictation: commands::Dictation,
    /// The segment the last final went into.
    last_segment: Option<ikk_output::live::SegmentId>,
    voice: Option<Voice>,
    /// When and how much text went into the target in this session; lets a
    /// spoken stop word take back what was typed for it.
    typed_log: Vec<(Instant, usize)>,
    /// Stopping by voice: text still in flight belongs to the stop word.
    discard_pending: bool,
    /// Replace by voice in progress: the selection waits for the words.
    replace: Option<ReplaceByVoice>,
    /// The running correction or replacement works on text a host selected:
    /// the result goes back as `HostReplace`, nothing is pasted here.
    host_mode: bool,
    replace_generation: u64,
    /// The session is draining after a spoken stop: a late final that is
    /// the stop phrase is dropped, everything else is typed.
    voice_stopped: bool,
    /// The session's dictation language (ISO-639-3): quotation marks,
    /// and other typography the commands need.
    language: String,
    /// The range a corrector is working on begins a sentence (`None`:
    /// cannot be told), and the language its first word is looked up in.
    range_at_start: Option<bool>,
    range_language: String,
    /// Word suggestions on offer: the marked part as it was read, and
    /// whether a host holds it (the pick then goes back as `HostReplace`).
    choice_original: Option<String>,
    choice_host: bool,
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
        let self_tx = cmd_tx.clone();
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
            corrector_id: String::new(),
            command_locale: String::new(),
            last_level: Instant::now(),
            diag: Diag::default(),
            live: LiveTyper::default(),
            insert_style: ikk_output::InsertStyle::default(),
            host_rhythm: ikk_output::rhythm::Rhythm::default(),
            self_tx,
            correction: None,
            correction_generation: 0,
            pending_correction: None,
            hold: None,
            correcting: None,
            dictation: commands::Dictation::default(),
            last_segment: None,
            voice: None,
            typed_log: Vec::new(),
            discard_pending: false,
            replace: None,
            replace_generation: 0,
            host_mode: false,
            voice_stopped: false,
            language: String::new(),
            range_at_start: None,
            range_language: String::new(),
            choice_original: None,
            choice_host: false,
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
            self.maintain_standby().await;
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
                frame = async { self.voice.as_mut().unwrap().standby.as_mut().unwrap().frames().recv().await },
                    if self.voice.as_ref().is_some_and(|v| v.standby.is_some()) => match frame {
                    Some(frame) => self.on_standby_frame(frame).await,
                    None => self.close_standby().await,
                },
                ev = async { self.session.as_mut().unwrap().events().recv().await }, if self.session.is_some() => match ev {
                    Some(ev) => self.on_transcript(ev).await,
                    None => self.stop().await,
                },
                // A fixed deadline: the audio frames that wake this loop
                // every few milliseconds must not restart the pause.
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.live.due())), if self.live.flow_has_pending() && self.hold.is_none() => {
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
                // The correction button asked for this consent: go on.
                if let Some(req) = self.pending_correction.take() {
                    let same = self
                        .registry
                        .corrector_named(&req.corrector)
                        .is_some_and(|c| c.id() == id);
                    if same {
                        self.correct_range(req).await;
                    }
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
            // An empty secret removes the key (the user deleted it).
            EngineCommand::SetCredential(p, s) => {
                if s.expose().is_empty() {
                    self.credentials.remove(&p);
                } else {
                    self.credentials.insert(p, s);
                }
                Ok(())
            }
            EngineCommand::TransferToTarget => self.transfer().await,
            EngineCommand::ClearSessionText => {
                self.session_text.clear();
                self.dictation = commands::Dictation {
                    caret_from_end: Some(0),
                    ..Default::default()
                };
                Ok(())
            }
            EngineCommand::SetVoiceCommands(setup) => {
                // Only the provider, language or similar changed: keep the
                // open microphone. Closing and at once reopening it lost
                // the start word on Windows until voice was switched off
                // and on again.
                if let (Some(v), Some(new)) = (self.voice.as_mut(), setup.as_ref()) {
                    if v.standby.is_some() && same_standby_capture(&v.start, &new.start) {
                        v.spotter = ikk_wake::Spotter::new(new.commands.clone());
                        v.start = new.start.clone();
                        return Ok(());
                    }
                }
                self.close_standby().await;
                self.voice = setup.map(|v| Voice {
                    spotter: ikk_wake::Spotter::new(v.commands),
                    segmenter: ikk_wake::Segmenter::new(),
                    start: v.start,
                    standby: None,
                    standby_failed: false,
                    standby_peak: f32::NEG_INFINITY,
                    standby_level_at: Instant::now(),
                });
                Ok(())
            }
            EngineCommand::CorrectRange(req) => {
                self.correct_range(req).await;
                Ok(())
            }
            EngineCommand::ReplaceByVoice {
                range,
                start,
                host_text,
            } => {
                self.replace_by_voice(range, start, host_text).await;
                Ok(())
            }
            EngineCommand::ReplaceTick(generation) => {
                self.replace_tick(generation).await;
                Ok(())
            }
            EngineCommand::SuggestWords(req) => {
                self.suggest_words(req).await;
                Ok(())
            }
            EngineCommand::WordChoicesReady {
                original,
                bounds,
                result,
            } => {
                self.word_choices_ready(original, bounds, result).await;
                Ok(())
            }
            EngineCommand::ApplyChoice(text) => {
                self.apply_choice(text).await;
                Ok(())
            }
            EngineCommand::ApplyRangeCorrection {
                original,
                prior_clipboard,
                result,
            } => {
                self.apply_range_correction(original, prior_clipboard, result).await;
                Ok(())
            }
            EngineCommand::ApplyCorrection {
                generation,
                from,
                through,
                result,
                throttled,
            } => {
                self.apply_correction(generation, from, through, result, throttled)
                    .await;
                Ok(())
            }
            EngineCommand::CorrectionTick(generation) => {
                if self.correction.as_ref().is_some_and(|c| c.generation == generation) {
                    if let Some(c) = self.correction.as_mut() {
                        c.tick_pending = false;
                    }
                    self.schedule_correction().await;
                }
                Ok(())
            }
            EngineCommand::Shutdown => Ok(()),
        }
    }

    /// The corrector to use for `wanted` (settings id), with its credential.
    /// A corrector that needs a key but has none is replaced by the
    /// on-device corrector, which needs neither key nor consent.
    fn pick_corrector(&self, wanted: &str) -> Option<(Arc<dyn TextCorrector>, Option<String>)> {
        let chosen = self.registry.corrector_named(wanted)?;
        if chosen.local() {
            return Some((chosen, None));
        }
        let credential = self
            .credentials
            .get(&ProviderId(chosen.id().to_string()))
            .map(|s| s.expose().to_string());
        if credential.is_some() {
            return Some((chosen, credential));
        }
        match self.registry.local_corrector() {
            Some(local) => {
                info!(chosen = chosen.id(), "no key: the on-device corrector steps in");
                Some((local, None))
            }
            None => Some((chosen, None)),
        }
    }

    /// A final was committed: ask the corrector for the newest segments,
    /// unless one request is already out (then once it is back) or the
    /// last request was moments ago (then after the gap).
    async fn schedule_correction(&mut self) {
        let Some((corrector, credential)) = self.pick_corrector(&self.corrector_id.clone()) else {
            return;
        };
        let Some(through) = self.last_segment else { return };
        let allowed = corrector.local() || self.consent.decision(corrector.id()) == Decision::Allowed;
        // The newest segments only: the ones before were checked already.
        let segments = self.live.segments();
        let start = segments.len().saturating_sub(CORRECTION_WINDOW);
        let Some(from) = segments.get(start).map(|s| s.id) else {
            return;
        };
        let Some(text) = self.live.text_of_range(from, through) else {
            return;
        };
        let Some(c) = self.correction.as_mut() else { return };
        if c.paused_until.is_some_and(|t| t > Instant::now()) {
            return;
        }
        if !allowed {
            if !c.asked_consent {
                c.asked_consent = true;
                let r = recipient_for(corrector.id(), corrector.display_name());
                let _ = self.events.send(EngineEvent::ConsentRequired(r)).await;
            }
            return;
        }
        // No account, no requests: the switch alone is not enough (an
        // on-device corrector needs none).
        if credential.is_none() && !corrector.local() {
            return;
        }
        if c.in_flight {
            c.dirty = true;
            return;
        }
        if text.trim().is_empty() || text == c.checked {
            return;
        }
        // Not too often: wait out the gap, then come back.
        let wait = c
            .last_request
            .map(|t| CORRECTION_MIN_GAP.saturating_sub(t.elapsed()))
            .unwrap_or(Duration::ZERO);
        if !wait.is_zero() {
            if !c.tick_pending {
                c.tick_pending = true;
                let (generation, tx) = (c.generation, self.self_tx.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    let _ = tx.send(EngineCommand::CorrectionTick(generation)).await;
                });
            }
            return;
        }
        c.in_flight = true;
        c.dirty = false;
        c.last_request = Some(Instant::now());
        corrector.set_gender_colon(self.text_cfg.gender_colon_auto);
        corrector.set_broken_words(self.text_cfg.broken_words);
        let (generation, language, tx) = (c.generation, c.language.clone(), self.self_tx.clone());
        tokio::spawn(async move {
            let (result, throttled) = match corrector.correct(&text, &language, credential.as_deref()).await {
                Ok(r) => (Ok(r), false),
                Err(e) => {
                    warn!(%e, "live correction failed");
                    let throttled = matches!(e, ikk_stt::SttError::Provider(ikk_stt::ErrorClass::Throttled));
                    (Err(e.to_string()), throttled)
                }
            };
            let _ = tx
                .send(EngineCommand::ApplyCorrection {
                    generation,
                    from,
                    through,
                    result,
                    throttled,
                })
                .await;
        });
    }

    async fn apply_correction(
        &mut self,
        generation: u64,
        from: ikk_output::live::SegmentId,
        through: ikk_output::live::SegmentId,
        result: Result<Option<String>, String>,
        throttled: bool,
    ) {
        if self.hold.is_some() {
            return; // a correction button job owns the field
        }
        let Some(c) = self.correction.as_mut() else { return };
        if c.generation != generation {
            return;
        }
        c.in_flight = false;
        if throttled {
            c.paused_until = Some(Instant::now() + CORRECTION_PAUSE);
        }
        let dirty = c.dirty;
        let rewrites = c.rewrites;
        match result {
            Ok(Some(corrected)) if !rewrites => {
                // Suggest only: the typed text stays as it is.
                if let Some(c) = self.correction.as_mut() {
                    c.checked = corrected.clone();
                }
                let _ = self.events.send(EngineEvent::Suggestion(corrected)).await;
            }
            Ok(Some(corrected)) if self.output_mode == OutputMode::Inject => {
                match self.live.replace_range(from, through, &corrected, &mut *self.sink) {
                    Ok(true) => {
                        if let Some(c) = self.correction.as_mut() {
                            c.checked = corrected.clone();
                        }
                        let text = self.live.text();
                        self.session_text = text.clone();
                        let _ = self
                            .events
                            .send(EngineEvent::Transcript(TranscriptEvent::Replace(text)))
                            .await;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                            .await
                    }
                }
            }
            Ok(_) => {
                let checked = self.live.text_of_range(from, through).unwrap_or_default();
                if let Some(c) = self.correction.as_mut() {
                    c.checked = checked;
                }
            }
            Err(_) => {}
        }
        if dirty {
            self.schedule_correction().await;
        }
    }

    /// While voice commands are on and no session runs, listen locally for
    /// the start word.
    async fn maintain_standby(&mut self) {
        // Open whenever our own session stream is not running: while idle,
        // and during sessions of providers with their own microphone.
        let idle = self.capture.is_none();
        let Some(voice) = self.voice.as_mut() else { return };
        if !idle || voice.standby.is_some() || voice.standby_failed {
            return;
        }
        match (self.open_capture)(&standby_capture(&voice.start)) {
            Ok(c) => {
                voice.standby = Some(c);
                voice.segmenter = ikk_wake::Segmenter::new();
                let _ = self.events.send(EngineEvent::VoiceStandby(true)).await;
            }
            Err(e) => {
                warn!(%e, "voice command standby could not open the microphone");
                voice.standby_failed = true;
            }
        }
    }

    async fn close_standby(&mut self) {
        if self.take_standby() {
            let _ = self.events.send(EngineEvent::VoiceStandby(false)).await;
        }
    }

    /// Close the standby stream without telling anyone; true if it was open.
    fn take_standby(&mut self) -> bool {
        match self.voice.as_mut().and_then(|v| v.standby.take()) {
            Some(c) => {
                c.stop();
                true
            }
            None => false,
        }
    }

    async fn on_standby_frame(&mut self, frame: ikk_audio::AudioFrame) {
        let Some(voice) = self.voice.as_mut() else { return };
        if frame.sample_rate_hz != ikk_wake::SAMPLE_RATE_HZ {
            return;
        }
        voice.standby_peak = voice.standby_peak.max(frame.level_dbfs());
        if voice.standby_level_at.elapsed() >= STANDBY_LEVEL_INTERVAL {
            voice.standby_level_at = Instant::now();
            let peak = std::mem::replace(&mut voice.standby_peak, f32::NEG_INFINITY);
            let _ = self.events.try_send(EngineEvent::VoiceStandbyLevel(peak));
        }
        let Some(utterance) = voice.segmenter.push(&frame.samples) else {
            return;
        };
        let judgement = voice.spotter.judge(&utterance);
        let heard = judgement.command;
        info!(judgement = %judgement.summary(), "start word listener heard");
        let _ = self.events.try_send(EngineEvent::VoiceHeard {
            standby: true,
            judgement,
        });
        // During a session with its own microphone this listener hears the
        // stop word; otherwise it waits for the start word.
        if self.session.is_some() {
            if heard == Some(ikk_wake::Command::Stop) {
                let secs = utterance.len() as f32 / ikk_wake::SAMPLE_RATE_HZ as f32;
                self.stop_by_voice(secs).await;
            }
            return;
        }
        if heard != Some(ikk_wake::Command::Start) {
            return;
        }
        let cfg = voice.start.clone();
        let _ = self
            .events
            .send(EngineEvent::VoiceCommand(ikk_wake::Command::Start))
            .await;
        if let Err(e) = self.start(cfg).await {
            warn!(%e, "start word heard, but dictation did not start");
            let _ = self.events.send(EngineEvent::Error(e.to_string())).await;
        }
    }

    /// The stop word was heard during dictation: the microphone closes,
    /// but everything said before it is still finished — the readings in
    /// flight, the last thought, the corrections — and typed. Nothing
    /// already written is taken back. Only a final that is the stop phrase
    /// itself (or ends with it) is left out while the session drains.
    async fn stop_by_voice(&mut self, _utterance_secs: f32) {
        let _ = self
            .events
            .send(EngineEvent::VoiceCommand(ikk_wake::Command::Stop))
            .await;
        self.voice_stopped = true;
        self.stop().await;
        self.voice_stopped = false;
    }

    async fn start(&mut self, mut cfg: StartConfig) -> Result<(), EngineError> {
        if self.session.is_some() {
            return Err(EngineError::Busy);
        }
        if let Some(v) = self.voice.as_mut() {
            v.standby_failed = false;
        }
        self.typed_log.clear();
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
        // Consent given; now anything the user must switch on elsewhere.
        if let Some(setup) = provider.setup_needed() {
            let _ = self
                .events
                .send(EngineEvent::SetupRequired {
                    provider: cfg.session.provider.0.clone(),
                    message: setup.message,
                    settings_url: setup.settings_url,
                })
                .await;
            return Ok(());
        }
        if cfg.session.credential.is_none() {
            cfg.session.credential = self.credentials.get(&cfg.session.provider).cloned();
        }
        self.session_text.clear();
        self.dictation = commands::Dictation {
            caret_from_end: Some(0),
            ..Default::default()
        };
        // Dictated text is typed in rhythm at the cursor, whether it comes
        // from a live stream or from batch chunks. Only a finished text
        // moved from the dictation window is pasted in one piece.
        self.insert_style = ikk_output::InsertStyle::Rhythm;
        self.sink.set_insert_style(self.insert_style);
        // A sink that pastes or sets its own rhythm wants whole segments; a
        // character flow would mean one paste per character.
        let flow_ms = if self.sink.wants_whole_segments() {
            0
        } else {
            cfg.flow_ms
        };
        self.live = LiveTyper::new(Box::new(ikk_output::guard::AllowAllGuard), flow_ms);
        self.last_segment = None;
        self.correction_generation += 1;
        self.correction = cfg.correction.then(|| Correction {
            language: cfg.session.language.clone(),
            generation: self.correction_generation,
            in_flight: false,
            dirty: false,
            checked: String::new(),
            rewrites: cfg.correction_rewrites,
            asked_consent: false,
            paused_until: None,
            last_request: None,
            tick_pending: false,
        });
        self.decide_output_mode().await?;

        // Our session stream takes over from the voice listener. Providers
        // with their own microphone run next to it (Windows and macOS let
        // several readers share a microphone), so the stop word works with
        // every provider, like Dragon's always-available commands.
        // Its "standby off" is sent after "listening", so a button goes
        // straight from waiting to dictating. Should the start fail, the
        // loop reopens the listener and the button never left "waiting".
        let standby_closed = !caps.own_audio && self.take_standby();
        // System recognizers record the microphone themselves.
        // Providers that want the microphone's full bandwidth get it; the
        // others get the pipeline rate (their wire rate anyway).
        cfg.capture.sample_rate_hz = caps.required_sample_rate_hz.unwrap_or(PIPELINE_SAMPLE_RATE_HZ);
        let capture = if caps.own_audio {
            None
        } else {
            Some((self.open_capture)(&cfg.capture)?)
        };
        let session = match open_session(provider, &cfg.session).await {
            Ok(s) => s,
            Err(e) => {
                if let Some(c) = capture {
                    c.stop();
                }
                return Err(e.into());
            }
        };
        let mut vad = VoiceActivityDetector::new(cfg.vad_mode, cfg.capture.sample_rate_hz);
        vad.set_pause_duration_ms(cfg.pause.as_millis().max(100) as u32);
        self.capture = capture;
        self.session = Some(session);
        self.vad = Some(vad);
        self.text_cfg = cfg.text;
        self.corrector_id = cfg.corrector;
        self.command_locale = cfg.command_locale;
        self.set_state(EngineState::Listening).await;
        if standby_closed {
            let _ = self.events.send(EngineEvent::VoiceStandby(false)).await;
        }
        self.language = cfg.session.language.clone();
        info!(
            provider = %cfg.session.provider.0,
            transfer = ?cfg.session.transfer,
            language = %cfg.session.language,
            chunk_ms = cfg.session.chunk_interval.as_millis() as u64,
            pause_ms = cfg.pause.as_millis() as u64,
            denoise = cfg.capture.denoise,
            agc = cfg.capture.agc,
            vocabulary = cfg.session.keywords.len(),
            agreement = cfg.session.agreement.is_some(),
            mode = ?self.output_mode,
            correction = cfg.correction,
            checker = self.pick_corrector(&self.corrector_id).map(|(c, _)| c.id()).unwrap_or("none"),
            "session started"
        );
        Ok(())
    }

    async fn on_frame(&mut self, frame: ikk_audio::AudioFrame) {
        if let Some(voice) = self.voice.as_mut() {
            // The stop word listener works at 16 kHz whatever the session's rate.
            let wake_samples;
            let samples = if frame.sample_rate_hz == ikk_wake::SAMPLE_RATE_HZ {
                &frame.samples
            } else {
                wake_samples =
                    ikk_audio::dsp::resample_linear(&frame.samples, frame.sample_rate_hz, ikk_wake::SAMPLE_RATE_HZ);
                &wake_samples
            };
            {
                if let Some(utterance) = voice.segmenter.push(samples) {
                    let judgement = voice.spotter.judge(&utterance);
                    let heard = judgement.command;
                    let _ = self.events.try_send(EngineEvent::VoiceHeard {
                        standby: false,
                        judgement,
                    });
                    if heard == Some(ikk_wake::Command::Stop) {
                        let secs = utterance.len() as f32 / ikk_wake::SAMPLE_RATE_HZ as f32;
                        self.stop_by_voice(secs).await;
                        return;
                    }
                }
            }
        }
        if self.last_level.elapsed() >= LEVEL_INTERVAL {
            self.last_level = Instant::now();
            let _ = self.events.try_send(EngineEvent::Level(frame.level_dbfs()));
        }
        self.diag.frames += 1;
        self.diag.audio_ms += frame.duration().as_millis() as u32;
        if self.session.is_some() && self.diag.since.elapsed() >= SUMMARY_INTERVAL {
            let quiet = self.diag.quiet;
            let d = std::mem::take(&mut self.diag);
            let waiting = self
                .live
                .text()
                .chars()
                .count()
                .saturating_sub(self.live.typed().chars().count());
            let something = d.interims + d.finals > 0 || d.typed > 0 || waiting > 0;
            // Debug level: visible with RUST_LOG=ikk_engine=debug, quiet otherwise.
            if something || quiet + 1 >= QUIET_SUMMARIES {
                tracing::debug!(
                    frames = d.frames,
                    audio_ms = d.audio_ms,
                    interims = d.interims,
                    finals = d.finals,
                    final_chars = d.final_chars,
                    typed = d.typed,
                    waiting,
                    quiet_intervals = quiet,
                    "dictation last 2 s"
                );
            } else {
                self.diag.quiet = quiet + 1;
            }
        }
        let ended = match self.vad.as_mut().map(|v| v.process(&frame.samples)) {
            Some(Some(VadEvent::SpeechStarted)) => {
                let _ = self.events.try_send(EngineEvent::Speech(true));
                if let Some(r) = self.replace.as_mut() {
                    r.speech = true;
                }
                false
            }
            Some(Some(VadEvent::SpeechEnded)) => {
                let _ = self.events.try_send(EngineEvent::Speech(false));
                if let Some(r) = self.replace.as_mut() {
                    r.speech = false;
                    r.speech_ended_at = Some(Instant::now());
                    self.schedule_replace_tick();
                }
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
                self.diag.finals += 1;
                self.diag.final_chars += text.chars().count();
                // Dictated text only at debug level, for test sessions.
                tracing::debug!(text = %text, "final");
                let processed = ikk_text::process(&self.text_cfg, &text);
                if processed.is_empty() {
                    return;
                }
                if self.replace.is_some() {
                    self.collect_replacement(processed).await;
                    return;
                }
                if !self.command_locale.is_empty() {
                    if let Some(intent) = ikk_commands::match_command(&processed, &self.command_locale) {
                        let _ = self.events.send(EngineEvent::CommandHeard(processed.clone())).await;
                        let _ = self.events.send(EngineEvent::Command(intent.clone())).await;
                        self.execute(intent).await;
                        return;
                    }
                    // Agreed words come in pieces: a command may have been
                    // split over the last piece and this one ("Neue" …
                    // "Zeile"). Then the last piece was typed by mistake.
                    if let Some(last) = self.dictation.pieces.last().cloned() {
                        let joined = format!("{} {processed}", last.trim());
                        if let Some(intent) = ikk_commands::match_command(&joined, &self.command_locale) {
                            if self.dictation.caret_from_end == Some(0) && self.dictation.selection == 0 {
                                self.dictation.pieces.pop();
                                let n = last.chars().count();
                                if self.output_mode == OutputMode::Inject {
                                    let _ = self.live.delete_tail(n, &mut *self.sink);
                                }
                                let keep = self.session_text.chars().count().saturating_sub(n);
                                self.session_text = self.session_text.chars().take(keep).collect();
                            }
                            let _ = self.events.send(EngineEvent::CommandHeard(joined)).await;
                            let _ = self.events.send(EngineEvent::Command(intent.clone())).await;
                            self.execute(intent).await;
                            return;
                        }
                    }
                }
                // Spoken signs anywhere in the piece ("Hallo Welt Punkt wie
                // geht es dir Fragezeichen", "… Zitat …", "Groß hallo"):
                // rendered into the text, no pause needed around them.
                let mut text = processed;
                let mut capitalize_next = false;
                if !self.command_locale.is_empty() {
                    let quotes = commands::quotation_marks(&self.language);
                    if let Some(inline) = ikk_commands::apply_inline(&text, &self.command_locale, quotes) {
                        let _ = self.events.send(EngineEvent::CommandHeard(text.clone())).await;
                        text = inline.text;
                        capitalize_next = inline.capitalize_next;
                    }
                }
                if !text.is_empty() {
                    let Some(shaped) = self.shape_final(&text) else {
                        return;
                    };
                    // A piece that starts with a sign ("Punkt") joins the text
                    // before it without a space.
                    let closing = commands::quotation_marks(&self.language).1;
                    let tight = shaped.starts_with(closing)
                        || shaped
                            .chars()
                            .next()
                            .is_some_and(|c| ".,;:!?)]}»”’…%°\n".contains(c) || c == '\u{202F}');
                    let with_space = if self.session_text.is_empty()
                        || self.dictation.selection > 0
                        || self.after_opening_quote()
                        || tight
                    {
                        shaped
                    } else {
                        format!(" {shaped}")
                    };
                    let _ = self.emit_text(&with_space).await;
                    self.note_piece(&with_space);
                    let _ = self
                        .events
                        .send(EngineEvent::Transcript(TranscriptEvent::Final(
                            with_space.trim().to_string(),
                        )))
                        .await;
                }
                if capitalize_next {
                    self.dictation.capitalize_next = true;
                }
                self.schedule_correction().await;
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
            TranscriptEvent::Refined { segments, text } => self.apply_refined(segments, text).await,
            TranscriptEvent::Replace(text) => {
                let processed = ikk_text::process(&self.text_cfg, &text);
                self.replace_all(&processed).await;
                let _ = self
                    .events
                    .send(EngineEvent::Transcript(TranscriptEvent::Replace(processed)))
                    .await;
            }
            other => {
                if matches!(other, TranscriptEvent::Interim(_)) {
                    self.diag.interims += 1;
                }
                let _ = self.events.send(EngineEvent::Transcript(other)).await;
            }
        }
    }

    /// The whole thought came back: replace the last `segments` typed
    /// finals with it (in place, common prefix kept).
    async fn apply_refined(&mut self, segments: usize, text: String) {
        tracing::debug!(segments, text = %text, "refined");
        if self.hold.is_some() {
            return; // a correction owns the field; the quick text stands
        }
        let processed = ikk_text::process(&self.text_cfg, &text);
        if processed.is_empty() {
            return;
        }
        let all = self.live.segments();
        let n = segments.min(all.len());
        if n == 0 || self.output_mode != OutputMode::Inject {
            // Nothing typed to replace: forward it, the app decides.
            let _ = self
                .events
                .send(EngineEvent::Transcript(TranscriptEvent::Refined {
                    segments,
                    text: processed,
                }))
                .await;
            return;
        }
        let from = all[all.len() - n].id;
        let through = all[all.len() - 1].id;
        let with_space = if all.len() == n {
            processed
        } else {
            format!(" {processed}")
        };
        match self.live.replace_range(from, through, &with_space, &mut *self.sink) {
            Ok(_) => {
                let text = self.live.text();
                self.session_text = text.clone();
                self.last_segment = self.live.segments().last().map(|s| s.id);
                let _ = self
                    .events
                    .send(EngineEvent::Transcript(TranscriptEvent::Replace(text)))
                    .await;
                self.schedule_correction().await;
            }
            Err(e) => {
                self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                    .await
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
                self.diag.typed += n;
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
        if self.hold.is_some() {
            return; // a correction owns the field
        }
        self.session_text = text.to_string();
        if self.output_mode != OutputMode::Inject {
            return;
        }
        if let Err(e) = self.live.replace_all(text, &mut *self.sink) {
            self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                .await;
        }
    }

    async fn stop(&mut self) {
        if self.replace.is_some() {
            // The session closes anyway; the words so far go in.
            let _ = self.finish_replace().await;
        }
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
        if self.discard_pending {
            // Queued text belongs to the stop word: drop it untyped.
            self.live = LiveTyper::default();
        } else if self.output_mode == OutputMode::Inject {
            if let Err(e) = self.live.flush(&mut *self.sink) {
                self.switch_to_fallback(FallbackReason::InjectionFailed(e.to_string()))
                    .await;
            }
        }
        self.vad = None;
        if let Some(mut session) = self.session.take() {
            let _ = tokio::time::timeout(FINISH_GRACE, session.finish()).await;
            // Drain what finish produced until Closed: the last chunks and,
            // after them, the whole thought that replaces them. In order,
            // because the replacement refers to the finals before it.
            let deadline = tokio::time::Instant::now() + LATE_RESULTS;
            let mut late = Vec::new();
            while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, session.events().recv()).await {
                if matches!(ev, TranscriptEvent::Closed) {
                    break;
                }
                late.push(ev);
            }
            for ev in late {
                match ev {
                    TranscriptEvent::Final(t) => {
                        let mut t = ikk_text::process(&self.text_cfg, &t);
                        if t.is_empty() || self.discard_pending {
                            continue;
                        }
                        if !self.command_locale.is_empty() {
                            // The spoken stop phrase came through the recogniser
                            // too: not typed. Words before it are.
                            if self.voice_stopped {
                                if ikk_commands::match_command(&t, &self.command_locale)
                                    == Some(ikk_commands::Intent::StopDictation)
                                {
                                    continue;
                                }
                                if let Some((before, ikk_commands::Intent::StopDictation)) =
                                    ikk_commands::split_trailing_any(&t, &self.command_locale)
                                {
                                    t = before;
                                    if t.is_empty() {
                                        continue;
                                    }
                                }
                            }
                            // Spoken signs inside the piece, as during the session.
                            let quotes = commands::quotation_marks(&self.language);
                            if let Some(inline) = ikk_commands::apply_inline(&t, &self.command_locale, quotes) {
                                t = inline.text;
                            }
                        }
                        let closing = commands::quotation_marks(&self.language).1;
                        let tight = t.starts_with(closing)
                            || t.chars()
                                .next()
                                .is_some_and(|c| ".,;:!?)]}»”’…%°\n".contains(c) || c == '\u{202F}');
                        let with_space = if self.session_text.is_empty() || self.after_opening_quote() || tight {
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
                    TranscriptEvent::Refined { segments, text } if !self.discard_pending => {
                        // The last chunks were queued for typing just now: let
                        // them out before the replacement looks at them.
                        if self.output_mode == OutputMode::Inject {
                            let _ = self.live.flush(&mut *self.sink);
                        }
                        self.apply_refined(segments, text).await;
                    }
                    other => {
                        let _ = self.events.send(EngineEvent::Transcript(other)).await;
                    }
                }
            }
        }
        // The listener is waiting again before "idle" goes out, so a button
        // goes straight from dictating to waiting.
        self.maintain_standby().await;
        self.set_state(EngineState::Idle).await;
    }

    /// Replace by voice: the selection (made by the shell, or made here
    /// from `range`) is read, the engine listens — in the running session,
    /// or in one it opens from `start` — and the first thought is pasted
    /// over the selection. Desktop only; the mobile hosts do it themselves.
    async fn replace_by_voice(
        &mut self,
        range: CorrectionRange,
        start: Option<Box<StartConfig>>,
        host_text: Option<String>,
    ) {
        if self.replace.is_some() || self.hold.is_some() {
            self.say_error("replace: a correction is running already").await;
            return;
        }
        if matches!(self.state, EngineState::AwaitingConsent | EngineState::Error) {
            self.say_error("replace: not while the engine waits for consent or shows an error")
                .await;
            return;
        }
        if matches!(range, CorrectionRange::ClickToClick | CorrectionRange::Word) {
            self.say_error("replace: click to click and single word need the app's floating button")
                .await;
            return;
        }
        let own_session = self.session.is_none();
        if own_session {
            let Some(mut cfg) = start else {
                self.say_error("replace: no dictation running and no settings to start one")
                    .await;
                return;
            };
            // A replacement is one short thought, often one word: the agreed-
            // words voting needs several readings of the same speech and
            // would drop it, the re-check would come too late. One plain
            // reading, a short pause, pieces cut soon after the speech.
            cfg.session.agreement = None;
            cfg.session.refine = false;
            cfg.session.refine_preview = false;
            cfg.pause = cfg.pause.min(Duration::from_millis(1500));
            cfg.session.endpointing = cfg.pause;
            // The whole short thought as one clip: no cut at a word gap
            // whose later piece could arrive after the first was taken.
            cfg.session.chunk_interval = Duration::from_secs(15);
            cfg.correction = false;
            if let Err(e) = self.start(*cfg).await {
                self.say_error(&format!("replace: {e}")).await;
                return;
            }
            if self.session.is_none() {
                // Consent or setup is being asked for; the user tries again.
                return;
            }
        }
        self.hold = Some(String::new());
        self.correcting = Some(range);
        self.host_mode = host_text.is_some();
        let selected = match host_text {
            Some(t) => Ok((t, None)),
            None => tokio::task::spawn_blocking(move || select_and_copy(range))
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
        };
        let (original, prior_clipboard) = match selected {
            Ok(v) => v,
            Err(e) => {
                self.host_mode = false;
                self.say_error(&format!("replace: {e}")).await;
                self.end_hold(false).await;
                if own_session {
                    self.discard_pending = true;
                    self.stop().await;
                    self.discard_pending = false;
                }
                return;
            }
        };
        info!(
            chars = original.chars().count(),
            own_session, "replace by voice: listening for the words"
        );
        self.replace = Some(ReplaceByVoice {
            original: original.clone(),
            at_start: {
                let context = take_context();
                let at_start = context.as_deref().and_then(sentence_start);
                info!(
                    read = context.is_some(),
                    sentence_start = ?at_start,
                    "replace by voice: what stands in front of the range"
                );
                at_start
            },
            prior_clipboard,
            collected: String::new(),
            own_session,
            started: Instant::now(),
            last_final: None,
            speech: false,
            speech_ended_at: None,
        });
        self.schedule_replace_tick();
        let _ = self.events.send(EngineEvent::ReplaceListening { original }).await;
    }

    /// A final while a replacement runs: the words are the replacement;
    /// a spoken stop ends it at once; spoken signs are rendered.
    async fn collect_replacement(&mut self, processed: String) {
        let mut text = processed;
        if !self.command_locale.is_empty() {
            if ikk_commands::match_command(&text, &self.command_locale) == Some(ikk_commands::Intent::StopDictation) {
                self.finish_and_close().await;
                return;
            }
            if let Some((before, ikk_commands::Intent::StopDictation)) =
                ikk_commands::split_trailing_any(&text, &self.command_locale)
            {
                if let Some(r) = self.replace.as_mut() {
                    if !before.trim().is_empty() {
                        if !r.collected.is_empty() {
                            r.collected.push(' ');
                        }
                        r.collected.push_str(before.trim());
                    }
                }
                self.finish_and_close().await;
                return;
            }
            let quotes = commands::quotation_marks(&self.language);
            if let Some(inline) = ikk_commands::apply_inline(&text, &self.command_locale, quotes) {
                text = inline.text;
            }
        }
        if let Some(r) = self.replace.as_mut() {
            if !text.trim().is_empty() {
                if !r.collected.is_empty() {
                    r.collected.push(' ');
                }
                r.collected.push_str(text.trim());
            }
            r.last_final = Some(Instant::now());
        }
        self.schedule_replace_tick();
    }

    fn schedule_replace_tick(&mut self) {
        self.replace_generation += 1;
        let (generation, tx) = (self.replace_generation, self.self_tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(REPLACE_SETTLE).await;
            let _ = tx.send(EngineCommand::ReplaceTick(generation)).await;
        });
    }

    /// The settle time is up: finish when words came and the speaker is
    /// quiet, wait on while the speaker talks or nothing came yet, give
    /// up after the timeout.
    async fn replace_tick(&mut self, generation: u64) {
        if generation != self.replace_generation {
            return;
        }
        let Some(r) = self.replace.as_ref() else { return };
        if r.speech {
            self.schedule_replace_tick();
            return;
        }
        if r.collected.is_empty() {
            if r.started.elapsed() >= REPLACE_TIMEOUT {
                info!("replace by voice: nothing heard; the selection stays");
                self.finish_and_close().await;
            } else {
                self.schedule_replace_tick();
            }
            return;
        }
        // Words came. The thought is finished when a reading arrived after
        // the speech ended (the one that covers its end) and has settled —
        // not merely when the last words are two seconds old, which closed
        // the replacement while the last piece was still being read and
        // kept only the first words. Without a speech end from the voice
        // detector the settle time alone decides; after the speech end a
        // reading is waited for at most REPLACE_LAST_READING.
        let settled = r
            .last_final
            .is_some_and(|t| t.elapsed() >= REPLACE_SETTLE - Duration::from_millis(50));
        let done = match (r.speech_ended_at, r.last_final) {
            (Some(ended), Some(last)) if last > ended => last.elapsed() >= Duration::from_millis(900),
            (Some(ended), _) => ended.elapsed() >= REPLACE_LAST_READING,
            (None, _) => settled,
        };
        if done {
            self.finish_and_close().await;
        } else {
            self.schedule_replace_tick();
        }
    }

    /// Finish, and close the session when the replacement opened it.
    async fn finish_and_close(&mut self) {
        if self.finish_replace().await {
            self.discard_pending = true;
            self.stop().await;
            self.discard_pending = false;
        }
    }

    /// Paste the collected words over the selection (or leave it when
    /// nothing came) and free the field. Returns whether the replacement
    /// had opened the session itself (the caller closes it).
    async fn finish_replace(&mut self) -> bool {
        let Some(r) = self.replace.take() else { return false };
        self.replace_generation += 1;
        let text = shape_replacement(&r.original, &r.collected, r.at_start, &self.language);
        let prior = r.prior_clipboard.clone();
        let changed = !text.is_empty();
        let host = std::mem::take(&mut self.host_mode);
        let outcome = if host {
            if changed {
                let _ = self.events.send(EngineEvent::HostReplace { text: text.clone() }).await;
            }
            Ok(())
        } else if changed {
            let t = text.clone();
            tokio::task::spawn_blocking(move || paste_over_selection(&t, prior))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        } else {
            tokio::task::spawn_blocking(move || leave_selection(prior))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        };
        info!(
            from = %r.original,
            to = %text,
            changed,
            ms = r.started.elapsed().as_millis() as u64,
            "replace by voice done"
        );
        if let Err(e) = outcome {
            self.say_error(&format!("replace: {e}")).await;
        }
        let _ = self
            .events
            .send(EngineEvent::Replaced {
                from: r.original,
                to: text,
                changed,
            })
            .await;
        self.end_hold(true).await;
        r.own_session
    }

    /// The correction button: select the range in the focused app, copy
    /// it, send it to the corrector, then paste the answer over the
    /// selection. Desktop only; the mobile hosts do it themselves.
    async fn correct_range(&mut self, req: CorrectRequest) {
        if self.hold.is_some() {
            self.say_error("correction: one is running already").await;
            return;
        }
        if matches!(self.state, EngineState::AwaitingConsent | EngineState::Error) {
            self.say_error("correction: not while the engine waits for consent or shows an error")
                .await;
            return;
        }
        let Some((corrector, credential)) = self.pick_corrector(&req.corrector) else {
            self.say_error("correction: no corrector available").await;
            return;
        };
        if !corrector.local() && self.consent.decision(corrector.id()) != Decision::Allowed {
            let r = recipient_for(corrector.id(), corrector.display_name());
            self.pending_correction = Some(req);
            let _ = self.events.send(EngineEvent::ConsentRequired(r)).await;
            return;
        }
        if credential.is_none() && !corrector.local() {
            self.say_error(&format!("correction: no key for {}", corrector.display_name()))
                .await;
            return;
        }
        if matches!(req.range, CorrectionRange::ClickToClick | CorrectionRange::Word) {
            // The shell turns the clicks into a selection first; a request
            // that still names clicks came without a shell.
            self.say_error("correction: click to click and single word need the app's floating button")
                .await;
            return;
        }
        let range = req.range;
        // While dictating: the typed text stays as it is until the
        // correction is through; words that arrive meanwhile wait.
        self.hold = Some(String::new());
        self.correcting = Some(range);
        self.host_mode = req.host_text.is_some();
        let selected = match req.host_text.clone() {
            Some(t) => Ok((t, None)),
            None => tokio::task::spawn_blocking(move || select_and_copy(range))
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
        };
        let (original, prior_clipboard) = match selected {
            Ok(v) => v,
            Err(e) => {
                self.host_mode = false;
                self.say_error(&format!("correction: {e}")).await;
                self.end_hold(false).await;
                return;
            }
        };
        let context = take_context();
        self.range_at_start = context.as_deref().and_then(sentence_start);
        self.range_language = req.language.clone();
        info!(
            read = context.is_some(),
            sentence_start = ?self.range_at_start,
            "correction: what stands in front of the range"
        );
        info!(
            chars = original.chars().count(),
            corrector = corrector.id(),
            dictating = self.state != EngineState::Idle,
            "correcting a range; the field is held meanwhile"
        );
        corrector.set_model(&req.model);
        corrector.set_gender_colon(self.text_cfg.gender_colon_auto);
        corrector.set_broken_words(self.text_cfg.broken_words);
        let (language, tx) = (req.language.clone(), self.self_tx.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            // Bounded: while it runs, dictated words wait (see `hold`).
            let result = match tokio::time::timeout(
                RANGE_CORRECTION_TIMEOUT,
                corrector.correct(&original, &language, credential.as_deref()),
            )
            .await
            {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err(format!(
                    "the corrector did not answer within {} s",
                    RANGE_CORRECTION_TIMEOUT.as_secs()
                )),
            };
            info!(
                ms = started.elapsed().as_millis() as u64,
                ok = result.is_ok(),
                "corrector answered"
            );
            let _ = tx
                .send(EngineCommand::ApplyRangeCorrection {
                    original,
                    prior_clipboard,
                    result,
                })
                .await;
        });
    }

    /// Word suggestions: read the selection, leave it selected, and ask
    /// the chosen text model for other things that may have been said.
    async fn suggest_words(&mut self, req: CorrectRequest) {
        self.choice_original = None;
        if self.hold.is_some() {
            self.say_error("suggestions: a correction is running").await;
            return;
        }
        if matches!(self.state, EngineState::AwaitingConsent | EngineState::Error) {
            self.say_error("suggestions: not while the engine waits for consent or shows an error")
                .await;
            return;
        }
        // Only a text model can: the on-device corrector that steps in
        // without a key knows spelling, not what was said.
        let chosen = self.registry.corrector_named(&req.corrector);
        let Some(corrector) = chosen.filter(|c| !c.local() && matches!(c.id(), "openai" | "anthropic")) else {
            self.say_error("suggestions: need-ai-corrector").await;
            return;
        };
        let credential = self
            .credentials
            .get(&ProviderId(corrector.id().to_string()))
            .map(|s| s.expose().to_string());
        if self.consent.decision(corrector.id()) != Decision::Allowed {
            let r = recipient_for(corrector.id(), corrector.display_name());
            let _ = self.events.send(EngineEvent::ConsentRequired(r)).await;
            return;
        }
        if credential.is_none() {
            self.say_error("suggestions: need-ai-corrector").await;
            return;
        }
        self.hold = Some(String::new());
        self.correcting = Some(CorrectionRange::Selection);
        // The marked part and the text around it: from the host that holds
        // the field, or read from the program in front with the keyboard
        // (the selection stays and the user's clipboard comes back).
        self.choice_host = req.host_text.is_some();
        let selected = match req.host_text.clone() {
            Some(t) if t.trim().is_empty() => Err("nothing marked".to_string()),
            Some(t) => Ok((t, req.host_around.clone(), None)),
            None => tokio::task::spawn_blocking(|| {
                // First the quiet way: the program's accessibility tree
                // gives the marked part and the text around it without
                // anything being selected, copied or moved. (A selection
                // just made needs a moment to show there.)
                std::thread::sleep(Duration::from_millis(150));
                if let Some((before, marked, after)) = range_keys::around_quietly() {
                    info!("word suggestions: context read from the program, nothing selected for it");
                    return Ok((marked, Some((before, after)), range_keys::marked_bounds()));
                }
                info!("word suggestions: the program does not tell its text; reading with the keyboard");
                // Otherwise with the keyboard: copy the selection and step
                // around it (the selection moves visibly for a moment).
                select_and_copy(CorrectionRange::Selection).map(|(text, prior)| {
                    let around = range_keys::read_around(&text);
                    range_keys::give_back_clipboard(prior);
                    (text, around, range_keys::marked_bounds())
                })
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string())),
        };
        let (original, around, bounds) = match selected {
            Ok(v) => v,
            Err(_) => {
                self.choice_host = false;
                self.say_error("suggestions: nothing-marked").await;
                self.end_hold(false).await;
                return;
            }
        };
        // Where the marked part stands decides the first letter of what
        // replaces it: from the text in front of it when that was read.
        let probed = take_context();
        self.range_at_start = match &around {
            Some((before, _)) if !before.is_empty() => sentence_start(before),
            // Nothing in front of it: the very start of the text, unless
            // the probe of the keyboard way says otherwise.
            Some(_) => probed.as_deref().and_then(sentence_start).or(Some(true)),
            None => probed.as_deref().and_then(sentence_start),
        };
        self.range_language = req.language.clone();
        // What the model judges with: the sentences around the marked
        // part, its place shown by ⟦ ⟧; failing that, the text typed in
        // this session.
        let context: String = match &around {
            Some((before, after)) if !(before.trim().is_empty() && after.trim().is_empty()) => {
                let skip = before.chars().count().saturating_sub(AROUND_BEFORE);
                let before: String = before.chars().skip(skip).collect();
                let after: String = after.chars().take(AROUND_AFTER).collect();
                format!("{before}⟦{}⟧{after}", original.trim())
            }
            _ => {
                let typed = self.live.text();
                let skip = typed.chars().count().saturating_sub(600);
                typed.chars().skip(skip).collect()
            }
        };
        info!(
            chars = original.chars().count(),
            context_chars = context.chars().count(),
            around_read = around.is_some(),
            bounds_known = bounds.is_some(),
            host = self.choice_host,
            corrector = corrector.id(),
            "asking for word suggestions"
        );
        corrector.set_model(&req.model);
        let (language, tx) = (req.language.clone(), self.self_tx.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            let result = match tokio::time::timeout(
                RANGE_CORRECTION_TIMEOUT,
                corrector.suggest(&original, &context, &language, credential.as_deref()),
            )
            .await
            {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err(format!(
                    "the corrector did not answer within {} s",
                    RANGE_CORRECTION_TIMEOUT.as_secs()
                )),
            };
            info!(
                ms = started.elapsed().as_millis() as u64,
                found = result.as_ref().map_or(0, |c| c.len()),
                "word suggestions answered"
            );
            let _ = tx
                .send(EngineCommand::WordChoicesReady {
                    original,
                    bounds,
                    result,
                })
                .await;
        });
    }

    async fn word_choices_ready(
        &mut self,
        original: String,
        bounds: Option<(f64, f64, f64, f64)>,
        result: Result<Vec<String>, String>,
    ) {
        // The field is free again while the user chooses: dictated words
        // that waited are typed (they go where the selection is, as any
        // typing would — the shell closes the offer when that happens).
        self.end_hold(false).await;
        match result {
            Ok(choices) => {
                if !choices.is_empty() {
                    self.choice_original = Some(original.clone());
                }
                let _ = self
                    .events
                    .send(EngineEvent::WordChoices {
                        word: original,
                        choices,
                        bounds,
                    })
                    .await;
            }
            Err(e) => self.say_error(&format!("suggestions: {e}")).await,
        }
    }

    /// The chosen candidate goes over the marked part, shaped for its place.
    async fn apply_choice(&mut self, text: String) {
        let Some(original) = self.choice_original.take() else {
            return;
        };
        if self.hold.is_some() {
            self.say_error("suggestions: a correction is running").await;
            return;
        }
        self.hold = Some(String::new());
        self.correcting = Some(CorrectionRange::Selection);
        let text = shape_correction(&original, &text, self.range_at_start.take(), &self.range_language);
        let chars = text.chars().count();
        let pasted = if std::mem::take(&mut self.choice_host) {
            // The host holds the marked part and puts the pick over it.
            let _ = self.events.send(EngineEvent::HostReplace { text }).await;
            Ok(())
        } else {
            tokio::task::spawn_blocking(move || range_keys::paste_choice(&text))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        };
        match pasted {
            Ok(()) => {
                let _ = self.events.send(EngineEvent::Corrected { changed: true, chars }).await;
            }
            Err(e) => self.say_error(&format!("suggestions: {e}")).await,
        }
        self.end_hold(true).await;
    }

    async fn apply_range_correction(
        &mut self,
        original: String,
        prior: Option<String>,
        result: Result<Option<String>, String>,
    ) {
        let host = std::mem::take(&mut self.host_mode);
        let outcome = match result {
            Ok(Some(text)) if host => {
                let chars = text.chars().count();
                let _ = self.events.send(EngineEvent::HostReplace { text }).await;
                Ok((true, chars))
            }
            Ok(Some(text)) => {
                // Where it goes decides the first letter; the range's own
                // spaces at both ends stay.
                let text = shape_correction(&original, &text, self.range_at_start.take(), &self.range_language);
                let chars = text.chars().count();
                tokio::task::spawn_blocking(move || paste_over_selection(&text, prior))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
                    .map(|_| (true, chars))
            }
            Ok(None) => {
                let chars = original.chars().count();
                if !host {
                    let _ = tokio::task::spawn_blocking(move || leave_selection(prior)).await;
                }
                Ok((false, chars))
            }
            Err(e) => {
                if !host {
                    let _ = tokio::task::spawn_blocking(move || leave_selection(prior)).await;
                }
                Err(e)
            }
        };
        let changed = matches!(outcome, Ok((true, _)));
        match outcome {
            Ok((changed, chars)) => {
                let _ = self.events.send(EngineEvent::Corrected { changed, chars }).await;
            }
            Err(e) => self.say_error(&format!("correction: {e}")).await,
        }
        self.end_hold(changed).await;
    }

    /// The correction is through: the engine's picture of the field is
    /// stale where text was replaced or the caret moved, so the typed
    /// history is forgotten (new words append at the caret, nothing is
    /// rewritten), the voice commands' caret is placed, and the words held
    /// back meanwhile are typed.
    async fn end_hold(&mut self, changed: bool) {
        let range = self.correcting.take();
        let moved = changed
            || matches!(
                range,
                Some(CorrectionRange::ClickToClick | CorrectionRange::Word | CorrectionRange::Selection)
            );
        if moved {
            self.live.reset();
            self.last_segment = None;
            if let Some(c) = self.correction.as_mut() {
                c.checked.clear();
            }
        }
        self.dictation.caret_from_end = match range {
            Some(CorrectionRange::CursorToEnd | CorrectionRange::WholeText) => Some(0),
            _ => None,
        };
        let held = self.hold.take().unwrap_or_default();
        info!(
            held_chars = held.chars().count(),
            moved, "correction done; the field is free again"
        );
        if !held.trim().is_empty() {
            let _ = self.emit_text(&held).await;
        }
    }

    /// The text so far ends with an opening quotation mark (or a line
    /// break): the next word joins without a space.
    fn after_opening_quote(&self) -> bool {
        // “ opens a quotation in English and closes one in German: only
        // the language's own opening mark counts.
        let opening = commands::quotation_marks(&self.language).0;
        self.session_text.ends_with(opening)
            || self
                .session_text
                .chars()
                .next_back()
                .is_some_and(|c| matches!(c, '„' | '«' | '‹' | '‘' | '\u{202F}' | '\n' | '(' | '['))
    }

    async fn say_error(&mut self, msg: &str) {
        warn!("{msg}");
        let _ = self.events.send(EngineEvent::Error(msg.to_string())).await;
    }

    async fn transfer(&mut self) -> Result<(), EngineError> {
        match self.sink.probe() {
            TargetStatus::Injectable(_) | TargetStatus::Unknown => {}
            TargetStatus::PasswordField => return Err(EngineError::PasswordField),
            other => return Err(EngineError::NotInjectable(other)),
        }
        let text = std::mem::take(&mut self.session_text);
        // Text from the dictation window goes in at once.
        self.insert_style = ikk_output::InsertStyle::Paste;
        self.sink.set_insert_style(self.insert_style);
        if self.sink.access_level() == AccessLevel::Host {
            let _ = self
                .events
                .send(EngineEvent::HostText {
                    text: text.clone(),
                    rhythm: false,
                    pauses_ms: Vec::new(),
                })
                .await;
        }
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
        let n = text.chars().count();
        if let Some(held) = self.hold.as_mut() {
            // A correction is working in the field: typed afterwards.
            held.push_str(text);
            return Ok(n);
        }
        self.session_text.push_str(text);
        if self.output_mode == OutputMode::Fallback {
            return Ok(n);
        }
        // Only the joining space is dropped here; a line break is text.
        // No joining space (a sign, a line break, the word after one):
        // the typer must not put one in either.
        let tight = !text.starts_with(' ');
        let segment = text.trim_start_matches(' ');
        self.typed_log.push((Instant::now(), n));
        if self.sink.access_level() == AccessLevel::Host {
            let rhythm = self.insert_style == ikk_output::InsertStyle::Rhythm;
            let pauses_ms = if rhythm {
                self.host_rhythm
                    .plan(text)
                    .iter()
                    .map(|d| d.as_millis().min(u16::MAX as u128) as u16)
                    .collect()
            } else {
                Vec::new()
            };
            let _ = self
                .events
                .send(EngineEvent::HostText {
                    text: text.to_string(),
                    rhythm,
                    pauses_ms,
                })
                .await;
        }
        match self.live.append_joined(segment, tight, &mut *self.sink) {
            Ok(id) => {
                self.last_segment = Some(id);
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

/// The microphone settings of the start word listener. The words were
/// recorded without AGC and preamp (ikk-ffi record_take); the start word
/// must be heard the same way.
fn standby_capture(start: &StartConfig) -> ikk_audio::CaptureConfig {
    let mut capture = start.capture.clone();
    capture.agc = false;
    capture.preamp_gain_db = 0.0;
    capture
}

/// CaptureConfig has no PartialEq (floats); its Debug text covers every field.
fn same_standby_capture(a: &StartConfig, b: &StartConfig) -> bool {
    format!("{:?}", standby_capture(a)) == format!("{:?}", standby_capture(b))
}

#[cfg(test)]
mod tests;

/// Does a range that follows `before` (the few characters in front of
/// it) begin a sentence? `None` when that cannot be told.
fn sentence_start(before: &str) -> Option<bool> {
    let line_end = before.trim_end_matches([' ', '\u{a0}', '\t']).ends_with(['\n', '\r']);
    if line_end {
        return Some(true);
    }
    let t = before.trim_end();
    // Closing quotation marks and brackets do not hide the sign before them.
    let t = t.trim_end_matches(['"', '”', '“', '»', '«', '\'', '’', ')', ']']);
    let last = t.chars().next_back()?;
    Some(matches!(last, '.' | '!' | '?' | '…'))
}

/// The whitespace a selected range begins and ends with, and what lies
/// between: a replacement is put between the same two pieces, so it
/// starts and stops exactly where the range did and no space is added
/// or lost at either end.
fn split_edges(original: &str) -> (&str, &str, &str) {
    let body = original.trim();
    if body.is_empty() {
        return ("", "", "");
    }
    let start = original.len() - original.trim_start().len();
    (&original[..start], body, &original[start + body.len()..])
}

/// The first letter of `text` in upper or lower case.
fn with_first(text: &str, upper: bool) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if upper => c.to_uppercase().chain(chars).collect(),
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The spoken words shaped to the place they go. The first letter: a
/// capital at the start of a sentence; inside a sentence small, unless
/// the word is a noun or a name (the spelling dictionary knows only its
/// capitalised form) or unknown; when the place cannot be told
/// (`at_start` is `None`) it follows the replaced text. A sign that ended
/// the replaced text (a comma, a full stop) is kept when the speaker did
/// not say one, and the range's own spaces at both ends stay as they were.
fn shape_replacement(original: &str, spoken: &str, at_start: Option<bool>, language: &str) -> String {
    let mut text = spoken.trim().to_string();
    if text.is_empty() {
        return text;
    }
    let (lead, orig, trail) = split_edges(original);
    let first_orig = orig.chars().next();
    let first_new = text.chars().next();
    let first_word: String = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(|c: char| ".,;:!?".contains(c))
        .to_string();
    match (at_start, first_new) {
        (Some(true), Some(_)) => text = with_first(&text, true),
        (Some(false), Some(n)) => match ikk_ondevice::capital_only(&first_word, language) {
            // A noun or a name keeps (or gets) its capital.
            Some(true) => text = with_first(&text, true),
            // An ordinary word: the recogniser's sentence capital goes.
            Some(false) => text = with_first(&text, false),
            // Unknown to the dictionary: an ordinary word of the word list
            // loses the capital, anything else (a name) stays as heard.
            None => {
                if n.is_uppercase() && ikk_text::dictionary::is_word(&with_first(&first_word, false)) {
                    text = with_first(&text, false);
                }
            }
        },
        _ => {
            // The place is unknown: the case follows the replaced text.
            // Capitalised stays capitalised (a sentence start, a noun);
            // for a lowercase word the recogniser's capital goes again
            // when the lowercase form is an ordinary word.
            if let (Some(o), Some(n)) = (first_orig, first_new) {
                if o.is_uppercase() && n.is_lowercase() {
                    text = with_first(&text, true);
                } else if o.is_lowercase()
                    && n.is_uppercase()
                    && ikk_text::dictionary::is_word(&with_first(&first_word, false))
                {
                    text = with_first(&text, false);
                }
            }
        }
    }
    // A full stop the recogniser added to a short replacement goes when
    // the replaced text had none.
    if text.ends_with('.') && !orig.ends_with(|c: char| ".,;:!?".contains(c)) && text.split_whitespace().count() <= 3 {
        text.pop();
    }
    let last_orig = orig.chars().next_back();
    let last_new = text.chars().next_back();
    if let (Some(o), Some(n)) = (last_orig, last_new) {
        if ".,;:!?".contains(o) && !".,;:!?".contains(n) {
            text.push(o);
        }
    }
    format!("{lead}{text}{trail}")
}

/// A corrector's answer shaped to the place it goes: between the range's
/// own spaces, with a capital at the start of a sentence. Inside a
/// sentence the first word is small when the spelling dictionary knows it
/// as an ordinary word (a corrector reads a piece of a sentence as a
/// sentence and capitalises it), and also when it was small before and
/// the dictionary does not know it; a noun or a name keeps its capital.
fn shape_correction(original: &str, answer: &str, at_start: Option<bool>, language: &str) -> String {
    let (lead, orig, trail) = split_edges(original);
    let mut text = answer.trim().to_string();
    if text.is_empty() {
        return text;
    }
    let first_word: String = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(|c: char| ".,;:!?".contains(c))
        .to_string();
    let was_small = orig.chars().next().is_some_and(char::is_lowercase);
    match at_start {
        Some(true) => text = with_first(&text, true),
        Some(false) => match ikk_ondevice::capital_only(&first_word, language) {
            Some(true) => text = with_first(&text, true),
            Some(false) => text = with_first(&text, false),
            None if was_small => text = with_first(&text, false),
            None => {}
        },
        None => {}
    }
    format!("{lead}{text}{trail}")
}

mod range_keys {
    //! The correction button's keystrokes: select, copy, paste, and the
    //! user's clipboard put back afterwards.
    use std::thread::sleep;
    use std::time::Duration;

    use ikk_output::desktop as d;
    use ikk_output::{Chord, Key, Modifiers};

    use super::CorrectionRange;

    /// Something no app would have on the clipboard: tells a served copy
    /// from a stale clipboard.
    const MARKER: &str = "\u{200B}ikk-copy\u{200B}";

    /// The characters in front of the last range taken (and when they were
    /// read): what tells a sentence start from the middle of a sentence.
    static CONTEXT: std::sync::Mutex<Option<(String, std::time::Instant)>> = std::sync::Mutex::new(None);

    fn stash(context: Option<String>) {
        if let Ok(mut c) = CONTEXT.lock() {
            *c = context.map(|t| (t, std::time::Instant::now()));
        }
    }

    /// The context of the range just taken, once; nothing when it is
    /// older than a few seconds (it belonged to another range).
    pub fn take_context() -> Option<String> {
        let (text, at) = CONTEXT.lock().ok()?.take()?;
        (at.elapsed() < Duration::from_secs(5)).then_some(text)
    }

    /// Read the three characters in front of the cursor (no selection):
    /// select them, copy, and put the cursor back. `None` at the very
    /// start of a text, where there is nothing to select. The user's
    /// clipboard is left as it was.
    fn before_cursor() -> Option<String> {
        let prior = d::clipboard_text();
        let _ = d::set_clipboard_text(MARKER);
        let back = Chord::Key(
            Key::Left,
            Modifiers {
                shift: true,
                command: false,
                word: false,
            },
        );
        let mut text = None;
        if d::press_chord(back, 3).is_ok() {
            sleep(Duration::from_millis(60));
            if d::press_chord(Chord::shortcut('c'), 1).is_ok() {
                for _ in 0..8 {
                    sleep(Duration::from_millis(50));
                    match d::clipboard_text() {
                        Some(t) if t != MARKER => {
                            text = Some(t);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            // Something was selected: back to where the cursor stood (the
            // right end of those characters).
            if text.is_some() {
                let _ = d::press_chord(Chord::key(Key::Right), 1);
                sleep(Duration::from_millis(40));
            }
        }
        restore(prior);
        text
    }

    /// For a caller that places the cursor at the start of a range itself
    /// (a click) and extends the selection afterwards: read what stands
    /// in front of the cursor now and keep it for the range.
    pub fn note_context_at_cursor() {
        // Nothing read (the very start of a text, or a program that did
        // not answer): the place stays unknown and the old rule decides.
        stash(before_cursor());
    }

    /// How many presses of Shift+Right select `text` again, when that can
    /// be counted safely (plain characters, not too many).
    fn steps(text: &str) -> Option<usize> {
        let plain = text.chars().all(|c| {
            (c as u32) < 0x1_0000 && !('\u{0300}'..='\u{036F}').contains(&c) && c != '\u{200D}' && c != '\u{FE0F}'
        });
        let n = text.replace("\r\n", "\n").chars().count();
        (plain && n <= 200).then_some(n)
    }

    pub fn select_and_copy(range: CorrectionRange) -> Result<(String, Option<String>), String> {
        // Cursor to end: the cursor stands at the start of the range, so
        // what is in front of it can be read before anything is selected.
        match range {
            CorrectionRange::CursorToEnd => note_context_at_cursor(),
            CorrectionRange::WholeText => stash(Some("\n".into())),
            _ => {}
        }
        let prior = d::clipboard_text();
        let _ = d::set_clipboard_text(MARKER);
        let select = match range {
            CorrectionRange::Selection | CorrectionRange::Word | CorrectionRange::ClickToClick => None,
            CorrectionRange::WholeText => Some(Chord::shortcut('a')),
            _ => Some({
                let m = Modifiers {
                    shift: true,
                    command: true,
                    word: false,
                };
                // Cmd+Shift+Down (macOS) / Ctrl+Shift+End: to the end of the text.
                if cfg!(target_os = "macos") {
                    Chord::Key(Key::Down, m)
                } else {
                    Chord::Key(Key::End, m)
                }
            }),
        };
        if let Some(select) = select {
            d::press_chord(select, 1).map_err(|e| e.to_string())?;
            sleep(Duration::from_millis(80));
        }
        d::press_chord(Chord::shortcut('c'), 1).map_err(|e| e.to_string())?;
        // The app in front needs a moment to serve the copy.
        let mut text = String::new();
        for _ in 0..12 {
            sleep(Duration::from_millis(60));
            if let Some(t) = d::clipboard_text() {
                if t != MARKER {
                    text = t;
                    break;
                }
            }
        }
        if text.trim().is_empty() {
            restore(prior);
            return Err("nothing selected to correct".into());
        }
        // A selection that was there already (a double-clicked word) and
        // whose context nobody read: step to its start, read, and select
        // the same characters again.
        let unread = CONTEXT.lock().map(|c| c.is_none()).unwrap_or(false);
        if unread && matches!(range, CorrectionRange::Selection | CorrectionRange::Word) {
            if let Some(n) = steps(&text) {
                if d::press_chord(Chord::key(Key::Left), 1).is_ok() {
                    sleep(Duration::from_millis(40));
                    note_context_at_cursor();
                    let forward = Chord::Key(
                        Key::Right,
                        Modifiers {
                            shift: true,
                            command: false,
                            word: false,
                        },
                    );
                    let _ = d::press_chord(forward, n);
                    sleep(Duration::from_millis(60));
                }
            }
        }
        Ok((text, prior))
    }

    pub fn paste_over_selection(text: &str, prior: Option<String>) -> Result<(), String> {
        d::paste_text(text).map_err(|e| e.to_string())?;
        // The paste helper puts the copied selection back after a moment;
        // the user's own clipboard comes back after that.
        std::thread::spawn(move || {
            sleep(Duration::from_millis(3500));
            restore(prior);
        });
        Ok(())
    }

    /// Up to `words` words in front of the cursor (`back`) or after it:
    /// selected word by word, copied, and the cursor put back. `None`
    /// where there is nothing, or the program did not serve the copy.
    fn beside_cursor(back: bool, words: usize) -> Option<String> {
        let _ = d::set_clipboard_text(MARKER);
        let (out, home) = if back {
            (Key::Left, Key::Right)
        } else {
            (Key::Right, Key::Left)
        };
        let reach = Chord::Key(
            out,
            Modifiers {
                shift: true,
                command: false,
                word: true,
            },
        );
        let mut text = None;
        if d::press_chord(reach, words).is_ok() {
            sleep(Duration::from_millis(80));
            if d::press_chord(Chord::shortcut('c'), 1).is_ok() {
                for _ in 0..8 {
                    sleep(Duration::from_millis(50));
                    match d::clipboard_text() {
                        Some(t) if t != MARKER => {
                            text = Some(t);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            // Something was selected: back to where the cursor stood, the
            // end of the selection it started from.
            if text.is_some() {
                let _ = d::press_chord(Chord::key(home), 1);
                sleep(Duration::from_millis(40));
            }
        }
        text
    }

    /// The text around a selection whose text is `marked`: what stands in
    /// front of it and after it (some sentences each). The selection is
    /// the same afterwards. `None` when the selection cannot be stepped
    /// over safely; the caller restores the clipboard.
    pub fn read_around(marked: &str) -> Option<(String, String)> {
        let n = steps(marked)?;
        // To the start of the selection, and read backwards from there.
        d::press_chord(Chord::key(Key::Left), 1).ok()?;
        sleep(Duration::from_millis(40));
        let before = beside_cursor(true, 40).unwrap_or_default();
        // Over the marked part, and read on from its end.
        let _ = d::press_chord(Chord::key(Key::Right), n);
        sleep(Duration::from_millis(40));
        let after = beside_cursor(false, 25).unwrap_or_default();
        // Mark the same characters again.
        let again = Chord::Key(
            Key::Left,
            Modifiers {
                shift: true,
                command: false,
                word: false,
            },
        );
        let _ = d::press_chord(again, n);
        sleep(Duration::from_millis(60));
        Some((before, after))
    }

    /// The marked part with the text in front of it and behind it, from
    /// the accessibility tree of the program in front: no key is pressed.
    pub fn around_quietly() -> Option<(String, String, String)> {
        d::selection_around(super::AROUND_BEFORE, super::AROUND_AFTER)
    }

    /// Where the marked part stands on the screen, when the program tells.
    pub fn marked_bounds() -> Option<(f64, f64, f64, f64)> {
        d::selection_bounds()
    }

    /// The selection was only read and stays as it is.
    pub fn give_back_clipboard(prior: Option<String>) {
        restore(prior);
    }

    /// Paste over a selection made earlier, keeping the user's clipboard.
    pub fn paste_choice(text: &str) -> Result<(), String> {
        paste_over_selection(text, d::clipboard_text())
    }

    /// Nothing to change: collapse the selection to its end.
    pub fn leave_selection(prior: Option<String>) -> Result<(), String> {
        let _ = d::press_chord(Chord::key(Key::Right), 1);
        restore(prior);
        Ok(())
    }

    fn restore(prior: Option<String>) {
        let _ = d::set_clipboard_text(prior.as_deref().unwrap_or(""));
    }
}

use range_keys::{leave_selection, paste_over_selection, select_and_copy, take_context};

/// For the shell: the cursor was just placed at the start of a range (a
/// click); read what stands in front of it before the range is selected.
pub fn note_range_context() {
    range_keys::note_context_at_cursor();
}
