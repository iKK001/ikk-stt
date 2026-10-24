use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ikk_audio::{AudioError, AudioFrame, Capture};
use ikk_output::{AccessLevel, OutputError, TargetStatus, TextSink};
use ikk_stt::*;
use tokio::sync::mpsc;

use super::*;

// ---------- fakes ----------

/// Provider whose streaming session turns every pushed frame into a final
/// "f<n>" after cut_segment, so tests control exactly when finals appear.
struct FakeProvider;

struct FakeSession {
    pushed: usize,
    tx: mpsc::Sender<TranscriptEvent>,
    rx: mpsc::Receiver<TranscriptEvent>,
}

#[async_trait::async_trait]
impl StreamingSession for FakeSession {
    async fn push(&mut self, _f: AudioFrame) -> Result<(), SttError> {
        self.pushed += 1;
        let _ = self
            .tx
            .send(TranscriptEvent::Interim(format!("i{}", self.pushed)))
            .await;
        Ok(())
    }
    async fn cut_segment(&mut self) -> Result<(), SttError> {
        let _ = self.tx.send(TranscriptEvent::Final(format!("f{}", self.pushed))).await;
        Ok(())
    }
    async fn finish(&mut self) -> Result<(), SttError> {
        let _ = self.tx.send(TranscriptEvent::Final("tail".into())).await;
        let _ = self.tx.send(TranscriptEvent::Closed).await;
        Ok(())
    }
    fn events(&mut self) -> &mut mpsc::Receiver<TranscriptEvent> {
        &mut self.rx
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> ProviderId {
        ProviderId("fake".into())
    }
    fn display_name(&self) -> &'static str {
        "Fake"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            interim: true,
            requires_network: true,
            ..Default::default()
        }
    }
    async fn transcribe(&self, _: AudioClip, _: &SessionConfig) -> Result<Transcript, SttError> {
        Err(SttError::Unsupported)
    }
    async fn stream(&self, _: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        let (tx, rx) = mpsc::channel(64);
        Ok(Box::new(FakeSession { pushed: 0, tx, rx }))
    }
}

/// Capture fed by the test through a channel.
struct FakeCapture {
    rx: mpsc::Receiver<AudioFrame>,
}
impl Capture for FakeCapture {
    fn frames(&mut self) -> &mut mpsc::Receiver<AudioFrame> {
        &mut self.rx
    }
    fn level_dbfs(&self) -> f32 {
        -30.0
    }
    fn stop(self: Box<Self>) {}
}

#[derive(Clone, Default)]
struct SinkLog {
    typed: Arc<StdMutex<Vec<String>>>,
}

struct FakeSink {
    status: TargetStatus,
    typing_ok: bool,
    log: SinkLog,
}
impl TextSink for FakeSink {
    fn access_level(&self) -> AccessLevel {
        AccessLevel::BasicTyping
    }
    fn probe(&mut self) -> TargetStatus {
        self.status.clone()
    }
    fn type_text(&mut self, text: &str) -> Result<(), OutputError> {
        if self.typing_ok {
            self.log.typed.lock().unwrap().push(text.into());
            Ok(())
        } else {
            Err(OutputError::Failed("dropped".into()))
        }
    }
    fn backspace(&mut self, _: usize) -> Result<(), OutputError> {
        Ok(())
    }
}

fn speech_frame() -> AudioFrame {
    AudioFrame {
        samples: vec![0.3; 1600],
        sample_rate_hz: 16_000,
        offset: Duration::ZERO,
    }
}
fn silence_frame() -> AudioFrame {
    AudioFrame {
        samples: vec![0.0005; 1600],
        sample_rate_hz: 16_000,
        offset: Duration::ZERO,
    }
}

fn start_cfg() -> StartConfig {
    StartConfig {
        session: SessionConfig::new("fake"),
        capture: ikk_audio::CaptureConfig::default(),
        text: ikk_text::TextConfig::default(),
        vad_mode: ikk_audio::vad::VadMode::Normal,
        pause: Duration::from_millis(300),
        command_locale: "de-CH".into(),
        flow_ms: 0,
    }
}

struct Rig {
    handle: EngineHandle,
    frames: mpsc::Sender<AudioFrame>,
    log: SinkLog,
}

fn rig(status: TargetStatus, typing_ok: bool) -> Rig {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(FakeProvider));
    let log = SinkLog::default();
    let sink = FakeSink {
        status,
        typing_ok,
        log: log.clone(),
    };
    let (ftx, frx) = mpsc::channel(256);
    let frx = Arc::new(StdMutex::new(Some(frx)));
    let opener: CaptureOpener = Box::new(move |_cfg| {
        let rx = frx.lock().unwrap().take().ok_or(AudioError::NoDevice)?;
        Ok(Box::new(FakeCapture { rx }) as Box<dyn Capture>)
    });
    let handle = Engine::spawn_with(reg, Box::new(sink), opener);
    Rig {
        handle,
        frames: ftx,
        log,
    }
}

async fn collect(handle: &mut EngineHandle, until: impl Fn(&EngineEvent) -> bool, max: Duration) -> Vec<EngineEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + max;
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, handle.events.recv()).await {
        let done = until(&ev);
        out.push(ev);
        if done {
            break;
        }
    }
    out
}

// ---------- tests ----------

