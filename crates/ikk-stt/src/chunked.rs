//! Chunked batch session: works with any provider that has `transcribe`.
//!
//! Audio is cut into chunks every `chunk_interval` (a timer, whatever is
//! being said) and at pauses (`cut_segment`). The fixed interval is what
//! keeps the first text coming within a couple of seconds; the recogniser
//! copes with the cuts, as long experience with the same recipe shows. Chunks are numbered and sent the moment they exist: the number
//! in flight follows the backlog, limited only by what the provider
//! tolerates. That limit is learnt the way TCP does it: halved when the
//! provider says "too many" (429), raised by one after a round of clean
//! answers, never above [`MAX_IN_FLIGHT`]. Each chunk has its own retries.
//! Results are released strictly in chunk order, so a slow connection or a
//! slow provider no longer builds a backlog that only a pause in speech
//! could clear.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use ikk_audio::AudioFrame;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{AudioClip, ErrorClass, Provider, SessionConfig, StreamingSession, SttError, TranscriptEvent};

/// Most transcriptions running at the same time, whatever the backlog.
pub const MAX_IN_FLIGHT: usize = 8;
/// Where the learnt limit starts.
const INITIAL_IN_FLIGHT: usize = 4;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Pauses before the second and third attempt of a chunk.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];
/// Shorter chunks are clicks, not words.
const MIN_CHUNK: Duration = Duration::from_millis(250);
/// A chunk that never got louder than this holds no speech.
const SILENT_DBFS: f32 = -70.0;

/// What a chunk's transcription ended with.
#[derive(Debug)]
enum Outcome {
    Text(String),
    /// Retries exhausted; the chunk is lost, dictation goes on.
    Skipped(ErrorClass),
    /// Nothing further will work (credential, config).
    Fatal(ErrorClass),
}

pub struct ChunkedBatchSession {
    buffer: Vec<AudioFrame>,
    buffered: Duration,
    peak_dbfs: f32,
    interval: Duration,
    next_seq: u64,
    uploads: mpsc::Sender<(u64, AudioClip)>,
    events: mpsc::Receiver<TranscriptEvent>,
    worker: tokio::task::JoinHandle<()>,
}

impl ChunkedBatchSession {
    pub fn new(provider: Arc<dyn Provider>, cfg: SessionConfig) -> Self {
        let (upload_tx, upload_rx) = mpsc::channel::<(u64, AudioClip)>(64);
        let (event_tx, event_rx) = mpsc::channel(64);
        let interval = cfg.chunk_interval.max(MIN_CHUNK);
        let worker = tokio::spawn(run_ordered(provider, cfg, upload_rx, event_tx));
        Self {
            buffer: Vec::new(),
            buffered: Duration::ZERO,
            peak_dbfs: f32::NEG_INFINITY,
            interval,
            next_seq: 0,
            uploads: upload_tx,
            events: event_rx,
            worker,
        }
    }

