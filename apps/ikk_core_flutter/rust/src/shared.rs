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
    /// Replace by voice is armed: the next click names the word, or a
    /// second press of the correction button names the range.
    replace_armed: AtomicBool,
    /// The switch: audio correction (replace by voice) instead of AI.
    audio_mode: AtomicBool,
    /// Serialises writes to the encrypted store.
    #[allow(dead_code)]
    persist_lock: Mutex<()>,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Why a start is refused while voice commands are recorded.
pub const TRAINING: &str = "voice commands are being recorded";

#[allow(dead_code)]
/// The lines of help shown under the floating buttons after the start
/// button, in the app's language (the shell sends them): 0 and 1 click
/// to click (AI, audio), 2 and 3 single word (AI, audio), 4 speak now,
/// 5 correcting, 6 click a word for suggestions, 7 no suggestions found,
/// 8 and 9 click to end (AI, audio), 10 suggestions need an AI
/// corrector, 11 looking for suggestions.
static HINTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Where the word was clicked whose suggestions are being fetched: the
/// list appears under it.
static CHOICES_AT: Mutex<Option<(f64, f64)>> = Mutex::new(None);

/// Word suggestions can be asked for (see `Shared::set_suggestions_available`).
/// True until the shell says otherwise, so nothing is grey before it knows.
static SUGGESTIONS_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_hints(hints: Vec<String>) {
    if let Ok(mut h) = HINTS.lock() {
        *h = hints;
    }
}

