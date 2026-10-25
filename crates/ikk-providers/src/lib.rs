//! Cloud STT adapters. Each module is one provider, feature-gated, and uses
//! plain HTTP / WebSocket / Socket.IO so no vendor SDK enters the AGPL build.
//!
//! Deepgram, Voiceitt, OpenAI: batch + streaming. ElevenLabs: batch (chunked session for live).
//! On Windows also Windows' own recognizer (crate `ikk-stt-windows`).

#[cfg(feature = "deepgram")]
pub mod deepgram;
#[cfg(feature = "elevenlabs")]
pub mod elevenlabs;
#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "voiceitt")]
pub mod voiceitt;

mod http;
pub mod socketio;

use ikk_stt::ProviderRegistry;

/// Registers every compiled-in provider.
pub fn register_all(reg: &mut ProviderRegistry) {
    #[cfg(feature = "deepgram")]
    reg.register(Box::new(deepgram::Deepgram));
    #[cfg(feature = "voiceitt")]
    reg.register(Box::new(voiceitt::Voiceitt::default()));
    #[cfg(feature = "openai")]
    reg.register(Box::new(openai::OpenAi));
    #[cfg(feature = "elevenlabs")]
    reg.register(Box::new(elevenlabs::ElevenLabs));
    reg.register(Box::new(ikk_stt_windows::WindowsSpeech));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_default_providers_register() {
        let mut reg = ProviderRegistry::new();
        register_all(&mut reg);
        let expected = if cfg!(target_os = "windows") { 5 } else { 4 };
        assert_eq!(reg.ids().len(), expected);
    }
}
