use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use ikk_stt::{
    AudioClip, Capabilities, ErrorClass, Provider, ProviderId, SessionConfig, Setup, StreamingSession, SttError,
    Transcript, TranscriptEvent,
};
use tokio::sync::mpsc;
use tracing::warn;
use windows::core::{Interface, HSTRING};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Globalization::Language;
use windows::Media::SpeechRecognition::{
    ISpeechRecognitionConstraint, SpeechContinuousRecognitionCompletedEventArgs,
    SpeechContinuousRecognitionResultGeneratedEventArgs, SpeechContinuousRecognitionSession,
    SpeechRecognitionHypothesisGeneratedEventArgs, SpeechRecognitionListConstraint, SpeechRecognitionResultStatus,
    SpeechRecognitionScenario, SpeechRecognitionTopicConstraint, SpeechRecognizer,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

use crate::{language_tag, PROVIDER_ID};

/// Windows refuses cloud dictation while the privacy switch is off.
const HRESULT_PRIVACY_NOT_ACCEPTED: i32 = 0x8004_5509_u32 as i32;
const PRIVACY_SETTINGS: &str = "ms-settings:privacy-speech";
const PRIVACY_MESSAGE: &str = "Windows speech recognition needs \"Online speech recognition\" switched on in the \
     Windows privacy settings (Privacy & security > Speech).";

static FIRST_HYPOTHESIS: std::sync::Once = std::sync::Once::new();

/// 100-ns ticks for a WinRT TimeSpan.
fn span_ms(ms: i64) -> TimeSpan {
    TimeSpan { Duration: ms * 10_000 }
}

/// The user's "Online speech recognition" switch.
fn online_speech_allowed() -> bool {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(r"Software\Microsoft\Speech_OneCore\Settings\OnlineSpeechPrivacy"),
            &HSTRING::from("HasAccepted"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok() && value == 1
}

#[derive(Default)]
pub struct WindowsSpeech;

#[async_trait::async_trait]
impl Provider for WindowsSpeech {
    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.into())
    }

    fn display_name(&self) -> &'static str {
        "Windows speech recognition (Microsoft)"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            batch: false,
            streaming: true,
            interim: true,
            batch_replacement: false,
            required_sample_rate_hz: None,
            languages: ["deu", "eng", "fra", "ita", "spa", "por", "nld"]
                .map(String::from)
                .to_vec(),
            requires_network: true,
            own_audio: true,
        }
    }

    async fn transcribe(&self, _audio: AudioClip, _cfg: &SessionConfig) -> Result<Transcript, SttError> {
        Err(SttError::Unsupported)
    }

    async fn stream(&self, cfg: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        let tag = language_tag(&cfg.language).map(str::to_string);
        let (events_tx, events) = mpsc::channel(256);
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), SttError>>();
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("ikk-windows-speech".into())
            .spawn(move || run(tag, events_tx, ready_tx, stop_rx))
            .map_err(|e| SttError::Provider(ErrorClass::Other(e.to_string())))?;
        let started = tokio::task::spawn_blocking(move || ready_rx.recv())
            .await
            .map_err(|e| SttError::Provider(ErrorClass::Other(e.to_string())))?
            .unwrap_or_else(|_| Err(SttError::Provider(ErrorClass::Other("recognizer thread ended".into()))));
        started?;
        Ok(Box::new(WindowsSession {
            events,
            stop: Some(stop_tx),
            worker: Some(worker),
        }))
    }

    fn setup_needed(&self) -> Option<Setup> {
        (!online_speech_allowed()).then(|| Setup {
            message: PRIVACY_MESSAGE.into(),
            settings_url: PRIVACY_SETTINGS.into(),
        })
    }
}

struct WindowsSession {
    events: mpsc::Receiver<TranscriptEvent>,
    stop: Option<std_mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

#[async_trait::async_trait]
impl StreamingSession for WindowsSession {
    async fn push(&mut self, _frame: ikk_audio::AudioFrame) -> Result<(), SttError> {
        Ok(()) // the recognizer records the microphone itself
    }

    async fn cut_segment(&mut self) -> Result<(), SttError> {
        Ok(()) // Windows closes segments at its own speech pauses
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
        Ok(())
    }

