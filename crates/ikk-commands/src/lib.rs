//! Voice commands spoken inside dictation: a final that is exactly a
//! command phrase is carried out instead of typed. The phrases follow the
//! Dragon NaturallySpeaking German quick-start guide; the full list with
//! what each one does on each platform is `Cmd.md` at the repository root.
//!
//! Matching is on the spoken words after normalisation (case, punctuation
//! the recogniser adds, ß/ss), with numbers as words or digits.

use serde::{Deserialize, Serialize};

/// What a command asks for. Text arguments are the spoken words as heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Intent {
    // --- microphone
    /// "Geh schlafen", "Mikrofon ausschalten", "Diktat stopp".
    StopDictation,
    // --- lines
    NewLine,
    NewParagraph,
    PressEnter,
    PressTab,
    // --- deleting
    /// "Streich das", "Lösch das": the last dictated piece.
    DeleteThat,
    /// "Streich das <n> mal".
    DeleteThatTimes(u32),
    /// "Lösche die letzten <n> Wörter" (1: "Lösche das letzte Wort").
    DeleteLastWords(u32),
    /// "Zeile löschen".
    DeleteLine,
    /// "Rückschritt <n>".
    Backspace(u32),
    Undo,
    Cut,
    Copy,
    Paste,
    // --- selection
    SelectAll,
    /// "Markier das": the last dictated piece.
    SelectThat,
    /// "Markier <Wort oder Wörter>".
    SelectWords(String),
    /// "Markier die nächsten <n> Wörter".
    SelectNextWords(u32),
    /// "Markier <Anfang> bis <Ende>".
    SelectRange(String, String),
    /// "Markier vorherigen Absatz".
    SelectPreviousParagraph,
    /// "Auswahl aufheben".
    Deselect,
    // --- cursor
    /// "Cursor vor <Text>".
    CursorBefore(String),
    /// "Weiter bei <Wort>", "Cursor nach <Text>".
    CursorAfter(String),
    GoLineStart,
    GoLineEnd,
    GoDocStart,
    GoDocEnd,
    /// "Gehe <n> Zeilen nach unten" (negative: up).
    Lines(i32),
    /// "Gehe <n> Zeichen nach rechts" (negative: left).
    Chars(i32),
    /// "Gehe <n> Wörter nach rechts" (negative: left).
    Words(i32),
    PageUp,
    PageDown,
    // --- formatting
    Bold,
    Italic,
    Underline,
    /// "Mach das groß": the last dictated piece in upper case.
    UpperCaseThat,
    /// "Mach das klein".
    LowerCaseThat,
    /// "Großschaltung anfangen" / "beenden".
    CapsOn,
    CapsOff,
    // --- correction and spelling
    /// "Korrigier das": select the last dictated piece so that the next
    /// dictation replaces it.
    CorrectThat,
    /// "Korrigier <Wort oder Wörter>".
    CorrectWords(String),
    /// "Schreib das": the next final is taken letter by letter.
    SpellNext,
    /// "Starte Buchstabiermodus" / "Buchstabiermodus beenden".
    SpellOn,
    SpellOff,
    /// "Schreib <groß b a Bindestrich 5>": the letters, ready to type.
    Spelled(String),
    // --- punctuation said as a word on its own
    Punctuation(String),
    /// "Zitat": the opening quotation mark of the dictation language.
    QuoteOpen,
    /// "Zitat Ende": its closing mark.
    QuoteClose,
    /// "Wiederherstellen": redo.
    Redo,
    /// "Groß <Wort>": the next dictated word starts with a capital.
    CapitalizeNext,
}

/// Commands that may also stand at the start or the end of a dictated
/// final ("… Zitat", "Groß Hallo"): typography, no cursor moves, so a
/// word that happens to match cannot wreck the text.
pub fn inline_ok(intent: &Intent) -> bool {
    matches!(
        intent,
        Intent::Punctuation(_)
            | Intent::QuoteOpen
            | Intent::QuoteClose
            | Intent::NewLine
            | Intent::NewParagraph
            | Intent::CapitalizeNext
    )
}

