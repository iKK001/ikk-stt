//! Carrying out voice commands ([`ikk_commands::Intent`]) on the target
//! text. The engine knows what it typed (`session_text`, the dictated
//! pieces) and where its caret is as long as only its own commands moved
//! it; after a move it cannot follow (a line up, a page down) the caret is
//! "unknown" and the commands that need the typed history refuse.
//!
//! Keys go through [`ikk_output::TextSink::press`]; sinks without it
//! (mobile hosts for now) answer `Unavailable` and the command is reported
//! as not carried out. The list of commands and their state per platform
//! is `Cmd.md` at the repository root.

use ikk_commands::Intent;
use ikk_output::{Chord, Key, Modifiers, OutputError};
use tracing::{info, warn};

use crate::{Engine, EngineEvent};

/// What the engine remembers about the dictation for the commands.
#[derive(Debug, Default)]
pub(crate) struct Dictation {
    /// The dictated pieces as typed, with their leading space; the last
    /// one is "das" in "Streich das".
    pub pieces: Vec<String>,
    /// Characters between the caret and the end of the typed text, when
    /// the engine still knows (`None` after a move it cannot follow).
    pub caret_from_end: Option<usize>,
    /// Characters selected, ending at the caret.
    pub selection: usize,
    /// "Großschaltung": dictated text goes out in upper case.
    pub caps: bool,
    /// "Buchstabiermodus": every final is spelled.
    pub spell_mode: bool,
    /// "Schreib das": only the next final is spelled.
    pub spell_next: bool,
}

/// Where spoken words stand in the typed text: `(chars_after, len)` —
/// characters from the end of the match to the end of the text, and the
/// match's length. Compared word by word without case and punctuation,
/// the last occurrence wins.
pub(crate) fn locate(text: &str, spoken: &str) -> Option<(usize, usize)> {
    let want: Vec<String> = spoken
        .split_whitespace()
        .map(norm_word)
        .filter(|w| !w.is_empty())
        .collect();
    if want.is_empty() {
        return None;
    }
    // Tokens with their char offsets.
    let chars: Vec<char> = text.chars().collect();
    let mut tokens: Vec<(usize, usize, String)> = Vec::new(); // (start, end, norm)
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        tokens.push((start, i, norm_word(&raw)));
    }
    if tokens.len() < want.len() {
        return None;
    }
    for k in (0..=tokens.len() - want.len()).rev() {
        if tokens[k..k + want.len()].iter().zip(&want).all(|(t, w)| t.2 == *w) {
            let start = tokens[k].0;
            // Trailing punctuation belongs to the match ("Lösung." for "Lösung").
            let end = tokens[k + want.len() - 1].1;
            return Some((chars.len() - end, end - start));
        }
    }
    None
}