    fn events(&mut self) -> &mut mpsc::Receiver<TranscriptEvent> {
        &mut self.events
    }
}

/// Owns the recognizer on one thread: set up, report readiness, wait for
/// stop, shut down, close.
fn run(
    tag: Option<String>,
    events: mpsc::Sender<TranscriptEvent>,
    ready: std_mpsc::Sender<Result<(), SttError>>,
    stop: std_mpsc::Receiver<()>,
) {
    // Windows delivers the recognizer's results and completions through the
    // message queue of the thread that owns it, like a window thread: so
    // this thread is a single-threaded apartment and keeps pumping.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let session = match start(tag.as_deref(), &events) {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            if com.is_ok() {
                unsafe { CoUninitialize() };
            }
            return;
        }
    };
    while stop.try_recv() == Err(std_mpsc::TryRecvError::Empty) {
        pump();
        std::thread::sleep(PUMP_INTERVAL);
    }
    if let Ok(op) = session.1.StopAsync() {
        if wait(|| op.Status().map(|s| s.0 != 0).unwrap_or(true), STOP_TIMEOUT) {
            let _ = op.GetResults();
        } else {
            warn!("Windows speech: stop not confirmed in time");
        }
    }
    drop(session);
    let _ = events.blocking_send(TranscriptEvent::Closed);
    if com.is_ok() {
        unsafe { CoUninitialize() };
    }
}

const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
const SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Process this thread's pending window messages (results, completions).
fn pump() {
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Wait for a Windows operation while pumping messages; false on timeout.
fn wait(done: impl Fn() -> bool, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        pump();
        if done() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(PUMP_INTERVAL);
    }
}

fn timeout_err(what: &str) -> SttError {
    SttError::Provider(ErrorClass::Other(format!("Windows speech: {what} timed out")))
}

fn win_err(e: windows::core::Error) -> SttError {
    if e.code().0 == HRESULT_PRIVACY_NOT_ACCEPTED {
        SttError::Provider(ErrorClass::BadRequest(PRIVACY_MESSAGE.into()))
    } else {
        SttError::Provider(ErrorClass::Other(format!("Windows speech: {e}")))
    }
}

