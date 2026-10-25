//! Microphone capture with cpal on a dedicated thread.
//!
//! Opens the device in its *own* default format (forcing 16 kHz mono fails
//! on WASAPI), downmixes and resamples in the consumer, runs the processing
//! chain, and delivers 16 kHz mono frames. A stream error (device unplugged)
//! triggers device re-selection and stream rebuild, bounded by a retry
//! limit, so a hot-plug never leaves the session silently dead.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{DeviceId, SampleFormat, StreamConfig};
use tracing::{error, info, warn};

use crate::chain::{CaptureChain, ChainConfig};
use crate::dsp::{downmix_mono, resample_linear, rms, rms_to_dbfs};
use crate::mmcss::elevate_audio_thread;
use crate::{AudioError, AudioFrame, Capture, CaptureConfig, InputDevice, PIPELINE_SAMPLE_RATE_HZ};

const REBUILD_MAX_ATTEMPTS: u32 = 10;
const REBUILD_RETRY: Duration = Duration::from_millis(300);
/// Frames waiting for the engine. Large enough to bridge a provider's
/// connection set-up (a second or more) without losing the first words.
const FRAME_CHANNEL_CAPACITY: usize = 512;
/// Log only every n-th of a frequent, harmless event.
const LOG_EVERY: u64 = 200;

/// Pure selection logic: requested id present → that device; requested
/// but missing → default with `fell_back`; nothing requested → default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePick {
    pub id: Option<String>,
    pub fell_back: bool,
}

pub fn pick_device(requested: Option<&str>, available: &[String]) -> DevicePick {
    match requested {
        Some(want) if available.iter().any(|d| d == want) => DevicePick {
            id: Some(want.into()),
            fell_back: false,
        },
        Some(_) => DevicePick {
            id: None,
            fell_back: true,
        },
        None => DevicePick {
            id: None,
            fell_back: false,
        },
    }
}

fn describe(device: &cpal::Device) -> Option<InputDevice> {
    let id = device.id().ok()?.to_string();
    let name = device
        .description()
        .ok()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| id.clone());
    Some(InputDevice {
        id,
        name,
        is_default: false,
    })
}

pub fn list_input_devices() -> Result<Vec<InputDevice>, AudioError> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|i| i.to_string());
    let devices = host
        .input_devices()
        .map_err(|e| AudioError::Backend(e.to_string()))?
        .filter_map(|d| describe(&d))
        .map(|mut d| {
            d.is_default = default_id.as_deref() == Some(d.id.as_str());
            d
        })
        .collect();
    Ok(devices)
}

fn resolve_device(host: &cpal::Host, requested: Option<&str>) -> Result<cpal::Device, AudioError> {
    let available: Vec<String> = host
        .input_devices()
        .map_err(|e| AudioError::Backend(e.to_string()))?
        .filter_map(|d| d.id().ok().map(|i| i.to_string()))
        .collect();
    let pick = pick_device(requested, &available);
    if pick.fell_back {
        warn!(
            requested = requested.unwrap_or(""),
            "input device not found, using system default"
        );
    }
    match pick.id {
        Some(id) => {
            let parsed: DeviceId = id.parse().map_err(|_| AudioError::NoDevice)?;
            host.device_by_id(&parsed).ok_or(AudioError::NoDevice)
        }
        None => host.default_input_device().ok_or(AudioError::NoDevice),
    }
}

/// Raw device blocks (already f32, interleaved, device rate) plus the
/// device format needed to convert them.
struct OpenStream {
    stream: cpal::Stream,
    channels: usize,
    rate_hz: u32,
}

