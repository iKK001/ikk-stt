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
    #[serde(default)]
    pub text: TextSettings,
    /// The app itself (language of its own words).
    #[serde(default)]
    pub app: AppSettings,
}

/// Settings of the shell, kept here so that they round-trip through the
/// core's settings file like the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSettings {
    /// ISO 639-3 code of the app's own language ("" = English).
    #[serde(default)]
    pub language: String,
}

/// What happens to the text after recognition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextSettings {
    /// Grammar and spelling checked live by LanguageTool and corrected in
    /// place. Text leaves the device; asked for consent like a provider.
    #[serde(default)]
    pub live_correction: bool,
    /// Live correction rewrites the typed text (deletes and types again).
    /// Off: the corrected text is only shown in the app as a suggestion.
    #[serde(default)]
    pub correction_rewrites: bool,
    /// Training mode: words entered once and said a few times.
    #[serde(default)]
    pub training: TrainingSettings,
}

/// Learned words and whether they are used. Stored with the settings, on
/// the device; the words go to a provider only as hints with a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub words: Vec<LearnedEntry>,
}

impl Default for TrainingSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            words: Vec::new(),
        }
    }
}

/// One trained word: how it is written, what the recogniser returned for
/// it (with how often), and whether its forms are swapped even where the
/// recogniser was sure (only for words whose mishearings are never real
/// words).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LearnedEntry {
    pub word: String,
    #[serde(default)]
    pub heard: Vec<HeardForm>,
    #[serde(default)]
    pub always: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeardForm {
    pub text: String,
    #[serde(default)]
    pub count: u32,
}

impl TrainingSettings {
    /// The entries as the session uses them (empty when switched off).
    pub fn learned_words(&self) -> Vec<ikk_text::LearnedWord> {
        if !self.enabled {
            return Vec::new();
        }
        self.words
            .iter()
            .filter(|e| !e.word.trim().is_empty())
            .map(|e| ikk_text::LearnedWord {
                word: e.word.trim().to_string(),
                heard: e
                    .heard
                    .iter()
                    .map(|h| h.text.trim().to_string())
                    .filter(|h| !h.is_empty())
                    .collect(),
                always: e.always,
            })
            .collect()
    }

    /// Record what the recogniser returned for `word`. A result equal to
    /// the word (ignoring case and punctuation) is a hit and not a form.
    pub fn learn(&mut self, word: &str, heard: &str) {
        let word = word.trim();
        let heard = heard.trim();
        if word.is_empty() {
            return;
        }
        let entry = match self
            .words
            .iter_mut()
            .find(|e| ikk_text::learned::norm(&e.word) == ikk_text::learned::norm(word))
        {
            Some(e) => e,
            None => {
                self.words.push(LearnedEntry {
                    word: word.to_string(),
                    ..Default::default()
                });
                self.words.last_mut().expect("just pushed")
            }
        };
        if heard.is_empty() || ikk_text::learned::norm(heard) == ikk_text::learned::norm(word) {
            return;
        }
        let heard_norm = ikk_text::learned::norm(heard);
        match entry
            .heard
            .iter_mut()
            .find(|h| ikk_text::learned::norm(&h.text) == heard_norm)
        {
            Some(h) => h.count += 1,
            None => entry.heard.push(HeardForm {
                text: heard.trim_end_matches(['.', ',', '!', '?', ';', ':']).to_string(),
                count: 1,
            }),
        }
    }
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
    Streaming,
    /// Chunked upload: chunks cut between words, higher accuracy, the
    /// default — a live stream of unclear speech comes back empty.
    #[default]
    Batch,
}

/// Partial entries (the app writes just `{"transfer": "Batch"}`) take the
/// defaults for the rest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
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
    /// Master switch. Off: the pipeline as released for clear speech
    /// (v0.0.1(51): 2 s chunks cut at word gaps, 0.9 s pause, live
    /// streams where the provider has them, no filters). On: the
    /// impaired-voice setup being developed (agreed words only, batch,
    /// steady-noise filter, 4 s pause). The app writes the matching
    /// values into the fields below when this is toggled.
    #[serde(default = "default_true")]
    pub impaired_voice: bool,
    /// Software gain before any filter, 0..=24 dB.
    pub preamp_gain_db: f32,
    /// Automatic gain: speech is brought to a steady level before it is
    /// sent, whatever the microphone and distance.
    #[serde(default = "default_true")]
    pub agc: bool,
    /// Turn the pauses between words down: breathing or a ventilator in
    /// the pauses is otherwise transcribed as words.
    #[serde(default)]
    pub gate: bool,
    /// Tone shaping (all platforms): high-pass cutoff Hz (0 = off),
    /// presence lift at 3 kHz in dB (0 = off), fast compressor.
    #[serde(default)]
    pub highpass_hz: f32,
    #[serde(default)]
    pub presence_db: f32,
    #[serde(default)]
    pub compressor: bool,
    #[serde(default)]
    pub trill_filler: bool,
    /// Learnt steady-noise profile (ventilator, fan) subtracted from the
    /// audio. On by default: neutral on clean speech, the only filter that
    /// helped on a noisy path (measured 2026-11-01).
    #[serde(default = "default_true")]
    pub noise_profile: bool,
    /// Microphone-path equaliser from a calibration (18 third-octave
    /// gains in dB, 100 Hz–6.3 kHz); empty = none.
    #[serde(default)]
    pub path_eq_db: Vec<f32>,
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
    /// Batch mode: once a chunk is this long it is cut at the next gap
    /// between words (never inside one); without any gap at three times
    /// this length. Pauses cut as well.
    pub chunk_interval_ms: u32,
    /// Batch mode: when a pause ends a thought that took several chunks,
    /// transcribe it again in one piece and replace the quick chunk texts.
    #[serde(default)]
    pub refine: bool,
    /// With `refine`: type the quick chunk texts too and replace them
    /// later (off: show them only in the app, type each thought once).
    #[serde(default = "default_true")]
    pub type_quick_text: bool,
    /// Agreed words only: the thought so far is transcribed every
    /// `agreement_step_ms`; words that the last `agreement_windows`
    /// results agree on are typed and never changed. Overrides `refine`.
    #[serde(default = "default_true")]
    pub agreement: bool,
    #[serde(default = "default_agreement_step")]
    pub agreement_step_ms: u32,
    #[serde(default = "default_agreement_windows")]
    pub agreement_windows: u32,
}

