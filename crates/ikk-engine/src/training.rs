//! Training mode: one take of a word. The microphone from the settings is
//! opened through the same capture chain dictation uses, the utterance is
//! cut out and sent to the provider **without hints or learned words**, so
//! the answer is what the recogniser really hears for this speaker. The
//! shell stores that answer as a heard form of the word
//! ([`ikk_settings::TrainingSettings::learn`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use ikk_audio::AudioFrame;
use ikk_settings::Settings;
use ikk_stt::{AudioClip, Provider, Secret};

/// Room level is measured for this long after the microphone opens.
const CALIBRATION: Duration = Duration::from_millis(600);
/// Audio kept before the first speech frame (a soft onset).
const PRE_ROLL: Duration = Duration::from_millis(300);
/// Quiet after speech that ends the take.
const TAIL_QUIET: Duration = Duration::from_millis(900);
/// Speech shorter than this is a click, not a word.
const MIN_SPEECH: Duration = Duration::from_millis(250);
/// Longest take.
const MAX_SPEECH: Duration = Duration::from_secs(6);
const MIN_LEVEL: f32 = 0.003;
const MIN_SNR: f32 = 2.0;

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

/// Record one utterance and return what the provider heard (trimmed; may
/// be empty when it heard nothing in it). `timeout`: how long to wait for
/// the speaker to start.
pub async fn take(
    provider: Arc<dyn Provider>,
    settings: &Settings,
    credential: Secret,
    timeout: Duration,
) -> Result<String, String> {
    let start = crate::config::start_config(settings);
    let mut capture = ikk_audio::open_capture(&start.capture).map_err(|e| e.to_string())?;
    let opened = Instant::now();
    let deadline = opened + CALIBRATION + timeout.max(Duration::from_secs(1));
    let mut room_levels: Vec<f32> = Vec::new();
    let mut room = 0f32;
    let mut pre: Vec<AudioFrame> = Vec::new();
    let mut taken: Vec<AudioFrame> = Vec::new();
    let mut speech_len = Duration::ZERO;
    let mut quiet_len = Duration::ZERO;
    let mut speaking = false;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if !speaking && left.is_zero() {
            return Err("nothing heard".into());
        }
        let wait = if speaking { Duration::from_secs(2) } else { left };
        let frame = match tokio::time::timeout(wait, capture.frames().recv()).await {
            Ok(Some(f)) => f,
            Ok(None) => return Err("microphone closed".into()),
            Err(_) if speaking => break,
            Err(_) => return Err("nothing heard".into()),
        };
        let level = rms(&frame.samples);
        if opened.elapsed() < CALIBRATION {
            room_levels.push(level);
            continue;
        }
        if !room_levels.is_empty() {
            room_levels.sort_by(|a, b| a.total_cmp(b));
            room = room_levels[room_levels.len() / 2];
            room_levels.clear();
        }
        let loud = level > MIN_LEVEL && level > room * MIN_SNR;
        let is_speech = frame.speech.map_or(loud, |p| p >= 0.5 || (loud && p >= 0.2));
        let dur = frame.duration();
        if !speaking {
            pre.push(frame);
            let mut kept = Duration::ZERO;
            let cut = pre
                .iter()
                .rposition(|f| {
                    kept += f.duration();
                    kept >= PRE_ROLL
                })
                .unwrap_or(0);
            pre.drain(..cut);
            if is_speech {
                speaking = true;
                taken.append(&mut pre);
                speech_len = dur;
            }
            continue;
        }
        taken.push(frame);
        if is_speech {
            speech_len += dur;
            quiet_len = Duration::ZERO;
        } else {
            quiet_len += dur;
        }
        if (quiet_len >= TAIL_QUIET && speech_len >= MIN_SPEECH) || speech_len >= MAX_SPEECH {
            break;
        }
        if quiet_len >= TAIL_QUIET {
            // A click, not a word: keep waiting for the real take.
            speaking = false;
            taken.clear();
            speech_len = Duration::ZERO;
            quiet_len = Duration::ZERO;
        }
    }
    drop(capture);
    tracing::debug!(
        speech_ms = speech_len.as_millis() as u64,
        clip_ms = taken.iter().map(|f| f.duration()).sum::<Duration>().as_millis() as u64,
        room,
        "training take"
    );
    if speech_len < MIN_SPEECH {
        return Err("heard something too short for a word".into());
    }
    let mut cfg = start.session;
    cfg.credential = Some(credential);
    cfg.keywords.clear();
    cfg.learned.clear();
    cfg.transfer = ikk_stt::TransferMode::Batch;
    let t = provider
        .transcribe(AudioClip { frames: taken }, &cfg)
        .await
        .map_err(|e| e.to_string())?;
    Ok(t.text.trim().to_string())
}
