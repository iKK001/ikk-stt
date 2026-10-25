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
            own_audio: false,
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

// ---------- offline voice commands ----------

/// Synthetic word: voiced sounds (pitch, formant 1, formant 2, seconds).
fn word(sounds: &[(f32, f32, f32, f32)], speed: f32, gain: f32) -> Vec<f32> {
    let sr = 16_000f32;
    let mut out = Vec::new();
    let mut phase = 0f32;
    for &(f0, f1, f2, dur) in sounds {
        let n = (dur * speed * sr) as usize;
        for i in 0..n {
            let env = (std::f32::consts::PI * i as f32 / n as f32).sin().max(0.0);
            phase += f0 / sr;
            let mut v = 0f32;
            for h in 1..40 {
                let fh = f0 * h as f32;
                if fh > 7000.0 {
                    break;
                }
                let w = 1.0 / (1.0 + ((fh - f1) / 90.0).powi(2)) + 0.6 / (1.0 + ((fh - f2) / 120.0).powi(2));
                v += w * (2.0 * std::f32::consts::PI * h as f32 * phase).sin();
            }
            out.push(gain * 0.1 * env * v);
        }
    }
    out
}

const START_WORD: &[(f32, f32, f32, f32)] = &[
    (120.0, 300.0, 2300.0, 0.18),
    (130.0, 700.0, 1200.0, 0.22),
    (110.0, 500.0, 1800.0, 0.15),
];
const STOP_WORD: &[(f32, f32, f32, f32)] = &[
    (150.0, 600.0, 1000.0, 0.12),
    (140.0, 350.0, 900.0, 0.25),
    (160.0, 750.0, 1700.0, 0.12),
];

/// Microphone the test speaks into; every open gets a fresh stream.
type Mic = Arc<StdMutex<Option<mpsc::Sender<AudioFrame>>>>;

async fn say(mic: &Mic, samples: &[f32]) {
    let tx = mic.lock().unwrap().clone().expect("microphone open");
    let mut padded = vec![0.0005f32; 16_000];
    padded.extend_from_slice(samples);
    padded.extend(std::iter::repeat_n(0.0005f32, 16_000));
    for chunk in padded.chunks(1600) {
        let _ = tx
            .send(AudioFrame {
                samples: chunk.to_vec(),
                sample_rate_hz: 16_000,
                offset: Duration::ZERO,
            })
            .await;
    }
}

#[tokio::test]
async fn voice_commands_start_and_stop_dictation() {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(FakeProvider));
    let mic: Mic = Arc::new(StdMutex::new(None));
    let opener_mic = mic.clone();
    let opener: CaptureOpener = Box::new(move |_cfg| {
        let (tx, rx) = mpsc::channel(512);
        *opener_mic.lock().unwrap() = Some(tx);
        Ok(Box::new(FakeCapture { rx }) as Box<dyn Capture>)
    });
    let sink = FakeSink {
        status: TargetStatus::Injectable(AccessLevel::BasicTyping),
        typing_ok: true,
        log: SinkLog::default(),
    };
    let mut handle = Engine::spawn_with(reg, Box::new(sink), opener);
    let takes = |w| vec![word(w, 1.0, 1.0), word(w, 0.9, 0.7), word(w, 1.12, 1.3)];
    let commands = ikk_wake::VoiceCommands::train(&takes(START_WORD), &takes(STOP_WORD)).unwrap();
    handle.send(EngineCommand::GrantConsent("fake".into())).await;
    handle
        .send(EngineCommand::SetVoiceCommands(Some(Box::new(VoiceSetup {
            commands,
            start: start_cfg(),
        }))))
        .await;
    let ev = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::VoiceStandby(true)),
        Duration::from_secs(3),
    )
    .await;
    assert!(
        ev.iter().any(|e| matches!(e, EngineEvent::VoiceStandby(true))),
        "standby: {ev:?}"
    );

    say(&mic, &word(START_WORD, 1.05, 0.8)).await;
    let ev = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Listening)),
        Duration::from_secs(5),
    )
    .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, EngineEvent::VoiceCommand(ikk_wake::Command::Start))),
        "start word: {ev:?}"
    );
    assert!(ev
        .iter()
        .any(|e| matches!(e, EngineEvent::State(EngineState::Listening))));

    say(&mic, &word(STOP_WORD, 0.95, 1.2)).await;
    let ev = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::VoiceStandby(true)),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, EngineEvent::VoiceCommand(ikk_wake::Command::Stop))),
        "stop word: {ev:?}"
    );
    assert!(ev.iter().any(|e| matches!(e, EngineEvent::State(EngineState::Idle))));
    assert!(
        ev.iter().any(|e| matches!(e, EngineEvent::VoiceStandby(true))),
        "back to standby"
    );
}

