//! The engine for Flutter: create once, subscribe to the event stream,
//! send commands. Events arrive as JSON strings (see `ikk_engine::EngineEvent`).
//! The engine is process-wide (see `shared.rs`) so the Android
//! accessibility service can drive the same instance.

use std::sync::Arc;

use flutter_rust_bridge::frb;

#[frb(opaque)]
pub struct IkkEngine {
    inner: Arc<crate::shared::Shared>,
}

impl IkkEngine {
    #[frb(sync)]
    pub fn create() -> IkkEngine {
        IkkEngine {
            inner: crate::shared::get_or_create(),
        }
    }

    /// Engine events as JSON; may be called again (e.g. after hot restart).
    pub fn events(&self, sink: crate::frb_generated::StreamSink<String, flutter_rust_bridge::for_generated::SseCodec>) {
        let mut rx = self.inner.events.subscribe();
        ikk_ffi::engine::runtime().spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if sink.add(ev).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    /// Settings the Android accessibility service starts with.
    #[frb(sync)]
    pub fn remember_settings(&self, settings_json: String) {
        self.inner.remember_settings(&settings_json);
    }

    #[frb(sync)]
    /// The app's dictate button: a click, so the voice listener is on again.
    pub fn start(&self, settings_json: String) -> Result<(), String> {
        if self.inner.is_training() {
            return Err(crate::shared::TRAINING.into());
        }
        self.inner.remember_settings(&settings_json);
        self.inner.set_mic_off(false);
        self.inner.engine.start(&settings_json)
    }
    /// The app's stop button: a click, so the voice listener goes off too.
    #[frb(sync)]
    pub fn stop(&self) -> Result<(), String> {
        self.inner.engine.stop()?;
        self.inner.set_mic_off(true);
        Ok(())
    }
    /// The voice command recording dialog opens (true) or closes (false):
    /// no dictation, no listening meanwhile, on every platform.
    #[frb(sync)]
    pub fn set_voice_training(&self, active: bool) {
        self.inner.set_training(active);
    }
    #[frb(sync)]
    pub fn cut_segment(&self) -> Result<(), String> {
        self.inner.engine.cut_segment()
    }
    #[frb(sync)]
    pub fn grant_consent(&self, provider: String) -> Result<(), String> {
        self.inner.engine.grant_consent(&provider)
    }
    #[frb(sync)]
    pub fn deny_consent(&self, provider: String) -> Result<(), String> {
        self.inner.engine.deny_consent(&provider)
    }
    #[frb(sync)]
    pub fn load_consent(&self, consent_json: String) -> Result<(), String> {
        self.inner.engine.load_consent(&consent_json)?;
        self.inner.remember_consent(&consent_json);
        Ok(())
    }
    #[frb(sync)]
    /// Also kept in the encrypted store on Android, so the accessibility
    /// service works after a reboot without opening the app.
    pub fn set_credential(&self, provider: String, mut secret: String) -> Result<(), String> {
        let r = self.inner.engine.set_credential(&provider, &secret);
        if r.is_ok() {
            self.inner.remember_credential(&provider, &secret);
        }
        zeroize::Zeroize::zeroize(&mut secret);
        r
    }
    /// Floating buttons over all apps (desktop): the microphone toggles
    /// dictation with the remembered settings (red while listening); the
    /// correction button corrects the text at the cursor (AI) or replaces
    /// a clicked word or a range by what is said (audio); the third button
    /// is the switch between the two.
    #[frb(sync)]
    pub fn show_floating_button(&self, show: bool) {
        if show {
            ikk_overlay::show(
                std::sync::Arc::new(|| {
                    if let Some(s) = crate::shared::get() {
                        if let Err(e) = s.click_toggle() {
                            log::warn!("floating button: {e}");
                        }
                    }
                }),
                Some(std::sync::Arc::new(|| {
                    if let Some(s) = crate::shared::get() {
                        if let Err(e) = s.correct_now() {
                            log::warn!("correction button: {e}");
                        }
                    }
                })),
                Some(std::sync::Arc::new(|audio| {
                    // The switch: remembered here, saved by the shell.
                    if let Some(s) = crate::shared::get() {
                        s.set_audio_mode(audio);
                        let _ = s.events.send(format!("{{\"CorrectionMode\":{{\"audio\":{audio}}}}}"));
                    }
                })),
                Some(std::sync::Arc::new(|x, y| {
                    // The shell remembers where the buttons were left.
                    if let Some(s) = crate::shared::get() {
                        let _ = s.events.send(format!("{{\"FloatingMoved\":{{\"x\":{x},\"y\":{y}}}}}"));
                    }
                })),
                Some(std::sync::Arc::new(|| {
                    // F8 anywhere: the next range; the shell saves it.
                    if let Some(s) = crate::shared::get() {
                        s.next_range();
                    }
                })),
            );
            ikk_overlay::set_listening(self.inner.listening.load(std::sync::atomic::Ordering::SeqCst));
            ikk_overlay::set_standby(self.inner.standby.load(std::sync::atomic::Ordering::SeqCst));
        } else {
            ikk_overlay::hide();
        }
    }

    /// Offline voice commands on (model JSON from `voice_train`) or off
    /// (`None`). `settings_json`: what a voice start dictates with.
    #[frb(sync)]
    pub fn set_voice_commands(&self, commands_json: Option<String>, settings_json: String) -> Result<(), String> {
        if let Some(json) = commands_json.as_deref() {
            // Validate before keeping it.
            ikk_wake::VoiceCommands::from_json(json).map_err(|e| e.to_string())?;
        }
        self.inner.remember_settings(&settings_json);
        if commands_json.is_some() {
            // Switching voice commands on means: listen for the start word,
            // even if the microphone was switched off by click before.
            self.inner.set_mic_off(false);
        }
        self.inner.remember_voice(commands_json.as_deref());
        Ok(())
    }
    #[frb(sync)]
    pub fn transfer_to_target(&self) -> Result<(), String> {
        self.inner.engine.transfer_to_target()
    }
    /// Where the floating buttons are (top-left corner, screen points), or
    /// empty when they do not exist on this platform.
    #[frb(sync)]
    pub fn floating_position(&self) -> Vec<f64> {
        ikk_overlay::position().map(|(x, y)| vec![x, y]).unwrap_or_default()
    }
    /// Move the floating buttons there.
    #[frb(sync)]
    pub fn set_floating_position(&self, x: f64, y: f64) {
        ikk_overlay::set_position(x, y);
    }
    /// The switch on the floating panel: audio correction (replace by
    /// voice) or AI correction; the shell keeps the choice.
    #[frb(sync)]
    pub fn set_correction_mode(&self, audio: bool) {
        self.inner.set_audio_mode(audio);
    }
    /// F8 in the shell's own window: the
    /// correction button's range moves on. The new range comes back as
    /// the `CorrectionRange` event, which the shell saves; "" when no
    /// settings are known yet.
    #[frb(sync)]
    pub fn next_correction_range(&self) -> String {
        self.inner.next_range()
    }
    /// Shrinking buttons: small and darker while the mouse is elsewhere,
    /// a little larger than plain under it. Off: always the plain size.
    #[frb(sync)]
    pub fn set_floating_shrink(&self, on: bool) {
        ikk_overlay::set_shrink(on);
    }
    /// Phones: replace by voice on text the host (keyboard, service)
    /// selected itself; the answer comes as the `HostReplace` event.
    #[frb(sync)]
    pub fn replace_by_voice_host(&self, settings_json: String, original: String) -> Result<(), String> {
        self.inner.remember_settings(&settings_json);
        self.inner.engine.replace_by_voice_host(&settings_json, &original)
    }
    /// Phones: correct text the host selected itself (`HostReplace` answers).
    #[frb(sync)]
    pub fn correct_host(&self, settings_json: String, original: String) -> Result<(), String> {
        self.inner.remember_settings(&settings_json);
        self.inner.engine.correct_host(&settings_json, &original)
    }
    /// The correction button from the app: correct the chosen range in the
    /// focused app with the chosen corrector.
    #[frb(sync)]
    pub fn correct_now(&self, settings_json: String) -> Result<(), String> {
        self.inner.remember_settings(&settings_json);
        self.inner.engine.correct_now(&settings_json)
    }
    #[frb(sync)]
    pub fn clear_session_text(&self) -> Result<(), String> {
        self.inner.engine.clear_session_text()
    }
}
