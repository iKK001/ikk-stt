//! Training mode: say a word a few times; what the recogniser heard is
//! stored by the shell as the word's heard forms.

/// Record one take (waits up to `timeout_secs` for speech) and return what
/// the active provider heard. `credential` is that provider's key.
pub fn training_take(settings_json: String, credential: String, timeout_secs: u32) -> Result<String, String> {
    ikk_ffi::training::take(&settings_json, &credential, timeout_secs)
}
