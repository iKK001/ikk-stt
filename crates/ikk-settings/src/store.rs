//! Where settings and consent live on disk, and how they are written.
//!
//! Desktop: the platform config directory (`~/Library/Application Support`,
//! `%APPDATA%`, `~/.config`) under `ch.ikk/ikk-stt`. Mobile shells pass an
//! explicit directory (app sandbox). Writes are atomic: temp file + rename,
//! so a crash never leaves a half-written settings file.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::SettingsError;

pub const SETTINGS_FILE: &str = "settings.json";
pub const CONSENT_FILE: &str = "consent.json";

/// Platform config directory for this app; `None` on platforms without a
/// home directory concept (the shell must then pass a path).
pub fn default_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("ch", "ikk", "ikk-stt").map(|d| d.config_dir().to_path_buf())
}

/// Platform directory for data that stays on this machine: on Windows
/// `%LOCALAPPDATA%` (not the roaming profile, which a domain may copy to
/// other PCs), elsewhere the same place as the config.
pub fn default_local_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("ch", "ikk", "ikk-stt").map(|d| d.data_local_dir().to_path_buf())
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), SettingsError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