fn show_hint(which: usize) {
    let text = HINTS.lock().ok().and_then(|h| h.get(which).cloned());
    ikk_overlay::set_hint(text.as_deref());
}

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
                replace_armed: AtomicBool::new(false),
                audio_mode: AtomicBool::new(false),
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
        // Replace by voice: mark the selection while the engine listens,
        // and let go of the yellow state when it is over.
        // The selection keeps the app's own colour; the arc on the wand is
        // the only progress sign (Stephan's choice).
        EngineEvent::ReplaceListening { .. } => ikk_overlay::set_busy(ikk_overlay::Busy::Listening),
        // While a replacement waits for its words: speech over means the
        // recogniser is at work (red) until the next words or the end.
        EngineEvent::Speech(on) if s.replace_armed.load(Ordering::SeqCst) => {
            ikk_overlay::set_busy(if *on {
                ikk_overlay::Busy::Listening
            } else {
                ikk_overlay::Busy::Processing
            });
        }
        EngineEvent::Corrected { .. } => ikk_overlay::set_busy(ikk_overlay::Busy::None),
        // Word suggestions: the candidates go under the buttons; the one
        // the user clicks goes over the marked part.
        EngineEvent::WordChoices { choices, bounds, .. } => {
            ikk_overlay::set_busy(ikk_overlay::Busy::None);
            // Under the word: its own lower edge when the program tells
            // where it stands, else half a line under the click.
            let clicked = CHOICES_AT.lock().ok().and_then(|mut at| at.take());
            let at = bounds
                .map(|(x, y, w, h)| (x + w / 2.0, y + h))
                .or(clicked.map(|(x, y)| (x, y + 12.0)));
            if choices.is_empty() {
                show_hint(7);
            } else {
                ikk_overlay::set_choices(
                    choices.clone(),
                    at,
                    Arc::new(|text| {
                        if let Some(s) = get() {
                            ikk_overlay::set_busy(ikk_overlay::Busy::Processing);
                            if s.engine.apply_choice(&text).is_err() {
                                ikk_overlay::set_busy(ikk_overlay::Busy::None);
                            }
                        }
                    }),
                );
            }
        }
        EngineEvent::Error(message) if message.starts_with("suggestions:") => {
            ikk_overlay::set_busy(ikk_overlay::Busy::None);
            match message.as_str() {
                "suggestions: nothing-marked" => show_hint(7),
                "suggestions: need-ai-corrector" => show_hint(10),
                _ => ikk_overlay::set_hint(None),
            }
        }
        EngineEvent::Error(message) if message.starts_with("correction:") => {
            ikk_overlay::set_busy(ikk_overlay::Busy::None)
        }
        // The engine asks for consent instead of correcting or replacing:
        // nothing is being worked on until the user has answered.
        EngineEvent::ConsentRequired(_) => s.end_replace(),
        EngineEvent::Replaced { .. } => s.end_replace(),
        EngineEvent::Error(message) if message.starts_with("replace:") => s.end_replace(),
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
        // The correction button shows the range it corrects.
        if let Ok(range) = ikk_ffi::engine::FfiEngine::correction_range(json) {
            ikk_overlay::set_range_mode(ikk_overlay::RangeMode::parse(&range));
        }
        self.show_start_availability();
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
    /// The floating correction button: with the remembered settings. For a
    /// click-to-click range the button arms a capture of the next two clicks
    /// (orange); pressing it again cancels; the two points become the
    /// selection that is corrected.
    pub fn correct_now(&self) -> Result<(), String> {
        let json = self
            .settings
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| "no settings yet".to_string())?;
        // The mode and the range decide what one press does; the arc on the
        // wand runs from the moment the range is fixed until the answer.
        let audio = self.audio_mode.load(Ordering::SeqCst);
        let range = ikk_ffi::engine::FfiEngine::correction_range(&json)?;
        // A press while a click capture waits: cancel.
        if ikk_overlay::cancel_capture() {
            self.end_replace();
            return Ok(());
        }
        match range.as_str() {
            // Two clicks name the range; two on the same spot name that word.
            "click_to_click" => {
                self.arm(audio);
                let armed = ikk_overlay::capture_clicks(
                    2,
                    std::time::Duration::from_secs(20),
                    std::sync::Arc::new(move |points| {
                        let Some(s) = get() else { return };
                        if points.len() < 2 {
                            // The capture timed out: nothing is armed any more.
                            s.end_replace();
                            return;
                        }
                        let (a, b) = (points[0], points[1]);
                        let same_spot = (a.0 - b.0).abs() < 8.0 && (a.1 - b.1).abs() < 8.0;
                        s.busy_start(audio);
                        // Two clicks on one spot were a double-click: the app
                        // selected the word itself; take the selection as it is.
                        let r = match (audio, same_spot) {
                            (true, true) => s.engine.replace_by_voice(&json, None, false),
                            (true, false) => s.engine.replace_between(&json, a, b),
                            (false, true) => s.engine.correct_selection(&json),
                            (false, false) => s.engine.correct_between(&json, a, b),
                        };
                        if let Err(e) = r {
                            log::warn!("click to click: {e}");
                            s.end_replace();
                        }
                    }),
                );
                if !armed {
                    self.end_replace();
                    return Err("click to click is not available on this platform".into());
                }
                show_hint(if audio { 1 } else { 0 });
                Ok(())
            }
            // One click names the word.
            "single_word" => {
                self.arm(audio);
                let armed = ikk_overlay::capture_clicks(
                    1,
                    std::time::Duration::from_secs(15),
                    std::sync::Arc::new(move |points| {
                        let Some(s) = get() else { return };
                        let Some(&point) = points.first() else {
                            // The capture timed out: nothing is armed any more.
                            s.end_replace();
                            return;
                        };
                        s.busy_start(audio);
                        let r = if audio {
                            s.engine.replace_by_voice(&json, Some(point), false)
                        } else {
                            s.engine.correct_word_at(&json, point)
                        };
                        if let Err(e) = r {
                            log::warn!("single word: {e}");
                            s.end_replace();
                        }
                    }),
                );
                if !armed {
                    self.end_replace();
                    return Err("single word needs the floating button on this platform".into());
                }
                show_hint(if audio { 3 } else { 2 });
                Ok(())
            }
            // Word suggestions: one click names the word. The click is
            // completed to a double-click (the word is selected), the text
            // model is asked what else may have been said there, and the
            // answers appear right under the word
            // (`EngineEvent::WordChoices`).
            "word_suggestions" => {
                ikk_overlay::clear_choices();
                if !self.start_available() {
                    // The grey button: say why nothing starts.
                    show_hint(10);
                    return Ok(());
                }
                let armed = ikk_overlay::capture_clicks(
                    1,
                    std::time::Duration::from_secs(15),
                    std::sync::Arc::new(move |points| {
                        let Some(s) = get() else { return };
                        let Some(&point) = points.first() else {
                            // The capture timed out: nothing is armed any more.
                            s.end_replace();
                            return;
                        };
                        if let Ok(mut at) = CHOICES_AT.lock() {
                            *at = Some(point);
                        }
                        ikk_overlay::set_busy(ikk_overlay::Busy::Processing);
                        show_hint(11);
                        if let Err(e) = s.engine.suggest_word_at(&json, point) {
                            log::warn!("word suggestions: {e}");
                            s.end_replace();
                        }
                    }),
                );
                if !armed {
                    self.end_replace();
                    return Err("word suggestions need the floating button on this platform".into());
                }
                show_hint(6);
                Ok(())
            }
            // From a click to the end: one click says where the range
            // begins. The click itself reaches the program and puts the
            // cursor there; from there to the end is then taken. (Acting at
            // once from wherever the cursor stood made a later click cancel
            // the selection, and the new text was inserted instead.)
            "cursor_to_end" => {
                self.arm(audio);
                let armed = ikk_overlay::capture_clicks(
                    1,
                    std::time::Duration::from_secs(15),
                    std::sync::Arc::new(move |points| {
                        let Some(s) = get() else { return };
                        if points.is_empty() {
                            // The capture timed out: nothing is armed any more.
                            s.end_replace();
                            return;
                        }
                        // The program needs a moment to place its cursor.
                        std::thread::sleep(std::time::Duration::from_millis(180));
                        s.busy_start(audio);
                        if audio {
                            s.replace_armed.store(true, Ordering::SeqCst);
                            ikk_overlay::set_replace_armed(true);
                        }
                        let r = if audio {
                            s.engine.replace_by_voice(&json, None, true)
                        } else {
                            s.engine.correct_now(&json)
                        };
                        if let Err(e) = r {
                            log::warn!("click to end: {e}");
                            s.end_replace();
                        }
                    }),
                );
                if !armed {
                    self.end_replace();
                    return Err("click to end needs the floating button on this platform".into());
                }
                show_hint(if audio { 9 } else { 8 });
                Ok(())
            }
            // The whole text: at once.
            _ => {
                self.busy_start(audio);
                if audio {
                    self.replace_armed.store(true, Ordering::SeqCst);
                    ikk_overlay::set_replace_armed(true);
                }
                let r = if audio {
                    self.engine.replace_by_voice(&json, None, true)
                } else {
                    self.engine.correct_now(&json)
                };
                if r.is_err() {
                    self.end_replace();
                }
                r
            }
        }
    }

    /// A click capture waits: the wand turns yellow (audio) or orange (AI).
    fn arm(&self, audio: bool) {
        if audio {
            self.replace_armed.store(true, Ordering::SeqCst);
            ikk_overlay::set_replace_armed(true);
        }
    }

    /// The range is fixed: the arc runs until the answer is there.
    fn busy_start(&self, audio: bool) {
        show_hint(if audio { 4 } else { 5 });
        if audio {
            self.replace_armed.store(true, Ordering::SeqCst);
            ikk_overlay::set_replace_armed(true);
            ikk_overlay::set_busy(ikk_overlay::Busy::Listening);
        } else {
            ikk_overlay::set_busy(ikk_overlay::Busy::Processing);
        }
    }

    /// The range button: the range moves on — click to click, click
    /// to end, single word, whole text, word suggestions, and round again. The settings copy
    /// here changes at once (the button shows it, every reader of the settings
    /// sees it); the `CorrectionRange` event tells the shell, which saves it.
    /// Returns the new range name ("" when no settings are known yet).
    pub fn next_range(&self) -> String {
        const ORDER: [&str; 5] = [
            "click_to_click",
            "cursor_to_end",
            "single_word",
            "whole_text",
            "word_suggestions",
        ];
        let mut next = String::new();
        if let Ok(mut s) = self.settings.lock() {
            let Some(json) = s.as_deref() else {
                return next;
            };
            let Ok(mut v) = serde_json::from_str::<serde_json::Value>(json) else {
                return next;
            };
            if !v.is_object() {
                return next;
            }
            let now = v["text"]["correction_range"]
                .as_str()
                .unwrap_or("cursor_to_end")
                .trim()
                .to_string();
            let i = ORDER.iter().position(|r| *r == now).unwrap_or(1);
            next = ORDER[(i + 1) % ORDER.len()].to_string();
            if !v["text"].is_object() {
                v["text"] = serde_json::json!({});
            }
            v["text"]["correction_range"] = serde_json::Value::String(next.clone());
            *s = Some(v.to_string());
        }
        ikk_overlay::set_range_mode(ikk_overlay::RangeMode::parse(&next));
        self.show_start_availability();
        self.persist();
        let _ = self
            .events
            .send(format!("{{\"CorrectionRange\":{{\"range\":\"{next}\"}}}}"));
        next
    }

    /// The switch flipped: audio correction (replace by voice) or AI.
    pub fn set_audio_mode(&self, on: bool) {
        self.audio_mode.store(on, Ordering::SeqCst);
        ikk_overlay::set_audio_mode(on);
        if !on && self.replace_armed.load(Ordering::SeqCst) {
            let _ = ikk_overlay::cancel_capture();
            self.end_replace();
        }
    }

    /// Phones: replace by voice on text the host selected itself.
    #[allow(dead_code)]
    pub fn replace_host(&self, original: &str) -> Result<(), String> {
        let json = self
            .settings
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| "open the app once to load settings".to_string())?;
        if self.training.load(Ordering::SeqCst) {
            return Err(TRAINING.into());
        }
        self.engine.replace_by_voice_host(&json, original)
    }

    /// Word suggestions for text a host marked itself, with the text
    /// around it.
    #[allow(dead_code)]
    pub fn suggest_host(&self, original: &str, before: &str, after: &str) -> Result<(), String> {
        let json = self
            .settings
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| "open the app once to load settings".to_string())?;
        self.engine.suggest_host(&json, original, before, after)
    }

    /// The suggestion the user tapped in a host's list.
    #[allow(dead_code)]
    pub fn apply_choice(&self, text: &str) -> Result<(), String> {
        self.engine.apply_choice(text)
    }

    /// Phones: correct text the host selected itself.
    #[allow(dead_code)]
    pub fn correct_host(&self, original: &str) -> Result<(), String> {
        let json = self
            .settings
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| "open the app once to load settings".to_string())?;
        self.engine.correct_host(&json, original)
    }

    /// The shell says whether word suggestions can be asked for (a text
    /// model is chosen as corrector and its key is stored).
    pub fn set_suggestions_available(&self, on: bool) {
        SUGGESTIONS_AVAILABLE.store(on, Ordering::SeqCst);
        self.show_start_availability();
    }

    /// Whether the start button can do what the range asks: false only
    /// for word suggestions without a text model's key.
    pub fn start_available(&self) -> bool {
        self.correction_range() != "word_suggestions" || SUGGESTIONS_AVAILABLE.load(Ordering::SeqCst)
    }

    /// The start button grey when its range is not available.
    fn show_start_availability(&self) {
        ikk_overlay::set_start_disabled(!self.start_available());
    }

    /// The correction range named in the settings ("" when unknown).
    pub fn correction_range(&self) -> String {
        self.settings
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .and_then(|json| ikk_ffi::engine::FfiEngine::correction_range(&json).ok())
            .unwrap_or_default()
    }

    /// Replace by voice is over (done, cancelled or failed): the button
    /// goes back to blue and the mark disappears.
    pub fn end_replace(&self) {
        self.replace_armed.store(false, Ordering::SeqCst);
        ikk_overlay::set_replace_armed(false);
        ikk_overlay::set_busy(ikk_overlay::Busy::None);
        ikk_overlay::clear_highlight();
    }

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