fn start(
    tag: Option<&str>,
    events: &mpsc::Sender<TranscriptEvent>,
) -> Result<(SpeechRecognizer, SpeechContinuousRecognitionSession), SttError> {
    tracing::info!(
        language = tag.unwrap_or("system default"),
        "Windows speech: preparing dictation"
    );
    let recognizer = match tag {
        Some(t) => {
            let lang = Language::CreateLanguage(&HSTRING::from(t)).map_err(win_err)?;
            SpeechRecognizer::Create(&lang).map_err(win_err)?
        }
        None => SpeechRecognizer::new().map_err(win_err)?,
    };
    // Diagnosis switches:
    // - IKK_WIN_SPEECH_WORDS="Hallo,Test": only these words, on the device.
    // - IKK_WIN_SPEECH_CONSTRAINT=none: no constraint, Windows' built-in
    //   dictation grammar; =websearch: the other online topic.
    // - IKK_WIN_SPEECH_TIMEOUTS=default: keep Windows' own waiting times.
    let mode = std::env::var("IKK_WIN_SPEECH_CONSTRAINT").unwrap_or_default();
    let keep_timeouts = std::env::var("IKK_WIN_SPEECH_TIMEOUTS").is_ok_and(|v| v == "default");
    tracing::info!(
        constraint = if mode.is_empty() { "dictation" } else { mode.as_str() },
        timeouts = if keep_timeouts { "windows defaults" } else { "long" },
        "Windows speech: mode"
    );
    let constraint: Option<ISpeechRecognitionConstraint> = match std::env::var("IKK_WIN_SPEECH_WORDS") {
        Ok(words) if !words.trim().is_empty() => {
            let list: Vec<HSTRING> = words.split(',').map(|w| HSTRING::from(w.trim())).collect();
            tracing::info!(words = %words, "Windows speech: word list on the device (diagnosis)");
            let iterable: windows_collections::IIterable<HSTRING> = list.into();
            Some(
                SpeechRecognitionListConstraint::Create(&iterable)
                    .and_then(|c| c.cast())
                    .map_err(win_err)?,
            )
        }
        _ if mode == "none" => None,
        _ => {
            let scenario = if mode == "websearch" {
                SpeechRecognitionScenario::WebSearch
            } else {
                SpeechRecognitionScenario::Dictation
            };
            Some(
                SpeechRecognitionTopicConstraint::Create(scenario, &HSTRING::from("dictation"))
                    .and_then(|c| c.cast())
                    .map_err(win_err)?,
            )
        }
    };
    if let Some(constraint) = constraint {
        recognizer
            .Constraints()
            .and_then(|c| c.Append(&constraint))
            .map_err(win_err)?;
    }
    let op = recognizer.CompileConstraintsAsync().map_err(win_err)?;
    if !wait(|| op.Status().map(|s| s.0 != 0).unwrap_or(true), SETUP_TIMEOUT) {
        return Err(timeout_err("preparing dictation"));
    }
    let compiled = op.GetResults().map_err(win_err)?;
    let status = compiled.Status().map_err(win_err)?;
    tracing::info!(status = status_text(status), "Windows speech: dictation prepared");
    if status != SpeechRecognitionResultStatus::Success {
        return Err(SttError::Provider(ErrorClass::BadRequest(format!(
            "Windows speech could not prepare dictation ({})",
            status_text(status)
        ))));
    }
    if !keep_timeouts {
        if let Ok(t) = recognizer.Timeouts() {
            let _ = t.SetInitialSilenceTimeout(span_ms(60 * 60 * 1000));
            let _ = t.SetEndSilenceTimeout(span_ms(1200));
        }
    }

    let tx = events.clone();
    recognizer
        .HypothesisGenerated(&TypedEventHandler::<
            SpeechRecognizer,
            SpeechRecognitionHypothesisGeneratedEventArgs,
        >::new(move |_, args| {
            if let Ok(text) = args.ok().and_then(|a| a.Hypothesis()).and_then(|h| h.Text()) {
                FIRST_HYPOTHESIS.call_once(|| tracing::info!("Windows speech: first live text arrived"));
                let _ = tx.try_send(TranscriptEvent::Interim(text.to_string()));
            }
            Ok(())
        }))
        .map_err(win_err)?;

    let session = recognizer.ContinuousRecognitionSession().map_err(win_err)?;
    // Keep listening through long pauses; the user stops dictation.
    if !keep_timeouts {
        let _ = session.SetAutoStopSilenceTimeout(span_ms(12 * 60 * 60 * 1000));
    }

    let tx = events.clone();
    session
        .ResultGenerated(&TypedEventHandler::<
            SpeechContinuousRecognitionSession,
            SpeechContinuousRecognitionResultGeneratedEventArgs,
        >::new(move |_, args| {
            if let Ok(result) = args.ok().and_then(|a| a.Result()) {
                let status = result.Status().unwrap_or(SpeechRecognitionResultStatus::Unknown);
                tracing::info!(status = status_text(status), "Windows speech: result");
                if status == SpeechRecognitionResultStatus::Success {
                    if let Ok(text) = result.Text() {
                        let text = text.to_string();
                        if !text.trim().is_empty() {
                            let _ = tx.try_send(TranscriptEvent::Final(text));
                        }
                    }
                }
            }
            Ok(())
        }))
        .map_err(win_err)?;

    let tx = events.clone();
    session
        .Completed(&TypedEventHandler::<
            SpeechContinuousRecognitionSession,
            SpeechContinuousRecognitionCompletedEventArgs,
        >::new(move |_, args| {
            let status = args
                .ok()
                .and_then(|a| a.Status())
                .unwrap_or(SpeechRecognitionResultStatus::Unknown);
            tracing::info!(status = status_text(status), "Windows speech: session completed");
            if status != SpeechRecognitionResultStatus::Success && status != SpeechRecognitionResultStatus::UserCanceled
            {
                warn!(status = status_text(status), "Windows speech session ended");
                let class = if status == SpeechRecognitionResultStatus::NetworkFailure {
                    ErrorClass::Transport
                } else {
                    ErrorClass::Other(format!("Windows speech ended: {}", status_text(status)))
                };
                let _ = tx.try_send(TranscriptEvent::Error(class));
                let _ = tx.try_send(TranscriptEvent::Closed);
            }
            Ok(())
        }))
        .map_err(win_err)?;

    let op = session.StartAsync().map_err(win_err)?;
    if !wait(|| op.Status().map(|s| s.0 != 0).unwrap_or(true), SETUP_TIMEOUT) {
        return Err(timeout_err("starting"));
    }
    op.GetResults().map_err(win_err)?;
    tracing::info!("Windows speech: listening (default microphone)");
    Ok((recognizer, session))
}

fn status_text(s: SpeechRecognitionResultStatus) -> &'static str {
    match s {
        SpeechRecognitionResultStatus::Success => "success",
        SpeechRecognitionResultStatus::TopicLanguageNotSupported => "language not supported",
        SpeechRecognitionResultStatus::GrammarLanguageMismatch => "language mismatch",
        SpeechRecognitionResultStatus::GrammarCompilationFailure => "grammar compilation failed",
        SpeechRecognitionResultStatus::AudioQualityFailure => "poor audio quality",
        SpeechRecognitionResultStatus::UserCanceled => "canceled",
        SpeechRecognitionResultStatus::TimeoutExceeded => "timeout",
        SpeechRecognitionResultStatus::PauseLimitExceeded => "pause too long",
        SpeechRecognitionResultStatus::NetworkFailure => "network failure",
        SpeechRecognitionResultStatus::MicrophoneUnavailable => "microphone unavailable",
        _ => "unknown",
    }
}
