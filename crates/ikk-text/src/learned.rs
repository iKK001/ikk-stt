//! Learned words — the training mode. A word the recogniser never produces
//! for this speaker is entered once and said a few times; what the
//! recogniser returns for those takes is kept as its *heard forms*. From
//! then on a heard form in a result is swapped for the word, on the device
//! (plain text matching, unlimited), and for providers that take hints the
//! few learned words that resemble what was just heard go along with the
//! request.
//!
//! Telling a heard form from a genuine word (Stephan's rule, 2026-11-01: a
//! learned word must never block a genuine similar word): a heard form,
//! whether one word or several, is swapped only when the recogniser itself
//! was unsure about that spot — the readings of that moment are not
//! unanimous. The one exception is an entry marked `always`, for words
//! whose mishearings are never real words; the user sets that per word.

use serde::{Deserialize, Serialize};

/// One trained word with what the recogniser produces for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedWord {
    /// The word (or short phrase) as it must be written.
    pub word: String,
    /// What came back for it: one entry per distinct result.
    pub heard: Vec<String>,
    /// Swap its heard forms even when every reading agrees on them: only
    /// for a word whose mishearings are never real words.
    #[serde(default)]
    pub always: bool,
}

/// Letters and digits only, lower case.
pub fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// A place in a word list where a heard form stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swap {
    /// Index of the first word of the heard form.
    pub at: usize,
    /// Words it spans.
    pub len: usize,
    /// The word to write instead.
    pub word: String,
    /// Certain: an `always` entry; otherwise the readings decide.
    pub certain: bool,
}

/// Tokens that are the same word in a different hearing: equal after
/// normalisation, or (six letters or more) one letter off. Nothing looser:
/// a shared beginning made *Bildschirm* count as *Bildschirmpassers*.
fn same_token(a: &str, b: &str) -> bool {
    let (na, nb) = (a.chars().count(), b.chars().count());
    a == b || (na >= 6 && nb >= 6 && distance(a, b) <= 1)
}

/// Every heard form standing in `words` (longest first at each place,
/// without overlaps). `words` are tokens as the recogniser gave them;
/// punctuation around them does not matter, nor one letter in a long one.
pub fn find_swaps(learned: &[LearnedWord], words: &[String]) -> Vec<Swap> {
    let normed: Vec<String> = words.iter().map(|w| norm(w)).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < normed.len() {
        let mut best: Option<Swap> = None;
        for l in learned {
            for form in &l.heard {
                let f: Vec<String> = form.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect();
                if f.is_empty() || i + f.len() > normed.len() || !same_token(&normed[i], &f[0]) {
                    continue;
                }
                let all = normed[i..i + f.len()].iter().zip(&f).all(|(a, b)| same_token(a, b));
                if all && best.as_ref().is_none_or(|b| f.len() > b.len) {
                    best = Some(Swap {
                        at: i,
                        len: f.len(),
                        word: l.word.clone(),
                        certain: l.always,
                    });
                }
            }
        }
        match best {
            Some(s) => {
                i += s.len;
                out.push(s);
            }
            None => i += 1,
        }
    }
    out
}

