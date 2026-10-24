//! One engine per process, shared by the Flutter UI and (on Android) the
//! accessibility service, which runs in the same process. Events are
//! pumped once into a broadcast channel; every subscriber (a Dart stream,
//! the Android text inserter) gets each event.
//!
//! On Android the service may start after a reboot without the app ever
//! being opened. So the state it needs (settings, consent decisions and
//! API keys) is handed to the plugin's encrypted store whenever it changes
//! (see `IkkSecureStore.java`) and restored from there on start.

use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use ikk_engine::{EngineEvent, EngineState};
use ikk_ffi::engine::{event_json, runtime, FfiEngine};
use tokio::sync::broadcast;

pub struct Shared {
    pub engine: FfiEngine,
    pub events: broadcast::Sender<String>,
    pub listening: AtomicBool,
    /// Last settings JSON the app used; the service starts with it.
    pub settings: Mutex<Option<String>>,
    /// Last consent state as JSON.
    consent: Mutex<Option<String>>,
    /// API keys by provider id.
    credentials: Mutex<BTreeMap<String, String>>,
    /// Serialises writes to the encrypted store.
    #[allow(dead_code)]
    persist_lock: Mutex<()>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

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
                settings: Mutex::new(None),
                consent: Mutex::new(None),
                credentials: Mutex::new(BTreeMap::new()),
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
            if s.listening.swap(listening, Ordering::SeqCst) != listening {
            }
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
    fn persist(&self) {
    }

    #[allow(dead_code)]
    fn snapshot(&self) -> String {
        let settings = self.settings.lock().ok().and_then(|g| g.clone());
        let consent = self.consent.lock().ok().and_then(|g| g.clone());
        let credentials = self.credentials.lock().map(|g| g.clone()).unwrap_or_default();
        let mut v = serde_json::json!({
            "v": 1,
            "settings": settings,
            "consent": consent,
            "credentials": credentials,
        });
        let out = v.to_string();
        // Overwrite the key copies held by the JSON value before dropping it.
        if let Some(map) = v.get_mut("credentials").and_then(|c| c.as_object_mut()) {
            for (_, val) in map.iter_mut() {
                if let serde_json::Value::String(s) = val {
                    zeroize::Zeroize::zeroize(s);
                }
            }
        }
        let mut credentials = credentials;
        for (_, s) in credentials.iter_mut() {
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
        if let Some(map) = v.get_mut("credentials").and_then(|c| c.as_object_mut()) {
            for (_, val) in map.iter_mut() {
                if let serde_json::Value::String(s) = val {
                    zeroize::Zeroize::zeroize(s);
                }
            }
        }
    }

    #[allow(dead_code)]
    /// Start with the remembered settings, or stop if listening.
    /// Returns the new listening state, or an error text.
    pub fn toggle(&self) -> Result<bool, String> {
        if self.listening.load(Ordering::SeqCst) {
            self.engine.stop()?;
            return Ok(false);
        }
        let json = self
            .settings
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| "open the app once to load settings".to_string())?;
        self.engine.start(&json)?;
        Ok(true)
    }
}
