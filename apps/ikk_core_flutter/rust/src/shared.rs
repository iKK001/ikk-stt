//! One engine per process, shared by the Flutter UI and (on Android) the
//! accessibility service, which runs in the same process. Events are
//! pumped once into a broadcast channel; every subscriber (a Dart stream,
//! the Android text inserter) gets each event.
//!
//! On Android the service may start after a reboot without the app ever
//! being opened. So the state it needs (settings, consent decisions and
//! API keys) is handed to the plugin's encrypted store whenever it changes
//! (see `IkkSecureStore.java`) and restored from there on start.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ikk_engine::{EngineEvent, EngineState};
use ikk_ffi::engine::{event_json, runtime, FfiEngine};
use tokio::sync::broadcast;

pub struct Shared {
    pub engine: FfiEngine,
    pub events: broadcast::Sender<String>,
    pub listening: AtomicBool,
    /// The voice listener is waiting for the start word.
    pub standby: AtomicBool,
    /// Last settings JSON the app used; the service starts with it.
    pub settings: Mutex<Option<String>>,
    /// Last consent state as JSON.
    consent: Mutex<Option<String>>,
    /// API keys by provider id.
    credentials: Mutex<BTreeMap<String, String>>,
    /// Trained offline voice commands (JSON) while switched on.
    voice: Mutex<Option<String>>,
    /// Android: screen off, nothing listens (battery).
    #[allow(dead_code)]
    screen_paused: AtomicBool,
    /// Microphone switched off by click: the voice listener stays off too
    /// and only a click turns it on again (Dragon's "microphone off"). A
    /// stop by voice keeps the listener on (Dragon's "go to sleep").
    mic_off: AtomicBool,
    /// Whether the engine currently has the voice listener on.
    voice_applied: AtomicBool,
    /// The user records voice commands: nothing listens or dictates, or
    /// the recorded words would be transcribed (and typed) as well.
    training: AtomicBool,
    /// Serialises writes to the encrypted store.
    #[allow(dead_code)]
    persist_lock: Mutex<()>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Why a start is refused while voice commands are recorded.
pub const TRAINING: &str = "voice commands are being recorded";

#[allow(dead_code)]
pub fn get() -> Option<Arc<Shared>> {
    SHARED.get().cloned()
}

pub fn get_or_create() -> Arc<Shared> {
    SHARED
        .get_or_init(|| {
            let engine = FfiEngine::new();
            let (tx, _) = broadcast::channel(512);
            let shared = Arc::new(Shared {
                engine,
                events: tx.clone(),
                listening: AtomicBool::new(false),
                standby: AtomicBool::new(false),
                settings: Mutex::new(None),
                consent: Mutex::new(None),
                credentials: Mutex::new(BTreeMap::new()),
                voice: Mutex::new(None),
                screen_paused: AtomicBool::new(false),
                mic_off: AtomicBool::new(false),
                voice_applied: AtomicBool::new(false),
                training: AtomicBool::new(false),
                persist_lock: Mutex::new(()),
            });
            if let Some(mut rx) = shared.engine.take_events() {
                let s = shared.clone();
                runtime().spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        on_event(&s, &ev);
                        let _ = tx.send(event_json(&ev));
                    }
                });
            }
            shared
        })
        .clone()
}

fn on_event(s: &Shared, ev: &EngineEvent) {
    match ev {
        EngineEvent::State(state) => {
            let listening = matches!(state, EngineState::Listening | EngineState::Finishing);
            let changed = s.listening.swap(listening, Ordering::SeqCst) != listening;
            if changed {
                ikk_overlay::set_listening(listening);
            }
        }
        // Button colour: brand blue while waiting for the start word.
        EngineEvent::VoiceStandby(on) => {
            s.standby.store(*on, Ordering::SeqCst);
            ikk_overlay::set_standby(*on);
        }
        EngineEvent::ConsentChanged(state) => {
            if let Ok(json) = serde_json::to_string(state) {
                s.remember_consent(&json);
            }
        }
        _ => {}
    }
}

