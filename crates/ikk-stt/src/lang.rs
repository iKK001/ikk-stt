//! Language code helpers. Settings store ISO-639-3 (`deu`), most APIs want
//! ISO-639-1 (`de`).

/// ISO-639-3 (or already 2-letter) → ISO-639-1. `None` = auto-detect.
pub fn iso639_1(code: &str) -> Option<&str> {
    let c = code.trim();
    if c.is_empty() {
        return None;
    }
    if c.len() == 2 {
        return Some(c);
    }
    Some(match c {
        "deu" | "ger" => "de",
        "eng" => "en",
        "fra" | "fre" => "fr",
        "ita" => "it",
        "spa" => "es",
        "por" => "pt",
        "nld" | "dut" => "nl",
        "gsw" => "de",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_three_letter_codes() {
        assert_eq!(iso639_1("deu"), Some("de"));
        assert_eq!(iso639_1("gsw"), Some("de"));
        assert_eq!(iso639_1("en"), Some("en"));
        assert_eq!(iso639_1(""), None);
        assert_eq!(iso639_1("xyz"), None);
    }
}
