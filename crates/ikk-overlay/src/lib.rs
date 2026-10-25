//! A floating microphone button over all apps, like the Android service's
//! button: red while dictating, brand blue while listening for the start
//! word, dark gray when off. A click toggles dictation, dragging moves it. It never takes keyboard focus, so text keeps going
//! into the app the user is working in.
//!
//! Windows and macOS; a no-op elsewhere (Android has its own service
//! button; iOS does not allow drawing over other apps).

use std::sync::Arc;

mod windows;
use windows as platform;

/// Called (on a background thread) when the button is clicked.
pub type OnTap = Arc<dyn Fn() + Send + Sync>;

/// Show the button (created on first use) with this click action.
pub fn show(on_tap: OnTap) {
    platform::show(on_tap);
}

pub fn hide() {
    platform::hide();
}

/// Dictating: red.
pub fn set_listening(listening: bool) {
    platform::set_listening(listening);
}

/// Listening for the start word: brand blue (when not dictating).
pub fn set_standby(standby: bool) {
    platform::set_standby(standby);
}

/// Whether this platform has a floating button.
pub const SUPPORTED: bool = cfg!(any(target_os = "windows", target_os = "macos"));
