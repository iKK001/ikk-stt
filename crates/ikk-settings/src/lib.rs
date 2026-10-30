//! Persistent settings. Deliberately small (decision 2026-10-23): three
//! areas only — provider selection, sound, microphone. Everything else
//! (commands, wake word, profiles, TTS, …) is out of scope for v1. Credentials are NOT settings; they live in the platform
//! keychain, handled by the shell.

pub mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub schema_version: u32,
    pub providers: ProviderSettings,
    pub sound: SoundSettings,
    pub microphone: MicrophoneSettings,
}

// ---------------------------------------------------------------- providers

/// Which STT API is active and the per-provider options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSettings {
    /// Provider id, e.g. "deepgram". Must be one the build compiled in.
    pub active: String,
    /// ISO-639-3 code, empty = auto-detect.
    pub language: String,
    /// Per-provider options keyed by provider id.
    pub options: BTreeMap<String, ProviderOptions>,
}

/// Transfer mode a provider should use when it supports both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TransferMode {
    /// WebSocket / Socket.IO, live interim text, no wait at stop.
    #[default]
    Streaming,
    /// Chunked upload, higher accuracy, 1–3 s wait per chunk.
    Batch,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderOptions {
    /// Model id, e.g. "nova-3", "whisper-1", "scribe_v2".
    pub model: Option<String>,
    pub transfer: TransferMode,
    /// Bias terms (Deepgram keywords / keyterms, OpenAI prompt).
    pub keywords: Vec<String>,
    /// Voiceitt: inverse text normalisation, capitalisation, profanity filter.
    pub inverse_text_normalisation: bool,
    pub capitalisation: bool,
    pub filter_profanities: bool,
}

// -------------------------------------------------------------------- sound

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Denoise {
    #[default]
    Off,
    /// RNNoise + HP 180 Hz / LP 6.5 kHz / auto-gain (nnnoiseless, all platforms).
    RnNoise,
    /// DeepFilterNet 3, heavier, tuned for disordered voices.
    DeepFilterNet,
}

/// iOS audio session mode; ignored on other platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IosSessionMode {
    /// "Raw": disables Apple's processing.
    Measurement,
    /// Mild AGC by iOS.
    #[default]
    Default,
    /// Aggressive voice processing.
    VoiceChat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundSettings {
    /// Software gain before any filter, 0..=24 dB.
    pub preamp_gain_db: f32,
    /// Automatic gain: speech is brought to a steady level before it is
    /// sent, whatever the microphone and distance.
    #[serde(default = "default_true")]
    pub agc: bool,
    pub denoise: Denoise,
    pub ios_session_mode: IosSessionMode,
    pub vad: VadSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadSettings {
    /// Below this level the frame counts as silence.
    pub silence_threshold_db: f32,
    /// Silence this long closes a segment (Deepgram: endpointing).
    pub silence_duration_ms: u32,
    /// Silence this long starts a new paragraph / entry.
    pub long_pause_ms: u32,
    /// Batch mode: audio is cut into chunks of this length (a timer),
    /// and at pauses.
    pub chunk_interval_ms: u32,
}

fn default_true() -> bool {
    true
}

// --------------------------------------------------------------- microphone

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MicrophoneSettings {
    /// Backend device id; `None` = system default. On Windows this is the
    /// WASAPI endpoint id, on macOS/iOS the CoreAudio uid, on Android the
    /// AAudio device id, on Linux the ALSA/PipeWire name.
    pub device_id: Option<String>,
    /// Human-readable name shown in the picker; refreshed on each listing.
    pub device_label: Option<String>,
}

// ----------------------------------------------------------------- defaults

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            active: "deepgram".into(),
            language: "deu".into(),
            options: BTreeMap::new(),
        }
    }
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            preamp_gain_db: 0.0,
            agc: true,
            denoise: Denoise::Off,
            ios_session_mode: IosSessionMode::Default,
            vad: VadSettings::default(),
        }
    }
}