/// The text with its heard forms swapped. `unanimous`: the readings agree
/// on this text, so only `always` entries apply (see the module docs); a
/// text from one reading alone counts as unanimous too. Punctuation after
/// the form stays with the word.
pub fn apply_text(learned: &[LearnedWord], text: &str, unanimous: bool) -> String {
    if learned.is_empty() {
        return text.to_string();
    }
    let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    let swaps = find_swaps(learned, &words);
    if swaps.is_empty() {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    for s in swaps {
        while i < s.at {
            out.push(words[i].clone());
            i += 1;
        }
        if s.certain || !unanimous {
            let last = &words[s.at + s.len - 1];
            let tail: String = last
                .chars()
                .rev()
                .take_while(|c| !c.is_alphanumeric())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            out.push(format!("{}{tail}", s.word));
        } else {
            out.extend(words[s.at..s.at + s.len].iter().cloned());
        }
        i = s.at + s.len;
    }
    out.extend(words[i..].iter().cloned());
    out.join(" ")
}

/// Edit distance on characters.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Two normalised strings that are the same word in a different hearing:
/// equal, or a quarter of the letters off at most (one letter in a short
/// word, four in "bildschirmpasstdas").
fn resembles(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let allowed = (a.chars().count().max(b.chars().count()) / 4).max(1);
    a == b || distance(a, b) <= allowed
}

/// The learned words whose heard forms (or the word itself) resemble some
/// stretch of `heard` — the hints worth sending with the next request.
/// Chosen per thought from however long a list; at most `max`.
pub fn similar_hints(learned: &[LearnedWord], heard: &str, max: usize) -> Vec<String> {
    let words: Vec<String> = heard.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect();
    if words.is_empty() || max == 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for l in learned {
        let mut forms: Vec<Vec<String>> = l
            .heard
            .iter()
            .map(|f| f.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect())
            .collect();
        forms.push(l.word.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect());
        // The heard text may split or join the form differently: compare
        // it with stretches of one to n+1 words.
        let hit = forms.iter().filter(|f| !f.is_empty()).any(|f| {
            let joined = f.concat();
            (1..=f.len() + 1).any(|n| words.windows(n).any(|w| resembles(&w.concat(), &joined)))
        });
        if hit && !out.contains(&l.word) {
            out.push(l.word.clone());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lw(word: &str, heard: &[&str], always: bool) -> LearnedWord {
        LearnedWord {
            word: word.into(),
            heard: heard.iter().map(|s| s.to_string()).collect(),
            always,
        }
    }

    #[test]
    fn a_form_of_several_words_is_swapped_with_its_punctuation() {
        let l = [lw(
            "Bildschirmtastatur",
            &["Bildschirm passt das", "Bildschirmpassers"],
            false,
        )];
        assert_eq!(
            apply_text(&l, "Lösung. Der Bildschirm passt das.", false),
            "Lösung. Der Bildschirmtastatur."
        );
        // Heard the same by every reading: what was said.
        assert_eq!(
            apply_text(&l, "Lösung. Der Bildschirm passt das.", true),
            "Lösung. Der Bildschirm passt das."
        );
    }

    #[test]
    fn a_single_word_form_needs_disagreement_or_always() {
        let l = [lw("Maccaud", &["Marco"], false)];
        assert_eq!(
            apply_text(&l, "Hallo Marco, wie geht es?", true),
            "Hallo Marco, wie geht es?"
        );
        assert_eq!(
            apply_text(&l, "Hallo Marco, wie geht es?", false),
            "Hallo Maccaud, wie geht es?"
        );
        let a = [lw("Maccaud", &["Marco"], true)];
        assert_eq!(
            apply_text(&a, "Hallo Marco, wie geht es?", true),
            "Hallo Maccaud, wie geht es?"
        );
    }

    #[test]
    fn a_genuine_word_sharing_the_beginning_is_not_a_heard_form() {
        let l = [lw(
            "Bildschirmtastatur",
            &["Bildschirmpassers", "Bildschirm passt das"],
            false,
        )];
        let words: Vec<String> = "der Bildschirm ist gross".split(' ').map(String::from).collect();
        assert!(find_swaps(&l, &words).is_empty());
        let words: Vec<String> = "der Bildschirmpasser ist".split(' ').map(String::from).collect();
        assert_eq!(find_swaps(&l, &words).len(), 1);
    }

    #[test]
    fn swaps_are_found_longest_first_without_overlap() {
        let l = [lw("Bildschirmtastatur", &["Bildschirm passt das", "Bildschirm"], false)];
        let words: Vec<String> = "der Bildschirm passt das gut".split(' ').map(String::from).collect();
        let s = find_swaps(&l, &words);
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].at, s[0].len, s[0].certain), (1, 3, false));
    }

    #[test]
    fn hints_are_the_learned_words_that_resemble_what_was_heard() {
        let l = [
            lw("Bildschirmtastatur", &["Bildschirm passt das"], false),
            lw("Maccaud", &["Marco"], false),
            lw("Korner", &["Kerner", "Körner"], false),
        ];
        let h = similar_hints(&l, "mit meiner Lösung der Bildschirm Pasta", 10);
        assert_eq!(h, vec!["Bildschirmtastatur".to_string()]);
        let h = similar_hints(&l, "Herr Koerner und Marko", 10);
        assert_eq!(h, vec!["Maccaud".to_string(), "Korner".to_string()]);
        assert!(similar_hints(&l, "Ich habe so viel zu schreiben", 10).is_empty());
        assert_eq!(similar_hints(&l, "Marko Kerner", 1).len(), 1);
    }
}
