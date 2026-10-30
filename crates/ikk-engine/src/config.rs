//! Settings → engine configuration. The only place that knows both shapes.

use std::time::Duration;

use ikk_audio::vad::VadMode;
use ikk_audio::{CaptureConfig, IosSessionMode};
use ikk_settings::{Denoise, IosSessionMode as SettingsIosMode, Settings, TransferMode as SettingsTransfer};
use ikk_stt::{SessionConfig, TransferMode};
use ikk_text::TextConfig;

use crate::StartConfig;

/// Builds a start configuration from the user's settings for the active
/// provider. Credentials are not part of settings; the engine injects them.
pub fn start_config(settings: &Settings) -> StartConfig {
    let opts = settings.active_options();
    let mut session = SessionConfig::new(settings.providers.active.clone());
    session.language = settings.providers.language.clone();
    session.model = opts.model.clone();
    session.transfer = match opts.transfer {
        SettingsTransfer::Streaming => TransferMode::Streaming,
        SettingsTransfer::Batch => TransferMode::Batch,
    };
    session.keywords = opts.keywords.iter().map(|k| (k.clone(), 5)).collect();
    session.endpointing = Duration::from_millis(settings.sound.vad.silence_duration_ms as u64);
    session.chunk_interval = Duration::from_millis(settings.sound.vad.chunk_interval_ms as u64);
    if settings.providers.active == "voiceitt" {
        // Voiceitt's three switches ride in the model string (see provider docs).
        let mut flags = vec!["si"];
        if !opts.inverse_text_normalisation {
            flags.push("noitn");
        }
        if !opts.capitalisation {
            flags.push("nocap");
        }
        if opts.filter_profanities {
            flags.push("prof");
        }
        session.model = Some(flags.join(";"));
    }

    let capture = CaptureConfig {
        device_id: settings.microphone.device_id.clone(),
        preamp_gain_db: settings.sound.preamp_gain_db,
        denoise: settings.sound.denoise != Denoise::Off,
        agc: settings.sound.agc,
        ios_mode: match settings.sound.ios_session_mode {
            SettingsIosMode::Measurement => IosSessionMode::Measurement,
            SettingsIosMode::Default => IosSessionMode::Default,
            SettingsIosMode::VoiceChat => IosSessionMode::VoiceChat,
        },
        // The engine raises this for providers that want full bandwidth.
        sample_rate_hz: ikk_audio::PIPELINE_SAMPLE_RATE_HZ,
    };
    let vad_mode = if settings.sound.vad.silence_threshold_db > -35.0 {
        VadMode::Aggressive
    } else if settings.sound.vad.silence_threshold_db < -55.0 {
        VadMode::Sensitive
    } else {
        VadMode::Normal
    };
    StartConfig {
        session,
        capture,
        text: TextConfig {
            auto_punctuation: false,
            auto_casing: false,
            vocabulary: Vec::new(),
        },
        vad_mode,
        pause: Duration::from_millis(settings.sound.vad.silence_duration_ms as u64),
        command_locale: match settings.providers.language.as_str() {
            "deu" | "gsw" => "de-CH".into(),
            "eng" => "en".into(),
            _ => String::new(),
        },
        flow_ms: 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikk_settings::ProviderOptions;

    #[test]
    fn maps_defaults() {
        let s = Settings::default();
        let c = start_config(&s);
        assert_eq!(c.session.provider.0, "deepgram");
        assert_eq!(c.session.transfer, TransferMode::Streaming);
        assert_eq!(c.pause, Duration::from_millis(900));
        assert_eq!(c.vad_mode, VadMode::Normal);
        assert_eq!(c.command_locale, "de-CH");
        assert!(!c.capture.denoise);
    }

    #[test]
    fn voiceitt_flags_in_model() {
        let mut s = Settings::default();
        s.providers.active = "voiceitt".into();
        s.providers.options.insert(
            "voiceitt".into(),
            ProviderOptions {
                filter_profanities: true,
                ..Default::default()
            },
        );
        let c = start_config(&s);
        assert_eq!(c.session.model.as_deref(), Some("si;noitn;nocap;prof"));
    }
}
