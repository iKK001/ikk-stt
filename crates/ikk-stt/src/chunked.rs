//! Chunked batch session: works with any provider that has `transcribe`.
//!
//! Audio is cut into chunks only between words: once a chunk is longer
//! than `chunk_interval`, at the next gap, and at every pause
//! (`cut_segment`). A gap is [`GAP_PAUSE`] without speech as the noise
//! model in the capture chain hears it ([`AudioFrame::speech`]), which
//! tells a voice from a television, a restaurant or a fan by its sound,
//! not by its loudness; a long soft consonant inside a word never counts
//! as one. Frames without that analysis fall back to the energy detector
//! that also finds pauses. A timer is only the safety net for speech
//! without any gap: after [`HARD_MAX_FACTOR`] intervals the chunk is cut
//! regardless.
//!
//! With `agreement` on, nothing is cut at gaps: every `step` the thought
//! so far goes out as one clip, and of each result only the words that
//! the last `windows` results agree on are released — typed once, never
//! changed; a pause releases the rest of the last result. Measured on
//! impaired speech with whisper-1 and the user's phrases as vocabulary:
//! the typed text equals the whole-recording text, about four seconds
//! behind the voice.
//!
//! With `refine` on, every frame is also kept until a pause ends the
//! thought; if that thought took several chunks it is transcribed once
//! more in one piece and the result replaces the chunk texts
//! ([`TranscriptEvent::Refined`]). Unclear speech is decoded from
//! context the short chunks never had, yet the first text still appears
//! after the first chunk. Chunks are numbered and sent the moment they exist: the number
//! in flight follows the backlog, limited only by what the provider
//! tolerates. That limit is learnt the way TCP does it: halved when the
//! provider says "too many" (429), raised by one after a round of clean
//! answers, never above [`MAX_IN_FLIGHT`]. Each chunk has its own retries.
//! Results are released strictly in chunk order, so a slow connection or a
//! slow provider no longer builds a backlog that only a pause in speech
//! could clear.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ikk_audio::vad::{VadEvent, VadMode, VoiceActivityDetector};
use ikk_audio::AudioFrame;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{
    Agreement, AudioClip, ErrorClass, Provider, SessionConfig, StreamingSession, SttError, Transcript, TranscriptEvent,
};

/// Most transcriptions running at the same time, whatever the backlog.
pub const MAX_IN_FLIGHT: usize = 8;
/// Where the learnt limit starts.
const INITIAL_IN_FLIGHT: usize = 4;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Pauses before the second and third attempt of a chunk.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];
/// Shorter chunks are clicks, not words.
const MIN_CHUNK: Duration = Duration::from_millis(250);
/// This long without speech is a gap worth cutting at; the closure
/// before a p, t or k lasts 50–100 ms and must not split the word.
const GAP_PAUSE: Duration = Duration::from_millis(250);
/// Below this speech probability a frame counts as background.
const GAP_SPEECH_PROB: f32 = 0.3;
/// Without any gap, a chunk is cut after this many `chunk_interval`s.
const HARD_MAX_FACTOR: u32 = 3;
/// A chunk that never got louder than this holds no speech.
const SILENT_DBFS: f32 = -70.0;
/// A chunk in which no frame reached this speech probability holds no
/// speech either: breathing, a ventilator, a room. Recognisers answer
/// such audio with invented text (Whisper: subtitle credits).
const CHUNK_SPEECH_PROB: f32 = 0.5;
/// Longest thought transcribed again in one piece.
const REFINE_MAX: Duration = Duration::from_secs(90);
/// Agreement windows reach back at most this far: the provider's response
/// time grows with the clip (whisper-1: 5 s clip 1.3 s, 28 s clip 5.3 s),
/// and the words still open only need the last several seconds of context;
/// new words are aligned to the typed ones by their last words.
const WINDOW_SPAN: Duration = Duration::from_secs(12);
/// Agreement windows end at most this long after the last speech: a clip
/// that trails off into seconds of ventilator makes the recogniser repeat
/// an earlier sentence at the end.
const WINDOW_TAIL: Duration = Duration::from_millis(400);
const REFINE_TIMEOUT: Duration = Duration::from_secs(120);
/// An agreement window's answer is useless once newer readings are in:
/// a slow provider's late answers are dropped rather than waited for (the
/// results are released in order, so one hanging request would hold
/// everything behind it).
const WINDOW_TIMEOUT: Duration = Duration::from_secs(6);
/// The final window of a thought may take longer: nothing follows it.
const LAST_WINDOW_TIMEOUT: Duration = Duration::from_secs(20);

/// What goes to the transcribers, in sequence order.
enum Upload {
    Chunk(u64, AudioClip),
    /// A thought ended without a second pass (one chunk): whatever quick
    /// text is held back for preview goes out as final at this point.
    Flush(u64),
    /// The whole thought; replaces the finals of the chunks from
    /// `first_seq` on.
    Refine {
        seq: u64,
        first_seq: u64,
        clip: AudioClip,
    },
    /// Agreement mode: the thought so far; `last` when a pause ended it,
    /// `from_start` when the clip begins where the thought began,
    /// `clip_start` its position in the thought.
    Window {
        seq: u64,
        clip: AudioClip,
        last: bool,
        from_start: bool,
        clip_start: Duration,
    },
}

/// What a chunk's transcription ended with.
#[derive(Debug)]
enum Outcome {
    Text(String),
    /// Retries exhausted; the chunk is lost, dictation goes on.
    Skipped(ErrorClass),
    /// Nothing further will work (credential, config).
    Fatal(ErrorClass),
    /// Marker, see [`Upload::Flush`].
    Flush,
    /// A window's answer with its word timestamps.
    Timed(Transcript),
}

pub struct ChunkedBatchSession {
    buffer: Vec<AudioFrame>,
    buffered: Duration,
    peak_dbfs: f32,
    /// Highest speech probability in the chunk (frames that carry one).
    peak_speech: Option<f32>,
    /// How long the chain has heard no speech.
    quiet_for: Duration,
    /// Agreement mode: the pause (speech probability below the gap level
    /// this long) that ends a thought, independent of the caller's cut.
    pause: Duration,
    /// Agreement mode: quiet since the last speech frame of the thought.
    thought_quiet: Duration,
    /// Agreement mode: set by the transcriber when the last readings of
    /// the thought came back identical — nothing new is being said.
    stale: Arc<AtomicBool>,
    /// Gap finder for frames without a speech probability (built for the
    /// first such frame's rate).
    gap_vad: Option<VoiceActivityDetector>,
    /// Earliest cut at a gap.
    interval: Duration,
    /// Cut regardless.
    hard_max: Duration,
    next_seq: u64,
    /// Frames of the current thought, for the second pass (`refine`).
    thought: Vec<AudioFrame>,
    thought_len: Duration,
    /// Sequence number of the thought's first chunk, and how many it had.
    thought_first_seq: u64,
    thought_chunks: u64,
    refine: bool,
    preview: bool,
    agreement: Option<Agreement>,
    /// Agreement mode: thought length at which the next window goes out.
    window_due: Duration,
    /// Highest speech probability heard in the thought.
    thought_speech: Option<f32>,
    uploads: mpsc::Sender<Upload>,
    events: mpsc::Receiver<TranscriptEvent>,
    worker: tokio::task::JoinHandle<()>,
}

impl ChunkedBatchSession {
    pub fn new(provider: Arc<dyn Provider>, cfg: SessionConfig) -> Self {
        let (upload_tx, upload_rx) = mpsc::channel::<Upload>(64);
        let (event_tx, event_rx) = mpsc::channel(64);
        let interval = cfg.chunk_interval.max(MIN_CHUNK);
        let refine = cfg.refine && cfg.agreement.is_none();
        let preview = refine && cfg.refine_preview;
        let agreement = cfg.agreement;
        let pause = cfg.endpointing.max(Duration::from_millis(1500));
        let stale = Arc::new(AtomicBool::new(false));
        let worker = tokio::spawn(run_ordered(provider, cfg, upload_rx, event_tx, stale.clone()));
        Self {
            buffer: Vec::new(),
            buffered: Duration::ZERO,
            peak_dbfs: f32::NEG_INFINITY,
            peak_speech: None,
            quiet_for: Duration::ZERO,
            pause,
            thought_quiet: Duration::ZERO,
            stale,
            gap_vad: None,
            interval,
            hard_max: interval * HARD_MAX_FACTOR,
            next_seq: 0,
            thought: Vec::new(),
            thought_len: Duration::ZERO,
            thought_first_seq: 0,
            thought_chunks: 0,
            refine,
            preview,
            agreement,
            window_due: agreement.map_or(Duration::ZERO, |a| a.step),
            thought_speech: None,
            uploads: upload_tx,
            events: event_rx,
            worker,
        }
    }

