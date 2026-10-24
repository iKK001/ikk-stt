//! Typing queue: even writing flow plus invisible in-queue correction.
//!
//! Without a queue every final is typed as a burst followed by silence,
//! which feels jerky. The queue holds committed text in two parts:
//! `emitted` (already physically typed) and `pending` (waiting). A writer
//! drains it at a constant rate. A correction that arrives before the
//! writer reached the affected text is applied inside `pending` with zero
//! visible backspaces; only already emitted text needs a [`VisibleEdit`].

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleEdit {
    pub before: String,
    pub after: String,
}

#[derive(Debug, Default, Clone)]
pub struct TypingQueue {
    emitted: String,
    pending: String,
}

impl TypingQueue {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn target(&self) -> String {
        format!("{}{}", self.emitted, self.pending)
    }
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub fn pending_len(&self) -> usize {
        self.pending.chars().count()
    }
    pub fn emitted(&self) -> &str {
        &self.emitted
    }
    pub fn enqueue(&mut self, added: &str) {
        self.pending.push_str(added);
    }

    /// Take up to `max_chars` from pending into emitted; returns them.
    pub fn drain_step(&mut self, max_chars: usize) -> String {
        if max_chars == 0 || self.pending.is_empty() {
            return String::new();
        }
        let take = max_chars.min(self.pending.chars().count());
        let idx = self
            .pending
            .char_indices()
            .nth(take)
            .map(|(i, _)| i)
            .unwrap_or(self.pending.len());
        let chunk = self.pending[..idx].to_string();
        self.pending = self.pending[idx..].to_string();
        self.emitted.push_str(&chunk);
        chunk
    }

    /// New full target text. Returns a visible edit only when already
    /// emitted text must change.
    pub fn retarget(&mut self, new_target: &str) -> Option<VisibleEdit> {
        let new_chars: Vec<char> = new_target.chars().collect();
        let emitted_chars: Vec<char> = self.emitted.chars().collect();
        let mut common = 0;
        while common < emitted_chars.len() && common < new_chars.len() && emitted_chars[common] == new_chars[common] {
            common += 1;
        }
        if common == emitted_chars.len() {
            self.pending = new_chars[common..].iter().collect();
            return None;
        }
        let before = self.emitted.clone();
        let keep = emitted_chars.len().min(new_chars.len());
        self.emitted = new_chars[..keep].iter().collect();
        self.pending = new_chars[keep..].iter().collect();
        Some(VisibleEdit {
            before,
            after: self.emitted.clone(),
        })
    }

    pub fn discard(&mut self) {
        self.emitted.clear();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_evenly_and_corrects_invisibly() {
        let mut q = TypingQueue::new();
        q.enqueue("hallo welt");
        assert_eq!(q.drain_step(3), "hal");
        assert_eq!(q.emitted(), "hal");
        // Correction inside pending: no visible edit.
        assert_eq!(q.retarget("hallo Welt!"), None);
        assert_eq!(q.target(), "hallo Welt!");
        // Correction of emitted text: visible edit of the emitted part only.
        let e = q.retarget("Hallo Welt!").unwrap();
        assert_eq!(
            e,
            VisibleEdit {
                before: "hal".into(),
                after: "Hal".into()
            }
        );
        assert_eq!(q.pending_len(), 8);
    }
}