    /// Close the current chunk and hand it to the transcribers.
    async fn rotate(&mut self) {
        let frames = std::mem::take(&mut self.buffer);
        let (buffered, peak) = (self.buffered, self.peak_dbfs);
        self.buffered = Duration::ZERO;
        self.peak_dbfs = f32::NEG_INFINITY;
        if frames.is_empty() || buffered < MIN_CHUNK || peak < SILENT_DBFS {
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.uploads.send((seq, AudioClip { frames })).await.is_err() {
            warn!(seq, "transcriber gone; dropping chunk");
        }
    }
}

/// Takes chunks as they come, runs up to [`MAX_IN_FLIGHT`] transcriptions
/// at once and releases the results in chunk order.
async fn run_ordered(
    provider: Arc<dyn Provider>,
    cfg: SessionConfig,
    mut uploads: mpsc::Receiver<(u64, AudioClip)>,
    events: mpsc::Sender<TranscriptEvent>,
) {
    let (done_tx, mut done_rx) = mpsc::channel::<(u64, Outcome, bool)>(MAX_IN_FLIGHT * 4);
    // Chunks that may not go out yet (the limit is reached).
    let mut queued: VecDeque<(u64, AudioClip)> = VecDeque::new();
    let mut waiting: BTreeMap<u64, Outcome> = BTreeMap::new();
    let mut next_emit = 0u64;
    let mut in_flight = 0usize;
    let mut limit = INITIAL_IN_FLIGHT;
    // Clean answers since the limit last changed.
    let mut clean = 0usize;
    let mut open = true;
    let mut fatal = false;
    loop {
        // Send whatever the limit allows.
        while in_flight < limit && !fatal {
            let Some((seq, clip)) = queued.pop_front() else { break };
            in_flight += 1;
            tokio::spawn(transcribe_chunk(
                provider.clone(),
                cfg.clone(),
                seq,
                clip,
                done_tx.clone(),
            ));
        }
        tokio::select! {
            up = uploads.recv(), if open && !fatal => match up {
                Some(item) => queued.push_back(item),
                None => open = false,
            },
            Some((seq, outcome, throttled)) = done_rx.recv(), if in_flight > 0 => {
                in_flight -= 1;
                if throttled {
                    // The provider pushed back: halve the parallelism.
                    limit = (limit / 2).max(1);
                    clean = 0;
                    info!(limit, "provider throttled; fewer chunks in flight");
                } else {
                    clean += 1;
                    if clean >= limit && limit < MAX_IN_FLIGHT {
                        limit += 1;
                        clean = 0;
                    }
                }
                waiting.insert(seq, outcome);
                // Release everything that is next in line.
                while let Some(outcome) = waiting.remove(&next_emit) {
                    next_emit += 1;
                    let event = match outcome {
                        Outcome::Text(text) if text.is_empty() => continue,
                        Outcome::Text(text) => TranscriptEvent::Final(text),
                        Outcome::Skipped(class) => {
                            warn!(seq = next_emit - 1, ?class, "chunk lost after retries");
                            TranscriptEvent::Error(class)
                        }
                        Outcome::Fatal(class) => {
                            fatal = true;
                            TranscriptEvent::Error(class)
                        }
                    };
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
            },
            else => break,
        }
        if (!open || fatal) && in_flight == 0 && (queued.is_empty() || fatal) {
            break;
        }
    }
    let _ = events.send(TranscriptEvent::Closed).await;
}

/// One chunk: transcribes with retries, reports back (and whether the
/// provider throttled it on the way).
async fn transcribe_chunk(
    provider: Arc<dyn Provider>,
    cfg: SessionConfig,
    seq: u64,
    clip: AudioClip,
    done: mpsc::Sender<(u64, Outcome, bool)>,
) {
    let mut attempt = 0usize;
    let mut throttled = false;
    let outcome = loop {
        let result = tokio::time::timeout(UPLOAD_TIMEOUT, provider.transcribe(clip.clone(), &cfg)).await;
        // A timeout is a transport problem.
        let class = match result {
            Ok(Ok(t)) => break Outcome::Text(t.text.trim().to_string()),
            Ok(Err(SttError::Provider(class))) if class.is_retryable() => class,
            Ok(Err(SttError::Provider(class))) => break Outcome::Fatal(class),
            Ok(Err(e)) => break Outcome::Fatal(ErrorClass::Other(e.to_string())),
            Err(_) => ErrorClass::Transport,
        };
        throttled |= class == ErrorClass::Throttled;
        if attempt >= RETRY_BACKOFF.len() {
            break Outcome::Skipped(class);
        }
        info!(
            seq,
            attempt = attempt + 1,
            ?class,
            "chunk transcription failed; retrying"
        );
        tokio::time::sleep(RETRY_BACKOFF[attempt]).await;
        attempt += 1;
    };
    let _ = done.send((seq, outcome, throttled)).await;
}

#[async_trait::async_trait]
impl StreamingSession for ChunkedBatchSession {
    async fn push(&mut self, frame: AudioFrame) -> Result<(), SttError> {
        self.buffered += frame.duration();
        self.peak_dbfs = self.peak_dbfs.max(frame.level_dbfs());
        self.buffer.push(frame);
        if self.buffered >= self.interval {
            self.rotate().await;
        }
        Ok(())
    }

    async fn cut_segment(&mut self) -> Result<(), SttError> {
        self.rotate().await;
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        self.rotate().await;
        // Close the upload channel: the worker finishes what is in flight,
        // releases it in order and emits `Closed`.
        let (closed_tx, _closed_rx) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.uploads, closed_tx));
        let _ = (&mut self.worker).await;
        Ok(())
    }

