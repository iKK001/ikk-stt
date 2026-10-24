//! Platform-neutral core of keystroke injection: chunk planning and
//! character-loss check. Windows drops SendInput events in large batches;
//! Unicode must never be split inside a code point.

/// Max characters per injection batch (Windows documents loss above ~500).
pub const MAX_CHUNK_CHARS: usize = 256;

/// Below this per-character delay Windows SendInput corrupted the first
/// character reproducibly (measured 2026-09-18).
pub const MIN_SAFE_CHAR_DELAY_MS: u32 = 5;

pub fn chunk_plan(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(MAX_CHUNK_CHARS).map(|c| c.iter().collect()).collect()
}

/// `true` = typewriter effect (per-char pause), `false` = burst.
pub fn is_typewriter(delay_ms: u32) -> bool {
    delay_ms > 1
}

/// `Some(diff)` when written ≠ expected (lost or duplicated chars).
pub fn char_loss(expected: usize, written: usize) -> Option<isize> {
    (expected != written).then(|| written as isize - expected as isize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_at_boundary_and_keeps_codepoints() {
        assert!(chunk_plan("").is_empty());
        assert_eq!(chunk_plan("kurz"), vec!["kurz"]);
        let text = "ä".repeat(260);
        let plan = chunk_plan(&text);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].chars().count(), 256);
        assert_eq!(plan.concat(), text);
    }

    #[test]
    fn typewriter_threshold_and_loss() {
        assert!(!is_typewriter(1) && is_typewriter(2));
        assert_eq!(char_loss(10, 10), None);
        assert_eq!(char_loss(10, 7), Some(-3));
    }
}
