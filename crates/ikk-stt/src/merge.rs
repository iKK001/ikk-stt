//! Text merging helpers for sessions that combine a live stream with a
//! higher-quality batch pass.

/// Words the batch transcript adds *after* what the stream already
/// committed. Returns empty when the two disagree too much to trust
/// (less than half the committed words match) or when batch adds nothing.
pub fn trailing_suffix(committed: &str, batch: &str) -> String {
    let committed_words: Vec<&str> = committed.split_whitespace().collect();
    let batch_words: Vec<&str> = batch.split_whitespace().collect();
    if committed_words.is_empty() || batch_words.len() <= committed_words.len() {
        return String::new();
    }
    let mut matched = 0;
    for (c, b) in committed_words.iter().zip(&batch_words) {
        if normalize(c) == normalize(b) {
            matched += 1;
        } else {
            break;
        }
    }
    if matched * 2 < committed_words.len() || matched >= batch_words.len() {
        return String::new();
    }
    batch_words[matched..].join(" ")
}

fn normalize(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Longest common prefix in chars; used by typing reconcile.
pub fn common_prefix_chars(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_when_batch_extends_stream() {
        assert_eq!(
            trailing_suffix("hallo das ist ein", "Hallo, das ist ein Test."),
            "Test."
        );
    }

    #[test]
    fn no_suffix_when_batch_shorter_or_diverges() {
        assert_eq!(trailing_suffix("hallo das ist ein test", "hallo das"), "");
        assert_eq!(
            trailing_suffix("eins zwei drei vier", "fünf sechs sieben acht neun"),
            ""
        );
        assert_eq!(trailing_suffix("", "etwas"), "");
    }

    #[test]
    fn prefix_len() {
        assert_eq!(common_prefix_chars("hallo welt", "hallo wald"), 7);
    }
}
