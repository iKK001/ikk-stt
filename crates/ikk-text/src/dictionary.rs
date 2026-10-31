//! A German word list on the device, to tell a real word (or a compound
//! of real words) from a token the recogniser invented. Nothing leaves
//! the device, nothing is downloaded; the list is embedded compressed
//! (`assets/de.txt.gz`, see `assets/README.md` for its source).

use std::sync::OnceLock;

/// Sorted, lower-case forms, one per line, kept as one blob with line
/// offsets: a few megabytes, no per-word allocation.
struct List {
    blob: Vec<u8>,
    starts: Vec<u32>,
}

static LIST: OnceLock<List> = OnceLock::new();
/// Shortest part of a compound.
const MIN_PART: usize = 5;

fn list() -> &'static List {
    LIST.get_or_init(|| {
        use std::io::Read;
        let gz = include_bytes!("../assets/de.txt.gz");
        let mut blob = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut blob)
            .expect("embedded word list");
        let mut starts = Vec::new();
        let mut at = 0usize;
        for line in blob.split(|&b| b == b'\n') {
            if !line.is_empty() {
                starts.push(at as u32);
            }
            at += line.len() + 1;
        }
        List { blob, starts }
    })
}

impl List {
    fn line(&self, i: usize) -> &[u8] {
        let s = self.starts[i] as usize;
        let e = self.blob[s..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(self.blob.len(), |n| s + n);
        &self.blob[s..e]
    }

    fn contains(&self, w: &str) -> bool {
        let w = w.as_bytes();
        let (mut lo, mut hi) = (0usize, self.starts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.line(mid).cmp(w) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }
}

/// Lower case, letters only (digits kept); what the list is keyed by.
fn key(token: &str) -> String {
    token
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Is this a German word as the list knows it? Short tokens and numbers
/// pass; a hyphenated token passes when its parts do.
pub fn is_word(token: &str) -> bool {
    if token.contains('-') {
        return token.split('-').filter(|p| !p.is_empty()).all(is_word);
    }
    let k = key(token);
    if k.chars().count() <= 2 || k.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let l = list();
    l.contains(&k) || is_compound(l, &k, 2)
}

/// German compounds: two (or three) list words glued together, with an
/// optional joint (-s-, -n-, -en-, -es-, -er-) between them. Parts have at
/// least five letters: with shorter ones every invented token splits into
/// subtitle scraps (*Leih-helm*, *Leine-ham*).
fn is_compound(l: &List, k: &str, depth: u8) -> bool {
    let chars: Vec<char> = k.chars().collect();
    let n = chars.len();
    if n < 2 * MIN_PART {
        return false;
    }
    for cut in MIN_PART..=n - MIN_PART {
        let head: String = chars[..cut].iter().collect();
        let rest: String = chars[cut..].iter().collect();
        if !l.contains(&head) {
            continue;
        }
        let tails = [rest.clone()].into_iter().chain(
            ["s", "n", "en", "es", "er"]
                .iter()
                .filter_map(|j| rest.strip_prefix(j).map(str::to_string)),
        );
        for t in tails {
            if t.chars().count() >= MIN_PART && (l.contains(&t) || (depth > 1 && is_compound(l, &t, depth - 1))) {
                return true;
            }
        }
    }
    false
}

/// A stretch of text is "real" when every token is a word; one invented
/// token makes it a mishearing.
pub fn all_words(text: &str) -> bool {
    text.split_whitespace().all(is_word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_compounds_and_nonsense() {
        assert!(is_word("Bildschirm"));
        assert!(is_word("Bildschirmtastatur"), "compound of two list words");
        assert!(is_word("Marco"));
        assert!(is_word("zu"));
        assert!(is_word("2026"));
        assert!(is_word("Reha-Antrag"));
        assert!(!is_word("Bildschirmpassers"));
        assert!(!is_word("Leihhelm"));
        assert!(!is_word("Leihnamen"));
        assert!(!is_word("Bildschirmpastasse"));
        assert!(is_word("Reißverschluss"), "a real compound");
        assert!(is_word("Bildschirmpasswörter"), "a real compound, however odd");
        assert!(all_words("der Bildschirm passt das"));
        assert!(!all_words("der Bildschirmpassers"));
    }
}
