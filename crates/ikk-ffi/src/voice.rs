//! Offline voice commands for shells: record the user's takes, train the
//! two words. Switching them on goes through [`crate::engine::FfiEngine`].

use std::time::{Duration, Instant};

use crate::engine::runtime;

/// Record one spoken word with the microphone from the settings: waits up
/// to `timeout_secs` for speech and returns the utterance (16 kHz mono).
/// Call it while no dictation and no voice standby runs.
pub fn record_take(settings_json: &str, timeout_secs: u32) -> Result<Vec<f32>, String> {
    let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
    let capture_cfg = ikk_engine::config::start_config(&settings).capture;
    runtime().block_on(async move {
        let mut capture = ikk_audio::open_capture(&capture_cfg).map_err(|e| e.to_string())?;
        let mut segmenter = ikk_wake::Segmenter::new();
        let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1) as u64);
        let result = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err("no word heard; speak a little louder or closer".to_string());
            }
            match tokio::time::timeout(left, capture.frames().recv()).await {
                Ok(Some(frame)) if frame.sample_rate_hz == ikk_wake::SAMPLE_RATE_HZ => {
                    if let Some(utterance) = segmenter.push(&frame.samples) {
                        break Ok(utterance);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break Err("microphone closed".to_string()),
                Err(_) => break Err("no word heard; speak a little louder or closer".to_string()),
            }
        };
        capture.stop();
        result
    })
}

/// Train start and stop word from three or more takes each. Returns the
/// model as JSON for the shell to store.
pub fn train(start_takes: &[Vec<f32>], stop_takes: &[Vec<f32>]) -> Result<String, String> {
    ikk_wake::VoiceCommands::train(start_takes, stop_takes)
        .map(|c| c.to_json())
        .map_err(|e| e.to_string())
}
