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

/// Idempotent logging setup (RUST_LOG or `info`). With `IKK_LOG_FILE` set
/// to a file path the lines go into that file (begun anew at each start)
/// instead of the terminal: for finding a fault on a computer where the
/// app's terminal output cannot be seen. The lines contain dictated text,
/// so nothing is written to a file unless the user asks for it this way.
pub fn init_logging() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let file = std::env::var_os("IKK_LOG_FILE")
            .filter(|p| !p.is_empty())
            .and_then(|p| std::fs::File::create(p).ok());
        if let Some(file) = file {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
                )
                .with_ansi(false)
                .with_writer(Mutex::new(file))
                .try_init();
            return;
        }
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
    /// The correction button: correct the range chosen in the settings
    /// with the corrector chosen there.
    pub fn correct_now(&self, settings_json: &str) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        self.send(EngineCommand::CorrectRange(ikk_engine::config::correct_request(
            &settings,
        )))
    }
    /// The correction range named in the settings ("cursor_to_end",
    /// "whole_text", "click_to_click"), for shells that arm a click capture.
    pub fn correction_range(settings_json: &str) -> Result<String, String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        Ok(settings.text.correction_range.trim().to_string())
    }
    /// Single word: the word under the clicked point is selected by a
    /// double-click, then corrected.
    pub fn correct_word_at(&self, settings_json: &str, at: (f64, f64)) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        ikk_output::desktop::complete_double_click_at(at.0, at.1).map_err(|e| e.to_string())?;
        let mut req = ikk_engine::config::correct_request(&settings);
        req.range = ikk_engine::CorrectionRange::Selection;
        self.send(EngineCommand::CorrectRange(req))
    }
    /// Correct the selection as the app in front has it (the user made it,
    /// for instance with a double-click of their own).
    pub fn correct_selection(&self, settings_json: &str) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        let mut req = ikk_engine::config::correct_request(&settings);
        req.range = ikk_engine::CorrectionRange::Selection;
        self.send(EngineCommand::CorrectRange(req))
    }
    /// Word suggestions: what else may have been said in place of the
    /// selection in the app in front; the answer is the `WordChoices` event.
    pub fn suggest_selection(&self, settings_json: &str) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        self.send(EngineCommand::SuggestWords(ikk_engine::config::correct_request(
            &settings,
        )))
    }
    /// Word suggestions for the word under a single click: the click is
    /// completed to a double-click, which selects the word, and the
    /// suggestions are asked for that selection.
    pub fn suggest_word_at(&self, settings_json: &str, at: (f64, f64)) -> Result<(), String> {
        ikk_output::desktop::complete_double_click_at(at.0, at.1).map_err(|e| e.to_string())?;
        self.suggest_selection(settings_json)
    }
    /// Word suggestions for a host that holds the text itself:
    /// `original` is the marked part, `before` and `after` the text around
    /// it. The answer is the `WordChoices` event; the pick comes back as
    /// `HostReplace` after [`Self::apply_choice`].
    pub fn suggest_host(&self, settings_json: &str, original: &str, before: &str, after: &str) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        let mut req = ikk_engine::config::correct_request(&settings);
        req.range = ikk_engine::CorrectionRange::Selection;
        req.host_text = Some(original.to_string());
        req.host_around = Some((before.to_string(), after.to_string()));
        self.send(EngineCommand::SuggestWords(req))
    }
    /// The candidate the user picked goes over the selection.
    pub fn apply_choice(&self, text: &str) -> Result<(), String> {
        self.send(EngineCommand::ApplyChoice(text.to_string()))
    }
    /// Click to click: the two screen points the user clicked become a
    /// selection (click, Shift-click), then the selection is corrected.
    pub fn correct_between(&self, settings_json: &str, from: (f64, f64), to: (f64, f64)) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        select_between_reading_context(from, to)?;
        let mut req = ikk_engine::config::correct_request(&settings);
        req.range = ikk_engine::CorrectionRange::Selection;
        self.send(EngineCommand::CorrectRange(req))
    }
    /// Replace by voice on the selection the app in front already has, or
    /// on the range named in the settings (cursor to end, whole text).
    /// `word_at`: a screen point; the word under it is selected by a
    /// double-click first. The engine listens in the running session or
    /// opens one from the settings — before anything is selected, so
    /// that what the user says right after the click is heard.
    pub fn replace_by_voice(
        &self,
        settings_json: &str,
        word_at: Option<(f64, f64)>,
        use_range: bool,
    ) -> Result<(), String> {
        let first = word_at.map(|(x, y)| ikk_engine::SelectFirst::WordAt(x, y));
        self.replace_listening(settings_json, word_at.is_none() && use_range, first)
    }
    /// Replace by voice from the cursor a click just placed to the end of
    /// the text: the program in front gets a moment to place its cursor
    /// while the microphone is open already.
    pub fn replace_from_click(&self, settings_json: &str) -> Result<(), String> {
        let settle = ikk_engine::SelectFirst::Settle(std::time::Duration::from_millis(180));
        self.replace_listening(settings_json, true, Some(settle))
    }
    /// Replace by voice between two clicked points.
    pub fn replace_between(&self, settings_json: &str, from: (f64, f64), to: (f64, f64)) -> Result<(), String> {
        self.replace_listening(settings_json, false, Some(ikk_engine::SelectFirst::Between(from, to)))
    }
    fn replace_listening(
        &self,
        settings_json: &str,
        use_range: bool,
        first: Option<ikk_engine::SelectFirst>,
    ) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        let range = if use_range {
            ikk_engine::CorrectionRange::parse(&settings.text.correction_range)
        } else {
            ikk_engine::CorrectionRange::Selection
        };
        let start = Box::new(ikk_engine::config::start_config(&settings));
        self.send(EngineCommand::ReplaceByVoice {
            range,
            start: Some(start),
            host_text: None,
            first,
        })
    }
    /// Replace by voice for a host that selected the text itself (phones):
    /// the engine listens and answers with `HostReplace`.
    pub fn replace_by_voice_host(&self, settings_json: &str, original: &str) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        let start = Box::new(ikk_engine::config::start_config(&settings));
        self.send(EngineCommand::ReplaceByVoice {
            range: ikk_engine::CorrectionRange::Selection,
            start: Some(start),
            host_text: Some(original.to_string()),
            first: None,
        })
    }
    /// Correct text a host selected itself; the answer comes as `HostReplace`.
    pub fn correct_host(&self, settings_json: &str, original: &str) -> Result<(), String> {
        let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
        let mut req = ikk_engine::config::correct_request(&settings);
        req.range = ikk_engine::CorrectionRange::Selection;
        req.host_text = Some(original.to_string());
        self.send(EngineCommand::CorrectRange(req))
    }
    pub fn clear_session_text(&self) -> Result<(), String> {
        self.send(EngineCommand::ClearSessionText)
    }
    /// Offline voice commands on (trained model JSON + the settings a voice
    /// start uses) or off (`None`).
    pub fn set_voice_commands(&self, commands_json: Option<&str>, settings_json: &str) -> Result<(), String> {
        let setup = match commands_json {
            None => None,
            Some(json) => {
                let commands = ikk_wake::VoiceCommands::from_json(json).map_err(|e| e.to_string())?;
                let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
                Some(Box::new(ikk_engine::VoiceSetup {
                    commands,
                    start: ikk_engine::config::start_config(&settings),
                }))
            }
        };
        self.send(EngineCommand::SetVoiceCommands(setup))
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

/// Click to click for the AI correction: the two points become a selection.
fn select_between_reading_context(from: (f64, f64), to: (f64, f64)) -> Result<(), String> {
    ikk_engine::select_first(ikk_engine::SelectFirst::Between(from, to))
}
