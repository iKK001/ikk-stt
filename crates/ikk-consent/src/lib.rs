//! Consent before personal data (voice, transcripts) leaves the device.
//! Cloud is OFF until the user allows a recipient. Follows App Store
//! guideline 5.1.2 and Swiss revDSG.
//!
//! Decisions are persisted as a dated record per recipient, so the app can
//! show when and for what the user consented, and revoke individually.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Use {
    Transcription,
    Suggestions,
    Translation,
    Speech,
}

/// A third party that can receive the user's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    pub id: String,
    pub name: String,
    pub uses: Vec<Use>,
    pub privacy_url: String,
}

/// Known cloud recipients with their privacy policies.
pub fn recipient_for(provider_id: &str, display_name: &str) -> Recipient {
    let (privacy_url, uses) = match provider_id {
        "deepgram" => ("https://deepgram.com/privacy", vec![Use::Transcription]),
        "voiceitt" => ("https://voiceitt.com/legal/privacy", vec![Use::Transcription]),
        "elevenlabs" => ("https://elevenlabs.io/privacy-policy", vec![Use::Transcription]),
        "openai" => (
            "https://openai.com/policies/privacy-policy/",
            vec![Use::Transcription, Use::Suggestions, Use::Translation],
        ),
        _ => ("", vec![Use::Transcription]),
    };
    Recipient {
        id: provider_id.to_string(),
        name: display_name.to_string(),
        uses,
        privacy_url: privacy_url.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allowed,
    Denied,
    NotAsked,
}

/// One persisted decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentRecord {
    pub recipient: Recipient,
    pub decision: Decision,
    /// Unix seconds of the decision.
    pub at: u64,
}

/// All decisions, keyed by recipient id. Serialisable; the shell or the
/// engine persists it via [`ConsentState::save`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ConsentState {
    pub schema_version: u32,
    pub records: BTreeMap<String, ConsentRecord>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl ConsentState {
    pub fn decision(&self, recipient_id: &str) -> Decision {
        self.records
            .get(recipient_id)
            .map(|r| r.decision)
            .unwrap_or(Decision::NotAsked)
    }
    pub fn allow(&mut self, recipient: Recipient) {
        self.records.insert(
            recipient.id.clone(),
            ConsentRecord {
                recipient,
                decision: Decision::Allowed,
                at: now_secs(),
            },
        );
    }
    pub fn deny(&mut self, recipient: Recipient) {
        self.records.insert(
            recipient.id.clone(),
            ConsentRecord {
                recipient,
                decision: Decision::Denied,
                at: now_secs(),
            },
        );
    }
    pub fn revoke(&mut self, recipient_id: &str) {
        self.records.remove(recipient_id);
    }
    pub fn revoke_all(&mut self) {
        self.records.clear();
    }

    pub fn default_path() -> Option<PathBuf> {
        ikk_settings::store::default_config_dir().map(|d| d.join(ikk_settings::store::CONSENT_FILE))
    }
    pub fn load(path: &Path) -> Result<Self, ikk_settings::SettingsError> {
        Ok(ikk_settings::store::read_json(path)?.unwrap_or_default())
    }
    pub fn save(&self, path: &Path) -> Result<(), ikk_settings::SettingsError> {
        ikk_settings::store::write_json(path, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_recorded_with_time() {
        let mut c = ConsentState::default();
        assert_eq!(c.decision("deepgram"), Decision::NotAsked);
        c.allow(recipient_for("deepgram", "Deepgram"));
        assert_eq!(c.decision("deepgram"), Decision::Allowed);
        assert!(c.records["deepgram"].at > 1_700_000_000);
        assert_eq!(
            c.records["deepgram"].recipient.privacy_url,
            "https://deepgram.com/privacy"
        );
        c.deny(recipient_for("deepgram", "Deepgram"));
        assert_eq!(c.decision("deepgram"), Decision::Denied);
        c.revoke("deepgram");
        assert_eq!(c.decision("deepgram"), Decision::NotAsked);
    }

    #[test]
    fn persists() {
        let dir = std::env::temp_dir().join(format!("ikk-consent-test-{}", std::process::id()));
        let path = dir.join("consent.json");
        let mut c = ConsentState::load(&path).unwrap();
        c.allow(recipient_for("openai", "OpenAI"));
        c.save(&path).unwrap();
        let back = ConsentState::load(&path).unwrap();
        assert_eq!(back.decision("openai"), Decision::Allowed);
        assert_eq!(back.records["openai"].recipient.uses.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }
}