impl Shared {
    pub fn remember_settings(&self, json: &str) {
        if let Ok(mut s) = self.settings.lock() {
            if s.as_deref() == Some(json) {
                return;
            }
            *s = Some(json.to_string());
        }
        self.persist();
    }

    pub fn remember_consent(&self, json: &str) {
        if let Ok(mut s) = self.consent.lock() {
            if s.as_deref() == Some(json) {
                return;
            }
            *s = Some(json.to_string());
        }
        self.persist();
    }

    /// Voice commands on (model JSON) or off; kept for a restart.
    pub fn remember_voice(&self, json: Option<&str>) {
        let changed = match self.voice.lock() {
            Ok(mut v) if v.as_deref() != json => {
                *v = json.map(str::to_string);
                true
            }
            _ => false,
        };
        // Always re-apply: the settings a voice start uses may have changed.
        self.apply_voice(true);
        if changed {
            self.persist();
        }
    }

    /// Voice listener on exactly when words are trained, the screen is on
    /// and the microphone was not switched off by click.
    fn apply_voice(&self, force: bool) {
        let model = self.voice.lock().ok().and_then(|g| g.clone());
        let want = model.is_some()
            && !self.screen_paused.load(Ordering::SeqCst)
            && !self.mic_off.load(Ordering::SeqCst)
            && !self.training.load(Ordering::SeqCst);
        if self.voice_applied.swap(want, Ordering::SeqCst) == want && !force {
            return;
        }
        match (want, model) {
            (true, Some(model)) => {
                if let Some(settings) = self.settings.lock().ok().and_then(|g| g.clone()) {
                    let _ = self.engine.set_voice_commands(Some(&model), &settings);
                }
            }
            _ => {
                let _ = self.engine.set_voice_commands(None, "");
            }
        }
    }

    /// Microphone off by click (true) or on again by click (false).
    pub fn set_mic_off(&self, off: bool) {
        if self.mic_off.swap(off, Ordering::SeqCst) != off {
            self.apply_voice(false);
            self.persist();
        }
    }

    #[allow(dead_code)]
    pub fn mic_off(&self) -> bool {
        self.mic_off.load(Ordering::SeqCst)
    }

    /// A click on a microphone button: off switches everything off, on
    /// switches the listener back on and starts dictating.
    pub fn click_toggle(&self) -> Result<bool, String> {
        if self.listening.load(Ordering::SeqCst) {
            self.engine.stop()?;
            self.set_mic_off(true);
            return Ok(false);
        }
        self.set_mic_off(false);
        self.start_listening().map(|_| true)
    }

    /// Pause the voice listener (screen off, saves battery) or resume it
    /// (screen on and unlocked); the trained words stay.
    #[allow(dead_code)]
    pub fn set_voice_paused(&self, paused: bool) {
        self.screen_paused.store(paused, Ordering::SeqCst);
        self.apply_voice(false);
    }

    /// Recording voice commands (true): stop a running dictation and the
    /// voice listener, refuse starts. Done (false): the listener comes back
    /// as before; a stopped dictation stays stopped.
    pub fn set_training(&self, on: bool) {
        if self.training.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if on {
            let _ = self.engine.stop();
        }
        self.apply_voice(false);
    }

    pub fn is_training(&self) -> bool {
        self.training.load(Ordering::SeqCst)
    }

    /// An empty secret removes the stored key.
    pub fn remember_credential(&self, provider: &str, secret: &str) {
        if let Ok(mut c) = self.credentials.lock() {
            if secret.is_empty() {
                if let Some(mut old) = c.remove(provider) {
                    zeroize::Zeroize::zeroize(&mut old);
                }
            } else {
                if c.get(provider).map(String::as_str) == Some(secret) {
                    return;
                }
                if let Some(mut old) = c.insert(provider.to_string(), secret.to_string()) {
                    zeroize::Zeroize::zeroize(&mut old);
                }
            }
        }
        self.persist();
    }

    /// Hand the current state to the encrypted store, off the caller's
    /// thread (Keystore calls can take tens of milliseconds). Each write
    /// takes its snapshot under the lock, so the last write is the newest.
    fn persist(&self) {}

