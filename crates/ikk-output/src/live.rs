//! Live typer: the one owner of the target field during a session.
//!
//! Holds the committed segments, a mirror of what is physically typed, and
//! the typing queue that spreads keystrokes evenly. Finals are appended
//! through the queue; a `Replace` (batch pass, correction) is applied with
//! the word-level reconcile, invisibly where the queue has not emitted yet.
//! Before every physical mutation the focus guard is asked; when it says
//! no, the mutation is deferred and retried on the next tick.
//!
//! Interims are deliberately *not* typed (preview only for cloud
//! streams); only committed text reaches the field.

use std::ops::Range;

use std::time::{Duration, Instant};

use crate::guard::{AllowAllGuard, ForegroundGuard};
use crate::queue::{TypingQueue, VisibleEdit};
use crate::reconcile::{compose_segment, reconcile};
use crate::rhythm::{Rhythm, RhythmConfig};
use crate::{OutputError, TextSink};

/// Floor for the flow rate; below this SendInput loses characters.
pub const FLOW_FLOOR_MS: u32 = 5;

pub type SegmentId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub id: SegmentId,
    pub text: String,
    pub span: Range<usize>,
}

pub struct LiveTyper {
    committed: Vec<Segment>,
    typed: String,
    guard: Box<dyn ForegroundGuard>,
    deferred: bool,
    next_id: SegmentId,
    flow: Option<TypingQueue>,
    flow_rate_ms: u32,
    /// Sets the pause before each character of the flow.
    rhythm: Rhythm,
    /// Pause before the next character; zero when text just arrived, so
    /// its first character appears at once.
    next_delay: Duration,
    /// When the next character is due. A fixed point in time, so a writer
    /// loop that wakes for other reasons does not restart the pause.
    due: Instant,
}

impl Default for LiveTyper {
    fn default() -> Self {
        Self::new(Box::new(AllowAllGuard), 0)
    }
}

impl LiveTyper {
    /// `flow_ms` = 0 types each final at once; > 0 drains the queue at that
    /// per-character rate via [`Self::flow_tick`].
    pub fn new(guard: Box<dyn ForegroundGuard>, flow_ms: u32) -> Self {
        Self {
            committed: Vec::new(),
            typed: String::new(),
            guard,
            deferred: false,
            next_id: 1,
            flow: (flow_ms > 0).then(TypingQueue::new),
            flow_rate_ms: if flow_ms > 0 { flow_ms.max(FLOW_FLOOR_MS) } else { 0 },
            rhythm: Rhythm::new(
                RhythmConfig {
                    floor_ms: flow_ms.max(FLOW_FLOOR_MS),
                    ..RhythmConfig::default()
                },
                0x5EED_1234_ABCD_0001,
            ),
            next_delay: Duration::ZERO,
            due: Instant::now(),
        }
    }

    /// Own rhythm (tests, settings).
    pub fn with_rhythm(mut self, rhythm: Rhythm) -> Self {
        self.rhythm = rhythm;
        self
    }

    /// How long the writer should wait before the next [`Self::flow_tick`].
    pub fn next_delay(&self) -> Duration {
        self.next_delay
    }

    /// When the next [`Self::flow_tick`] is due.
    pub fn due(&self) -> Instant {
        self.due
    }

    fn set_delay(&mut self, delay: Duration) {
        self.next_delay = delay;
        self.due = Instant::now() + delay;
    }