fn open_stream(
    host: &cpal::Host,
    cfg: &CaptureConfig,
    tx: &mpsc::Sender<Vec<f32>>,
    device_error: &Arc<AtomicBool>,
) -> Result<OpenStream, AudioError> {
    let device = resolve_device(host, cfg.device_id.as_deref())?;
    let supported = device
        .default_input_config()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let format = supported.sample_format();
    let mut stream_config: StreamConfig = supported.config();
    if stream_config.channels == 0 {
        // Mono is always valid; some backends report 0 before the session is fully up.
        stream_config.channels = 1;
    }
    let channels = stream_config.channels as usize;
    let rate_hz = stream_config.sample_rate;
    info!(device = ?describe(&device).map(|d| d.name), ?stream_config, ?format, "opening capture");

    let err_fn = {
        let flag = device_error.clone();
        move |err: cpal::Error| {
            let msg = err.to_string();
            // iOS reports audio route changes (speaker/Bluetooth switch, session
            // activation) through the error callback; the stream keeps running.
            if msg.to_lowercase().contains("route") {
                info!(%msg, "audio route changed; stream continues");
                return;
            }
            // A buffer over- or underrun loses a few milliseconds; the stream
            // keeps running. Reopening the device for it would cause the next
            // one (seen on Windows: a reopen loop many times per second).
            if err.kind() == cpal::ErrorKind::Xrun {
                static XRUNS: AtomicU64 = AtomicU64::new(0);
                let n = XRUNS.fetch_add(1, Ordering::Relaxed);
                if n % LOG_EVERY == 0 {
                    warn!(count = n + 1, "audio buffer over/underrun; stream continues");
                }
                return;
            }
            error!(%err, "capture stream error");
            flag.store(true, Ordering::SeqCst);
        }
    };

    macro_rules! build {
        ($t:ty, $conv:expr) => {{
            let tx = tx.clone();
            device
                .build_input_stream(
                    stream_config,
                    move |data: &[$t], _| {
                        elevate_audio_thread();
                        let _ = tx.send(data.iter().map($conv).collect());
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| AudioError::Backend(e.to_string()))?
        }};
    }
    let stream = match format {
        SampleFormat::F32 => build!(f32, |s: &f32| *s),
        SampleFormat::I16 => build!(i16, |s: &i16| *s as f32 / 32768.0),
        SampleFormat::U16 => build!(u16, |s: &u16| (*s as f32 / 32768.0) - 1.0),
        SampleFormat::I32 => build!(i32, |s: &i32| *s as f32 / 2_147_483_648.0),
        other => return Err(AudioError::Backend(format!("unsupported sample format {other:?}"))),
    };
    Ok(OpenStream {
        stream,
        channels,
        rate_hz,
    })
}

pub struct CpalCapture {
    frames: tokio::sync::mpsc::Receiver<AudioFrame>,
    stop: Arc<AtomicBool>,
    level_milli_dbfs: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl Capture for CpalCapture {
    fn frames(&mut self) -> &mut tokio::sync::mpsc::Receiver<AudioFrame> {
        &mut self.frames
    }
    fn level_dbfs(&self) -> f32 {
        // Stored as (dBFS + 200) * 1000 to fit an unsigned atomic.
        self.level_milli_dbfs.load(Ordering::Relaxed) as f32 / 1000.0 - 200.0
    }
    fn stop(mut self: Box<Self>) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn open_capture(cfg: &CaptureConfig) -> Result<Box<dyn Capture>, AudioError> {
    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(FRAME_CHANNEL_CAPACITY);
    let stop = Arc::new(AtomicBool::new(false));
    let level = Arc::new(AtomicU32::new(0));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), AudioError>>();

    let cfg = cfg.clone();
    let stop_t = stop.clone();
    let level_t = level.clone();
    let thread = thread::Builder::new()
        .name("ikk-audio-capture".into())
        .spawn(move || run_capture(cfg, frame_tx, stop_t, level_t, ready_tx))
        .map_err(|e| AudioError::Backend(e.to_string()))?;

    // Surface the open error synchronously (no device, no permission).
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = thread.join();
            return Err(e);
        }
        Err(_) => return Err(AudioError::Backend("capture thread died".into())),
    }
    Ok(Box::new(CpalCapture {
        frames: frame_rx,
        stop,
        level_milli_dbfs: level,
        thread: Some(thread),
    }))
}

