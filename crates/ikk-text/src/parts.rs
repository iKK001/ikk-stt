//! Word parts written apart. A speaker on a ventilator has to stop for
//! breath wherever the breath falls, also in the middle of a word, and a
//! recogniser then writes the halves as two words ("Kran ken haus",
//! "Tele- fon"). Here such parts are put together again — carefully: only
//! when the whole is a word of the dictionary and at least one part is
//! not a word on its own (or the recogniser itself marked the break with
//! a hyphen or dots). Two real words that could also be one ("zu Hause",
//! "so wie") are left alone; only the context can decide those.

/// A token split into its letters and what follows them (punctuation).
fn core(token: &str) -> Option<(&str, &str)> {
    let end = token
        .char_indices()
        .find(|(_, c)| !c.is_alphabetic())
        .map_or(token.len(), |(i, _)| i);
    let (word, rest) = token.split_at(end);
    if word.is_empty() || rest.chars().any(|c| c.is_alphanumeric()) {
        return None;
    }
    Some((word, rest))
}

/// Whether the rest of a token only says "the word broke off here".
fn break_mark(rest: &str) -> bool {
    !rest.is_empty() && rest.chars().all(|c| matches!(c, '-' | '–' | '…' | '.')) && rest != "."
}

fn lower_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `left` and `right` as one word when they are its parts.
fn joined(left: &str, right: &str, is_word: &dyn Fn(&str) -> bool) -> Option<String> {
    let (a, a_rest) = core(left)?;
    let (b, b_rest) = core(right)?;
    let marked = break_mark(a_rest);
    if !a_rest.is_empty() && !marked {
        return None;
    }
    if b.chars().count() < 2 {
        return None;
    }
    let whole = format!("{a}{}", b.to_lowercase());
    if whole.chars().count() < 4 || !(is_word(&whole) || is_word(&lower_first(&whole))) {
        return None;
    }
    if !marked && is_word(a) && is_word(b) {
        return None;
    }
    Some(format!("{whole}{b_rest}"))
}

/// Joins the parts of words in a row of tokens. Returns each resulting
/// token with the number of input tokens it was made of.
pub fn join_parts(tokens: &[&str], is_word: &dyn Fn(&str) -> bool) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let mut word = tokens[i].to_string();
        let mut used = 1;
        loop {
            let next = i + used;
            if next >= tokens.len() {
                break;
            }
            if let Some(w) = joined(&word, tokens[next], is_word) {
                word = w;
                used += 1;
                continue;
            }
            // Three parts of which no two make a word ("Te le fon").
            if next + 1 < tokens.len() {
                if let (Some((a, "")), Some((b, ""))) = (core(&word), core(tokens[next])) {
                    let two = format!("{a}{}", b.to_lowercase());
                    if !is_word(&two) {
                        if let Some(w) = joined(&two, tokens[next + 1], is_word) {
                            word = w;
                            used += 2;
                            continue;
                        }
                    }
                }
            }
            break;
        }
        out.push((word, used));
        i += used;
    }
    out
}

/// [`join_parts`] over a text; the spaces of joined parts go, every
/// other character stays.
pub fn join_word_parts(text: &str, is_word: &dyn Fn(&str) -> bool) -> String {
    text.split('\n')
        .map(|line| {
            let tokens: Vec<&str> = line.split(' ').collect();
            join_parts(&tokens, is_word)
                .into_iter()
                .map(|(w, _)| w)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(w: &str) -> bool {
        // Case as a spelling dictionary sees it: nouns only capitalised,
        // other words in both forms.
        const NOUNS: [&str; 6] = ["Krankenhaus", "Kran", "Telefon", "Haus", "Beatmung", "Hause"];
        const WORDS: [&str; 8] = ["kranken", "gemacht", "macht", "zu", "zuhause", "das", "ist", "im"];
        NOUNS.contains(&w) || WORDS.contains(&w) || WORDS.contains(&lower_first(w).as_str())
    }

    #[test]
    fn parts_of_a_word_are_joined() {
        assert_eq!(join_word_parts("im Kran ken haus", &dict), "im Krankenhaus");
        assert_eq!(join_word_parts("das ist ge macht.", &dict), "das ist gemacht.");
        assert_eq!(join_word_parts("das Te le fon", &dict), "das Telefon");
        assert_eq!(join_word_parts("Beat mung", &dict), "Beatmung");
    }

    #[test]
    fn a_marked_break_is_joined_even_between_words() {
        assert_eq!(join_word_parts("zu- Hause", &dict), "zuhause");
        assert_eq!(join_word_parts("Kranken… Haus,", &dict), "Krankenhaus,");
    }

    #[test]
    fn real_words_stay_apart() {
        assert_eq!(join_word_parts("zu Hause", &dict), "zu Hause");
        assert_eq!(join_word_parts("das ist. Macht", &dict), "das ist. Macht");
        assert_eq!(join_word_parts("das Haus", &dict), "das Haus");
        assert_eq!(
            join_word_parts("Zeile eins\nzwei  drei", &dict),
            "Zeile eins\nzwei  drei"
        );
    }

    #[test]
    fn counts_say_what_was_joined() {
        let got = join_parts(&["im", "Kran", "ken", "haus"], &dict);
        assert_eq!(got, vec![("im".to_string(), 1), ("Krankenhaus".to_string(), 3)]);
    }
}
