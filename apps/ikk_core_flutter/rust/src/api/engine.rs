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
    pub fn start(&self, settings_json: String) -> Result<(), String> {
        self.inner.remember_settings(&settings_json);
        self.inner.engine.start(&settings_json)
    }
    #[frb(sync)]
    pub fn stop(&self) -> Result<(), String> {
        self.inner.engine.stop()
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
    #[frb(sync)]
    pub fn transfer_to_target(&self) -> Result<(), String> {
        self.inner.engine.transfer_to_target()
    }
    #[frb(sync)]
    pub fn clear_session_text(&self) -> Result<(), String> {
        self.inner.engine.clear_session_text()
    }
}