/// A dictated piece with its spoken signs turned into signs: "Hallo Welt
/// Punkt wie geht es dir Fragezeichen" → "Hallo Welt. Wie geht es dir?".
/// Commands may stand anywhere, without a pause; `quotes` are the
/// dictation language's marks for Zitat / Zitat Ende. `None` when the piece
/// holds no such command.
pub struct Inline {
    pub text: String,
    /// The piece ended with "Groß": the next piece starts with a capital.
    pub capitalize_next: bool,
}

pub fn apply_inline(text: &str, locale: &str, quotes: (&str, &str)) -> Option<Inline> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 2 {
        return None;
    }
    let mut out = String::new();
    let mut found = false;
    let mut capitalize = false;
    let mut i = 0;
    while i < words.len() {
        let mut matched = None;
        for k in (1..=3).rev() {
            if i + k > words.len() {
                continue;
            }
            let phrase = words[i..i + k].join(" ");
            if let Some(intent) = match_command(&phrase, locale).filter(inline_ok) {
                matched = Some((k, intent));
                break;
            }
        }
        match matched {
            Some((k, intent)) => {
                found = true;
                i += k;
                match intent {
                    Intent::Punctuation(p) => {
                        push_sign(&mut out, &p);
                        if matches!(p.as_str(), "." | "!" | "?" | "…") {
                            capitalize = true;
                        }
                    }
                    Intent::QuoteOpen => push_sign(&mut out, quotes.0),
                    Intent::QuoteClose => push_sign(&mut out, quotes.1),
                    Intent::NewLine => {
                        out.push('\n');
                        capitalize = true;
                    }
                    Intent::NewParagraph => {
                        out.push_str("\n\n");
                        capitalize = true;
                    }
                    Intent::CapitalizeNext => capitalize = true,
                    _ => {}
                }
            }
            None => {
                let mut w = words[i].to_string();
                if capitalize {
                    let mut chars = w.chars();
                    if let Some(first) = chars.next() {
                        w = first.to_uppercase().collect::<String>() + chars.as_str();
                    }
                    capitalize = false;
                }
                push_word(&mut out, &w);
                i += 1;
            }
        }
    }
    found.then_some(Inline {
        text: out,
        capitalize_next: capitalize,
    })
}

/// Signs that attach to the word before them.
const AFTER_PREV: &str = ".,;:!?)]}»”’“…%°";
/// Signs that attach to the word after them.
const BEFORE_NEXT: &str = "([{„«‹‘#";
/// Signs glued to both sides.
const TIGHT: &str = "-/@";

fn push_word(out: &mut String, w: &str) {
    let tail = out.chars().next_back();
    let glue = match tail {
        None => true,
        Some(c) => c == '\n' || c == '\u{202F}' || BEFORE_NEXT.contains(c) || TIGHT.contains(c),
    };
    if !glue {
        out.push(' ');
    }
    out.push_str(w);
}

fn push_sign(out: &mut String, sign: &str) {
    let first = sign.chars().next().unwrap_or(' ');
    let tail = out.chars().next_back();
    let after_prev = AFTER_PREV.contains(first) || sign.starts_with('\u{202F}') || TIGHT.contains(first);
    if sign == " " {
        out.push(' ');
        return;
    }
    if after_prev {
        while out.ends_with(' ') {
            out.pop();
        }
    } else if tail.is_some_and(|c| c != '\n' && c != ' ' && !BEFORE_NEXT.contains(c) && !TIGHT.contains(c)) {
        out.push(' ');
    }
    out.push_str(sign);
}

/// A command phrase (up to three words) at the end of `text`: the text
/// before it and the command. The recogniser's punctuation on the phrase
/// does not matter.
pub fn split_trailing(text: &str, locale: &str) -> Option<(String, Intent)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for k in (1..=3).rev() {
        if words.len() <= k {
            continue;
        }
        let phrase = words[words.len() - k..].join(" ");
        if let Some(intent) = match_command(&phrase, locale).filter(inline_ok) {
            return Some((words[..words.len() - k].join(" "), intent));
        }
    }
    None
}

/// Like [`split_trailing`], but for any command, not only the inline ones:
/// used when the engine already knows a command was spoken (the stop
/// phrase that the recogniser wrote down as well).
pub fn split_trailing_any(text: &str, locale: &str) -> Option<(String, Intent)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for k in (1..=4).rev() {
        if words.len() <= k {
            continue;
        }
        let phrase = words[words.len() - k..].join(" ");
        if let Some(intent) = match_command(&phrase, locale) {
            return Some((words[..words.len() - k].join(" "), intent));
        }
    }
    None
}

