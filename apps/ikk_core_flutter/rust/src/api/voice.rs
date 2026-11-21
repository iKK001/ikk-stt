//! Offline voice commands: record takes and train the start and stop word.
//! Switching them on: `IkkEngine::set_voice_commands`.

/// Record one spoken word (waits up to `timeout_secs` for speech).
pub fn voice_record_take(settings_json: String, timeout_secs: u32) -> Result<Vec<f32>, String> {
    ikk_ffi::voice::record_take(&settings_json, timeout_secs)
}

/// Check one word's takes right after recording them (error text if the
/// takes differ too much, e.g. noise or a cough among them).
/// `attempt`: how often this command was recorded again already.
pub fn voice_check_word(takes: Vec<Vec<f32>>, attempt: u32) -> Result<(), String> {
    ikk_ffi::voice::check_word(&takes, attempt)
}

/// Train both words from their takes; returns the model JSON to store.
pub fn voice_train(start_takes: Vec<Vec<f32>>, stop_takes: Vec<Vec<f32>>, attempt: u32) -> Result<String, String> {
    ikk_ffi::voice::train(&start_takes, &stop_takes, attempt)
}

/// The words the active speech provider hears in one recorded take, to
/// name a voice command. Refused unless the user has allowed that
/// provider (`consent_json`) or it works on the device.
pub fn voice_transcribe_take(
    settings_json: String,
    consent_json: String,
    credential: String,
    take: Vec<f32>,
) -> Result<String, String> {
    ikk_ffi::training::transcribe_clip(&settings_json, &consent_json, &credential, take)
}

/// Record one command anew (`start`: the start command, else the stop
/// command) and keep the other from the stored model; returns the new
/// model JSON.
pub fn voice_retrain_one(
    commands_json: String,
    start: bool,
    takes: Vec<Vec<f32>>,
    attempt: u32,
) -> Result<String, String> {
    ikk_ffi::voice::retrain_one(&commands_json, start, &takes, attempt)
}
