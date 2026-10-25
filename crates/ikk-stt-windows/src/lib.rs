//! Windows' built-in speech recognition (`Windows.Media.SpeechRecognition`)
//! as a provider. No API key; free. Its dictation mode is a Microsoft cloud
//! service: it only works with "Online speech recognition" switched on in
//! the Windows privacy settings, and it needs consent like any cloud
//! provider. The recognizer records the default microphone itself, so the
//! engine opens no capture for it (`own_audio`).
//!
//! Empty on other platforms.

mod imp;

pub use imp::WindowsSpeech;

/// Provider id in settings and consent records.
pub const PROVIDER_ID: &str = "windows";

/// BCP-47 tag Windows expects for a settings language (ISO-639-3).
/// `None` = the system's speech language.
pub fn language_tag(iso639_3: &str) -> Option<&'static str> {
    Some(match ikk_stt::lang::iso639_1(iso639_3)? {
        "de" => "de-DE",
        "en" => "en-US",
        "fr" => "fr-FR",
        "it" => "it-IT",
        "es" => "es-ES",
        "pt" => "pt-PT",
        "nl" => "nl-NL",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_settings_languages_to_windows_tags() {
        assert_eq!(language_tag("deu"), Some("de-DE"));
        assert_eq!(language_tag("gsw"), Some("de-DE"));
        assert_eq!(language_tag("eng"), Some("en-US"));
        assert_eq!(language_tag(""), None);
        assert_eq!(language_tag("xyz"), None);
    }
}