/// A command phrase (up to three words) at the start of `text`: the
/// command and the text after it.
pub fn split_leading(text: &str, locale: &str) -> Option<(Intent, String)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for k in (1..=3).rev() {
        if words.len() <= k {
            continue;
        }
        let phrase = words[..k].join(" ");
        if let Some(intent) = match_command(&phrase, locale).filter(inline_ok) {
            return Some((intent, words[k..].join(" ")));
        }
    }
    None
}

/// Letters, digits and marks the recogniser must spell: lower case, with ß
/// kept, as `(spoken, written)`.
const SPELL: &[(&str, &str)] = &[
    ("anton", "a"),
    ("berta", "b"),
    ("cäsar", "c"),
    ("caesar", "c"),
    ("dora", "d"),
    ("emil", "e"),
    ("friedrich", "f"),
    ("gustav", "g"),
    ("heinrich", "h"),
    ("ida", "i"),
    ("julius", "j"),
    ("kaufmann", "k"),
    ("konrad", "k"),
    ("ludwig", "l"),
    ("martha", "m"),
    ("nordpol", "n"),
    ("otto", "o"),
    ("paula", "p"),
    ("quelle", "q"),
    ("richard", "r"),
    ("samuel", "s"),
    ("siegfried", "s"),
    ("theodor", "t"),
    ("ulrich", "u"),
    ("viktor", "v"),
    ("wilhelm", "w"),
    ("xanthippe", "x"),
    ("xaver", "x"),
    ("ypsilon", "y"),
    ("zacharias", "z"),
    ("zürich", "z"),
    ("ärger", "ä"),
    ("ökonom", "ö"),
    ("übermut", "ü"),
    ("eszett", "ß"),
    ("scharfes", "ß"),
    ("leerzeichen", " "),
    ("bindestrich", "-"),
    ("punkt", "."),
    ("komma", ","),
    ("unterstrich", "_"),
    ("at", "@"),
    ("klammeraffe", "@"),
];

/// Punctuation said on its own.
const PUNCTUATION: &[(&str, &str)] = &[
    ("punkt", "."),
    ("komma", ","),
    ("ausrufezeichen", "!"),
    ("fragezeichen", "?"),
    ("apostroph", "'"),
    ("klammer auf", "("),
    ("klammer zu", ")"),
    ("anführungszeichen", "\""),
    ("anführungszeichen unten", "„"),
    ("anführungszeichen oben", "“"),
    ("anführungsstriche oben", "“"),
    ("at", "@"),
    ("at zeichen", "@"),
    ("klammeraffe", "@"),
    ("pluszeichen", "+"),
    ("unterstrich", "_"),
    ("bindestrich", "-"),
    ("gedankenstrich", "–"),
    ("semikolon", ";"),
    ("strichpunkt", ";"),
    ("doppelpunkt", ":"),
    ("und zeichen", "&"),
    ("paragraphenzeichen", "§"),
    ("nummernzeichen", "#"),
    ("prozentzeichen", "%"),
    ("schrägstrich", "/"),
    ("zwinkerndes gesicht", ";-)"),
    ("smiley", ":-)"),
    ("lachendes gesicht", ":-)"),
    ("trauriges gesicht", ":-("),
    ("leerzeichen", " "),
    ("sternchen", "*"),
    ("gleichheitszeichen", "="),
    ("gleich zeichen", "="),
    ("auslassungspunkte", "…"),
    ("drei punkte", "…"),
    ("eurozeichen", "€"),
    ("euro zeichen", "€"),
    ("dollarzeichen", "$"),
    ("dollar zeichen", "$"),
    ("gradzeichen", "°"),
    ("et zeichen", "&"),
    ("kaufmännisches und", "&"),
    ("raute", "#"),
    ("hashtag", "#"),
    ("rückwärtsschrägstrich", "\\"),
    ("senkrechter strich", "|"),
    ("tilde", "~"),
    ("eckige klammer auf", "["),
    ("eckige klammer zu", "]"),
    ("geschweifte klammer auf", "{"),
    ("geschweifte klammer zu", "}"),
    ("kleiner als", "<"),
    ("grösser als", ">"),
    ("größer als", ">"),
];

