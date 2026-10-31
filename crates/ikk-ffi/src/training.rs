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
