//! Offline voice commands for shells: record the user's takes, train the
//! two words. Switching them on goes through [`crate::engine::FfiEngine`].

use std::time::{Duration, Instant};

use crate::engine::runtime;

/// Room noise is measured for this long before a take counts.
const CALIBRATION: Duration = Duration::from_millis(600);
/// A take must be this much louder than the room (RMS).
const MIN_SNR: f32 = 2.5;
const MIN_LEVEL: f32 = 0.003;
const MIN_WORD: Duration = Duration::from_millis(250);
const MAX_WORD: Duration = Duration::from_secs(3);

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

/// Record one spoken word with the microphone from the settings: measures
/// the room first, then waits up to `timeout_secs` for an utterance that is
/// clearly louder than the room and as long as a word. Opening the
/// microphone, noise and clicks never count as a take.
/// Call it while no dictation and no voice standby runs.
pub fn record_take(settings_json: &str, timeout_secs: u32) -> Result<Vec<f32>, String> {
    let settings = ikk_settings::Settings::from_json(settings_json).map_err(|e| e.to_string())?;
    let mut capture_cfg = ikk_engine::config::start_config(&settings).capture;
    // Raw signal: automatic gain raises the room noise in the first seconds
    // after opening, which looked like a word. The matcher compares the
    // shape of the sound, so the gain settings do not matter for it.
    capture_cfg.agc = false;
    capture_cfg.preamp_gain_db = 0.0;
    runtime().block_on(async move {
        let mut capture = ikk_audio::open_capture(&capture_cfg).map_err(|e| e.to_string())?;
        let mut segmenter = ikk_wake::Segmenter::new();
        let opened = Instant::now();
        let deadline = opened + CALIBRATION + Duration::from_secs(timeout_secs.max(1) as u64);
        let mut noise: Vec<f32> = Vec::new();
        let mut room = 0f32;
        let mut last_reason = String::new();
        let result = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(no_word(&last_reason));
            }
            match tokio::time::timeout(left, capture.frames().recv()).await {
                Ok(Some(frame)) if frame.sample_rate_hz == ikk_wake::SAMPLE_RATE_HZ => {
                    let heard = segmenter.push(&frame.samples);
                    if opened.elapsed() < CALIBRATION {
                        noise.push(rms(&frame.samples));
                        continue; // warm-up: the detector learns the room
                    }
                    if !noise.is_empty() {
                        // Whatever the detector took for speech while it was
                        // still learning the room does not count.
                        segmenter.discard_current();
                        // A typical quiet moment, not the loudest: the click of
                        // the microphone opening must not count as the room.
                        noise.sort_by(f32::total_cmp);
                        room = noise[noise.len() / 5];
                        noise.clear();
                    }
                    let Some(utterance) = heard else { continue };
                    let word = ikk_wake::features::trim_quiet_edges(&utterance);
                    let secs = word.len() as f32 / ikk_wake::SAMPLE_RATE_HZ as f32;
                    let level = rms(word);
                    let needed = (room * MIN_SNR).max(MIN_LEVEL);
                    if level < needed {
                        last_reason = format!("too quiet (level {level:.4}, room {room:.4}, needed {needed:.4})");
                    } else if secs < MIN_WORD.as_secs_f32() {
                        last_reason = format!("too short ({secs:.2} s)");
                    } else if secs > MAX_WORD.as_secs_f32() {
                        last_reason = format!("too long ({secs:.2} s)");
                    } else {
                        break Ok(utterance);
                    }
                    tracing::info!(reason = %last_reason, "voice take ignored");
                }
                Ok(Some(_)) => {}
                Ok(None) => break Err("microphone closed".to_string()),
                Err(_) => break Err(no_word(&last_reason)),
            }
        };
        capture.stop();
        result
    })
}

/// The message for a take that did not count, with the last reason.
fn no_word(last_reason: &str) -> String {
    if last_reason.is_empty() {
        "nothing heard".to_string()
    } else {
        format!("heard something, but {last_reason}")
    }
}

/// Check the takes of one word right after recording them.
pub fn check_word(takes: &[Vec<f32>]) -> Result<(), String> {
    ikk_wake::VoiceCommands::check_word(takes).map_err(|e| e.to_string())
}

/// Train start and stop word from three or more takes each. Returns the
/// model as JSON for the shell to store.
pub fn train(start_takes: &[Vec<f32>], stop_takes: &[Vec<f32>]) -> Result<String, String> {
    ikk_wake::VoiceCommands::train(start_takes, stop_takes)
        .map(|c| c.to_json())
        .map_err(|e| e.to_string())
}
