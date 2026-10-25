//! Offline voice commands: record takes and train the start and stop word.
//! Switching them on: `IkkEngine::set_voice_commands`.

/// Record one spoken word (waits up to `timeout_secs` for speech).
pub fn voice_record_take(settings_json: String, timeout_secs: u32) -> Result<Vec<f32>, String> {
    ikk_ffi::voice::record_take(&settings_json, timeout_secs)
}

/// Train both words from their takes; returns the model JSON to store.
pub fn voice_train(start_takes: Vec<Vec<f32>>, stop_takes: Vec<Vec<f32>>) -> Result<String, String> {
    ikk_ffi::voice::train(&start_takes, &stop_takes)
}