    pub fn flow_rate_ms(&self) -> u32 {
        self.flow_rate_ms
    }
    pub fn flow_has_pending(&self) -> bool {
        self.flow.as_ref().is_some_and(|q| q.has_pending())
    }
    pub fn injection_deferred(&self) -> bool {
        self.deferred
    }
    pub fn segments(&self) -> &[Segment] {
        &self.committed
    }
    /// What the field should contain (committed text).
    pub fn text(&self) -> String {
        self.committed
            .iter()
            .fold(String::new(), |acc, s| compose_segment(&acc, &s.text))
    }
    /// What is physically in the field as far as we know.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Commit a final and bring the field up to date (or enqueue it).
    pub fn append(&mut self, text: &str, sink: &mut dyn TextSink) -> Result<SegmentId, OutputError> {
        let id = self.next_id;
        self.next_id += 1;
        self.committed.push(Segment {
            id,
            text: text.to_string(),
            span: 0..0,
        });
        self.reindex();
        // Nothing was waiting: the new text starts without a pause.
        if !self.flow_has_pending() {
            self.set_delay(Duration::ZERO);
        }
        self.drive(sink)?;
        Ok(id)
    }

    /// Replace the whole committed text (batch replacement pass).
    pub fn replace_all(&mut self, text: &str, sink: &mut dyn TextSink) -> Result<(), OutputError> {
        let id = self.next_id;
        self.next_id += 1;
        self.committed = vec![Segment {
            id,
            text: text.to_string(),
            span: 0..0,
        }];
        self.reindex();
        self.drive(sink)
    }

    /// Replace one segment (context correction); unchanged later segments
    /// stay physically untouched thanks to the word-level reconcile.
    pub fn replace_segment(&mut self, id: SegmentId, text: &str, sink: &mut dyn TextSink) -> Result<bool, OutputError> {
        let Some(pos) = self.committed.iter().position(|s| s.id == id) else {
            return Ok(false);
        };
        self.committed[pos].text = text.to_string();
        self.reindex();
        self.drive(sink)?;
        Ok(true)
    }

    /// Delete the last committed word from model and field.
    pub fn delete_last_word(&mut self, sink: &mut dyn TextSink) -> Result<(), OutputError> {
        let current = self.text();
        let trimmed = current.trim_end();
        let cut = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let new_text = trimmed[..cut].trim_end().to_string();
        let id = self.next_id;
        self.next_id += 1;
        self.committed = if new_text.is_empty() {
            Vec::new()
        } else {
            vec![Segment {
                id,
                text: new_text,
                span: 0..0,
            }]
        };
        self.reindex();
        self.drive(sink)
    }

    /// Writer tick: type up to `max_chars` of pending text. Returns chars typed.
    pub fn flow_tick(&mut self, max_chars: usize, sink: &mut dyn TextSink) -> Result<usize, OutputError> {
        if !self.guard.may_inject() {
            self.deferred = true;
            return Ok(0);
        }
        self.deferred = false;
        let Some(queue) = self.flow.as_mut() else {
            return Ok(0);
        };
        let chunk = queue.drain_step(max_chars);
        if chunk.is_empty() {
            return Ok(0);
        }
        let n = chunk.chars().count();
        sink.type_text(&chunk)?;
        self.typed.push_str(&chunk);
        if let (Some(last), Some(queue)) = (chunk.chars().last(), self.flow.as_ref()) {
            let delay = self.rhythm.delay_after(last, queue.pending_len());
            self.set_delay(delay);
        }
        Ok(n)
    }

    /// Type everything pending now (session end).
    pub fn flush(&mut self, sink: &mut dyn TextSink) -> Result<(), OutputError> {
        while self.flow_has_pending() {
            if self.flow_tick(usize::MAX, sink)? == 0 {
                break; // guard blocks; do not spin
            }
        }
        Ok(())
    }

    /// Forget everything (new session). Does not touch the field.
    pub fn reset(&mut self) {
        self.committed.clear();
        self.typed.clear();
        self.deferred = false;
        self.set_delay(Duration::ZERO);
        if let Some(q) = self.flow.as_mut() {
            q.discard();
        }
    }

    fn drive(&mut self, sink: &mut dyn TextSink) -> Result<(), OutputError> {
        if !self.guard.may_inject() {
            self.deferred = true;
            return Err(OutputError::FocusLost);
        }
        self.deferred = false;
        let target = self.text();
        match self.flow.as_mut() {
            None => reconcile(&mut self.typed, &target, sink),
            Some(queue) => match queue.retarget(&target) {
                Some(VisibleEdit { after, .. }) => reconcile(&mut self.typed, &after, sink),
                None => Ok(()),
            },
        }
    }