/// Lower case, punctuation gone, ß kept, one space between words.
fn norm(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == '-' || c.is_whitespace() {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn number(word: &str) -> Option<u32> {
    if let Ok(n) = word.parse::<u32>() {
        return Some(n);
    }
    Some(match word {
        "ein" | "eins" | "eine" | "einen" | "einem" | "einer" => 1,
        "zwei" | "zwo" => 2,
        "drei" => 3,
        "vier" => 4,
        "fünf" => 5,
        "sechs" => 6,
        "sieben" => 7,
        "acht" => 8,
        "neun" => 9,
        "zehn" => 10,
        "elf" => 11,
        "zwölf" => 12,
        "dreizehn" => 13,
        "vierzehn" => 14,
        "fünfzehn" => 15,
        "sechzehn" => 16,
        "siebzehn" => 17,
        "achtzehn" => 18,
        "neunzehn" => 19,
        "zwanzig" => 20,
        "dreissig" | "dreißig" => 30,
        "vierzig" => 40,
        "fünfzig" => 50,
        "hundert" => 100,
        _ => return None,
    })
}

/// The command for `text`, if the whole text is one. `locale`: "de" and
/// "de-CH" carry the German set; "en" the few English ones.
pub fn match_command(text: &str, locale: &str) -> Option<Intent> {
    let t = norm(text);
    if t.is_empty() {
        return None;
    }
    if locale == "en" {
        return match t.as_str() {
            "new line" => Some(Intent::NewLine),
            "new paragraph" => Some(Intent::NewParagraph),
            "stop dictation" => Some(Intent::StopDictation),
            "quote" | "open quote" | "begin quote" => Some(Intent::QuoteOpen),
            "end quote" | "unquote" | "close quote" => Some(Intent::QuoteClose),
            "period" | "full stop" => Some(Intent::Punctuation(".".into())),
            "comma" => Some(Intent::Punctuation(",".into())),
            "question mark" => Some(Intent::Punctuation("?".into())),
            "exclamation mark" => Some(Intent::Punctuation("!".into())),
            "colon" => Some(Intent::Punctuation(":".into())),
            "semicolon" => Some(Intent::Punctuation(";".into())),
            "redo" => Some(Intent::Redo),
            "capital" | "capitalize" => Some(Intent::CapitalizeNext),
            _ => None,
        };
    }
    if !locale.starts_with("de") {
        return None;
    }
    german(&t)
}

fn german(t: &str) -> Option<Intent> {
    use Intent::*;
    let w: Vec<&str> = t.split(' ').collect();
    let rest = |n: usize| w[n..].join(" ");
    // Fixed phrases first.
    let fixed = match t {
        "geh schlafen" | "mikrofon ausschalten" | "mikrofon aus" | "diktat stopp" | "diktat stop" => {
            Some(StopDictation)
        }
        "neue zeile" => Some(NewLine),
        "neuer absatz" => Some(NewParagraph),
        "zitat"
        | "zitat anfang"
        | "zitatanfang"
        | "zitat beginn"
        | "anführungszeichen auf"
        | "anführungszeichen öffnen"
        | "anführungsstriche unten"
        | "gänsefüsschen auf"
        | "gänsefüßchen auf" => Some(QuoteOpen),
        "zitat ende"
        | "zitatende"
        | "ende zitat"
        | "zitat schluss"
        | "anführungszeichen zu"
        | "anführungszeichen schliessen"
        | "anführungszeichen schließen"
        | "gänsefüsschen zu"
        | "gänsefüßchen zu" => Some(QuoteClose),
        "wiederherstellen" | "wiederholen" | "wieder herstellen" => Some(Redo),
        "gross" | "groß" | "grossbuchstaben" | "großbuchstaben" | "grossbuchstabe" | "großbuchstabe" => {
            Some(CapitalizeNext)
        }
        "nächstes wort" | "ein wort vor" | "wort vor" => Some(Words(1)),
        "vorheriges wort" | "ein wort zurück" | "wort zurück" => Some(Words(-1)),
        "drücke eingabetaste" | "eingabetaste" => Some(PressEnter),
        "drücke tab" | "tabulator" => Some(PressTab),
        "streich das" | "lösch das" | "lösche das" => Some(DeleteThat),
        "lösche das letzte wort" | "lösch das letzte wort" | "letztes wort löschen" => Some(DeleteLastWords(1)),
        "zeile löschen" | "lösche die zeile" => Some(DeleteLine),
        "rückgängig machen" | "rückgängig" => Some(Undo),
        "schneide das aus" | "ausschneiden" => Some(Cut),
        "kopiere das" | "kopieren" => Some(Copy),
        "füge das ein" | "einfügen" => Some(Paste),
        "alles markieren" | "markier dokument" | "markiere dokument" => Some(SelectAll),
        "markier das" | "markiere das" => Some(SelectThat),
        "markier vorherigen absatz" | "markiere vorherigen absatz" => Some(SelectPreviousParagraph),
        "auswahl aufheben" => Some(Deselect),
        "gehe zum anfang" | "gehe zum zeilenanfang" | "zeilenanfang" => Some(GoLineStart),
        "gehe zum ende" | "gehe zum zeilenende" | "zeilenende" => Some(GoLineEnd),
        "gehe zum dokumentanfang" | "dokumentanfang" => Some(GoDocStart),
        "gehe zum dokumentende" | "dokumentende" => Some(GoDocEnd),
        "seite nach oben" => Some(PageUp),
        "seite nach unten" => Some(PageDown),
        "drücke pfeil nach rechts" | "pfeil nach rechts" => Some(Chars(1)),
        "drücke pfeil nach links" | "pfeil nach links" => Some(Chars(-1)),
        "drücke pfeil nach oben" | "pfeil nach oben" => Some(Lines(-1)),
        "drücke pfeil nach unten" | "pfeil nach unten" => Some(Lines(1)),
        "fett" | "mach das fett" => Some(Bold),
        "kursiv" | "mach das kursiv" => Some(Italic),
        "unterstreich das" | "unterstreichen" | "mach das unterstrichen" => Some(Underline),
        "mach das gross" | "mach das groß" => Some(UpperCaseThat),
        "mach das klein" => Some(LowerCaseThat),
        "grossschaltung anfangen" | "großschaltung anfangen" | "grossschreibung anfangen" => Some(CapsOn),
        "grossschaltung beenden" | "großschaltung beenden" | "grossschreibung beenden" => Some(CapsOff),
        "korrigier das" | "korrigiere das" => Some(CorrectThat),
        "schreib das" | "schreibe das" | "buchstabier das" | "buchstabiere das" => Some(SpellNext),
        "starte buchstabiermodus" | "buchstabiermodus starten" => Some(SpellOn),
        "buchstabiermodus beenden" | "beende buchstabiermodus" => Some(SpellOff),
        _ => None,
    };
    if fixed.is_some() {
        return fixed;
    }
    if let Some(p) = PUNCTUATION.iter().find(|(s, _)| *s == t) {
        return Some(Punctuation(p.1.to_string()));
    }
    // "streich das <n> mal"
    if w.len() == 4 && w[0] == "streich" && w[1] == "das" && w[3] == "mal" {
        return number(w[2]).map(DeleteThatTimes);
    }
    // "lösche die letzten <n> wörter"
    if w.len() == 5
        && (w[0] == "lösche" || w[0] == "lösch")
        && w[1] == "die"
        && w[2] == "letzten"
        && w[4].starts_with("wört")
    {
        return number(w[3]).map(DeleteLastWords);
    }
    // "rückschritt <n>"
    if w.len() == 2 && w[0] == "rückschritt" {
        return number(w[1]).map(Backspace);
    }
    if w.len() == 1 && w[0] == "rückschritt" {
        return Some(Backspace(1));
    }
    // "markier die nächsten <n> wörter"
    if w.len() == 5 && w[0].starts_with("markier") && w[1] == "die" && w[2] == "nächsten" && w[4].starts_with("wört")
    {
        return number(w[3]).map(SelectNextWords);
    }
    // "markier <a> bis <b>"
    if w.len() >= 4 && w[0].starts_with("markier") {
        if let Some(i) = w.iter().position(|x| *x == "bis") {
            if i >= 2 && i + 1 < w.len() {
                return Some(SelectRange(w[1..i].join(" "), w[i + 1..].join(" ")));
            }
        }
    }
    // "markier <wörter>"
    if w.len() >= 2 && w[0].starts_with("markier") {
        return Some(SelectWords(rest(1)));
    }
    // "korrigier <wörter>"
    if w.len() >= 2 && w[0].starts_with("korrigier") {
        return Some(CorrectWords(rest(1)));
    }
    // "cursor vor <text>", "cursor nach <text>", "weiter bei <wort>"
    if w.len() >= 3 && w[0] == "cursor" && w[1] == "vor" {
        return Some(CursorBefore(rest(2)));
    }
    if w.len() >= 3 && w[0] == "cursor" && (w[1] == "nach" || w[1] == "hinter") {
        return Some(CursorAfter(rest(2)));
    }
    if w.len() >= 3 && w[0] == "weiter" && w[1] == "bei" {
        return Some(CursorAfter(rest(2)));
    }
    // "gehe <n> nach unten|oben", "gehe <n> zeilen nach unten|oben",
    // "gehe <n> zeichen nach links|rechts", "gehe <n> wörter nach links|rechts"
    if w.len() >= 4 && (w[0] == "gehe" || w[0] == "geh") {
        if let Some(n) = number(w[1]) {
            let n = n as i32;
            let (unit, dir) = if w.len() == 4 { ("zeilen", w[3]) } else { (w[2], w[4]) };
            let ok = w.len() == 4 || w[3] == "nach";
            if ok {
                return match (unit, dir) {
                    (u, "unten") if u.starts_with("zeile") => Some(Lines(n)),
                    (u, "oben") if u.starts_with("zeile") => Some(Lines(-n)),
                    (u, "rechts") if u.starts_with("zeichen") => Some(Chars(n)),
                    (u, "links") if u.starts_with("zeichen") => Some(Chars(-n)),
                    (u, "rechts") if u.starts_with("wört") || u.starts_with("wort") => Some(Words(n)),
                    (u, "links") if u.starts_with("wört") || u.starts_with("wort") => Some(Words(-n)),
                    _ => None,
                };
            }
        }
    }
    // "schreib <letters…>"
    if w.len() >= 2 && (w[0] == "schreib" || w[0] == "schreibe" || w[0] == "buchstabier" || w[0] == "buchstabiere") {
        return spell(&rest(1)).map(Spelled);
    }
    None
}

/// Spoken letters to text: single letters, the German spelling alphabet,
/// digits, "groß <letter>" for upper case, and the marks in [`SPELL`].
/// `None` when a word is none of those.
pub fn spell(text: &str) -> Option<String> {
    let t = norm(text);
    let mut out = String::new();
    let mut upper = false;
    for w in t.split(' ') {
        if w.is_empty() {
            continue;
        }
        if w == "gross" || w == "groß" {
            upper = true;
            continue;
        }
        let piece: String = if w.chars().count() == 1 {
            w.to_string()
        } else if let Some(n) = number(w) {
            n.to_string()
        } else if let Some((_, c)) = SPELL.iter().find(|(s, _)| *s == w) {
            (*c).to_string()
        } else {
            return None;
        };
        if upper {
            out.push_str(&piece.to_uppercase());
            upper = false;
        } else {
            out.push_str(&piece);
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Intent::*;

    fn de(s: &str) -> Option<Intent> {
        match_command(s, "de-CH")
    }

    #[test]
    fn signs_spoken_inside_a_sentence() {
        let q = ("„", "“");
        let r = apply_inline("Hallo Welt Punkt wie geht es dir Fragezeichen", "de-CH", q).unwrap();
        assert_eq!(r.text, "Hallo Welt. Wie geht es dir?");
        // After the question mark the next piece starts with a capital.
        assert!(r.capitalize_next);
        let r = apply_inline("er sagte Zitat kommt gut Zitat Ende und ging", "de-CH", q).unwrap();
        assert_eq!(r.text, "er sagte „kommt gut“ und ging");
        let r = apply_inline("erste Zeile Neue Zeile zweite Komma dritte Groß", "de-CH", q).unwrap();
        assert_eq!(r.text, "erste Zeile\nZweite, dritte");
        assert!(r.capitalize_next);
        let r = apply_inline("Klammer auf so Klammer zu Bindestrich fertig", "de-CH", q).unwrap();
        assert_eq!(r.text, "(so)-fertig");
        assert!(apply_inline("ganz normale Wörter hier", "de-CH", q).is_none());
        // Cursor commands are never signs.
        assert!(apply_inline("gehe zum Zeilenende bitte", "de-CH", q).is_none());
    }

    #[test]
    fn commands_at_the_edges_of_a_final() {
        assert_eq!(
            split_trailing("Hallo Welt Zitat", "de-CH"),
            Some(("Hallo Welt".to_string(), Intent::QuoteOpen))
        );
        assert_eq!(
            split_trailing("so geht das Zitat Ende.", "de-CH"),
            Some(("so geht das".to_string(), Intent::QuoteClose))
        );
        assert_eq!(
            split_trailing("das ist gut Punkt", "de-CH"),
            Some(("das ist gut".to_string(), Intent::Punctuation(".".into())))
        );
        assert_eq!(
            split_leading("Groß hallo welt", "de-CH"),
            Some((Intent::CapitalizeNext, "hallo welt".to_string()))
        );
        // Cursor moves never come out of running text.
        assert_eq!(split_trailing("gehe zum Zeilenende", "de-CH"), None);
        assert_eq!(split_trailing("Zitat", "de-CH"), None);
    }

    #[test]
    fn quotes_open_and_close() {
        assert_eq!(de("Zitat"), Some(Intent::QuoteOpen));
        assert_eq!(de("Zitat Ende."), Some(Intent::QuoteClose));
        assert_eq!(de("Zitatende"), Some(Intent::QuoteClose));
        assert_eq!(match_command("end quote", "en"), Some(Intent::QuoteClose));
        assert_eq!(match_command("quote", "en"), Some(Intent::QuoteOpen));
    }

    #[test]
    fn fixed_phrases_with_the_recognisers_punctuation() {
        assert_eq!(de("Neue Zeile."), Some(NewLine));
        assert_eq!(de("Neuer Absatz"), Some(NewParagraph));
        assert_eq!(de("Neue Zeile bitte"), None);
        assert_eq!(de("Geh schlafen!"), Some(StopDictation));
        assert_eq!(de("Streich das."), Some(DeleteThat));
        assert_eq!(de("Rückgängig machen"), Some(Undo));
        assert_eq!(de("Alles markieren"), Some(SelectAll));
        assert_eq!(de("Gehe zum Ende"), Some(GoLineEnd));
        assert_eq!(de("Mach das groß"), Some(UpperCaseThat));
        assert_eq!(de("Großschaltung anfangen"), Some(CapsOn));
        assert_eq!(de("Korrigier das"), Some(CorrectThat));
        assert_eq!(de("Schreib das"), Some(SpellNext));
    }

    #[test]
    fn numbers_as_words_or_digits() {
        assert_eq!(de("Streich das drei Mal"), Some(DeleteThatTimes(3)));
        assert_eq!(de("Lösche die letzten 2 Wörter"), Some(DeleteLastWords(2)));
        assert_eq!(de("Rückschritt fünf"), Some(Backspace(5)));
        assert_eq!(de("Gehe drei nach unten"), Some(Lines(3)));
        assert_eq!(de("Gehe 2 Zeilen nach oben"), Some(Lines(-2)));
        assert_eq!(de("Gehe vier Zeichen nach links"), Some(Chars(-4)));
        assert_eq!(de("Markier die nächsten zwei Wörter"), Some(SelectNextWords(2)));
    }

    #[test]
    fn text_arguments() {
        assert_eq!(de("Markier längeren Text"), Some(SelectWords("längeren text".into())));
        assert_eq!(
            de("Markier Hallo bis schreiben"),
            Some(SelectRange("hallo".into(), "schreiben".into()))
        );
        assert_eq!(
            de("Korrigier Bildschirm passt das"),
            Some(CorrectWords("bildschirm passt das".into()))
        );
        assert_eq!(de("Weiter bei Lösung"), Some(CursorAfter("lösung".into())));
        assert_eq!(de("Cursor vor Text"), Some(CursorBefore("text".into())));
    }

    #[test]
    fn punctuation_and_spelling() {
        assert_eq!(de("Punkt."), Some(Punctuation(".".into())));
        assert_eq!(de("Fragezeichen"), Some(Punctuation("?".into())));
        assert_eq!(de("Klammer auf"), Some(Punctuation("(".into())));
        assert_eq!(de("Schreib groß B A Bindestrich 5"), Some(Spelled("Ba-5".into())));
        assert_eq!(spell("Anton groß Berta cäsar eins zwei"), Some("aBc12".into()));
        assert_eq!(spell("hallo welt"), None);
    }
}
