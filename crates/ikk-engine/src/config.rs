//! Settings → engine configuration. The only place that knows both shapes.

use std::time::Duration;

use ikk_audio::vad::VadMode;
use ikk_audio::{CaptureConfig, IosSessionMode};
use ikk_settings::{Denoise, IosSessionMode as SettingsIosMode, Settings, TransferMode as SettingsTransfer};
use ikk_stt::{SessionConfig, TransferMode};
use ikk_text::TextConfig;

use crate::StartConfig;

/// Ventilator mode: a thought ends this long after the longest breath.
const VENTILATOR_MARGIN: Duration = Duration::from_millis(1500);

/// The correction button's request from the settings.
pub fn correct_request(settings: &Settings) -> crate::CorrectRequest {
    crate::CorrectRequest {
        range: crate::CorrectionRange::parse(&settings.text.correction_range),
        corrector: settings.text.corrector.clone(),
        model: settings.text.ai_model.clone(),
        language: settings.providers.language.clone(),
        host_text: None,
    }
}

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
    session.learned = settings.text.training.learned_words();
    session.endpointing = Duration::from_millis(settings.sound.vad.silence_duration_ms as u64);
    session.chunk_interval = Duration::from_millis(settings.sound.vad.chunk_interval_ms as u64);
    session.refine = settings.sound.vad.refine;
    session.refine_preview = !settings.sound.vad.type_quick_text;
    session.agreement = settings.sound.vad.agreement.then(|| ikk_stt::Agreement {
        step: Duration::from_millis(settings.sound.vad.agreement_step_ms.max(500) as u64),
        windows: settings.sound.vad.agreement_windows.max(1) as usize,
        span: Duration::from_millis(settings.sound.vad.agreement_span_ms.max(3000) as u64),
    });
    let ventilator = &settings.sound.ventilator;
    if ventilator.enabled {
        let longest = Duration::from_millis(ventilator.breath_pause_ms.clamp(500, 10_000) as u64);
        session.breath = Some(ikk_stt::BreathPauses {
            longest,
            shorten: ventilator.shorten_pauses,
            is_word: ventilator
                .join_words
                .then_some(ikk_ondevice::is_word as fn(&str, &str) -> bool),
            language_of: Some(ikk_ondevice::language_of),
        });
        // A live stream is cut by the service at every pause, in the
        // middle of a word as well: pieces are sent instead, cut here.
        session.transfer = TransferMode::Batch;
        // No pause shorter than a breath plus a margin ends anything.
        session.endpointing = session.endpointing.max(longest + VENTILATOR_MARGIN);
    }
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

    let session_pause = if ventilator.enabled {
        session.endpointing
    } else {
        Duration::from_millis(settings.sound.vad.silence_duration_ms as u64)
    };
    let capture = CaptureConfig {
        device_id: settings.microphone.device_id.clone(),
        preamp_gain_db: settings.sound.preamp_gain_db,
        denoise: settings.sound.denoise != Denoise::Off,
        agc: settings.sound.agc,
        gate: settings.sound.gate,
        highpass_hz: settings.sound.highpass_hz,
        presence_db: settings.sound.presence_db,
        compressor: settings.sound.compressor,
        trill_filler: settings.sound.trill_filler,
        noise_profile: settings.sound.noise_profile,
        path_eq_db: settings.sound.path_eq_db.clone(),
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
            learned: settings.text.training.learned_words(),
            gender_colon: Vec::new(),
            gender_colon_auto: settings.text.gender_colon_auto
                && matches!(settings.providers.language.as_str(), "deu" | "gsw"),
            broken_words: ventilator.enabled,
        },
        vad_mode,
        pause: session_pause,
        command_locale: match settings.providers.language.as_str() {
            "deu" | "gsw" => "de-CH".into(),
            "eng" => "en".into(),
            _ => String::new(),
        },
        flow_ms: 10,
        correction: settings.text.live_correction,
        corrector: settings.text.corrector.clone(),
        correction_rewrites: settings.text.correction_rewrites,
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
        assert_eq!(c.session.provider.0, "elevenlabs");
        assert_eq!(c.session.transfer, TransferMode::Batch);
        assert_eq!(c.pause, Duration::from_millis(900));
        assert!(!c.session.refine);
        assert!(c.session.agreement.is_none());
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

    #[test]
    fn ventilator_sends_pieces_and_waits_out_a_breath() {
        let mut settings = Settings::default();
        settings.sound.vad.silence_duration_ms = 900;
        settings
            .providers
            .options
            .entry(settings.providers.active.clone())
            .or_default()
            .transfer = SettingsTransfer::Streaming;
        assert!(start_config(&settings).session.breath.is_none());
        settings.sound.ventilator.enabled = true;
        let c = start_config(&settings);
        let breath = c.session.breath.expect("ventilator mode");
        assert_eq!(breath.longest, Duration::from_millis(3000));
        assert!(breath.shorten && breath.is_word.is_some());
        assert_eq!(c.session.transfer, TransferMode::Batch);
        assert_eq!(c.pause, Duration::from_millis(4500));
        assert_eq!(c.session.endpointing, c.pause);
        assert!(c.text.broken_words);
        settings.sound.ventilator.join_words = false;
        assert!(start_config(&settings).session.breath.unwrap().is_word.is_none());
    }

    #[test]
    fn word_parts_are_joined_by_the_german_dictionary() {
        let join = |t: &str| ikk_text::join_word_parts(t, &|w| ikk_ondevice::is_word(w, "deu"));
        assert_eq!(join("Ich bin im Kran ken haus."), "Ich bin im Krankenhaus.");
        assert_eq!(join("Das Tele fon klingelt"), "Das Telefon klingelt");
        assert_eq!(join("Die Beat mung läuft"), "Die Beatmung läuft");
        assert_eq!(join("Wir haben es ge macht"), "Wir haben es gemacht");
        // Real words next to each other stay as they are.
        for plain in [
            "Ich bin zu Hause und es geht mir gut.",
            "Wir kommen heute Abend an und bleiben da.",
            "Das ist so wie es sein soll.",
            "Er hat mit Arbeit viel zu tun.",
            "Sie geht in die Stadt um ein Buch zu kaufen.",
        ] {
            assert_eq!(join(plain), plain);
        }
    }

    #[test]
    fn word_parts_are_joined_in_french_italian_and_english() {
        let join = |t: &str, lang: &str| {
            // As the session does it: without a set language, the text's own.
            let lang = if lang.is_empty() {
                ikk_ondevice::language_of(t)
            } else {
                lang
            };
            ikk_text::join_word_parts(t, &|w| ikk_ondevice::is_word(w, lang))
        };
        assert_eq!(
            join("Je suis à l'hôpital avec le respi rateur", "fra"),
            "Je suis à l'hôpital avec le respirateur"
        );
        assert_eq!(join("Le venti lateur marche", "fra"), "Le ventilateur marche");
        assert_eq!(join("Sono in ospe dale", "ita"), "Sono in ospedale");
        assert_eq!(join("Il respi ratore funziona", "ita"), "Il respiratore funziona");
        assert_eq!(join("I am in the hos pital", "eng"), "I am in the hospital");
        // Without a set language the text's own words say which it is
        // (a few words alone can belong to several languages; then the
        // guess may miss and nothing is joined).
        assert_eq!(ikk_ondevice::language_of("Ich bin im Kran ken haus."), "deu");
        assert_eq!(
            ikk_ondevice::language_of("Le venti lateur marche très bien aujourd'hui"),
            "fra"
        );
        assert_eq!(
            ikk_ondevice::language_of("Oggi sono in ospe dale e sto abbastanza bene"),
            "ita"
        );
        assert_eq!(
            ikk_ondevice::language_of("I am in the hos pital and I feel well today"),
            "eng"
        );
        assert_eq!(join("Ich bin im Kran ken haus.", ""), "Ich bin im Krankenhaus.");
        assert_eq!(
            join("Le venti lateur marche très bien aujourd'hui", ""),
            "Le ventilateur marche très bien aujourd'hui"
        );
        for (plain, lang) in [
            ("Je vais à la maison et tout va bien.", "fra"),
            // The limit of the rule: "télé" and "phone" are words themselves.
            ("Le télé phone sonne", "fra"),
            ("Il a mis le livre sur la table en bois.", "fra"),
            ("Vado a casa e va tutto bene.", "ita"),
            ("Ho messo il libro sul tavolo di legno.", "ita"),
            ("I am going home and all is well.", "eng"),
            ("Ich bin zu Hause und es geht mir gut.", ""),
        ] {
            assert_eq!(join(plain, lang), plain);
        }
    }
}