    fn reindex(&mut self) {
        let mut acc = String::new();
        let mut spans = Vec::with_capacity(self.committed.len());
        for seg in &self.committed {
            acc = compose_segment(&acc, &seg.text);
            let after = acc.chars().count();
            spans.push(after.saturating_sub(seg.text.chars().count())..after);
        }
        for (seg, span) in self.committed.iter_mut().zip(spans) {
            seg.span = span;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::testsink::RecordingSink;

    struct Fixed(bool);
    impl ForegroundGuard for Fixed {
        fn may_inject(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn immediate_mode_types_finals_with_single_space() {
        let mut lt = LiveTyper::default();
        let mut s = RecordingSink::default();
        lt.append("hallo", &mut s).unwrap();
        lt.append("welt.", &mut s).unwrap();
        assert_eq!(s.ops, vec!["type:hallo", "type: welt."]);
        assert_eq!(lt.text(), "hallo welt.");
        assert_eq!(lt.segments()[1].span, 6..11);
    }

    #[test]
    fn flow_mode_drains_evenly_and_corrects_in_queue() {
        let mut lt = LiveTyper::new(Box::new(AllowAllGuard), 10);
        let mut s = RecordingSink::default();
        lt.append("hallo welt", &mut s).unwrap();
        assert!(s.ops.is_empty(), "queued, not typed yet");
        assert_eq!(lt.flow_tick(3, &mut s).unwrap(), 3);
        // Batch replacement arrives: only the emitted "hal" could need a visible edit.
        lt.replace_all("hallo Welt!", &mut s).unwrap();
        assert_eq!(s.ops, vec!["type:hal"], "correction landed invisibly in the queue");
        lt.flush(&mut s).unwrap();
        assert_eq!(lt.typed(), "hallo Welt!");
    }

    #[test]
    fn flow_starts_at_once_then_breathes_and_catches_up() {
        let mut lt = LiveTyper::new(Box::new(AllowAllGuard), 5);
        let mut s = RecordingSink::default();
        lt.append("Hallo Welt. Noch ein Satz", &mut s).unwrap();
        // New text with nothing waiting: the first character comes at once.
        assert_eq!(lt.next_delay(), Duration::ZERO);
        let mut delays = Vec::new();
        while lt.flow_has_pending() {
            lt.flow_tick(1, &mut s).unwrap();
            delays.push(lt.next_delay());
        }
        assert_eq!(lt.typed(), "Hallo Welt. Noch ein Satz");
        // Never below the floor, never a dead stop, and not one fixed pace.
        assert!(delays.iter().all(|d| *d >= Duration::from_millis(5)));
        let distinct: std::collections::HashSet<_> = delays.iter().collect();
        assert!(distinct.len() > 5, "{delays:?}");
        // A long backlog is typed within the budget: 120 characters waiting
        // give at most 10 ms each.
        lt.append(&"x".repeat(121), &mut s).unwrap();
        lt.flow_tick(1, &mut s).unwrap();
        assert!(lt.next_delay() <= Duration::from_millis(10), "{:?}", lt.next_delay());
    }

    #[test]
    fn guard_blocks_and_defers() {
        let mut lt = LiveTyper::new(Box::new(Fixed(false)), 0);
        let mut s = RecordingSink::default();
        assert!(matches!(lt.append("x", &mut s), Err(OutputError::FocusLost)));
        assert!(lt.injection_deferred());
        assert!(s.ops.is_empty());
    }

    #[test]
    fn delete_last_word() {
        let mut lt = LiveTyper::default();
        let mut s = RecordingSink::default();
        lt.append("das ist ein test", &mut s).unwrap();
        lt.delete_last_word(&mut s).unwrap();
        assert_eq!(lt.text(), "das ist ein");
        assert_eq!(s.ops.last().unwrap(), "bs:5");
    }
}