impl Default for VadSettings {
    fn default() -> Self {
        // Field-tested defaults: threshold, silence, long pause, chunk interval.
        Self {
            silence_threshold_db: -45.0,
            silence_duration_ms: 900,
            long_pause_ms: 2500,
            chunk_interval_ms: 2000,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            providers: ProviderSettings::default(),
            sound: SoundSettings::default(),
            microphone: MicrophoneSettings::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Current on-disk schema. Bump when a field changes meaning; `migrate`
/// upgrades older files in place.
pub const SCHEMA_VERSION: u32 = 3;

impl Settings {
    pub fn to_json(&self) -> Result<String, SettingsError> {
        Ok(serde_json::to_string_pretty(self)?)
    }
    pub fn from_json(s: &str) -> Result<Self, SettingsError> {
        let mut v: Self = serde_json::from_str(s)?;
        v.migrate();
        Ok(v)
    }

    /// Default settings file location (desktop).
    pub fn default_path() -> Option<PathBuf> {
        store::default_config_dir().map(|d| d.join(store::SETTINGS_FILE))
    }

    /// Load from `path`; a missing file yields defaults (first start).
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        let mut s: Self = store::read_json(path)?.unwrap_or_default();
        s.migrate();
        Ok(s)
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        store::write_json(path, self)
    }

    /// Upgrade older schemas. Unknown newer versions are kept as they are.
    pub fn migrate(&mut self) {
        // Up to v3: a short-lived default of 12 s chunks is taken back
        // (files stamped v2 by that build included).
        if self.schema_version < 3 && self.sound.vad.chunk_interval_ms == 12000 {
            self.sound.vad.chunk_interval_ms = Self::default().sound.vad.chunk_interval_ms;
        }
        if self.schema_version < SCHEMA_VERSION {
            self.schema_version = SCHEMA_VERSION;
        }
    }
    /// Options for the active provider (defaults if none stored).
    pub fn active_options(&self) -> ProviderOptions {
        self.providers
            .options
            .get(&self.providers.active)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let s = Settings::default();
        let back = Settings::from_json(&s.to_json().unwrap()).unwrap();
        assert_eq!(back.providers.active, "deepgram");
        assert_eq!(back.sound.vad.silence_duration_ms, 900);
        assert!(back.microphone.device_id.is_none());
        assert_eq!(back.active_options().transfer, TransferMode::Streaming);
    }

    #[test]
    fn load_save_roundtrip_and_missing_file_is_default() {
        let dir = std::env::temp_dir().join(format!("ikk-settings-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.providers.active, "deepgram");
        let mut s = loaded;
        s.providers.active = "openai".into();
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().providers.active, "openai");
        assert!(
            !dir.join("settings.json.tmp").exists(),
            "atomic write leaves no temp file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn per_provider_options() {
        let mut s = Settings::default();
        s.providers.options.insert(
            "voiceitt".into(),
            ProviderOptions {
                transfer: TransferMode::Batch,
                capitalisation: true,
                ..Default::default()
            },
        );
        s.providers.active = "voiceitt".into();
        assert_eq!(s.active_options().transfer, TransferMode::Batch);
        assert!(s.active_options().capitalisation);
    }

    #[test]
    fn old_chunk_interval_is_migrated() {
        for stamped in [1, 2] {
            let mut s = Settings {
                schema_version: stamped,
                ..Default::default()
            };
            s.sound.vad.chunk_interval_ms = 12000;
            s.migrate();
            assert_eq!(s.sound.vad.chunk_interval_ms, 2000);
            assert_eq!(s.schema_version, SCHEMA_VERSION);
        }
        // A value the user chose on purpose stays.
        let mut s = Settings {
            schema_version: 1,
            ..Default::default()
        };
        s.sound.vad.chunk_interval_ms = 3000;
        s.migrate();
        assert_eq!(s.sound.vad.chunk_interval_ms, 3000);
    }
}
