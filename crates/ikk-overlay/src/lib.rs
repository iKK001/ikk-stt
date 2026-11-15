//! Floating buttons over all apps, like the Android service's button: the
//! microphone (red while dictating, brand blue while listening for the
//! start word, dark gray when off; a click toggles dictation) and, next to
//! it, the correction button (a click corrects the text at the cursor).
//! Dragging moves them together. They never take keyboard focus, so text
//! keeps going into the app the user is working in.
//!
//! Windows and macOS; a no-op elsewhere (Android has its own service
//! button; iOS does not allow drawing over other apps).

use std::sync::Arc;

mod windows;
use windows as platform;

/// Called (on a background thread) when the button is clicked.
pub type OnTap = Arc<dyn Fn() + Send + Sync>;
/// Called (on a background thread) with the screen points of the captured
/// clicks, in the coordinates the platform's click injection expects.
pub type OnClicks = Arc<dyn Fn(Vec<(f64, f64)>) + Send + Sync>;
/// Called (on a background thread) with the buttons' new top-left corner
/// after the user dragged them (screen points, origin top-left).
pub type OnMoved = Arc<dyn Fn(f64, f64) + Send + Sync>;

/// Show the buttons (created on first use): `on_tap` for the microphone,
/// `on_correct` for the correction button (none: microphone only),
/// `on_moved` after every drag.
pub fn show(on_tap: OnTap, on_correct: Option<OnTap>, on_moved: Option<OnMoved>) {
    platform::show(on_tap, on_correct, on_moved);
}

/// The buttons' top-left corner on the screen (points, origin top-left of
/// the main display), once they exist.
pub fn position() -> Option<(f64, f64)> {
    platform::position()
}

/// Move the buttons there (same coordinates), kept on a screen.
pub fn set_position(x: f64, y: f64) {
    platform::set_position(x, y);
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

/// Shrinking buttons, as dictation suites do it: small and darker while
/// the mouse is elsewhere, full size (a little larger than plain) under
/// the mouse. Off: always the plain size.
pub fn set_shrink(on: bool) {
    platform::set_shrink(on);
}

/// Click to click: the correction button turns orange and the next `count`
/// clicks anywhere on the screen (not on the buttons) are collected; then
/// `on_done` gets their positions. Gives up after `timeout`. Returns false
/// when the platform cannot capture clicks.
pub fn capture_clicks(count: usize, timeout: std::time::Duration, on_done: OnClicks) -> bool {
    platform::capture_clicks(count, timeout, on_done)
}

/// Stop a capture (the button pressed again); true when one was running.
pub fn cancel_capture() -> bool {
    platform::cancel_capture()
}

pub fn is_capturing() -> bool {
    platform::is_capturing()
}

/// Whether this platform has a floating button.
pub const SUPPORTED: bool = cfg!(any(target_os = "windows", target_os = "macos"));
