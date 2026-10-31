//! A recording as the microphone: device id `file:<path to .wav>`. The
//! file goes through the same chain as a microphone would (preamp,
//! denoise, AGC, noise model listening) and is delivered in real time, so
//! providers, cut points and typing behave exactly as in a live session;
//! after the last sample the source keeps delivering silence until it is
//! stopped. The same sample can thus be fed to every platform and
//! provider, which a loudspeaker in a room never does reproducibly.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::info;

use crate::chain::{CaptureChain, ChainConfig};
use crate::dsp::{rms, rms_to_dbfs};
use crate::{AudioError, AudioFrame, Capture, CaptureConfig};

/// Device ids with this prefix name a WAV file instead of a microphone.
pub const DEVICE_PREFIX: &str = "file:";

/// Delivered every 20 ms like a typical microphone callback.
const BLOCK: Duration = Duration::from_millis(20);
const FRAME_CHANNEL_CAPACITY: usize = 512;

pub struct FileCapture {
    frames: tokio::sync::mpsc::Receiver<AudioFrame>,
    stop: Arc<AtomicBool>,
    level_milli_dbfs: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl Capture for FileCapture {
    fn frames(&mut self) -> &mut tokio::sync::mpsc::Receiver<AudioFrame> {
        &mut self.frames
    }
    fn level_dbfs(&self) -> f32 {
        self.level_milli_dbfs.load(Ordering::Relaxed) as f32 / 1000.0 - 200.0
    }
    fn stop(mut self: Box<Self>) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn open(cfg: &CaptureConfig, path: &Path) -> Result<Box<dyn Capture>, AudioError> {
    let rate = cfg.sample_rate_hz.max(8_000);
    let pcm = crate::wav::read_mono(path, rate)?;
    info!(file = %path.display(), seconds = pcm.len() as f32 / rate as f32, "replaying a recording as the microphone");
    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(FRAME_CHANNEL_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let level = Arc::new(AtomicU32::new(0));
    let mut chain = CaptureChain::new(ChainConfig {
        preamp_gain_db: cfg.preamp_gain_db,
        highpass_hz: cfg.highpass_hz,
        presence_db: cfg.presence_db,
        compressor: cfg.compressor,
        trill_filler: cfg.trill_filler,
        noise_profile: cfg.noise_profile,
        path_eq_db: cfg.path_eq_db.clone(),
        denoise: cfg.denoise,
        listen: true,
        gate: cfg.gate,
        agc: cfg.agc,
        agc_target_dbfs: -16.0,
        sample_rate_hz: rate,
    });
    let (stop_t, level_t) = (stop.clone(), level.clone());
    let thread = thread::Builder::new()
        .name("ikk-audio-replay".into())
        .spawn(move || {
            let block = (rate as usize * BLOCK.as_millis() as usize) / 1000;
            let silence = vec![0.0f32; block];
            let start = Instant::now();
            let mut sent = 0usize;
            let mut next = start;
            while !stop_t.load(Ordering::Relaxed) {
                next += BLOCK;
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
                let input = if sent < pcm.len() {
                    &pcm[sent..(sent + block).min(pcm.len())]
                } else {
                    &silence[..]
                };
                let processed = chain.process(input).into_owned();
                let offset = Duration::from_secs_f64(sent as f64 / rate as f64);
                sent += input.len().max(block);
                if processed.is_empty() {
                    continue;
                }
                let dbfs = rms_to_dbfs(rms(&processed));
                level_t.store(((dbfs + 200.0) * 1000.0).max(0.0) as u32, Ordering::Relaxed);
                let frame = AudioFrame {
                    samples: processed,
                    sample_rate_hz: rate,
                    offset,
                    speech: chain.speech_probability(),
                };
                if frame_tx.blocking_send(frame).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    Ok(Box::new(FileCapture {
        frames: frame_rx,
        stop,
        level_milli_dbfs: level,
        thread: Some(thread),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plays_the_file_in_real_time_then_silence() {
        let dir = std::env::temp_dir().join(format!("ikk-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        // 100 ms of a loud signal.
        crate::wav::write(&path, &vec![0.5f32; 1600], 16_000).unwrap();
        let cfg = CaptureConfig {
            device_id: Some(format!("{DEVICE_PREFIX}{}", path.display())),
            ..Default::default()
        };
        let started = Instant::now();
        let mut cap = crate::open_capture(&cfg).unwrap();
        let mut loud = 0;
        let mut quiet = 0;
        for _ in 0..8 {
            let f = cap.frames().recv().await.unwrap();
            assert_eq!(f.sample_rate_hz, 16_000);
            assert!(f.speech.is_some(), "the noise model listens");
            if f.level_dbfs() > -20.0 {
                loud += 1;
            } else {
                quiet += 1;
            }
        }
        // 5 blocks of signal, then silence; 8 blocks take about 160 ms.
        assert_eq!((loud, quiet), (5, 3));
        assert!(
            started.elapsed() >= Duration::from_millis(140),
            "{:?}",
            started.elapsed()
        );
        cap.stop();
        let _ = std::fs::remove_dir_all(dir);
    }
}