/// Provider that records the microphone itself (like Windows' recognizer).
struct OwnMicProvider;

struct SilentSession {
    rx: mpsc::Receiver<TranscriptEvent>,
    _tx: mpsc::Sender<TranscriptEvent>,
}

#[async_trait::async_trait]
impl StreamingSession for SilentSession {
    async fn push(&mut self, _f: AudioFrame) -> Result<(), SttError> {
        Ok(())
    }
    async fn cut_segment(&mut self) -> Result<(), SttError> {
        Ok(())
    }
    async fn finish(&mut self) -> Result<(), SttError> {
        let _ = self._tx.send(TranscriptEvent::Closed).await;
        Ok(())
    }
    fn events(&mut self) -> &mut mpsc::Receiver<TranscriptEvent> {
        &mut self.rx
    }
}

#[async_trait::async_trait]
impl Provider for OwnMicProvider {
    fn id(&self) -> ProviderId {
        ProviderId("fake".into())
    }
    fn display_name(&self) -> &'static str {
        "Own mic"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            interim: true,
            requires_network: true,
            own_audio: true,
            ..Default::default()
        }
    }
    async fn transcribe(&self, _: AudioClip, _: &SessionConfig) -> Result<Transcript, SttError> {
        Err(SttError::Unsupported)
    }
    async fn stream(&self, _: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
        let (tx, rx) = mpsc::channel(8);
        Ok(Box::new(SilentSession { rx, _tx: tx }))
    }
}

#[tokio::test]
async fn stop_word_works_with_providers_that_own_the_microphone() {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(OwnMicProvider));
    let mic: Mic = Arc::new(StdMutex::new(None));
    let opens = Arc::new(StdMutex::new(0usize));
    let (opener_mic, opener_opens) = (mic.clone(), opens.clone());
    let opener: CaptureOpener = Box::new(move |_cfg| {
        let (tx, rx) = mpsc::channel(512);
        *opener_mic.lock().unwrap() = Some(tx);
        *opener_opens.lock().unwrap() += 1;
        Ok(Box::new(FakeCapture { rx }) as Box<dyn Capture>)
    });
    let sink = FakeSink {
        status: TargetStatus::Injectable(AccessLevel::BasicTyping),
        typing_ok: true,
        log: SinkLog::default(),
    };
    let mut handle = Engine::spawn_with(reg, Box::new(sink), opener);
    let takes = |w| vec![word(w, 1.0, 1.0), word(w, 0.9, 0.7), word(w, 1.12, 1.3)];
    let commands = ikk_wake::VoiceCommands::train(&takes(START_WORD), &takes(STOP_WORD)).unwrap();
    handle.send(EngineCommand::GrantConsent("fake".into())).await;
    handle
        .send(EngineCommand::SetVoiceCommands(Some(Box::new(VoiceSetup {
            commands,
            start: start_cfg(),
        }))))
        .await;
    collect(
        &mut handle,
        |e| matches!(e, EngineEvent::VoiceStandby(true)),
        Duration::from_secs(3),
    )
    .await;

    say(&mic, &word(START_WORD, 1.0, 1.0)).await;
    let ev = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Listening)),
        Duration::from_secs(5),
    )
    .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, EngineEvent::State(EngineState::Listening))),
        "{ev:?}"
    );
    // The listener stayed open: no second microphone stream was needed.
    assert!(
        !ev.iter().any(|e| matches!(e, EngineEvent::VoiceStandby(false))),
        "{ev:?}"
    );

    say(&mic, &word(STOP_WORD, 1.0, 1.0)).await;
    let ev = collect(
        &mut handle,
        |e| matches!(e, EngineEvent::State(EngineState::Idle)),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, EngineEvent::VoiceCommand(ikk_wake::Command::Stop))),
        "stop word: {ev:?}"
    );
    assert!(ev.iter().any(|e| matches!(e, EngineEvent::State(EngineState::Idle))));
    assert_eq!(*opens.lock().unwrap(), 1, "one listener for the whole cycle");
}
