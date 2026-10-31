//! LanguageTool: grammar and spelling for dictated text over the public
//! HTTPS API (`/v2/check`). Only corrections the rules are sure about are
//! applied (spelling, grammar, casing, punctuation, typography); style
//! advice is left alone, so a sentence never changes its meaning. An
//! account is required (`USERNAME:APIKEY`, like Azure's `REGION:KEY`): the
//! shared free service is not meant for applications and would be
//! exhausted by many users at once. A 429 pauses corrections for a while.

use ikk_stt::{ErrorClass, SttError, TextCorrector};
use tracing::debug;

const URL: &str = "https://api.languagetoolplus.com/v2/check";

/// `USERNAME:APIKEY` → (username, key). The username is the account's
/// e-mail address, so the split is at the last colon.
pub fn split_credential(credential: &str) -> Result<(String, String), SttError> {
    const FORMAT: &str = "LanguageTool needs e-mail and API key, e.g. you@example.ch:KEY";
    let (user, key) = credential
        .trim()
        .rsplit_once(':')
        .ok_or(SttError::BadCredential(FORMAT))?;
    if user.trim().is_empty() || key.trim().is_empty() {
        return Err(SttError::BadCredential(FORMAT));
    }
    Ok((user.trim().to_string(), key.trim().to_string()))
}
/// Rule categories whose first suggestion is applied as it is.
const CATEGORIES: &[&str] = &[
    "TYPOS",
    "GRAMMAR",
    "CASING",
    "PUNCTUATION",
    "TYPOGRAPHY",
    "COMPOUNDING",
    "CONFUSED_WORDS",
    "MISC",
];

#[derive(Default)]
pub struct LanguageTool;

/// LanguageTool's language code for a settings language (ISO-639-3).
/// German gets the Swiss variant (no ß), as the people this is built for
/// write it; empty = let LanguageTool detect.
pub fn language_code(iso639_3: &str) -> &'static str {
    match iso639_3.trim() {
        "" => "auto",
        "deu" | "gsw" => "de-CH",
        "eng" => "en-US",
        "fra" => "fr",
        "ita" => "it",
        "spa" => "es",
        "por" => "pt-PT",
        "nld" => "nl",
        _ => "auto",
    }
}

/// Apply the safe matches of a `/v2/check` answer. Offsets are UTF-16
/// units (Java strings). `None` when nothing changed.
pub fn apply_matches(text: &str, body: &serde_json::Value) -> Option<String> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    let mut edits: Vec<(usize, usize, Vec<u16>)> = body
        .get("matches")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .filter_map(|m| {
                    let category = m.pointer("/rule/category/id").and_then(|c| c.as_str())?;
                    if !CATEGORIES.contains(&category) {
                        return None;
                    }
                    let offset = m.get("offset")?.as_u64()? as usize;
                    let length = m.get("length")?.as_u64()? as usize;
                    let replacements = m.get("replacements").and_then(|r| r.as_array())?;
                    // A speller with several ideas is guessing ("tset" →
                    // "stet"): the dictated word stays unless there is one.
                    if category == "TYPOS" && replacements.len() != 1 {
                        return None;
                    }
                    let value = replacements.first()?.get("value").and_then(|v| v.as_str())?;
                    (offset + length <= units.len() && length > 0)
                        .then(|| (offset, length, value.encode_utf16().collect()))
                })
                .collect()
        })
        .unwrap_or_default();
    if edits.is_empty() {
        return None;
    }
    // From the end, so earlier offsets stay valid; overlapping matches
    // (two rules on one word) take the first only.
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut end_of_previous = usize::MAX;
    for (offset, length, replacement) in edits {
        if offset + length > end_of_previous {
            continue;
        }
        units.splice(offset..offset + length, replacement);
        end_of_previous = offset;
    }
    let corrected = String::from_utf16_lossy(&units);
    (corrected != text).then_some(corrected)
}

#[async_trait::async_trait]
impl TextCorrector for LanguageTool {
    fn id(&self) -> &'static str {
        "languagetool"
    }
    fn display_name(&self) -> &'static str {
        "LanguageTool"
    }
    async fn correct(&self, text: &str, language: &str, credential: Option<&str>) -> Result<Option<String>, SttError> {
        let err = |e: reqwest::Error| SttError::Provider(ErrorClass::from_message(&e.to_string()));
        let (user, key) = split_credential(credential.ok_or(SttError::MissingCredential)?)?;
        // Built before the request, so nothing non-`Send` lives across the await.
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("text", text)
            .append_pair("language", language_code(language))
            .append_pair("username", &user)
            .append_pair("apiKey", &key)
            .finish();
        let r = crate::http::client()
            .post(URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(err)?;
        let status = r.status().as_u16();
        let body = r.text().await.unwrap_or_default();
        if status != 200 {
            return Err(SttError::Provider(ErrorClass::from_status(status, &body)));
        }
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| SttError::Provider(ErrorClass::Other(e.to_string())))?;
        let corrected = apply_matches(text, &json);
        debug!(changed = corrected.is_some(), "languagetool answered");
        Ok(corrected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_is_email_and_key() {
        assert_eq!(
            split_credential("you@example.ch:abc123").unwrap(),
            ("you@example.ch".to_string(), "abc123".to_string())
        );
        assert!(split_credential("abc123").is_err());
        assert!(split_credential("you@example.ch:").is_err());
    }

    #[test]
    fn languages() {
        assert_eq!(language_code("gsw"), "de-CH");
        assert_eq!(language_code("deu"), "de-CH");
        assert_eq!(language_code("eng"), "en-US");
        assert_eq!(language_code(""), "auto");
    }

    #[test]
    fn applies_safe_matches_with_utf16_offsets_and_skips_style() {
        // "Ä" is one UTF-16 unit, so offsets line up with chars here; the
        // test still goes through the UTF-16 path.
        let text = "das ist ein tset. Ähm das ist gut";
        let body = serde_json::json!({"matches": [
            {"offset": 0, "length": 3, "replacements": [{"value": "Das"}],
             "rule": {"category": {"id": "CASING"}}},
            {"offset": 12, "length": 4, "replacements": [{"value": "Test"}],
             "rule": {"category": {"id": "TYPOS"}}},
            {"offset": 22, "length": 3, "replacements": [{"value": "Das"}],
             "rule": {"category": {"id": "STYLE"}}},
            {"offset": 30, "length": 3, "replacements": [{"value": "Gut"}, {"value": "Glut"}],
             "rule": {"category": {"id": "TYPOS"}}},
        ]});
        // Style advice and a speller with several guesses are left alone.
        assert_eq!(
            apply_matches(text, &body).as_deref(),
            Some("Das ist ein Test. Ähm das ist gut")
        );
        assert_eq!(apply_matches(text, &serde_json::json!({"matches": []})), None);
    }
}