    /// Close the current chunk and hand it to the transcribers. `why` names
    /// the cut point for the log: "gap", "timer" or "pause".
    async fn rotate(&mut self, why: &'static str) {
        let frames = std::mem::take(&mut self.buffer);
        let (buffered, peak, speech) = (self.buffered, self.peak_dbfs, self.peak_speech.take());
        self.buffered = Duration::ZERO;
        self.peak_dbfs = f32::NEG_INFINITY;
        self.quiet_for = Duration::ZERO;
        if frames.is_empty() || buffered < MIN_CHUNK || peak < SILENT_DBFS {
            return;
        }
        if speech.is_some_and(|p| p < CHUNK_SPEECH_PROB) {
            tracing::debug!(
                ms = buffered.as_millis() as u64,
                why,
                "no speech in chunk; not uploaded"
            );
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.thought_chunks += 1;
        info!(seq, ms = buffered.as_millis() as u64, why, "chunk cut");
        if self
            .uploads
            .send(Upload::Chunk(seq, AudioClip { frames }))
            .await
            .is_err()
        {
            warn!(seq, "transcriber gone; dropping chunk");
        }
    }

    /// Agreement mode: a frame arrived.
    async fn push_window_frame(&mut self, frame: AudioFrame, a: Agreement) {
        if let Some(p) = frame.speech {
            self.thought_speech = Some(self.thought_speech.map_or(p, |q| q.max(p)));
            if p < GAP_SPEECH_PROB {
                self.thought_quiet += frame.duration();
            } else {
                self.thought_quiet = Duration::ZERO;
            }
        }
        self.thought_len += frame.duration();
        self.thought.push(frame);
        if self.thought_len >= REFINE_MAX {
            // Too long for one clip: close the thought here.
            self.send_window(true).await;
            return;
        }
        if self.stale.swap(false, Ordering::SeqCst) && self.thought_len > Duration::ZERO {
            // Several readings in a row heard nothing new: the speaker has
            // stopped, whatever the level and the noise model say.
            info!("readings unchanged; the thought ends");
            self.send_window(true).await;
            return;
        }
        if self.thought_quiet >= self.pause {
            // The speaker stopped (by the noise model, not the level: a
            // ventilator keeps the level up for ever): the thought ends
            // here, or the windows would slide on over noise and the
            // recogniser would invent sentences in it.
            if self.thought_speech.is_some_and(|p| p >= CHUNK_SPEECH_PROB) {
                self.send_window(true).await;
            } else {
                // Noise only: drop it.
                self.thought.clear();
                self.thought_len = Duration::ZERO;
                self.window_due = a.step;
                self.thought_speech = None;
            }
            self.thought_quiet = Duration::ZERO;
            return;
        }
        if self.thought_len >= self.window_due {
            self.window_due += a.step;
            self.send_window(false).await;
        }
    }

    /// Agreement mode: the thought so far goes out; `last` also resets.
    async fn send_window(&mut self, last: bool) {
        let a = self.agreement.expect("agreement mode");
        let has_speech = self.thought_speech.is_none_or(|p| p >= CHUNK_SPEECH_PROB);
        let speech_sure = self.thought_speech.is_none_or(|p| p >= SURE_SPEECH_PROB);
        let worth = self.thought_len >= MIN_CHUNK && has_speech;
        let mut frames = if last {
            self.thought_len = Duration::ZERO;
            self.window_due = a.step;
            self.thought_speech = None;
            self.thought_quiet = Duration::ZERO;
            std::mem::take(&mut self.thought)
        } else {
            self.thought.clone()
        };
        if !worth {
            return;
        }
        trim_tail(&mut frames, WINDOW_TAIL);
        // Only the last WINDOW_SPAN of the thought.
        let mut span = Duration::ZERO;
        let keep_from = frames
            .iter()
            .rposition(|f| {
                span += f.duration();
                span >= WINDOW_SPAN
            })
            .unwrap_or(0);
        let from_start = keep_from == 0;
        let clip_start: Duration = frames[..keep_from].iter().map(|f| f.duration()).sum();
        if keep_from > 0 {
            frames.drain(..keep_from);
        }
        let clip_len: Duration = frames.iter().map(|f| f.duration()).sum();
        if !last && clip_len < MIN_RELEASE_WINDOW {
            return;
        }
        if last && clip_len < MIN_RELEASE_WINDOW && !speech_sure {
            // A short thought that never got a window before the pause:
            // only with clearly heard speech, or it is breath and the
            // recogniser invents subtitle credits for it.
            return;
        }
        if last && clip_len >= MIN_RELEASE_WINDOW + ENDING_OFFSET {
            // A second opinion on the ending: the same clip from a little
            // later, read before the final one. The final reading's tail is
            // then covered by a reading that heard it in another context,
            // and goes out only when both agree — never as heard by one.
            let mut skipped = Duration::ZERO;
            let cut = frames
                .iter()
                .position(|f| {
                    skipped += f.duration();
                    skipped >= ENDING_OFFSET
                })
                .map_or(0, |i| i + 1);
            let second: Vec<AudioFrame> = frames[cut..].to_vec();
            let seq = self.next_seq;
            self.next_seq += 1;
            info!(seq, ms = (clip_len - skipped).as_millis() as u64, "ending check");
            let up = Upload::Window {
                seq,
                clip: AudioClip { frames: second },
                last: false,
                from_start: false,
                clip_start: clip_start + skipped,
            };
            if self.uploads.send(up).await.is_err() {
                warn!(seq, "transcriber gone; dropping window");
            }
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        info!(seq, ms = clip_len.as_millis() as u64, last, "window");
        let up = Upload::Window {
            seq,
            clip: AudioClip { frames },
            last,
            from_start,
            clip_start,
        };
        if self.uploads.send(up).await.is_err() {
            warn!(seq, "transcriber gone; dropping window");
        }
    }

    fn refine_overflow(&self) {
        warn!(
            seconds = REFINE_MAX.as_secs(),
            "thought longer than the second pass allows; its chunks stand"
        );
    }

    /// A pause ended the thought: transcribe it again in one piece if it
    /// took several chunks (one chunk already was the whole thought).
    async fn end_thought(&mut self) {
        let frames = std::mem::take(&mut self.thought);
        let (len, chunks, first_seq) = (self.thought_len, self.thought_chunks, self.thought_first_seq);
        self.thought_len = Duration::ZERO;
        self.thought_chunks = 0;
        self.thought_first_seq = self.next_seq;
        if !self.refine || chunks < 2 || frames.is_empty() {
            if self.preview && chunks > 0 {
                let seq = self.next_seq;
                self.next_seq += 1;
                if self.uploads.send(Upload::Flush(seq)).await.is_err() {
                    warn!(seq, "transcriber gone");
                }
            }
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        // The thought goes as spoken, pauses included: shortening them
        // made gpt-4o-transcribe paraphrase and stop early more often.
        info!(
            seq,
            ms = len.as_millis() as u64,
            chunks,
            first_seq,
            "transcribing the whole thought again"
        );
        let up = Upload::Refine {
            seq,
            first_seq,
            clip: AudioClip { frames },
        };
        if self.uploads.send(up).await.is_err() {
            warn!(seq, "transcriber gone; dropping the second pass");
        }
    }
}

/// Drop trailing frames without speech beyond `keep` (frames without a
/// probability count as speech).
fn trim_tail(frames: &mut Vec<AudioFrame>, keep: Duration) {
    let mut quiet = Duration::ZERO;
    let mut cut = frames.len();
    for (i, f) in frames.iter().enumerate().rev() {
        match f.speech {
            Some(p) if p < GAP_SPEECH_PROB => {
                quiet += f.duration();
                if quiet > keep {
                    cut = i;
                }
            }
            _ => break,
        }
    }
    // `cut` is the first frame beyond `keep` of trailing quiet, counted
    // from the end: keep everything before the quiet started plus `keep`.
    if cut < frames.len() {
        let mut kept = Duration::ZERO;
        let start = frames
            .iter()
            .rposition(|f| !matches!(f.speech, Some(p) if p < GAP_SPEECH_PROB))
            .map_or(0, |i| i + 1);
        let mut end = start;
        while end < frames.len() && kept < keep {
            kept += frames[end].duration();
            end += 1;
        }
        frames.truncate(end);
    }
}

/// Agreement bookkeeping for one thought: the last results as words, and
/// how many words are out already.
struct Agreed {
    windows: usize,
    /// Recent results, each cut to the words after the typed ones.
    history: Vec<Vec<String>>,
    /// Normalised words typed so far in this thought.
    committed: Vec<String>,
    /// Results in a row that could not be placed.
    lost: usize,
}

/// After this many unplaceable results the thought moved on without us:
/// the next result counts as all new words.
const LOST_LIMIT: usize = 3;

/// How many typed words a new result must repeat to be aligned.
const ALIGN_WORDS: usize = 3;
/// Windows shorter than this release nothing: a two-second clip of
/// unclear speech is read confidently wrong, and three of them agree.
const MIN_RELEASE_WINDOW: Duration = Duration::from_millis(3500);
/// The ending check reads the final clip from this much later.
const ENDING_OFFSET: Duration = Duration::from_millis(600);
/// This many identical readings in a row (one per step) end the thought:
/// the speaker has stopped, whatever the level and the noise model say.
const STALE_READINGS: usize = 4;
/// A short thought that is all the pause left needs speech this sure.
const SURE_SPEECH_PROB: f32 = 0.8;

impl Agreed {
    fn new(windows: usize) -> Self {
        Self {
            windows: windows.max(1),
            history: Vec::new(),
            committed: Vec::new(),
            lost: 0,
        }
    }

    fn reset(&mut self) {
        self.history.clear();
        self.committed.clear();
        self.lost = 0;
    }

    /// Where the words not yet typed start in `words`: after the last
    /// typed words, wherever the window begins. When the result no longer
    /// repeats the last typed words (it changed its mind about them; they
    /// stay), an earlier run of typed words is found and the disputed ones
    /// are skipped over, so one wrong word never blocks the rest. `None`
    /// when nothing of the typed text is in the result.
    fn align(&self, words: &[String], from_start: bool) -> Option<usize> {
        let norm: Vec<String> = words.iter().map(|w| norm_word(w)).collect();
        if self.committed.is_empty() {
            // Nothing typed yet: the result starts the comparison. (Lining
            // slid windows up with each other by dropping what they slid
            // past was tried and lost more words than it saved.)
            let _ = from_start;
            return Some(0);
        }
        // As many of the last typed words as the window still holds (it
        // may start inside them); the last occurrence, since a phrase may
        // repeat earlier in the thought.
        let by_tail = (1..=ALIGN_WORDS.min(self.committed.len())).rev().find_map(|n| {
            let tail = &self.committed[self.committed.len() - n..];
            (n..=norm.len())
                .rev()
                .find(|&end| norm[end - n..end].iter().zip(tail).all(|(x, y)| alike(x, y)))
        });
        if by_tail.is_some() {
            return by_tail;
        }
        // Any run of two typed words, the latest one; skip what was typed after it.
        let c = &self.committed;
        for i in (0..c.len().saturating_sub(1)).rev() {
            let pair = &c[i..i + 2];
            if let Some(end) = (2..=norm.len())
                .rev()
                .find(|&end| norm[end - 2..end].iter().zip(pair).all(|(x, y)| alike(x, y)))
            {
                return Some((end + (c.len() - i - 2)).min(norm.len()));
            }
        }
        // A window from the thought's start: the typed words come first.
        if from_start {
            return Some(c.len().min(norm.len()));
        }
        // The window slid past the typed words: place it by the words
        // the previous result still had open (consecutive windows overlap
        // by all but one step, so they share text).
        self.align_to_open(&norm)
    }

    /// Place a result by the open words of a recent result (newest first).
    fn align_to_open(&self, norm: &[String]) -> Option<usize> {
        for open in self.history.iter().rev().filter(|h| !h.is_empty()) {
            for n in (1..=open.len().min(ALIGN_WORDS)).rev() {
                if n <= norm.len() {
                    if let Some(pos) =
                        (0..=norm.len() - n).find(|&p| norm[p..p + n].iter().zip(&open[..n]).all(|(x, y)| alike(x, y)))
                    {
                        return Some(pos);
                    }
                }
            }
        }
        None
    }

    /// A new result for the thought so far: the words to release now, with
    /// the newest result's spelling. `last` releases everything left.
    fn offer(&mut self, text: &str, last: bool, from_start: bool) -> Option<String> {
        let all: Vec<String> = text.split_whitespace().map(str::to_string).collect();

        let start = match self.align(&all, from_start) {
            Some(start) => {
                self.lost = 0;
                start
            }
            None if self.lost + 1 >= LOST_LIMIT => {
                // Lost for good: the thought moved on; everything in this
                // window is new. A repeated word is the price of going on.
                warn!(
                    typed = self.committed.len(),
                    "thought moved past the typed words; continuing fresh"
                );
                self.lost = 0;
                self.history.clear();
                0
            }
            None => {
                self.lost += 1;
                warn!(
                    typed = self.committed.len(),
                    lost = self.lost,
                    "result does not repeat the typed words; skipped"
                );
                if last {
                    self.history.clear();
                }
                return None;
            }
        };
        let words: Vec<String> = all[start..].to_vec();
        let same = |a: &str, b: &str| norm_word(a) == norm_word(b);
        self.history.push(words.clone());
        if self.history.len() > self.windows {
            self.history.remove(0);
        }
        let mut k = 0;
        // Votes: of the last `windows` results, how many agree with the
        // newest at a position. Up to three results all must agree; from
        // four on, a word is out when more than half agree, in any order —
        // a path that flips one word every other second then stalls for a
        // moment, not for ten. The pause (`last`) uses the same rule with
        // one vote less, but never fewer than two readings: two readings
        // agreeing on an ending is how a wrong tail got typed once, and an
        // ending that finds no majority stays untyped rather than wrong.
        let votes = if self.windows >= 4 {
            self.windows / 2 + 1
        } else {
            self.windows
        };
        let needed = if last { (votes - 1).max(2) } else { votes };
        if last || self.history.len() >= self.windows {
            while k < words.len()
                && self
                    .history
                    .iter()
                    .filter(|h| k < h.len() && same(&h[k], &words[k]))
                    .count()
                    >= needed
            {
                k += 1;
            }
        }
        if k == 0 {
            return None;
        }
        let out = words[..k].join(" ");
        self.committed.extend(words[..k].iter().map(|w| norm_word(w)));
        // The released words are behind us now: later results are compared
        // from the next open word on.
        for h in &mut self.history {
            let drop = k.min(h.len());
            h.drain(..drop);
        }
        Some(out)
    }
}

/// Agreement by time: every recognised word has a place on the thought's
/// timeline (its clip's start plus its own offset). A word is typed once
/// enough of the recent readings that cover that moment agree on it;
/// readings are compared per moment, not by searching for typed words, so
/// a reading that flips one word cannot lose the anchor and a disputed
/// moment is given up after a while (an omission, never a wrong word).
///
/// Trained words ([`ikk_text::learned`]): a heard form is noted on the
/// reading's words and becomes the trained word only when the readings of
/// that moment are not unanimous about it (an `always` entry: as the
/// reading arrives).
struct TimedAgreed {
    windows: usize,
    readings: Vec<TimedReading>,
    /// Thought time up to which words are out.
    done_until: Duration,
    /// The last words out, to catch them again when a reading moves them.
    last_out: VecDeque<String>,
    learned: Vec<ikk_text::LearnedWord>,
    /// Thought time (newest reading's end) since which the readings have
    /// been stable; a disputed word is given up only once stability has
    /// held as long as the word is old, so an unsettled onset does not
    /// lose its words while the readings are still finding themselves.
    stable_since: Duration,
}

/// A word like the last one typed that starts this close to the typed edge
/// is that word again (timestamps move a little between readings).
const SAME_WORD_SLACK: Duration = Duration::from_millis(1000);

struct TimedReading {
    start: Duration,
    end: Duration,
    words: Vec<TimedWord>,
}

/// A word of one reading at its absolute thought time.
#[derive(Debug, Clone)]
struct TimedWord {
    text: String,
    start: Duration,
    end: Duration,
    /// The trained word this is a single-word heard form of.
    learned: Option<String>,
}

/// Two readings mean the same word when their starts are this close.
const SAME_MOMENT: Duration = Duration::from_millis(400);
/// A moment nobody agrees on is skipped once the newest reading ends this
/// long after it.
const GIVE_UP_AFTER: Duration = Duration::from_secs(5);
/// A heard form counts as what was really said only when this many
/// readings of the moment agree on it.
const SURE_READINGS: usize = 3;
/// The stability check needs this many comparable words to judge.
const STABLE_MIN_WORDS: usize = 4;
/// A word ending this close to its reading's end may be cut mid-word
/// ("Scheiße" for "schreiben"): it neither votes nor goes out until a
/// later reading has it whole. Shorter than the quiet a window keeps after
/// the last speech (`WINDOW_TAIL`), so a word followed by quiet — the last
/// word of a thought, in every reading of the pause — does vote.
const CUT_EDGE: Duration = Duration::from_millis(300);

impl TimedAgreed {
    fn new(windows: usize, learned: Vec<ikk_text::LearnedWord>) -> Self {
        Self {
            windows: windows.max(1),
            readings: Vec::new(),
            done_until: Duration::ZERO,
            last_out: VecDeque::new(),
            learned,
            stable_since: Duration::ZERO,
        }
    }

    fn reset(&mut self) {
        self.readings.clear();
        self.done_until = Duration::ZERO;
        self.last_out.clear();
        self.stable_since = Duration::ZERO;
    }

    /// The reading's words with the certain swaps made and the uncertain
    /// ones noted.
    fn timed_words(&self, t: &Transcript, clip_start: Duration) -> Vec<TimedWord> {
        let raw: Vec<TimedWord> = t
            .words
            .iter()
            .map(|w| TimedWord {
                text: w.text.clone(),
                start: clip_start + w.start,
                end: clip_start + w.end,
                learned: None,
            })
            .collect();
        if self.learned.is_empty() {
            return raw;
        }
        let texts: Vec<String> = raw.iter().map(|w| w.text.clone()).collect();
        let swaps = ikk_text::find_swaps(&self.learned, &texts);
        if swaps.is_empty() {
            return raw;
        }
        let mut out = Vec::with_capacity(raw.len());
        let mut i = 0;
        for s in swaps {
            out.extend(raw[i..s.at].iter().cloned());
            let first = &raw[s.at];
            let last = &raw[s.at + s.len - 1];
            let tail: String = last
                .text
                .chars()
                .rev()
                .take_while(|c| !c.is_alphanumeric())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if s.certain {
                out.push(TimedWord {
                    text: format!("{}{tail}", s.word),
                    start: first.start,
                    end: last.end,
                    learned: None,
                });
            } else {
                // The form as heard, spanning its words, the trained word
                // noted: the vote decides (unanimous → as heard, else the word).
                let heard: Vec<&str> = raw[s.at..s.at + s.len].iter().map(|w| w.text.as_str()).collect();
                out.push(TimedWord {
                    text: heard.join(" "),
                    start: first.start,
                    end: last.end,
                    learned: Some(format!("{}{tail}", s.word)),
                });
            }
            i = s.at + s.len;
        }
        out.extend(raw[i..].iter().cloned());
        out
    }

    /// Real speech reads nearly the same from one reading to the next;
    /// invented speech over noise does not. Compares the newest reading
    /// with the one before it, leaving out the newest 1.5 s (the edge,
    /// where the last words are still cut or unsettled).
    fn stable(&self) -> bool {
        let n = self.readings.len();
        // The newest pair and the one before it: at the pause the final
        // reading repeats the ending check almost verbatim even over noise.
        (2..=3.min(n)).all(|k| Self::pair_stable(&self.readings[n - k], &self.readings[n - k + 1]))
    }

    fn pair_stable(prev: &TimedReading, newest: &TimedReading) -> bool {
        let edge = newest.end.saturating_sub(Duration::from_millis(1500));
        let compared: Vec<&TimedWord> = newest
            .words
            .iter()
            .filter(|w| w.start >= prev.start && w.start < edge)
            .collect();
        if compared.len() < STABLE_MIN_WORDS {
            return true;
        }
        let same = compared
            .iter()
            .filter(|w| {
                prev.words
                    .iter()
                    .any(|x| x.start.abs_diff(w.start) < SAME_MOMENT && alike(&x.text, &w.text))
            })
            .count();
        // Real speech reads ≈ 90 % the same, a noisy onset ≈ 50 %, invented
        // speech over noise ≈ 30 %: below 40 % it is noise.
        same * 5 >= compared.len() * 2
    }

    /// `clip_end`: where the clip's audio ends on the thought's timeline
    /// (a word ending just before it may be cut).
    fn offer(&mut self, t: &Transcript, clip_start: Duration, clip_end: Duration, last: bool) -> Option<String> {
        let words = self.timed_words(t, clip_start);
        // No clip end given (tests): the audio ends a little after the
        // last word, as a trimmed window does.
        let clip_end = if clip_end.is_zero() {
            words.last().map_or(clip_start, |w| w.end + Duration::from_millis(600))
        } else {
            clip_end
        };
        let end = words.last().map_or(clip_start, |w| w.end).max(clip_end);
        debug!(
            last,
            clip_start_ms = clip_start.as_millis() as u64,
            raw = %t.text,
            raw_words = %t.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "),
            words = %words
                .iter()
                .map(|w| format!("{}@{:.2}-{:.2}", w.text, w.start.as_secs_f64(), w.end.as_secs_f64()))
                .collect::<Vec<_>>()
                .join(" "),
            "reading"
        );
        self.readings.push(TimedReading {
            start: clip_start,
            end,
            words,
        });
        if self.readings.len() > self.windows {
            self.readings.remove(0);
        }
        if !self.stable() {
            // The readings change wholesale from one to the next: that is
            // the recogniser inventing speech over noise, not a thought —
            // or a noisy onset that has not settled yet. Nothing from it is
            // typed, and nothing is given up while it lasts.
            warn!("readings unstable; nothing typed from this reading");
            self.stable_since = end;
            return None;
        }
        let votes = if self.windows >= 4 {
            self.windows / 2 + 1
        } else {
            self.windows
        };
        let needed = if last { (votes - 1).max(2) } else { votes };
        if !last && self.readings.len() < self.windows {
            return None;
        }
        let newest = self.readings.last()?;
        let newest_end = newest.end;
        let settled_for = newest_end.saturating_sub(self.stable_since);
        let mut out = Vec::new();
        let mut done_until = self.done_until;
        let mut last_out = self.last_out.clone();
        let near = |x: &TimedWord, start: Duration| x.start.abs_diff(start) < SAME_MOMENT;
        for w in &newest.words {
            let (start, wend) = (w.start, w.end);
            if (start + wend) / 2 < done_until {
                continue; // already out (its middle lies before the typed edge)
            }
            if !last && wend + CUT_EDGE > newest_end {
                break; // may be cut mid-word: the next reading has it whole
            }
            // Readings that reach this moment at all; only they can vote.
            let covering: Vec<&TimedReading> = self
                .readings
                .iter()
                .filter(|r| r.start <= start && r.end + SAME_MOMENT >= start)
                .collect();
            // A single-word heard form becomes the trained word unless
            // every reading of the moment heard the same.
            let target = match &w.learned {
                Some(l) => {
                    // "Sure" needs at least three readings that agree; the
                    // two readings of a thought's tail (the final one and
                    // the ending check, 0.6 s apart) are nearly the same
                    // audio and prove nothing.
                    // A form with a token that is no German word (nor a
                    // compound of words) is a mishearing however often it
                    // repeats; a form of real words is sure only when at
                    // least three readings heard the same.
                    let unanimous = ikk_text::all_words(&w.text)
                        && covering.len() >= SURE_READINGS
                        && covering
                            .iter()
                            .all(|r| r.words.iter().any(|x| near(x, start) && same_hearing(&x.text, &w.text)));
                    if unanimous {
                        w.text.clone()
                    } else {
                        l.clone()
                    }
                }
                None => w.text.clone(),
            };
            if out.is_empty()
                && last_out.iter().any(|l| alike(l, &target))
                && start.abs_diff(done_until) < SAME_WORD_SLACK
            {
                done_until = done_until.max(wend);
                continue; // one of the last typed words, moved a little
            }
            // The readings that agree, and where each of them ends the word:
            // the typed edge moves to the earliest of those ends, because a
            // mishearing swapped for the word may stretch over the next one
            // ("Reißverschluss" over "längeren Text").
            let ends: Vec<Duration> = self
                .readings
                .iter()
                .filter_map(|r| {
                    r.words
                        .iter()
                        .find(|x| {
                            near(x, start)
                                && (last || x.end + CUT_EDGE <= r.end)
                                && (alike(&x.text, &target) || x.learned.as_deref().is_some_and(|l| alike(l, &target)))
                        })
                        .map(|x| x.end)
                })
                .collect();
            let agree = ends.len();
            let agreed_end = ends.iter().copied().min().unwrap_or(wend).min(wend);
            // At the pause two readings must agree (the ending check covers
            // the final reading's tail); a tail only one reading heard is
            // left out rather than typed as heard.
            let needed = if last {
                needed.min(covering.len()).max(2)
            } else {
                needed
            };
            if agree >= needed {
                out.push(target.clone());
                last_out.push_back(target);
                if last_out.len() > 3 {
                    last_out.pop_front();
                }
                done_until = done_until.max(agreed_end);
            } else if !last
                && newest_end.saturating_sub(start) > GIVE_UP_AFTER
                && settled_for > GIVE_UP_AFTER
                && !previous_agrees(&self.readings, start, &target)
            {
                // Nobody agrees and the moment is long past: leave it out
                // and go on, rather than stall behind it. Unless the two
                // newest readings have just come to agree on it — a word that
                // needed the words after it to be heard right (a long pause
                // inside the word) gets its third vote a moment later.
                warn!(word = %target, "no agreement on a word; left out");
                done_until = wend;
            } else {
                break;
            }
        }
        self.done_until = done_until;
        self.last_out = last_out;
        (!out.is_empty()).then(|| out.join(" "))
    }
}

/// The reading before the newest has a like word at this moment.
fn previous_agrees(readings: &[TimedReading], start: Duration, target: &str) -> bool {
    let n = readings.len();
    n >= 2
        && readings[n - 2]
            .words
            .iter()
            .any(|x| x.start.abs_diff(start) < SAME_MOMENT && alike(&x.text, target))
}

/// The same hearing, strictly: equal after normalisation, or one letter
/// off in a word of six or more. (The tolerant [`alike`] would take
/// *Bildschirmpassers* and *Bildschirm passt das* for one hearing by their
/// shared beginning, and call a disputed spot unanimous.)
fn same_hearing(a: &str, b: &str) -> bool {
    let (a, b) = (norm_word(a), norm_word(b));
    let (na, nb) = (a.chars().count(), b.chars().count());
    a == b || (na == nb && na >= 6 && a.chars().zip(b.chars()).filter(|(x, y)| x != y).count() == 1)
}

/// Alignment match: equal after normalisation, or the same word in another
/// inflection (both five letters or more, same first five letters) —
/// "längeren"/"längeres" must not break the anchor. Release decisions
/// still use the exact word.
fn alike(a: &str, b: &str) -> bool {
    let (a, b) = (norm_word(a), norm_word(b));
    let (na, nb) = (a.chars().count(), b.chars().count());
    a == b
        || (na >= 5 && nb >= 5 && a.chars().take(5).eq(b.chars().take(5)))
        || (na == nb && na >= 5 && a.chars().zip(b.chars()).filter(|(x, y)| x != y).count() == 1)
}

fn norm_word(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Takes chunks as they come, runs up to [`MAX_IN_FLIGHT`] transcriptions
/// at once and releases the results in chunk order.
async fn run_ordered(
    provider: Arc<dyn Provider>,
    cfg: SessionConfig,
    mut uploads: mpsc::Receiver<Upload>,
    events: mpsc::Sender<TranscriptEvent>,
    stale: Arc<AtomicBool>,
) {
    // Texts of the last readings of the current thought, to notice when
    // nothing new is being said (see `STALE_READINGS`).
    let mut recent_texts: VecDeque<String> = VecDeque::new();
    let (done_tx, mut done_rx) = mpsc::channel::<(u64, Outcome, bool)>(MAX_IN_FLIGHT * 4);
    // Chunks that may not go out yet (the limit is reached).
    let mut queued: VecDeque<Upload> = VecDeque::new();
    let mut waiting: BTreeMap<u64, Outcome> = BTreeMap::new();
    // Second passes by sequence number → first chunk they replace and,
    // for one retry, the audio.
    let mut refines: BTreeMap<u64, (u64, Option<AudioClip>)> = BTreeMap::new();
    // Finals released since the last second pass: sequence number and
    // length, to know what a second pass replaces and whether it may.
    let mut final_seqs: Vec<(u64, usize)> = Vec::new();
    // Preview mode: quick texts held back until the thought is decided.
    let preview = cfg.refine && cfg.refine_preview;
    let mut held: Vec<String> = Vec::new();
    // Agreement mode: which sequence numbers are windows (and last ones),
    // the recent results of the thought, how many words are out already.
    let mut windows: BTreeMap<u64, (bool, bool, Duration, Duration)> = BTreeMap::new();
    let mut agreed = Agreed::new(cfg.agreement.map_or(1, |a| a.windows));
    let mut timed = TimedAgreed::new(cfg.agreement.map_or(1, |a| a.windows), cfg.learned.clone());
    // What the current thought sounded like so far, for the hints.
    let mut heard_so_far = String::new();
    let mut next_emit = 0u64;
    let mut in_flight = 0usize;
    let caps = provider.capabilities();
    let cap = caps.max_in_flight.unwrap_or(MAX_IN_FLIGHT).max(1);
    let timed_words = caps.timed_words;
    let mut limit = INITIAL_IN_FLIGHT.min(cap);
    // Clean answers since the limit last changed.
    let mut clean = 0usize;
    let mut open = true;
    let mut fatal = false;
    loop {
        // Send whatever the limit allows.
        while in_flight < limit && !fatal {
            let Some(up) = queued.pop_front() else { break };
            // A window still waiting while a newer one is queued already
            // says nothing the newer one will not: skip it (a slow provider
            // must not pile up requests).
            if let Upload::Window { seq, last: false, .. } = &up {
                if queued.iter().any(|q| matches!(q, Upload::Window { .. })) {
                    debug!(seq, "window superseded before upload; skipped");
                    continue;
                }
            }
            let (seq, clip, timeout) = match up {
                Upload::Flush(seq) => {
                    waiting.insert(seq, Outcome::Flush);
                    continue;
                }
                Upload::Chunk(seq, clip) => (seq, clip, UPLOAD_TIMEOUT),
                Upload::Window {
                    seq,
                    clip,
                    last,
                    from_start,
                    clip_start,
                } => {
                    let clip_len: Duration = clip.frames.iter().map(|f| f.duration()).sum();
                    windows.insert(seq, (last, from_start, clip_start, clip_len));
                    (seq, clip, if last { LAST_WINDOW_TIMEOUT } else { WINDOW_TIMEOUT })
                }
                Upload::Refine { seq, first_seq, clip } => {
                    match refines.get_mut(&seq) {
                        // The retry: no further one.
                        Some(entry) => entry.1 = None,
                        None => {
                            refines.insert(seq, (first_seq, Some(clip.clone())));
                        }
                    }
                    (seq, clip, REFINE_TIMEOUT)
                }
            };
            in_flight += 1;
            let want_words = timed_words && windows.contains_key(&seq);
            let single_try = windows.get(&seq).is_some_and(|w| !w.0);
            // Trained words resembling the thought so far go along as hints
            // (providers without hints ignore them).
            let mut cfg_req = cfg.clone();
            for h in ikk_text::similar_hints(&cfg.learned, &heard_so_far, 10) {
                if !cfg_req.keywords.iter().any(|(k, _)| k == &h) {
                    debug!(hint = %h, "learned word sent as hint");
                    cfg_req.keywords.push((h, 5));
                }
            }
            tokio::spawn(transcribe_chunk(
                provider.clone(),
                cfg_req,
                seq,
                clip,
                timeout,
                want_words,
                single_try,
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
                    if clean >= limit && limit < cap {
                        limit += 1;
                        clean = 0;
                    }
                }
                waiting.insert(seq, outcome);
                // Release everything that is next in line.
                while let Some(outcome) = waiting.remove(&next_emit) {
                    let seq = next_emit;
                    // A truncated second pass gets one more try before the
                    // chunk texts are left standing.
                    if let (Outcome::Text(text), Some((first, Some(_)))) = (&outcome, refines.get(&seq)) {
                        let chars: usize = final_seqs.iter().filter(|(s, _)| s >= first).map(|(_, n)| n).sum();
                        if chars > 0 && text.chars().count() * 2 < chars {
                            let (first_seq, clip) = refines.remove(&seq).unwrap();
                            warn!(seq, chars, got = text.chars().count(), "second pass too short; trying once more");
                            refines.insert(seq, (first_seq, None));
                            queued.push_front(Upload::Refine {
                                seq,
                                first_seq,
                                clip: clip.unwrap(),
                            });
                            break;
                        }
                    }
                    next_emit += 1;
                    let refine_of = refines.remove(&seq).map(|(first, _)| first);
                    if let Some((last, from_start, clip_start, clip_len)) = windows.remove(&seq) {
                        let answer = match outcome {
                            Outcome::Timed(t) => t,
                            Outcome::Text(t) => Transcript {
                                text: t,
                                language: None,
                                confidence: None,
                                words: Vec::new(),
                            },
                            Outcome::Fatal(class) => {
                                fatal = true;
                                if events.send(TranscriptEvent::Error(class)).await.is_err() {
                                    return;
                                }
                                continue;
                            }
                            // A lost window: the next one carries the words.
                            Outcome::Skipped(_) | Outcome::Flush => {
                                if last {
                                    agreed.reset();
                                    timed.reset();
                                }
                                continue;
                            }
                        };
                        let text = ikk_text::apply_text(&cfg.learned, &answer.text, true);
                        heard_so_far = if last { String::new() } else { answer.text.clone() };
                        if last {
                            recent_texts.clear();
                        } else {
                            recent_texts.push_back(answer.text.trim().to_string());
                            if recent_texts.len() > STALE_READINGS {
                                recent_texts.pop_front();
                            }
                            if recent_texts.len() == STALE_READINGS
                                && !recent_texts[0].is_empty()
                                && recent_texts.iter().all(|t| *t == recent_texts[0])
                            {
                                stale.store(true, Ordering::SeqCst);
                                recent_texts.clear();
                            }
                        }
                        let release = if !crate::text_fits_language(&text, &cfg.language) {
                            info!(seq, %text, "window in another script; ignored");
                            None
                        } else if !answer.words.is_empty() {
                            // Word timestamps: vote per moment of the thought.
                            timed.offer(&answer, clip_start, clip_start + clip_len, last)
                        } else {
                            agreed.offer(&text, last, from_start)
                        };
                        if last {
                            agreed.reset();
                            timed.reset();
                        }
                        if let Some(words) = release {
                            if events.send(TranscriptEvent::Final(words)).await.is_err() {
                                return;
                            }
                        }
                        continue;
                    }
                    // Timed answers come only for windows; a stale one has no
                    // window left, its text is all that matters.
                    let outcome = match outcome {
                        Outcome::Timed(t) => Outcome::Text(ikk_text::apply_text(&cfg.learned, &t.text, true)),
                        Outcome::Text(t) => Outcome::Text(ikk_text::apply_text(&cfg.learned, &t, true)),
                        o => o,
                    };
                    let event = match outcome {
                        Outcome::Timed(_) => unreachable!("remapped above"),
                        Outcome::Flush => {
                            // The thought is over without a second pass: the
                            // quick text is the text.
                            final_seqs.clear();
                            let text = std::mem::take(&mut held).join(" ");
                            if text.is_empty() {
                                continue;
                            }
                            TranscriptEvent::Final(text)
                        }
                        Outcome::Text(text) if text.is_empty() => continue,
                        Outcome::Text(text) if !crate::text_fits_language(&text, &cfg.language) => {
                            info!(seq, %text, "answer in another script; dropped");
                            continue;
                        }
                        Outcome::Text(text) => match refine_of {
                            Some(first) => {
                                let replaced: Vec<_> = final_seqs.iter().filter(|(s, _)| *s >= first).collect();
                                let segments = replaced.len();
                                let chars: usize = replaced.iter().map(|(_, n)| n).sum();
                                final_seqs.clear();
                                let too_short = segments > 0 && text.chars().count() * 2 < chars;
                                if too_short {
                                    // Still truncated after the retry: the chunk texts stay.
                                    warn!(seq, chars, got = text.chars().count(), "second pass too short; the chunk texts stay");
                                }
                                if preview {
                                    // Nothing was typed yet: the whole thought's text is
                                    // the better one even when it came back short — the
                                    // quick chunks of unclear speech are the worse fallback.
                                    held.clear();
                                    TranscriptEvent::Final(text)
                                } else if too_short {
                                    continue;
                                } else if segments == 0 {
                                    // The chunks gave nothing; this is the first text.
                                    final_seqs.push((seq, text.chars().count()));
                                    TranscriptEvent::Final(text)
                                } else {
                                    TranscriptEvent::Refined { segments, text }
                                }
                            }
                            None => {
                                final_seqs.push((seq, text.chars().count()));
                                if final_seqs.len() > 256 {
                                    final_seqs.remove(0);
                                }
                                if preview {
                                    held.push(text);
                                    TranscriptEvent::Interim(held.join(" "))
                                } else {
                                    TranscriptEvent::Final(text)
                                }
                            }
                        },
                        Outcome::Skipped(class) if refine_of.is_some() => {
                            warn!(seq, ?class, "second pass lost; the chunk texts stay");
                            continue;
                        }
                        Outcome::Skipped(class) => {
                            warn!(seq, ?class, "chunk lost after retries");
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
    if !held.is_empty() {
        let _ = events.send(TranscriptEvent::Final(held.join(" "))).await;
    }
    let _ = events.send(TranscriptEvent::Closed).await;
}

/// One chunk: transcribes with retries, reports back (and whether the
/// provider throttled it on the way).
#[allow(clippy::too_many_arguments)]
async fn transcribe_chunk(
    provider: Arc<dyn Provider>,
    cfg: SessionConfig,
    seq: u64,
    clip: AudioClip,
    timeout: Duration,
    want_words: bool,
    single_try: bool,
    done: mpsc::Sender<(u64, Outcome, bool)>,
) {
    let mut attempt = 0usize;
    let mut throttled = false;
    let outcome = loop {
        let result = tokio::time::timeout(timeout, provider.transcribe(clip.clone(), &cfg)).await;
        // A timeout is a transport problem.
        let class = match result {
            Ok(Ok(t)) if want_words && !t.words.is_empty() => {
                break Outcome::Timed(Transcript {
                    text: t.text.trim().to_string(),
                    ..t
                })
            }
            Ok(Ok(t)) => break Outcome::Text(t.text.trim().to_string()),
            Ok(Err(SttError::Provider(class))) if class.is_retryable() => class,
            Ok(Err(SttError::Provider(class))) => break Outcome::Fatal(class),
            Ok(Err(e)) => break Outcome::Fatal(ErrorClass::Other(e.to_string())),
            Err(_) => ErrorClass::Transport,
        };
        throttled |= class == ErrorClass::Throttled;
        if attempt >= RETRY_BACKOFF.len() || single_try {
            // An agreement window is not retried: the next one is already
            // on its way and says the same and more.
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
        if let Some(a) = self.agreement {
            self.push_window_frame(frame, a).await;
            return Ok(());
        }
        if self.refine {
            if self.thought_len + frame.duration() > REFINE_MAX {
                // Too long for one request: the chunks stand as they are.
                self.thought.clear();
                self.thought_len = Duration::ZERO;
                self.thought_chunks = 0;
                self.thought_first_seq = self.next_seq;
                self.refine_overflow();
            } else {
                self.thought_len += frame.duration();
                self.thought.push(frame.clone());
            }
        }
        self.buffered += frame.duration();
        self.peak_dbfs = self.peak_dbfs.max(frame.level_dbfs());
        if let Some(p) = frame.speech {
            self.peak_speech = Some(self.peak_speech.map_or(p, |q| q.max(p)));
        }
        let gap = match frame.speech {
            Some(p) => {
                if p < GAP_SPEECH_PROB {
                    self.quiet_for += frame.duration();
                } else {
                    self.quiet_for = Duration::ZERO;
                }
                self.quiet_for >= GAP_PAUSE
            }
            None => {
                let vad = self.gap_vad.get_or_insert_with(|| {
                    let mut v = VoiceActivityDetector::new(VadMode::Normal, frame.sample_rate_hz);
                    v.set_pause_duration_ms(GAP_PAUSE.as_millis() as u32);
                    v
                });
                vad.process(&frame.samples) == Some(VadEvent::SpeechEnded)
            }
        };
        self.buffer.push(frame);
        if self.buffered >= self.hard_max {
            self.rotate("timer").await;
        } else if self.buffered >= self.interval && gap {
            self.rotate("gap").await;
        }
        Ok(())
    }

    async fn cut_segment(&mut self) -> Result<(), SttError> {
        if self.agreement.is_some() {
            self.send_window(true).await;
            return Ok(());
        }
        self.rotate("pause").await;
        self.end_thought().await;
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        if self.agreement.is_some() {
            self.send_window(true).await;
        } else {
            self.rotate("pause").await;
            self.end_thought().await;
        }
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
        /// Answer clips longer than this with one character (0 = never):
        /// a recogniser that stops after the first sentence.
        truncate_above_ms: u64,
        /// Answer with the first `ms / 500` words of a fixed sentence
        /// instead of the length (agreement-mode tests); the word at
        /// `flip_word` is wrong while the clip is shorter than `flip_until_ms`.
        sentence: bool,
        flip_word: usize,
        flip_until_ms: u64,
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
                truncate_above_ms: 0,
                sentence: false,
                flip_word: usize::MAX,
                flip_until_ms: 0,
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
            let text = if self.sentence {
                const SENTENCE: [&str; 12] = [
                    "Hallo,",
                    "ich",
                    "möchte",
                    "einen",
                    "längeren",
                    "Text",
                    "schreiben.",
                    "Ich",
                    "habe",
                    "so",
                    "viel",
                    "zu",
                ];
                let n = ((ms / 500) as usize).min(SENTENCE.len());
                let mut words: Vec<String> = SENTENCE[..n].iter().map(|w| w.to_string()).collect();
                if self.flip_word < n && ms < self.flip_until_ms {
                    words[self.flip_word] = "kann".into();
                }
                words.join(" ")
            } else if self.truncate_above_ms > 0 && ms > self.truncate_above_ms {
                "x".to_string()
            } else {
                format!("{ms}ms")
            };
            Ok(Transcript {
                text,
                language: None,
                confidence: None,
                words: Vec::new(),
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
            speech: None,
        }
    }

    /// A frame the capture chain has listened to: loud background noise
    /// throughout, only the speech probability differs.
    fn heard(ms: u64, speech: f32) -> AudioFrame {
        AudioFrame {
            speech: Some(speech),
            ..frame(ms, 0.2)
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
    async fn refines_the_whole_thought_after_a_pause() {
        // Interval 1 s. The thought takes two chunks (a gap cut and the
        // pause), so it is transcribed again in one piece; a thought of
        // one chunk is not.
        let fake = Arc::new(Fake::new(|_| 1));
        let mut cfg = config();
        cfg.refine = true;
        cfg.refine_preview = false;
        let mut s = open_session(fake.clone(), &cfg).await.unwrap();
        for _ in 0..60 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        for _ in 0..20 {
            s.push(heard(20, 0.05)).await.unwrap();
        }
        for _ in 0..50 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        // Second thought: a single chunk, no second pass.
        for _ in 0..50 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        assert_eq!(
            drain(&mut s).await,
            vec![
                TranscriptEvent::Final("1460ms".into()),
                TranscriptEvent::Final("1140ms".into()),
                TranscriptEvent::Refined {
                    segments: 2,
                    text: "2600ms".into()
                },
                TranscriptEvent::Final("1000ms".into()),
            ]
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn agreed_aligns_a_sliding_window_to_the_typed_words() {
        let mut a = Agreed::new(2);
        assert_eq!(a.offer("Hallo ich möchte", false, true), None);
        assert_eq!(
            a.offer("Hallo ich möchte einen", false, true).as_deref(),
            Some("Hallo ich möchte")
        );
        // The window slid: it starts inside the typed text.
        assert_eq!(
            a.offer("ich möchte einen längeren", false, false).as_deref(),
            Some("einen")
        );
        // A result without the typed text is skipped, not misaligned.
        assert_eq!(a.offer("ganz etwas anderes hier", false, false), None);
        assert_eq!(
            a.offer("möchte einen längeren Text", false, false).as_deref(),
            Some("längeren")
        );
        // The pause: only what the previous result also had.
        assert_eq!(
            a.offer("einen längeren Text schreiben Untertitel", true, false)
                .as_deref(),
            Some("Text")
        );
    }

    #[test]
    fn majority_releases_a_word_one_result_got_wrong_and_inflections_keep_the_anchor() {
        let mut a = Agreed::new(4);
        for t in [
            "Hallo ich möchte einen",
            "Hallo ich möchte einen",
            "Hallo ich möchte einem",
            "Hallo ich möchte einen längeren",
        ] {
            let _ = a.offer(t, false, true);
        }
        // Three of four say "einen": it goes out although one said "einem".
        assert_eq!(a.committed, vec!["hallo", "ich", "möchte", "einen"]);
        // A slid window beginning with another inflection of the typed word still aligns.
        assert_eq!(a.offer("einem längeren Text", false, false), None);
        // Three recent results carry "längeren": out it goes.
        assert_eq!(
            a.offer("einen längeren Text schreiben", false, false).as_deref(),
            Some("längeren")
        );
        // One result's "längeres" cannot change what is typed; "Text" follows.
        assert_eq!(
            a.offer("einen längeres Text schreiben Ich", false, false).as_deref(),
            Some("Text")
        );
    }

    fn timed(words: &[(&str, f64)], clip_start: f64) -> Transcript {
        Transcript {
            text: words.iter().map(|w| w.0).collect::<Vec<_>>().join(" "),
            language: None,
            confidence: None,
            words: words
                .iter()
                .map(|(w, t)| crate::Word {
                    text: w.to_string(),
                    start: Duration::from_secs_f64(*t - clip_start),
                    end: Duration::from_secs_f64(*t - clip_start + 0.3),
                })
                .collect(),
        }
    }

    #[test]
    fn timed_agreement_votes_per_moment_and_gives_up_disputed_ones() {
        let mut a = TimedAgreed::new(4, Vec::new());
        let s = |c: f64| Duration::from_secs_f64(c);
        // Four readings of growing clips; the third mishears "einen" as "einem"
        // and the fourth starts later (slid window) — both must not matter.
        assert_eq!(
            a.offer(
                &timed(&[("Hallo", 0.5), ("ich", 1.5)], 0.0),
                s(0.0),
                Duration::ZERO,
                false
            ),
            None
        );
        assert_eq!(
            a.offer(
                &timed(&[("Hallo", 0.5), ("ich", 1.5), ("möchte", 2.5)], 0.0),
                s(0.0),
                Duration::ZERO,
                false
            ),
            None
        );
        assert_eq!(
            a.offer(
                &timed(&[("Hallo", 0.5), ("ich", 1.5), ("möchte", 2.5), ("einem", 3.5)], 0.0),
                s(0.0),
                Duration::ZERO,
                false
            ),
            None
        );
        assert_eq!(
            a.offer(
                &timed(&[("ich", 1.5), ("möchte", 2.5), ("einen", 3.5), ("längeren", 4.5)], 1.0),
                s(1.0),
                Duration::ZERO,
                false
            )
            .as_deref(),
            Some("ich möchte")
        );
        // "einem" counts as agreement with "einen" (inflection), so three of
        // four readings carry it; "längeren" has only two yet.
        assert_eq!(
            a.offer(
                &timed(
                    &[("möchte", 2.5), ("einen", 3.5), ("längeren", 4.5), ("Text", 5.5)],
                    2.0
                ),
                s(2.0),
                Duration::ZERO,
                false
            )
            .as_deref(),
            Some("einen")
        );
        // The pause: two agreeing readings suffice for the ending; the tail
        // only the final reading reaches ("schreiben") stays out.
        assert_eq!(
            a.offer(
                &timed(
                    &[("einen", 3.5), ("längeren", 4.5), ("Text", 5.5), ("schreiben", 6.5)],
                    3.0
                ),
                s(3.0),
                Duration::ZERO,
                true
            )
            .as_deref(),
            Some("längeren Text")
        );
    }

    #[test]
    fn timed_agreement_does_not_repeat_a_word_that_moved() {
        let mut a = TimedAgreed::new(4, Vec::new());
        let s = |c: f64| Duration::from_secs_f64(c);
        let w = [("eine", 1.0), ("gute", 1.5), ("Lösung", 2.0), ("bekommen", 2.6)];
        for i in 0..3 {
            assert_eq!(a.offer(&timed(&w, 0.0), s(0.0), Duration::ZERO, false), None, "{i}");
        }
        assert_eq!(
            a.offer(&timed(&w, 0.0), s(0.0), Duration::ZERO, false).as_deref(),
            Some("eine gute Lösung bekommen")
        );
        // At the pause the last word is heard 0.5 s later than before.
        let moved = [("eine", 1.0), ("gute", 1.5), ("Lösung", 2.0), ("bekommen", 3.1)];
        assert_eq!(a.offer(&timed(&moved, 0.0), s(0.0), Duration::ZERO, true), None);
        // Two words shifted together ("so viel" heard again 0.4 s later)
        // are not typed again either.
        let mut a = TimedAgreed::new(4, Vec::new());
        let w = [("habe", 1.0), ("so", 1.5), ("viel", 1.9)];
        for _ in 0..3 {
            assert_eq!(a.offer(&timed(&w, 0.0), s(0.0), Duration::ZERO, false), None);
        }
        assert_eq!(
            a.offer(&timed(&w, 0.0), s(0.0), Duration::ZERO, false).as_deref(),
            Some("habe so viel")
        );
        let shifted = [
            ("habe", 1.0),
            ("so", 1.95),
            ("viel", 2.3),
            ("zu", 2.8),
            ("schreiben", 3.1),
        ];
        for _ in 0..3 {
            let r = a.offer(&timed(&shifted, 0.0), s(0.0), Duration::ZERO, false);
            assert!(r.as_deref().is_none_or(|t| !t.contains("so")), "{r:?}");
        }
    }

    #[test]
    fn trained_words_replace_their_heard_forms_in_the_readings() {
        let learned = vec![
            ikk_text::LearnedWord {
                word: "Bildschirmtastatur".into(),
                heard: vec!["Bildschirm passt das".into(), "Bildschirmpassers".into()],
                always: false,
            },
            ikk_text::LearnedWord {
                word: "Maccaud".into(),
                heard: vec!["Marco".into(), "Markus".into()],
                always: false,
            },
        ];
        let s = |c: f64| Duration::from_secs_f64(c);
        // The compound: readings hear it three different ways, two of them
        // trained forms — one typed word, with its full stop.
        let mut a = TimedAgreed::new(4, learned.clone());
        let r1 = [("der", 1.0), ("Bildschirm", 1.5), ("passt", 2.2), ("das.", 2.6)];
        let r2 = [("der", 1.0), ("Bildschirmpassers.", 1.5)];
        let r3 = [("der", 1.0), ("Bildschirm", 1.5), ("Pasta.", 2.2)];
        assert_eq!(a.offer(&timed(&r1, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(a.offer(&timed(&r2, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(a.offer(&timed(&r3, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(
            a.offer(&timed(&r1, 0.0), s(0.0), Duration::ZERO, true).as_deref(),
            Some("der Bildschirmtastatur.")
        );
        // A single-word form everybody agrees on is a genuine word.
        let mut a = TimedAgreed::new(4, learned.clone());
        let m = [("Hallo", 0.5), ("Marco", 1.0), ("wie", 1.5), ("geht", 1.9)];
        for _ in 0..3 {
            assert_eq!(a.offer(&timed(&m, 0.0), s(0.0), Duration::ZERO, false), None);
        }
        assert_eq!(
            a.offer(&timed(&m, 0.0), s(0.0), Duration::ZERO, false).as_deref(),
            Some("Hallo Marco wie geht")
        );
        // Heard two ways: the trained word.
        let mut a = TimedAgreed::new(4, learned);
        let m2 = [("Hallo", 0.5), ("Markus", 1.0), ("wie", 1.5), ("geht", 1.9)];
        assert_eq!(a.offer(&timed(&m, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(a.offer(&timed(&m2, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(a.offer(&timed(&m, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(
            a.offer(&timed(&m, 0.0), s(0.0), Duration::ZERO, false).as_deref(),
            Some("Hallo Maccaud wie geht")
        );
    }

    #[test]
    fn a_word_cut_at_the_reading_edge_waits_for_the_whole_word() {
        let mut a = TimedAgreed::new(4, Vec::new());
        let s = |c: f64| Duration::from_secs_f64(c);
        // "schreiben" is long; three readings in a row end inside it and
        // hear "Scheiße" — the fourth has it whole.
        let cut = |end: f64| vec![("Text", 1.0), ("Scheiße", end - 0.3)];
        assert_eq!(a.offer(&timed(&cut(2.0), 0.0), s(0.0), s(2.0), false), None);
        assert_eq!(a.offer(&timed(&cut(3.0), 0.0), s(0.0), s(3.0), false), None);
        assert_eq!(a.offer(&timed(&cut(4.0), 0.0), s(0.0), s(4.0), false), None);
        let r = a.offer(&timed(&[("Text", 1.0), ("Scheiße", 1.7)], 0.0), s(0.0), s(2.0), false);
        assert_eq!(r.as_deref(), Some("Text"), "the cut word never voted");
        let whole = [
            ("Text", 1.0),
            ("schreiben", 1.7),
            ("Ich", 3.0),
            ("habe", 3.4),
            ("so", 3.8),
        ];
        assert_eq!(a.offer(&timed(&whole, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(a.offer(&timed(&whole, 0.0), s(0.0), Duration::ZERO, false), None);
        assert_eq!(
            a.offer(&timed(&whole, 0.0), s(0.0), Duration::ZERO, false).as_deref(),
            Some("schreiben Ich habe so")
        );
    }

    #[test]
    fn invented_speech_over_noise_is_not_typed() {
        let mut a = TimedAgreed::new(4, Vec::new());
        let s = |c: f64| Duration::from_secs_f64(c);
        // Four readings of the same noise: the first words repeat (the
        // recogniser's habit), the rest is different every time.
        let r = [
            [
                ("Ich", 0.6),
                ("habe", 0.8),
                ("ein", 1.0),
                ("Bein", 1.1),
                ("gebrochen", 1.3),
                ("Bitte", 2.1),
                ("helfen", 2.3),
                ("Sie", 2.4),
                ("mir", 2.6),
            ],
            [
                ("Ich", 0.6),
                ("hatte", 0.8),
                ("ein", 1.0),
                ("Bärchen", 1.1),
                ("gehabt", 1.4),
                ("Hier", 2.1),
                ("geht", 2.3),
                ("es", 2.4),
                ("rund", 2.6),
            ],
            [
                ("Ich", 0.6),
                ("bin", 0.8),
                ("mir", 1.0),
                ("wehgetan", 1.2),
                ("Also", 1.8),
                ("hör", 2.1),
                ("mal", 2.3),
                ("zu", 2.4),
                ("jetzt", 2.6),
            ],
            [
                ("Ich", 0.6),
                ("habe", 0.8),
                ("einen", 1.0),
                ("Plan", 1.2),
                ("gemacht", 1.5),
                ("Danke", 2.1),
                ("für", 2.3),
                ("alles", 2.4),
                ("hier", 2.6),
            ],
        ];
        for (i, w) in r.iter().enumerate() {
            assert_eq!(
                a.offer(&timed(w, 0.0), s(0.0), Duration::ZERO, false),
                None,
                "reading {i}"
            );
        }
        assert_eq!(
            a.offer(&timed(&r[3], 0.0), s(0.0), Duration::ZERO, true),
            None,
            "the pause"
        );
    }

    #[test]
    fn a_late_settling_word_is_not_given_up_while_the_newest_readings_agree() {
        let mut a = TimedAgreed::new(4, Vec::new());
        let s = |c: f64| Duration::from_secs_f64(c);
        let rest: Vec<(&str, f64)> = vec![
            ("Text", 2.4),
            ("schreiben", 3.4),
            ("Ich", 4.6),
            ("habe", 5.0),
            ("so", 5.4),
            ("viel", 5.8),
            ("zu", 6.2),
        ];
        let with = |w: &'static str, n: usize| {
            let mut v = vec![("einen", 0.5), (w, 1.0)];
            v.extend(rest[..n].iter().cloned());
            v
        };
        // Heard wildly while at the edge, then "langen" twice in a row.
        assert_eq!(a.offer(&timed(&with("Leihhelm", 1), 0.0), s(0.0), s(3.0), false), None);
        assert_eq!(
            a.offer(&timed(&with("Reißverschluss", 2), 0.0), s(0.0), s(4.0), false),
            None
        );
        assert_eq!(a.offer(&timed(&with("Brief", 3), 0.0), s(0.0), s(5.0), false), None);
        assert_eq!(
            a.offer(&timed(&with("langen", 4), 0.0), s(0.0), s(6.0), false)
                .as_deref(),
            Some("einen")
        );
        // 6.5 s after the word, the previous reading agrees: not given up.
        assert_eq!(a.offer(&timed(&with("langen", 5), 0.0), s(0.0), s(7.5), false), None);
        assert_eq!(
            a.offer(&timed(&with("langen", 6), 0.0), s(0.0), s(8.5), false)
                .as_deref(),
            Some("langen Text schreiben Ich habe")
        );
    }

    #[test]
    fn a_stretched_mishearing_does_not_swallow_the_next_word() {
        let learned = vec![ikk_text::LearnedWord {
            word: "längeren".into(),
            heard: vec!["Reißverschluss".into()],
            always: false,
        }];
        let mut a = TimedAgreed::new(4, learned);
        let s = |c: f64| Duration::from_secs_f64(c);
        let ok = [
            ("einen", 0.5),
            ("längeren", 1.0),
            ("Text", 2.4),
            ("schreiben", 3.4),
            ("Ich", 4.6),
        ];
        let mk = |w: &[(&str, f64)]| timed(w, 0.0);
        let mut t1 = mk(&ok);
        t1.words[1].text = "Reißverschluss".into();
        t1.words[1].end = Duration::from_secs_f64(2.9); // over "Text"
        let t2 = mk(&[
            ("einen", 0.5),
            ("langen", 1.0),
            ("Text", 2.4),
            ("schreiben", 3.4),
            ("Ich", 4.6),
        ]);
        let t3 = mk(&ok);
        assert_eq!(a.offer(&t3, s(0.0), s(6.0), false), None);
        assert_eq!(a.offer(&t2, s(0.0), s(6.0), false), None);
        assert_eq!(a.offer(&t3, s(0.0), s(6.0), false), None);
        assert_eq!(
            a.offer(&t1, s(0.0), s(6.0), false).as_deref(),
            Some("einen längeren Text schreiben Ich"),
            "Text survives the stretched hearing"
        );
    }

    #[test]
    fn timed_agreement_gives_up_a_long_disputed_moment() {
        let mut a = TimedAgreed::new(4, Vec::new());
        let s = |c: f64| Duration::from_secs_f64(c);
        let v = |x: &'static str, n: usize| -> Vec<(&'static str, f64)> {
            let mut w = vec![(x, 0.5)];
            for i in 0..n {
                w.push((
                    ["ich", "will", "jetzt", "schreiben", "gehen", "und", "lesen"][i],
                    1.5 + i as f64,
                ));
            }
            w
        };
        // The first word is heard differently every time; the rest agrees.
        for (i, x) in ["Hallo", "Also", "Wallo", "Halo"].iter().enumerate() {
            let r = a.offer(&timed(&v(x, i + 1), 0.0), s(0.0), Duration::ZERO, false);
            assert_eq!(r, None, "reading {i}");
        }
        // Four words later the disputed first word is 5 s old: left out, the
        // agreed words follow.
        assert_eq!(
            a.offer(&timed(&v("Hallo", 6), 0.0), s(0.0), Duration::ZERO, false)
                .as_deref(),
            Some("ich will jetzt")
        );
    }

    #[test]
    fn a_wrong_typed_word_does_not_block_the_rest() {
        // Three short results agreed on "bin"; later results say "möchte".
        let mut a = Agreed::new(3);
        for _ in 0..3 {
            let _ = a.offer("Hallo ich bin", false, true);
        }
        assert_eq!(a.committed, vec!["hallo", "ich", "bin"]);
        // "bin" stays typed; "möchte" is its correction and is skipped,
        // the words after it continue.
        assert_eq!(a.offer("Hallo ich möchte einen", false, true), None);
        assert_eq!(a.offer("Hallo ich möchte einen längeren", false, true), None);
        // Three aligned results agree on "einen" (the first held only that).
        assert_eq!(
            a.offer("Hallo ich möchte einen längeren Text", false, true).as_deref(),
            Some("einen")
        );
        // Slid windows without any typed word are skipped, with some they align.
        assert_eq!(a.offer("ganz anders", false, false), None);
        assert_eq!(
            a.offer("ich möchte einen längeren Text schreiben", false, false)
                .as_deref(),
            Some("längeren")
        );
    }

    #[test]
    fn a_window_past_the_typed_words_is_placed_by_the_open_words() {
        let mut a = Agreed::new(2);
        let _ = a.offer("Hallo ich möchte dir einen", false, true);
        assert_eq!(
            a.offer("Hallo ich möchte dir einen längeren", false, true).as_deref(),
            Some("Hallo ich möchte dir einen")
        );
        // Slid windows without the typed words: placed by the open "längeren"
        // (two results agree on it already), then the rest follows.
        assert_eq!(
            a.offer("längeren Text schreiben", false, false).as_deref(),
            Some("längeren")
        );
        assert_eq!(
            a.offer("längeren Text schreiben Ich", false, false).as_deref(),
            Some("Text schreiben")
        );
        // Nothing at all in common three times: the third starts fresh.
        assert_eq!(a.offer("xx yy", false, false), None);
        assert_eq!(a.offer("xx yy", false, false), None);
        assert_eq!(a.offer("xx yy zz", false, false), None);
        assert_eq!(a.offer("xx yy zz ww", false, false).as_deref(), Some("xx yy zz"));
    }

    #[test]
    fn window_tail_is_trimmed_to_a_short_quiet() {
        let mut frames: Vec<AudioFrame> = (0..10).map(|_| heard(20, 0.9)).collect();
        frames.extend((0..100).map(|_| heard(20, 0.05)));
        trim_tail(&mut frames, Duration::from_millis(400));
        let total: Duration = frames.iter().map(|f| f.duration()).sum();
        assert_eq!(total, Duration::from_millis(200 + 400));
        // Nothing to trim when speech runs to the end.
        let mut frames: Vec<AudioFrame> = (0..10).map(|_| heard(20, 0.9)).collect();
        trim_tail(&mut frames, Duration::from_millis(400));
        assert_eq!(frames.len(), 10);
    }

    #[tokio::test]
    async fn agreement_releases_only_words_three_results_agree_on() {
        // Windows every second over a 6 s thought; the fake answers with
        // the first ms/500 words, so each window adds two words. Word 3
        // ("einen") is wrong until the clip is 4 s long: it must not go
        // out before three results agree on the right one.
        let mut fake = Fake::new(|_| 1);
        fake.sentence = true;
        fake.flip_word = 3;
        fake.flip_until_ms = 4000;
        let fake = Arc::new(fake);
        let mut cfg = config();
        cfg.agreement = Some(Agreement {
            step: Duration::from_millis(1000),
            windows: 3,
        });
        let mut s = open_session(fake.clone(), &cfg).await.unwrap();
        for _ in 0..300 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        let out = drain(&mut s).await;
        let texts: Vec<String> = out
            .iter()
            .map(|e| match e {
                TranscriptEvent::Final(t) => t.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        // Windows under 3.5 s go nowhere; 4 s: 8 words (right by now),
        // 5 s: 10, 6 s: 12 → the three agree on the first 8; last → the rest.
        assert_eq!(
            texts.join(" "),
            "Hallo, ich möchte einen längeren Text schreiben. Ich habe so viel zu"
        );
        assert_eq!(texts[0], "Hallo, ich möchte einen längeren Text schreiben. Ich");
        assert!(!texts.iter().any(|t| t.contains("kann")), "{texts:?}");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            5,
            "three windows, the ending check and the last one"
        );
    }

    #[tokio::test]
    async fn preview_shows_quick_text_and_types_each_thought_once() {
        let fake = Arc::new(Fake::new(|_| 1));
        let mut cfg = config();
        cfg.refine = true;
        cfg.refine_preview = true;
        let mut s = open_session(fake.clone(), &cfg).await.unwrap();
        // Two-chunk thought: both chunks only previewed, the whole thought is the final.
        for _ in 0..60 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        for _ in 0..20 {
            s.push(heard(20, 0.05)).await.unwrap();
        }
        for _ in 0..50 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        // One-chunk thought: previewed, then final as it is.
        for _ in 0..50 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        assert_eq!(
            drain(&mut s).await,
            vec![
                TranscriptEvent::Interim("1460ms".into()),
                TranscriptEvent::Interim("1460ms 1140ms".into()),
                TranscriptEvent::Final("2600ms".into()),
                TranscriptEvent::Interim("1000ms".into()),
                TranscriptEvent::Final("1000ms".into()),
            ]
        );
    }

    #[tokio::test]
    async fn truncated_second_pass_is_rejected_and_speechless_chunks_stay_home() {
        let mut fake = Fake::new(|_| 1);
        fake.truncate_above_ms = 2000;
        let fake = Arc::new(fake);
        let mut cfg = config();
        cfg.refine = true;
        cfg.refine_preview = false;
        let mut s = open_session(fake.clone(), &cfg).await.unwrap();
        // Breathing only, 1.5 s, then a pause: nothing is uploaded.
        for _ in 0..75 {
            s.push(heard(20, 0.1)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        // A two-chunk thought whose second pass comes back as one letter.
        for _ in 0..60 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        for _ in 0..20 {
            s.push(heard(20, 0.05)).await.unwrap();
        }
        for _ in 0..50 {
            s.push(heard(20, 0.9)).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        assert_eq!(
            drain(&mut s).await,
            vec![
                TranscriptEvent::Final("1460ms".into()),
                TranscriptEvent::Final("1140ms".into()),
            ]
        );
        // Breathing chunk not sent; two chunks, the second pass and its one retry were.
        assert_eq!(fake.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn cuts_where_the_noise_model_hears_no_speech() {
        // Interval 1 s, hard maximum 3 s. Frames of 20 ms, all at the same
        // loudness (a noisy room): only the speech probability counts.
        let fake = Arc::new(Fake::new(|_| 1));
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        let speech = || heard(20, 0.9);
        let quiet = || heard(20, 0.05);
        // 1.2 s of speech, a 100 ms closure inside a word (no cut), a
        // syllable, then a gap of 400 ms: the cut comes 260 ms into it
        // (the first whole frame past 250 ms).
        for _ in 0..60 {
            s.push(speech()).await.unwrap();
        }
        for _ in 0..5 {
            s.push(quiet()).await.unwrap();
        }
        for _ in 0..5 {
            s.push(speech()).await.unwrap();
        }
        for _ in 0..20 {
            s.push(quiet()).await.unwrap();
        }
        // Fluent speech without any gap: the safety net cuts at 3 s.
        for _ in 0..175 {
            s.push(speech()).await.unwrap();
        }
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        assert_eq!(
            drain(&mut s).await,
            vec![
                TranscriptEvent::Final("1660ms".into()),
                TranscriptEvent::Final("3000ms".into()),
                TranscriptEvent::Final("640ms".into()),
            ]
        );
    }

    #[tokio::test]
    async fn cuts_only_at_gaps_between_words() {
        // Frames without a speech probability: the energy detector decides.
        // Interval 1 s, hard maximum 3 s. Frames of 20 ms.
        let fake = Arc::new(Fake::new(|_| 1));
        let mut s = open_session(fake.clone(), &config()).await.unwrap();
        let speech = || frame(20, 0.3);
        let quiet = || frame(20, 0.001);
        // 1.2 s of speech, a 100 ms closure inside a word (no cut), a
        // syllable, then a real gap of 400 ms: the cut comes in that gap.
        for _ in 0..60 {
            s.push(speech()).await.unwrap();
        }
        for _ in 0..5 {
            s.push(quiet()).await.unwrap();
        }
        for _ in 0..5 {
            s.push(speech()).await.unwrap();
        }
        for _ in 0..20 {
            s.push(quiet()).await.unwrap();
        }
        // Fluent speech without any gap: the safety net cuts at 3 s.
        for _ in 0..175 {
            s.push(speech()).await.unwrap();
        }
        // And whatever is left goes at the pause.
        s.cut_segment().await.unwrap();
        s.finish().await.unwrap();
        let ms: Vec<u64> = drain(&mut s)
            .await
            .iter()
            .map(|e| match e {
                TranscriptEvent::Final(t) => t.trim_end_matches("ms").parse().unwrap(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ms.len(), 3, "{ms:?}");
        // First cut inside the gap (1.4 s of speech plus some of the gap),
        // never in the closure, never in the speech after it.
        assert!((1400..=1800).contains(&ms[0]), "{ms:?}");
        assert_eq!(ms[1], 3000, "{ms:?}");
        assert_eq!(ms.iter().sum::<u64>(), 1200 + 100 + 100 + 400 + 3500);
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