fn default_agreement_step() -> u32 {
    1000
}
fn default_agreement_windows() -> u32 {
    // Four readings remembered, a word out when three agree. Six with
    // four agreeing was tried live on the loudspeaker→phone path and lost
    // whole sentences (124 wrong of 240) where four lost 44–51.
    4
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
            // ElevenLabs: the steadiest reading of unclear speech in the
            // measurements (same words every run); others are one tap away.
            active: "elevenlabs".into(),
            language: "deu".into(),
            options: BTreeMap::new(),
        }
    }
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            preamp_gain_db: 0.0,
            impaired_voice: true,
            agc: true,
            gate: false,
            highpass_hz: 0.0,
            presence_db: 0.0,
            compressor: false,
            trill_filler: false,
            noise_profile: true,
            path_eq_db: Vec::new(),
            denoise: Denoise::Off,
            ios_session_mode: IosSessionMode::Default,
            vad: VadSettings::default(),
        }
    }
}

impl Default for VadSettings {
    fn default() -> Self {
        // The "word by word" preset, measured on impaired speech: quick
        // 3 s chunks, a thought ends after 4 s, then it is re-checked whole.
        // Fluent speakers switch to 2 s / 0.9 s without re-check.
        Self {
            silence_threshold_db: -45.0,
            silence_duration_ms: 4000,
            long_pause_ms: 2500,
            chunk_interval_ms: 3000,
            refine: true,
            type_quick_text: true,
            agreement: true,
            agreement_step_ms: 1000,
            agreement_windows: 4,
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
            text: TextSettings::default(),
            app: AppSettings::default(),
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
pub const SCHEMA_VERSION: u32 = 4;

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
        // v4: the defaults tuned for fluent speech (2 s chunks, 0.9 s
        // pause, live streams, RNNoise) gave word salad for the people
        // this is built for; every install moves to the word-by-word
        // preset. The Fluent preset is one tap away.
        if self.schema_version < 4 {
            let d = Self::default();
            self.sound.vad.chunk_interval_ms = d.sound.vad.chunk_interval_ms;
            self.sound.vad.silence_duration_ms = d.sound.vad.silence_duration_ms;
            self.sound.vad.refine = true;
            self.sound.denoise = Denoise::Off;
            for o in self.providers.options.values_mut() {
                o.transfer = TransferMode::Batch;
            }
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
        assert_eq!(back.providers.active, "elevenlabs");
        assert_eq!(back.sound.vad.silence_duration_ms, 4000);
        assert!(back.sound.vad.refine);
        assert!(back.microphone.device_id.is_none());
        assert_eq!(back.active_options().transfer, TransferMode::Batch);
    }

    #[test]
    fn partial_provider_options_take_defaults() {
        let mut json: serde_json::Value = serde_json::from_str(&Settings::default().to_json().unwrap()).unwrap();
        json["providers"]["options"]["deepgram"] = serde_json::json!({ "transfer": "Batch" });
        let s = Settings::from_json(&json.to_string()).unwrap();
        assert_eq!(s.active_options().transfer, TransferMode::Batch);
        assert!(s.active_options().keywords.is_empty());
    }

    #[test]
    fn load_save_roundtrip_and_missing_file_is_default() {
        let dir = std::env::temp_dir().join(format!("ikk-settings-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.providers.active, "elevenlabs");
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
            assert_eq!(s.sound.vad.chunk_interval_ms, 3000);
            assert_eq!(s.schema_version, SCHEMA_VERSION);
        }
        // v3 installs ran the fluent defaults: they move to the preset,
        // their provider entries to batch, RNNoise off.
        let mut s = Settings {
            schema_version: 3,
            ..Default::default()
        };
        s.sound.vad.chunk_interval_ms = 2000;
        s.sound.vad.silence_duration_ms = 900;
        s.sound.vad.refine = false;
        s.sound.denoise = Denoise::RnNoise;
        s.providers.options.insert(
            "elevenlabs".into(),
            ProviderOptions {
                transfer: TransferMode::Streaming,
                ..Default::default()
            },
        );
        s.migrate();
        assert_eq!(s.sound.vad.chunk_interval_ms, 3000);
        assert_eq!(s.sound.vad.silence_duration_ms, 4000);
        assert!(s.sound.vad.refine);
        assert_eq!(s.sound.denoise, Denoise::Off);
        assert_eq!(s.providers.options["elevenlabs"].transfer, TransferMode::Batch);
        // A current file is left alone.
        let mut s = Settings::default();
        s.sound.vad.chunk_interval_ms = 2000;
        s.migrate();
        assert_eq!(s.sound.vad.chunk_interval_ms, 2000);
    }
}
