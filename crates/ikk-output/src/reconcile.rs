//! Word-level reconcile: turn what is physically typed into the new target
//! with the fewest visible keystrokes. Common (word-snapped) prefix and
//! suffix stay; only the middle is retyped. The suffix is skipped by caret
//! movement instead of deleted, so later unchanged text is never destroyed
//! ("backspace storm"). Sinks without caret movement fall back to
//! backspace-and-retype of the suffix.

use crate::{OutputError, TextSink};

/// Join a committed text and a new segment with exactly one space.
pub fn compose_segment(committed: &str, seg: &str) -> String {
    if committed.is_empty() {
        seg.to_string()
    } else if committed.ends_with(' ') || seg.is_empty() {
        format!("{committed}{seg}")
    } else {
        format!("{committed} {seg}")
    }
}

fn is_word_boundary(chars: &[char], i: usize) -> bool {
    i == 0 || i == chars.len() || chars[i - 1] == ' ' || chars[i] == ' '
}

/// The keystroke plan for one reconcile, in char units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Caret steps left before deleting (suffix to keep).
    pub skip_left: usize,
    pub backspaces: usize,
    pub insert: String,
    /// Caret steps right afterwards (= skip_left).
    pub skip_right: usize,
}

/// Compute the plan without touching any sink.
pub fn plan(typed: &str, target: &str) -> Plan {
    let o: Vec<char> = typed.chars().collect();
    let n: Vec<char> = target.chars().collect();
    let min_len = o.len().min(n.len());
    let mut p = 0;
    while p < min_len && o[p] == n[p] {
        p += 1;
    }
    let mut s = 0;
    while s < min_len - p && o[o.len() - 1 - s] == n[n.len() - 1 - s] {
        s += 1;
    }
    if s > 0 {
        while p > 0 && !(is_word_boundary(&o, p) && is_word_boundary(&n, p)) {
            p -= 1;
        }
    }
    while s > 0 && !(is_word_boundary(&o, o.len() - s) && is_word_boundary(&n, n.len() - s)) {
        s -= 1;
    }
    let old_middle = o.len() - s - p;
    let insert: String = n[p..n.len() - s].iter().collect();
    Plan {
        skip_left: s,
        backspaces: old_middle,
        insert,
        skip_right: s,
    }
}

/// Apply `plan(typed, target)` on the sink and update `typed`.
pub fn reconcile(typed: &mut String, target: &str, sink: &mut dyn TextSink) -> Result<(), OutputError> {
    let mut pl = plan(typed, target);
    if pl.skip_left > 0 && sink.cursor_left(pl.skip_left).is_err() {
        // No caret movement: delete the suffix too and retype it.
        let suffix: String = target
            .chars()
            .rev()
            .take(pl.skip_left)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        pl.backspaces += pl.skip_left;
        pl.insert.push_str(&suffix);
        pl.skip_left = 0;
        pl.skip_right = 0;
    }
    if pl.backspaces > 0 {
        sink.backspace(pl.backspaces)?;
    }
    if !pl.insert.is_empty() {
        sink.type_text(&pl.insert)?;
    }
    if pl.skip_right > 0 {
        sink.cursor_right(pl.skip_right)?;
    }
    *typed = target.to_string();
    Ok(())
}

#[cfg(test)]
pub(crate) mod testsink {
    use crate::*;

    /// Records every keystroke-level operation.
    #[derive(Default)]
    pub struct RecordingSink {
        pub ops: Vec<String>,
        pub no_cursor: bool,
    }
    impl TextSink for RecordingSink {
        fn access_level(&self) -> AccessLevel {
            AccessLevel::BasicTyping
        }
        fn probe(&mut self) -> TargetStatus {
            TargetStatus::Injectable(AccessLevel::BasicTyping)
        }
        fn type_text(&mut self, t: &str) -> Result<(), OutputError> {
            self.ops.push(format!("type:{t}"));
            Ok(())
        }
        fn backspace(&mut self, n: usize) -> Result<(), OutputError> {
            self.ops.push(format!("bs:{n}"));
            Ok(())
        }
        fn cursor_left(&mut self, n: usize) -> Result<(), OutputError> {
            if self.no_cursor {
                return Err(OutputError::Unavailable);
            }
            self.ops.push(format!("left:{n}"));
            Ok(())
        }
        fn cursor_right(&mut self, n: usize) -> Result<(), OutputError> {
            self.ops.push(format!("right:{n}"));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testsink::RecordingSink;
    use super::*;

    #[test]
    fn compose_spacing() {
        assert_eq!(compose_segment("", "hallo"), "hallo");
        assert_eq!(compose_segment("hallo", "welt"), "hallo welt");
        assert_eq!(compose_segment("hallo ", "welt"), "hallo welt");
        assert_eq!(compose_segment("hallo", ""), "hallo");
    }

    #[test]
    fn append_is_pure_typing() {
        let mut typed = "hallo".to_string();
        let mut s = RecordingSink::default();
        reconcile(&mut typed, "hallo welt", &mut s).unwrap();
        assert_eq!(s.ops, vec!["type: welt"]);
        assert_eq!(typed, "hallo welt");
    }

    #[test]
    fn early_word_change_skips_suffix_by_caret() {
        let mut typed = "das ist ein test".to_string();
        let mut s = RecordingSink::default();
        reconcile(&mut typed, "Das ist ein test", &mut s).unwrap();
        // Suffix " ist ein test" (13 chars) is skipped, "das" deleted, "Das" typed.
        assert_eq!(s.ops, vec!["left:13", "bs:3", "type:Das", "right:13"]);
    }

    #[test]
    fn without_caret_movement_suffix_is_retyped() {
        let mut typed = "das ist".to_string();
        let mut s = RecordingSink {
            no_cursor: true,
            ..Default::default()
        };
        reconcile(&mut typed, "Das ist", &mut s).unwrap();
        assert_eq!(s.ops, vec!["bs:7", "type:Das ist"]);
    }

    #[test]
    fn growing_last_word_is_append_only() {
        let mut typed = "hallo we".to_string();
        let mut s = RecordingSink::default();
        reconcile(&mut typed, "hallo welt", &mut s).unwrap();
        assert_eq!(s.ops, vec!["type:lt"]);
    }
}