    fn events(&mut self) -> &mut mpsc::Receiver<TranscriptEvent> {
        &mut self.events
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::*;

    /// Transcribes each clip as its duration; sleeps `delay(duration)` first,
    /// fails the first `fail_first` calls with `fail_with`.
    struct Fake {
        delay: fn(u64) -> u64,
        fail_first: usize,
        fail_with: Option<ErrorClass>,
        /// Answer 429 while more than this many calls run at once (0 = never).
        throttle_above: usize,
        throttled: AtomicUsize,
        calls: AtomicUsize,
        running: AtomicUsize,
        max_running: AtomicUsize,
        order: Mutex<Vec<u64>>,
    }

    impl Fake {
        fn new(delay: fn(u64) -> u64) -> Self {
            Self {
                delay,
                fail_first: 0,
                fail_with: None,
                throttle_above: 0,
                throttled: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                running: AtomicUsize::new(0),
                max_running: AtomicUsize::new(0),
                order: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for Fake {
        fn id(&self) -> ProviderId {
            ProviderId("fake".into())
        }
        fn display_name(&self) -> &'static str {
            "Fake"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                batch: true,
                ..Default::default()
            }
        }
        async fn transcribe(&self, audio: AudioClip, _: &SessionConfig) -> Result<Transcript, SttError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.fail_first {
                return Err(SttError::Provider(self.fail_with.clone().unwrap()));
            }
            let n = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_running.fetch_max(n, Ordering::SeqCst);
            if self.throttle_above > 0 && n > self.throttle_above {
                self.running.fetch_sub(1, Ordering::SeqCst);
                self.throttled.fetch_add(1, Ordering::SeqCst);
                return Err(SttError::Provider(ErrorClass::Throttled));
            }
            let ms = audio.duration().as_millis() as u64;
            tokio::time::sleep(Duration::from_millis((self.delay)(ms))).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            self.order.lock().unwrap().push(ms);
            Ok(Transcript {
                text: format!("{ms}ms"),
                language: None,
                confidence: None,
            })
        }
        async fn stream(&self, _: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
            Err(SttError::Unsupported)
        }
    }

    fn frame(ms: u64, amplitude: f32) -> AudioFrame {
        AudioFrame {
            samples: vec![amplitude; (16 * ms) as usize],
            sample_rate_hz: 16_000,
            offset: Duration::ZERO,
        }
    }

    fn config() -> SessionConfig {
        let mut cfg = SessionConfig::new("fake");
        cfg.chunk_interval = Duration::from_millis(1000);
        cfg.transfer = TransferMode::Batch;
        cfg
    }

    async fn drain(s: &mut Box<dyn StreamingSession>) -> Vec<TranscriptEvent> {
        let mut events = Vec::new();
        while let Some(ev) = s.events().recv().await {
            if ev == TranscriptEvent::Closed {
                break;
            }
            events.push(ev);
        }
        events
    }

    #[tokio::test]
    async fn chunks_come_back_in_order_even_when_later_ones_finish_first() {
        // The longest clip is the slowest: 300 ms waits 300 ms, 260 ms only 260.
        let fake = Arc::new(Fake::new(|ms| ms));
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        for ms in [300u64, 400, 260] {
            s.push(frame(ms, 0.1)).await.unwrap();
            s.cut_segment().await.unwrap();
        }
        s.finish().await.unwrap();
        let events = drain(&mut s).await;
        assert_eq!(
            events,
            vec![
                TranscriptEvent::Final("300ms".into()),
                TranscriptEvent::Final("400ms".into()),
                TranscriptEvent::Final("260ms".into()),
            ]
        );
        // They were transcribed at the same time, and the short one finished first.
        assert!(
            fake.max_running.load(Ordering::SeqCst) >= 2,
            "no parallel transcription"
        );
        assert_eq!(fake.order.lock().unwrap()[0], 260);
    }

    #[tokio::test]
    async fn parallelism_follows_the_backlog_up_to_the_ceiling() {
        // A slow provider and 12 chunks at once: the first 4 go out, the
        // limit grows by one per clean round, and never above the ceiling.
        let fake = Arc::new(Fake::new(|_| 120));
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        for _ in 0..12 {
            s.push(frame(300, 0.1)).await.unwrap();
            s.cut_segment().await.unwrap();
        }
        s.finish().await.unwrap();
        assert_eq!(drain(&mut s).await.len(), 12);
        let peak = fake.max_running.load(Ordering::SeqCst);
        assert!(peak > INITIAL_IN_FLIGHT && peak <= MAX_IN_FLIGHT, "peak {peak}");
    }

    #[tokio::test]
    async fn throttling_halves_the_parallelism_and_nothing_is_lost() {
        // The provider rejects every call while more than 2 run at once.
        let mut fake = Fake::new(|_| 60);
        fake.throttle_above = 2;
        let fake = Arc::new(fake);
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        for _ in 0..10 {
            s.push(frame(300, 0.1)).await.unwrap();
            s.cut_segment().await.unwrap();
        }
        s.finish().await.unwrap();
        let events = drain(&mut s).await;
        assert_eq!(events.len(), 10, "{events:?}");
        assert!(
            events.iter().all(|e| matches!(e, TranscriptEvent::Final(_))),
            "{events:?}"
        );
        assert!(
            fake.throttled.load(Ordering::SeqCst) > 0,
            "the provider never pushed back"
        );
    }

    #[tokio::test]
    async fn a_failing_chunk_is_retried_then_skipped_with_a_notice() {
        let mut fake = Fake::new(|_| 1);
        fake.fail_first = 1;
        fake.fail_with = Some(ErrorClass::Transport);
        let fake = Arc::new(fake);
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        s.push(frame(300, 0.1)).await.unwrap();
        s.finish().await.unwrap();
        assert_eq!(drain(&mut s).await, vec![TranscriptEvent::Final("300ms".into())]);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2, "one retry");

        let mut fake = Fake::new(|_| 1);
        fake.fail_first = 10;
        fake.fail_with = Some(ErrorClass::Throttled);
        let fake = Arc::new(fake);
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        s.push(frame(300, 0.1)).await.unwrap();
        s.cut_segment().await.unwrap();
        s.push(frame(400, 0.1)).await.unwrap();
        s.finish().await.unwrap();
        let events = drain(&mut s).await;
        // Both chunks gave up (10 failures cover 3 attempts each) and said so.
        assert_eq!(
            events,
            vec![
                TranscriptEvent::Error(ErrorClass::Throttled),
                TranscriptEvent::Error(ErrorClass::Throttled)
            ]
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn a_fatal_error_ends_the_session() {
        let mut fake = Fake::new(|_| 1);
        fake.fail_first = 1;
        fake.fail_with = Some(ErrorClass::Unauthorized);
        let fake = Arc::new(fake);
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        s.push(frame(300, 0.1)).await.unwrap();
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        assert_eq!(
            drain(&mut s).await,
            vec![TranscriptEvent::Error(ErrorClass::Unauthorized)]
        );
    }

    #[tokio::test]
    async fn cuts_every_interval_and_at_pauses() {
        let fake = Arc::new(Fake::new(|_| 1));
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        // 2.5 s of speech: one chunk at the 1 s interval, another at 2 s,
        // the rest at the pause.
        for _ in 0..25 {
            s.push(frame(100, 0.3)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        assert_eq!(
            drain(&mut s).await,
            vec![
                TranscriptEvent::Final("1000ms".into()),
                TranscriptEvent::Final("1000ms".into()),
                TranscriptEvent::Final("500ms".into()),
            ]
        );
    }

    #[tokio::test]
    async fn clicks_and_silence_are_not_uploaded() {
        let fake = Arc::new(Fake::new(|_| 1));
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        s.push(frame(100, 0.3)).await.unwrap(); // too short
        s.cut_segment().await.unwrap();
        s.push(frame(600, 0.0)).await.unwrap(); // silence
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        assert!(drain(&mut s).await.is_empty());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
}
