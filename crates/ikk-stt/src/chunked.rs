//! Chunked batch session: works with any provider that has `transcribe`.
//! Buffers frames, uploads every `chunk_interval` or on `cut_segment`,
//! emits each result as `Final`. Uploads run sequentially in a worker so
//! order is preserved.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ikk_audio::AudioFrame;
use tokio::sync::mpsc;
use tracing::warn;

use crate::{AudioClip, Provider, SessionConfig, StreamingSession, SttError, TranscriptEvent};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ChunkedBatchSession {
    buffer: Vec<AudioFrame>,
    last_rotate: Instant,
    chunk_interval: Duration,
    uploads: mpsc::Sender<AudioClip>,
    events: mpsc::Receiver<TranscriptEvent>,
    worker: tokio::task::JoinHandle<()>,
}

impl ChunkedBatchSession {
    pub fn new(provider: Arc<dyn Provider>, cfg: SessionConfig) -> Self {
        let (upload_tx, mut upload_rx) = mpsc::channel::<AudioClip>(16);
        let (event_tx, event_rx) = mpsc::channel(64);
        let chunk_interval = cfg.chunk_interval.max(Duration::from_millis(250));
        let worker = tokio::spawn(async move {
            while let Some(clip) = upload_rx.recv().await {
                match tokio::time::timeout(UPLOAD_TIMEOUT, provider.transcribe(clip, &cfg)).await {
                    Ok(Ok(t)) => {
                        let text = t.text.trim().to_string();
                        if !text.is_empty() && event_tx.send(TranscriptEvent::Final(text)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Err(SttError::Provider(class))) => {
                        let fatal = !class.is_retryable();
                        let _ = event_tx.send(TranscriptEvent::Error(class)).await;
                        if fatal {
                            break;
                        }
                    }
                    Ok(Err(e)) => warn!(%e, "chunk transcription failed"),
                    Err(_) => warn!("chunk transcription timed out"),
                }
            }
            let _ = event_tx.send(TranscriptEvent::Closed).await;
        });
        Self {
            buffer: Vec::new(),
            last_rotate: Instant::now(),
            chunk_interval,
            uploads: upload_tx,
            events: event_rx,
            worker,
        }
    }

    async fn rotate(&mut self) {
        self.last_rotate = Instant::now();
        if self.buffer.is_empty() {
            return;
        }
        let clip = AudioClip {
            frames: std::mem::take(&mut self.buffer),
        };
        if self.uploads.send(clip).await.is_err() {
            warn!("upload worker gone; dropping chunk");
        }
    }
}

#[async_trait::async_trait]
impl StreamingSession for ChunkedBatchSession {
    async fn push(&mut self, frame: AudioFrame) -> Result<(), SttError> {
        self.buffer.push(frame);
        if self.last_rotate.elapsed() >= self.chunk_interval {
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
        // Close the upload channel so the worker drains and emits `Closed`.
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
    use super::*;
    use crate::*;

    struct Echo;
    #[async_trait::async_trait]
    impl Provider for Echo {
        fn id(&self) -> ProviderId {
            ProviderId("echo".into())
        }
        fn display_name(&self) -> &'static str {
            "Echo"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                batch: true,
                ..Default::default()
            }
        }
        async fn transcribe(&self, audio: AudioClip, _: &SessionConfig) -> Result<Transcript, SttError> {
            Ok(Transcript {
                text: format!("{}ms", audio.duration().as_millis()),
                language: None,
                confidence: None,
            })
        }
        async fn stream(&self, _: &SessionConfig) -> Result<Box<dyn StreamingSession>, SttError> {
            Err(SttError::Unsupported)
        }
    }

    fn frame(ms: u64) -> AudioFrame {
        AudioFrame {
            samples: vec![0.1; (16 * ms) as usize],
            sample_rate_hz: 16_000,
            offset: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn chunks_are_transcribed_in_order_and_flushed_on_finish() {
        let mut cfg = SessionConfig::new("echo");
        cfg.chunk_interval = Duration::from_millis(250);
        cfg.transfer = TransferMode::Batch;
        let mut s = open_session(Arc::new(Echo), &cfg).await.unwrap();
        s.push(frame(100)).await.unwrap();
        s.cut_segment().await.unwrap();
        s.push(frame(50)).await.unwrap();
        let mut events = Vec::new();
        // Drain what the first cut produced.
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), s.events().recv()).await {
            events.push(ev);
            if events.len() == 1 {
                break;
            }
        }
        s.finish().await.unwrap();
        while let Some(ev) = s.events().recv().await {
            if ev == TranscriptEvent::Closed {
                break;
            }
            events.push(ev);
        }
        assert_eq!(
            events,
            vec![
                TranscriptEvent::Final("100ms".into()),
                TranscriptEvent::Final("50ms".into())
            ]
        );
    }
}
