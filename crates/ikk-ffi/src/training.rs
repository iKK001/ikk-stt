//! Training mode for shells: one take of a word, answered with what the
//! recogniser heard. The shell stores it (`TrainingSettings::learn`).

use std::sync::Arc;
use std::time::Duration;

use ikk_stt::{ProviderId, ProviderRegistry, Secret};

use crate::engine::runtime;

/// Record one take with the microphone from the settings and return what
/// the active provider heard (may be empty). `credential`: the provider's
/// key from the shell's secure storage. Call it while no dictation runs.
pub fn take(settings_json: &str, credential: &str, timeout_secs: u32) -> Result<String, String> {
    let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
    let mut reg = ProviderRegistry::new();
    ikk_providers::register_all(&mut reg);
    let provider: Arc<dyn ikk_stt::Provider> = reg
        .get(&ProviderId(settings.providers.active.clone()))
        .ok_or_else(|| format!("unknown provider {}", settings.providers.active))?;
    let secret = Secret::new(credential.to_string());
    runtime().block_on(ikk_engine::training::take(
        provider,
        &settings,
        secret,
        Duration::from_secs(timeout_secs.max(1) as u64),
    ))
}

/// What the active provider hears in a recorded clip (16 kHz mono): the
/// words of a recorded voice command. Nothing is sent unless the user
/// has allowed that provider (`consent_json`, the stored consent) or the
/// provider works on the device.
pub fn transcribe_clip(
    settings_json: &str,
    consent_json: &str,
    credential: &str,
    samples: Vec<f32>,
) -> Result<String, String> {
    let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
    let mut reg = ProviderRegistry::new();
    ikk_providers::register_all(&mut reg);
    let provider: Arc<dyn ikk_stt::Provider> = reg
        .get(&ProviderId(settings.providers.active.clone()))
        .ok_or_else(|| format!("unknown provider {}", settings.providers.active))?;
    if provider.capabilities().requires_network {
        let consent: ikk_consent::ConsentState = serde_json::from_str(consent_json).map_err(|e| e.to_string())?;
        if consent.decision(&settings.providers.active) != ikk_consent::Decision::Allowed {
            return Err("not allowed to send audio to this provider".into());
        }
    }
    runtime().block_on(ikk_engine::training::transcribe_samples(
        provider,
        &settings,
        Secret::new(credential.to_string()),
        samples,
    ))
}
