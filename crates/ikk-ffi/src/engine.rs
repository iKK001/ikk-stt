//! Engine wrapper for foreign shells: one process-wide tokio runtime, the
//! engine spawned on it, commands as plain calls, events as JSON strings.
//! JSON keeps the binding layer (flutter_rust_bridge, UniFFI) free of
//! mirrored enums; shells decode with the schema in `EngineEvent`.

use std::sync::{Mutex, OnceLock};

use ikk_engine::{Engine, EngineCommand, EngineEvent, EngineHandle};
use ikk_stt::{ProviderId, ProviderRegistry, Secret};
use tokio::sync::mpsc;

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("ikk-core")
            .build()
            .expect("tokio runtime")
    })
}

/// Idempotent logging setup (RUST_LOG or `info`).
pub fn init_logging() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
            )
            .try_init();
    });
}

pub struct FfiEngine {
    commands: mpsc::Sender<EngineCommand>,
    events: Mutex<Option<mpsc::Receiver<EngineEvent>>>,
    _handle: Mutex<Option<EngineHandle>>,
}

impl FfiEngine {
    /// Spawns an engine with all compiled-in providers and the platform sink.
    pub fn new() -> Self {
        init_logging();
        let mut reg = ProviderRegistry::new();
        ikk_providers::register_all(&mut reg);
        let sink = ikk_output::default_sink();
        let handle = runtime().block_on(async { Engine::spawn(reg, sink) });
        Self::from_handle(handle)
    }

    pub fn from_handle(mut handle: EngineHandle) -> Self {
        let commands = handle.commands.clone();
        let (_, rx) = mpsc::channel(1);
        let events = std::mem::replace(&mut handle.events, rx);
        Self {
            commands,
            events: Mutex::new(Some(events)),
            _handle: Mutex::new(Some(handle)),
        }
    }

    /// Takes the event receiver once; the shell pumps it into its stream.
    pub fn take_events(&self) -> Option<mpsc::Receiver<EngineEvent>> {
        self.events.lock().ok().and_then(|mut e| e.take())
    }

    fn send(&self, cmd: EngineCommand) -> Result<(), String> {
        runtime()
            .block_on(self.commands.send(cmd))
            .map_err(|_| "engine stopped".to_string())
    }

    /// Start from a settings JSON (same schema as settings.json).
    pub fn start(&self, settings_json: &str) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        let cfg = ikk_engine::config::start_config(&settings);
        self.send(EngineCommand::Start(Box::new(cfg)))
    }
    pub fn stop(&self) -> Result<(), String> {
        self.send(EngineCommand::Stop)
    }
    pub fn cut_segment(&self) -> Result<(), String> {
        self.send(EngineCommand::CutSegment)
    }
    pub fn grant_consent(&self, provider: &str) -> Result<(), String> {
        self.send(EngineCommand::GrantConsent(provider.into()))
    }
    pub fn deny_consent(&self, provider: &str) -> Result<(), String> {
        self.send(EngineCommand::DenyConsent(provider.into()))
    }
    pub fn load_consent(&self, consent_json: &str) -> Result<(), String> {
        let state: ikk_consent::ConsentState = serde_json::from_str(consent_json).map_err(|e| e.to_string())?;
        self.send(EngineCommand::LoadConsent(state))
    }
    pub fn set_credential(&self, provider: &str, secret: &str) -> Result<(), String> {
        self.send(EngineCommand::SetCredential(
            ProviderId(provider.into()),
            Secret::new(secret),
        ))
    }
    pub fn transfer_to_target(&self) -> Result<(), String> {
        self.send(EngineCommand::TransferToTarget)
    }
    pub fn clear_session_text(&self) -> Result<(), String> {
        self.send(EngineCommand::ClearSessionText)
    }
}

impl Default for FfiEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// One event as JSON, e.g. `{"Level":-31.2}`, `{"State":"Listening"}`,
/// `{"Transcript":{"Final":"hallo"}}`, `{"ConsentRequired":{...}}`.
pub fn event_json(ev: &EngineEvent) -> String {
    serde_json::to_string(ev).unwrap_or_else(|e| format!("{{\"Error\":\"serialize: {e}\"}}"))
}