fn run_capture(
    cfg: CaptureConfig,
    frame_tx: tokio::sync::mpsc::Sender<AudioFrame>,
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    ready: mpsc::Sender<Result<(), AudioError>>,
) {
    let host = cpal::default_host();
    let (raw_tx, raw_rx) = mpsc::channel::<Vec<f32>>();
    let device_error = Arc::new(AtomicBool::new(false));

    let mut open = match open_stream(&host, &cfg, &raw_tx, &device_error) {
        Ok(o) => o,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if let Err(e) = open.stream.play() {
        let _ = ready.send(Err(AudioError::Backend(e.to_string())));
        return;
    }
    let _ = ready.send(Ok(()));

    let mut chain = CaptureChain::new(ChainConfig {
        preamp_gain_db: cfg.preamp_gain_db,
        denoise: cfg.denoise,
        agc: cfg.agc,
        agc_target_dbfs: -20.0,
        sample_rate_hz: PIPELINE_SAMPLE_RATE_HZ,
    });
    let mut samples_sent: u64 = 0;
    let mut rebuild_attempts = 0u32;

    let mut consume = |buf: Vec<f32>, open: &OpenStream| {
        let mono = downmix_mono(&buf, open.channels);
        let res = resample_linear(&mono, open.rate_hz, PIPELINE_SAMPLE_RATE_HZ);
        let processed = chain.process(&res);
        if processed.is_empty() {
            return;
        }
        let dbfs = rms_to_dbfs(rms(&processed));
        level.store(((dbfs + 200.0) * 1000.0).max(0.0) as u32, Ordering::Relaxed);
        let offset = Duration::from_secs_f64(samples_sent as f64 / PIPELINE_SAMPLE_RATE_HZ as f64);
        samples_sent += processed.len() as u64;
        let frame = AudioFrame {
            samples: processed.into_owned(),
            sample_rate_hz: PIPELINE_SAMPLE_RATE_HZ,
            offset,
        };
        if frame_tx.try_send(frame).is_err() {
            static DROPPED: AtomicU64 = AtomicU64::new(0);
            let n = DROPPED.fetch_add(1, Ordering::Relaxed);
            if n % LOG_EVERY == 0 {
                warn!(count = n + 1, "audio consumer too slow, dropping frames");
            }
        }
    };

    while !stop.load(Ordering::SeqCst) {
        if device_error.swap(false, Ordering::SeqCst) {
            warn!("capture device error, re-selecting device");
            match open_stream(&host, &cfg, &raw_tx, &device_error).and_then(|o| {
                o.stream.play().map_err(|e| AudioError::Backend(e.to_string()))?;
                Ok(o)
            }) {
                Ok(o) => {
                    open = o;
                    rebuild_attempts = 0;
                    info!(channels = open.channels, rate = open.rate_hz, "capture stream rebuilt");
                }
                Err(e) => {
                    rebuild_attempts += 1;
                    if rebuild_attempts >= REBUILD_MAX_ATTEMPTS {
                        error!(%e, "giving up after {REBUILD_MAX_ATTEMPTS} rebuild attempts");
                    } else {
                        warn!(%e, attempt = rebuild_attempts, "rebuild failed, retrying");
                        device_error.store(true, Ordering::SeqCst);
                        thread::sleep(REBUILD_RETRY);
                    }
                }
            }
        }
        match raw_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(buf) => consume(buf, &open),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    while let Ok(buf) = raw_rx.try_recv() {
        consume(buf, &open);
    }
    drop(open.stream);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_prefers_existing_selection() {
        let avail = vec!["Mic A".to_string(), "Mic B".to_string()];
        assert_eq!(
            pick_device(Some("Mic B"), &avail),
            DevicePick {
                id: Some("Mic B".into()),
                fell_back: false
            }
        );
    }

    #[test]
    fn pick_falls_back_when_missing() {
        let avail = vec!["Mic A".to_string()];
        assert_eq!(
            pick_device(Some("Ghost"), &avail),
            DevicePick {
                id: None,
                fell_back: true
            }
        );
        assert_eq!(
            pick_device(None, &avail),
            DevicePick {
                id: None,
                fell_back: false
            }
        );
    }
}