#[tokio::test]
async fn consent_gate_then_full_dictation_into_target() {
    let Rig {
        mut handle,
        frames,
        log,
    } = rig(TargetStatus::Injectable(AccessLevel::BasicTyping), true);

    handle.send(EngineCommand::Start(Box::new(start_cfg()))).await;
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::ConsentRequired(_)),
        Duration::from_secs(1),
    )
    .await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::State(EngineState::AwaitingConsent))));

    handle.send(EngineCommand::GrantConsent("fake".into())).await;
    handle.send(EngineCommand::Start(Box::new(start_cfg()))).await;
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Listening)),
        Duration::from_secs(1),
    )
    .await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::State(EngineState::Listening))));

    // Speech, then a pause long enough for the VAD to close the segment.
    for _ in 0..5 {
        frames.send(speech_frame()).await.unwrap();
    }
    for _ in 0..12 {
        frames.send(silence_frame()).await.unwrap();
    }
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::Typed(_)),
        Duration::from_secs(2),
    )
    .await;
    assert!(evs.iter().any(|e| matches!(e, EngineEvent::Speech(true))), "{evs:?}");
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::Transcript(TranscriptEvent::Interim(_)))));
    assert!(evs.iter().any(|e| matches!(e, EngineEvent::Typed(_))));
    let first = log.typed.lock().unwrap()[0].clone();
    assert!(first.starts_with('f'), "{first}");

    handle.send(EngineCommand::Stop).await;
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Idle)),
        Duration::from_secs(3),
    )
    .await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::State(EngineState::Finishing))));
    // The session's finish() tail was typed too.
    let typed = log.typed.lock().unwrap().clone();
    assert_eq!(typed.len(), 2, "{typed:?}");
    assert_eq!(typed[1], " tail");
    handle.shutdown().await;
}

#[tokio::test]
async fn elevated_target_opens_fallback_and_transfers_later() {
    let Rig {
        mut handle,
        frames,
        log,
    } = rig(TargetStatus::Elevated, true);
    handle.send(EngineCommand::GrantConsent("fake".into())).await;
    handle.send(EngineCommand::Start(Box::new(start_cfg()))).await;
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Listening)),
        Duration::from_secs(1),
    )
    .await;
    assert!(evs.iter().any(|e| matches!(
        e,
        EngineEvent::FallbackRequired {
            reason: FallbackReason::Elevated,
            ..
        }
    )));

    for _ in 0..5 {
        frames.send(speech_frame()).await.unwrap();
    }
    for _ in 0..12 {
        frames.send(silence_frame()).await.unwrap();
    }
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::Transcript(TranscriptEvent::Final(_))),
        Duration::from_secs(2),
    )
    .await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::Transcript(TranscriptEvent::Final(t)) if t.starts_with('f'))));
    assert!(log.typed.lock().unwrap().is_empty(), "fallback must not type");

    handle.send(EngineCommand::Stop).await;
    collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Idle)),
        Duration::from_secs(3),
    )
    .await;
    // Probe still says Elevated → transfer refused, text kept.
    handle.send(EngineCommand::TransferToTarget).await;
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::Error(_)),
        Duration::from_secs(1),
    )
    .await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::Error(m) if m.contains("Elevated"))));
    handle.shutdown().await;
}

#[tokio::test]
async fn injection_failure_switches_to_fallback_keeping_text() {
    let Rig { mut handle, frames, .. } = rig(TargetStatus::Unknown, false);
    handle.send(EngineCommand::GrantConsent("fake".into())).await;
    handle.send(EngineCommand::Start(Box::new(start_cfg()))).await;
    collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Listening)),
        Duration::from_secs(1),
    )
    .await;
    for _ in 0..5 {
        frames.send(speech_frame()).await.unwrap();
    }
    for _ in 0..12 {
        frames.send(silence_frame()).await.unwrap();
    }
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::FallbackRequired { .. }),
        Duration::from_secs(2),
    )
    .await;
    assert!(evs.iter().any(|e| matches!(
        e,
        EngineEvent::FallbackRequired { reason: FallbackReason::InjectionFailed(_), carried_text } if carried_text.starts_with('f')
    )));
    handle.shutdown().await;
}

#[tokio::test]
async fn spoken_stop_command_ends_session() {
    // Provider that emits the command phrase as a final.
    struct CmdProvider;
    #[async_trait::async_trait]
    impl Provider for CmdProvider {
        fn id(&self) -> ProviderId {
            ProviderId("fake".into())
        }
        fn display_name(&self) -> &'static str {
            "Cmd"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                streaming: true,
                ..Default::default()
            }
        }
        async fn transcribe(&self, _: AudioClip, _: &SessionConfig) -> Result<Transcript, SttError> {
            Err(SttError::Unsupported)
        }
        async fn stream(&self, _: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
            let (tx, rx) = mpsc::channel(8);
            tx.send(TranscriptEvent::Final("Diktat Stopp".into())).await.unwrap();
            Ok(Box::new(FakeSession { pushed: 0, tx, rx }))
        }
    }
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(CmdProvider));
    let log = SinkLog::default();
    let (_ftx, frx) = mpsc::channel(8);
    let frx = Arc::new(StdMutex::new(Some(frx)));
    let opener: CaptureOpener = Box::new(move |_| {
        Ok(Box::new(FakeCapture {
            rx: frx.lock().unwrap().take().unwrap(),
        }) as Box<dyn Capture>)
    });
    let mut handle = Engine::spawn_with(
        reg,
        Box::new(FakeSink {
            status: TargetStatus::Unknown,
            typing_ok: true,
            log: log.clone(),
        }),
        opener,
    );
    handle.send(EngineCommand::Start(Box::new(start_cfg()))).await;
    let evs = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Idle)),
        Duration::from_secs(3),
    )
    .await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::Command(Intent::StopDictation))));
    assert!(evs
        .iter()
        .any(|e| matches!(e, EngineEvent::State(EngineState::Listening))));
    assert!(
        log.typed.lock().unwrap().iter().all(|t| !t.contains("Stopp")),
        "command text must not be typed"
    );
    handle.shutdown().await;
}