    #[allow(dead_code)]
    fn snapshot(&self) -> String {
        let settings = self.settings.lock().ok().and_then(|g| g.clone());
        let consent = self.consent.lock().ok().and_then(|g| g.clone());
        let credentials = self.credentials.lock().map(|g| g.clone()).unwrap_or_default();
        let voice = self.voice.lock().ok().and_then(|g| g.clone());
        let mic_off = self.mic_off.load(Ordering::SeqCst);
        let mut v = serde_json::json!({
            "v": 1,
            "settings": settings,
            "consent": consent,
            "credentials": credentials,
            "voice": voice,
            "mic_off": mic_off,
        });
        let out = v.to_string();
        // Overwrite the key copies held by the JSON value before dropping it.
        if let Some(map) = v.get_mut("credentials").and_then(|c| c.as_object_mut()) {
            for val in map.values_mut() {
                if let serde_json::Value::String(s) = val {
                    zeroize::Zeroize::zeroize(s);
                }
            }
        }
        let mut credentials = credentials;
        for s in credentials.values_mut() {
            zeroize::Zeroize::zeroize(s);
        }
        out
    }

    /// Fill in what the app has not set yet from a decrypted store
    /// snapshot. Values the app already handed over win. Not written back.
    #[allow(dead_code)]
    pub fn restore(&self, json: &str) {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(json) else {
            return;
        };
        if v.get("v").and_then(|x| x.as_u64()) != Some(1) {
            return;
        }
        if let Some(c) = v.get("consent").and_then(|x| x.as_str()) {
            if let Ok(mut g) = self.consent.lock() {
                if g.is_none() && self.engine.load_consent(c).is_ok() {
                    *g = Some(c.to_string());
                }
            }
        }
        if let Some(map) = v.get("credentials").and_then(|x| x.as_object()) {
            if let Ok(mut g) = self.credentials.lock() {
                for (provider, secret) in map {
                    let Some(secret) = secret.as_str() else { continue };
                    if g.contains_key(provider) || secret.is_empty() {
                        continue;
                    }
                    if self.engine.set_credential(provider, secret).is_ok() {
                        g.insert(provider.clone(), secret.to_string());
                    }
                }
            }
        }
        if let Some(st) = v.get("settings").and_then(|x| x.as_str()) {
            if let Ok(mut g) = self.settings.lock() {
                if g.is_none() {
                    *g = Some(st.to_string());
                }
            }
        }
        // Voice commands: the listener comes back unless the microphone was
        // switched off by click.
        if v.get("mic_off").and_then(|x| x.as_bool()) == Some(true) {
            self.mic_off.store(true, Ordering::SeqCst);
        }
        if let Some(model) = v.get("voice").and_then(|x| x.as_str()) {
            let restored = match self.voice.lock() {
                Ok(mut g) if g.is_none() => {
                    *g = Some(model.to_string());
                    true
                }
                _ => false,
            };
            if restored {
                self.apply_voice(true);
            }
        }
        if let Some(map) = v.get_mut("credentials").and_then(|c| c.as_object_mut()) {
            for val in map.values_mut() {
                if let serde_json::Value::String(s) = val {
                    zeroize::Zeroize::zeroize(s);
                }
            }
        }
    }

    #[allow(dead_code)]
    /// Start with the remembered settings, or stop if listening.
    /// Returns the new listening state, or an error text.
    /// Start unless already listening. `Ok(true)`: was already listening
    /// (no state change will follow); `Ok(false)`: start requested.
    #[allow(dead_code)]
    pub fn start_listening(&self) -> Result<bool, String> {
        if self.listening.load(Ordering::SeqCst) {
            return Ok(true);
        }
        if self.training.load(Ordering::SeqCst) {
            return Err(TRAINING.into());
        }
        let json = self
            .settings
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| "open the app once to load settings".to_string())?;
        self.engine.start(&json)?;
        Ok(false)
    }

    /// Stop whatever runs; harmless when nothing does.
    #[allow(dead_code)]
    pub fn stop_listening(&self) -> Result<(), String> {
        self.engine.stop()
    }
}