fn norm_word(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Characters of the last `n` words of `text` (with the spaces before
/// them); all of it when there are fewer words.
pub(crate) fn last_words_len(text: &str, n: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut i = chars.len();
    let mut words = 0;
    while words < n && i > 0 {
        // The word, then the spaces before it (they belong to the piece).
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        words += 1;
    }
    chars.len() - i
}

impl Engine {
    /// A final to type while a command state is on: caps, or spelled.
    pub(crate) fn shape_final(&mut self, processed: &str) -> Option<String> {
        if self.dictation.spell_mode || self.dictation.spell_next {
            self.dictation.spell_next = false;
            return ikk_commands::spell(processed).or_else(|| Some(processed.to_string()));
        }
        if self.dictation.caps {
            return Some(processed.to_uppercase());
        }
        Some(processed.to_string())
    }

    /// Typed text went out: remember it as a piece; a selection was replaced.
    pub(crate) fn note_piece(&mut self, with_space: &str) {
        if self.dictation.selection > 0 {
            // The target replaced the selection with the new text; mirror
            // that in the typed history.
            let sel = self.dictation.selection;
            let from_end = self.dictation.caret_from_end.unwrap_or(0);
            let total = self.session_text.chars().count();
            let typed = with_space.chars().count();
            // session_text already has the new text appended at its end by
            // emit_text; move it to where the selection was.
            if let Some(after) = self.dictation.caret_from_end {
                let chars: Vec<char> = self.session_text.chars().collect();
                let new_start = total - typed;
                let sel_end = new_start - after;
                let sel_start = sel_end.saturating_sub(sel);
                let mut out: String = chars[..sel_start].iter().collect();
                out.extend(chars[new_start..].iter());
                out.extend(chars[sel_end..new_start].iter());
                self.session_text = out;
                self.dictation.caret_from_end = Some(after);
            }
            let _ = from_end;
            self.dictation.selection = 0;
            self.dictation.pieces.push(with_space.to_string());
            return;
        }
        self.dictation.pieces.push(with_space.to_string());
    }

    fn press(&mut self, chord: Chord, n: usize) -> bool {
        match self.sink.press(chord, n) {
            Ok(()) => true,
            Err(OutputError::Unavailable) => {
                warn!("command needs key presses this target does not take");
                false
            }
            Err(e) => {
                warn!(%e, "key press failed");
                false
            }
        }
    }

    fn caret_at_end(&self) -> bool {
        self.dictation.caret_from_end == Some(0) && self.dictation.selection == 0
    }

    /// Delete the last `n` characters of the typed history (caret at end).
    fn delete_tail(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if self.output_mode == crate::OutputMode::Inject {
            if let Err(e) = self.live.delete_tail(n, &mut *self.sink) {
                warn!(%e, "deleting failed");
            }
        }
        let keep = self.session_text.chars().count().saturating_sub(n);
        let cut: String = self.session_text.chars().take(keep).collect();
        self.session_text = cut;
    }

    /// Select `len` characters that end `after` characters before the end
    /// of the typed text (caret moves there first).
    fn select_back(&mut self, after: usize, len: usize) -> bool {
        if after > 0 && !self.press(Chord::key(Key::Left), after) {
            return false;
        }
        if !self.press(Chord::shift(Key::Left), len) {
            return false;
        }
        self.dictation.caret_from_end = Some(after);
        self.dictation.selection = len;
        true
    }

    fn unknown_caret(&mut self) {
        self.dictation.caret_from_end = None;
        self.dictation.selection = 0;
    }

    async fn refuse(&mut self, intent: &Intent, why: &str) {
        warn!(?intent, why, "command not carried out");
        let _ = self.events.send(EngineEvent::CommandRefused(why.to_string())).await;
    }

    pub(crate) async fn execute(&mut self, intent: Intent) {
        use Intent::*;
        info!(?intent, "command");
        let mac = cfg!(target_os = "macos");
        match intent.clone() {
            StopDictation => self.stop().await,
            NewLine => {
                let _ = self.emit_text("\n").await;
                self.dictation.pieces.push("\n".into());
            }
            NewParagraph => {
                let _ = self.emit_text("\n\n").await;
                self.dictation.pieces.push("\n\n".into());
            }
            PressEnter => {
                self.press(Chord::key(Key::Return), 1);
                self.unknown_caret();
            }
            PressTab => {
                self.press(Chord::key(Key::Tab), 1);
                self.unknown_caret();
            }
            DeleteThat | DeleteThatTimes(_) => {
                let n = if let DeleteThatTimes(n) = intent { n as usize } else { 1 };
                if !self.caret_at_end() {
                    self.refuse(&intent, "the caret was moved; say what to delete").await;
                    return;
                }
                let mut chars = 0;
                for _ in 0..n {
                    match self.dictation.pieces.pop() {
                        Some(p) => chars += p.chars().count(),
                        None => break,
                    }
                }
                self.delete_tail(chars);
            }
            DeleteLastWords(n) => {
                if !self.caret_at_end() {
                    self.refuse(&intent, "the caret was moved").await;
                    return;
                }
                let chars = last_words_len(&self.session_text, n as usize);
                self.delete_tail(chars);
                self.dictation.pieces.clear();
            }
            DeleteLine => {
                if self.press(Chord::key(Key::Home), 1) && self.press(Chord::shift(Key::End), 1) {
                    self.press(Chord::key(Key::Backspace), 1);
                }
                self.unknown_caret();
            }
            Backspace(n) => {
                if self.caret_at_end() {
                    self.delete_tail(n as usize);
                } else {
                    self.press(Chord::key(Key::Backspace), n as usize);
                }
            }
            Undo => {
                self.press(Chord::shortcut('z'), 1);
                self.unknown_caret();
            }
            Cut => {
                self.press(Chord::shortcut('x'), 1);
                self.unknown_caret();
            }
            Copy => {
                self.press(Chord::shortcut('c'), 1);
            }
            Paste => {
                self.press(Chord::shortcut('v'), 1);
                self.unknown_caret();
            }
            SelectAll => {
                self.press(Chord::shortcut('a'), 1);
                self.unknown_caret();
            }
            SelectThat | CorrectThat => {
                if !self.caret_at_end() {
                    self.refuse(&intent, "the caret was moved; say the words to select")
                        .await;
                    return;
                }
                let Some(last) = self.dictation.pieces.last().cloned() else {
                    self.refuse(&intent, "nothing dictated yet").await;
                    return;
                };
                let len = last.trim_start().chars().count();
                self.select_back(0, len);
            }
            SelectWords(words) | CorrectWords(words) => {
                let Some(caret) = self.dictation.caret_from_end else {
                    self.refuse(&intent, "the caret was moved out of the typed text").await;
                    return;
                };
                let _ = caret;
                match locate(&self.session_text, &words) {
                    Some((after, len)) => {
                        // From the caret's place to the match: the caret may
                        // sit anywhere in the typed text.
                        let here = self.dictation.caret_from_end.unwrap_or(0);
                        if self.dictation.selection > 0 {
                            self.press(Chord::key(Key::Right), 1);
                            self.dictation.selection = 0;
                        }
                        if after >= here {
                            self.select_back(after - here, len);
                            self.dictation.caret_from_end = Some(after);
                        } else {
                            let right = here - after;
                            if self.press(Chord::key(Key::Right), right) {
                                self.dictation.caret_from_end = Some(after);
                                self.select_back(0, len);
                                self.dictation.caret_from_end = Some(after);
                            }
                        }
                    }
                    None => self.refuse(&intent, "those words are not in the dictated text").await,
                }
            }
            SelectRange(a, b) => {
                let (Some((after_a, len_a)), Some((after_b, _))) =
                    (locate(&self.session_text, &a), locate(&self.session_text, &b))
                else {
                    self.refuse(&intent, "those words are not in the dictated text").await;
                    return;
                };
                let start_from_end = after_a + len_a;
                if start_from_end < after_b {
                    self.refuse(&intent, "the end comes before the start").await;
                    return;
                }
                let here = self.dictation.caret_from_end.unwrap_or(0);
                if after_b >= here {
                    self.select_back(after_b - here, start_from_end - after_b);
                } else if self.press(Chord::key(Key::Right), here - after_b) {
                    self.select_back(0, start_from_end - after_b);
                }
                self.dictation.caret_from_end = Some(after_b);
            }
            SelectPreviousParagraph => {
                self.press(
                    Chord::Key(
                        Key::Up,
                        Modifiers {
                            shift: true,
                            word: true,
                            ..Default::default()
                        },
                    ),
                    1,
                );
                self.unknown_caret();
            }
            SelectNextWords(n) => {
                self.press(
                    Chord::Key(
                        Key::Right,
                        Modifiers {
                            shift: true,
                            word: true,
                            ..Default::default()
                        },
                    ),
                    n as usize,
                );
                self.unknown_caret();
            }
            Deselect => {
                self.press(Chord::key(Key::Right), 1);
                self.dictation.selection = 0;
            }
            CursorBefore(words) | CursorAfter(words) => {
                let before = matches!(intent, CursorBefore(_));
                match locate(&self.session_text, &words) {
                    Some((after, len)) => {
                        let target = if before { after + len } else { after };
                        let here = self.dictation.caret_from_end.unwrap_or(0);
                        let ok = if target >= here {
                            self.press(Chord::key(Key::Left), target - here)
                        } else {
                            self.press(Chord::key(Key::Right), here - target)
                        };
                        if ok {
                            self.dictation.caret_from_end = Some(target);
                            self.dictation.selection = 0;
                        }
                    }
                    None => self.refuse(&intent, "those words are not in the dictated text").await,
                }
            }
            GoLineStart => {
                self.press(Chord::key(Key::Home), 1);
                self.unknown_caret();
            }
            GoLineEnd => {
                self.press(Chord::key(Key::End), 1);
                self.unknown_caret();
            }
            GoDocStart => {
                let chord = if mac {
                    Chord::Key(
                        Key::Up,
                        Modifiers {
                            command: true,
                            ..Default::default()
                        },
                    )
                } else {
                    Chord::Key(
                        Key::Home,
                        Modifiers {
                            command: true,
                            ..Default::default()
                        },
                    )
                };
                self.press(chord, 1);
                self.unknown_caret();
            }
            GoDocEnd => {
                let chord = if mac {
                    Chord::Key(
                        Key::Down,
                        Modifiers {
                            command: true,
                            ..Default::default()
                        },
                    )
                } else {
                    Chord::Key(
                        Key::End,
                        Modifiers {
                            command: true,
                            ..Default::default()
                        },
                    )
                };
                if self.press(chord, 1) {
                    // The end of the document is the end of the typed text
                    // when we are its only writer; assume so.
                    self.dictation.caret_from_end = Some(0);
                    self.dictation.selection = 0;
                }
            }
            Lines(n) => {
                let key = if n < 0 { Key::Up } else { Key::Down };
                self.press(Chord::key(key), n.unsigned_abs() as usize);
                self.unknown_caret();
            }
            Chars(n) => {
                let key = if n < 0 { Key::Left } else { Key::Right };
                if self.press(Chord::key(key), n.unsigned_abs() as usize) {
                    self.dictation.caret_from_end = self.dictation.caret_from_end.map(|c| {
                        if n < 0 {
                            c + n.unsigned_abs() as usize
                        } else {
                            c.saturating_sub(n as usize)
                        }
                    });
                    self.dictation.selection = 0;
                }
            }
            Words(n) => {
                let key = if n < 0 { Key::Left } else { Key::Right };
                self.press(
                    Chord::Key(
                        key,
                        Modifiers {
                            word: true,
                            ..Default::default()
                        },
                    ),
                    n.unsigned_abs() as usize,
                );
                self.unknown_caret();
            }
            PageUp => {
                self.press(Chord::key(Key::PageUp), 1);
                self.unknown_caret();
            }
            PageDown => {
                self.press(Chord::key(Key::PageDown), 1);
                self.unknown_caret();
            }
            Bold => {
                self.press(Chord::shortcut('b'), 1);
            }
            Italic => {
                self.press(Chord::shortcut('i'), 1);
            }
            Underline => {
                self.press(Chord::shortcut('u'), 1);
            }
            UpperCaseThat | LowerCaseThat => {
                if !self.caret_at_end() {
                    self.refuse(&intent, "the caret was moved").await;
                    return;
                }
                let Some(last) = self.dictation.pieces.pop() else {
                    self.refuse(&intent, "nothing dictated yet").await;
                    return;
                };
                let changed = if matches!(intent, UpperCaseThat) {
                    last.to_uppercase()
                } else {
                    last.to_lowercase()
                };
                self.delete_tail(last.chars().count());
                let _ = self.emit_text(&changed).await;
                self.dictation.pieces.push(changed);
            }
            CapsOn => self.dictation.caps = true,
            CapsOff => self.dictation.caps = false,
            SpellNext => self.dictation.spell_next = true,
            SpellOn => self.dictation.spell_mode = true,
            SpellOff => self.dictation.spell_mode = false,
            Spelled(letters) => {
                let text = if self.session_text.ends_with(char::is_whitespace) || self.session_text.is_empty() {
                    letters
                } else {
                    format!(" {letters}")
                };
                let _ = self.emit_text(&text).await;
                self.dictation.pieces.push(text);
            }
            Punctuation(p) => {
                let _ = self.emit_text(&p).await;
                self.dictation.pieces.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locating_words_in_the_typed_text() {
        let t = "Hallo, ich möchte einen längeren Text schreiben. Ich habe so viel zu schreiben.";
        // Last occurrence, punctuation included in the match.
        assert_eq!(locate(t, "schreiben"), Some((0, 10)));
        assert_eq!(
            locate(t, "längeren text"),
            Some((" schreiben. Ich habe so viel zu schreiben.".chars().count(), 13))
        );
        assert_eq!(locate(t, "Hallo"), Some((t.chars().count() - 6, 6)));
        assert_eq!(locate(t, "nicht da"), None);
    }

    #[test]
    fn length_of_the_last_words() {
        assert_eq!(last_words_len("Hallo, ich möchte", 1), " möchte".chars().count());
        assert_eq!(last_words_len("Hallo, ich möchte", 2), " ich möchte".chars().count());
        assert_eq!(last_words_len("Hallo", 3), 5);
        assert_eq!(last_words_len("", 1), 0);
    }
}
