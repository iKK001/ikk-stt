//! Voice commands spoken inside dictation ("new line", "delete that").
//! Scaffold: the intent enum only.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Intent {
    NewLine,
    NewParagraph,
    DeleteLastWord,
    DeleteLastSentence,
    Undo,
    StopDictation,
}

/// Returns the intent if `text` is exactly a command phrase, else `None`.
pub fn match_command(text: &str, locale: &str) -> Option<Intent> {
    let t = text.trim().to_lowercase();
    match (locale, t.as_str()) {
        ("de" | "de-CH", "neue zeile") | ("en", "new line") => Some(Intent::NewLine),
        ("de" | "de-CH", "neuer absatz") | ("en", "new paragraph") => Some(Intent::NewParagraph),
        ("de" | "de-CH", "diktat stopp") | ("en", "stop dictation") => Some(Intent::StopDictation),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn german_new_line() {
        assert_eq!(match_command("Neue Zeile", "de-CH"), Some(Intent::NewLine));
        assert_eq!(match_command("Neue Zeile bitte", "de-CH"), None);
    }
}
