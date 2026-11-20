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
/// Called (on a background thread) when the user flips the switch:
/// `true` = audio correction (replace by voice), `false` = AI correction.
pub type OnMode = Arc<dyn Fn(bool) + Send + Sync>;

/// Called (on a background thread) with the range the user chose by key.
pub type OnRange = Arc<dyn Fn(RangeMode) + Send + Sync>;

/// Show the buttons (created on first use): `on_tap` for the microphone,
/// `on_correct` for the correction button, `on_mode` for the switch
/// between AI and audio correction (none: fewer buttons), `on_moved`
/// after every drag, `on_next_range` when F1 is pressed anywhere while
/// the buttons are shown (the correction button's range moves on).
pub fn show(
    on_tap: OnTap,
    on_correct: Option<OnTap>,
    on_mode: Option<OnMode>,
    on_moved: Option<OnMoved>,
    on_next_range: Option<OnTap>,
) {
    platform::show(on_tap, on_correct, on_mode, on_moved, on_next_range);
}

/// What the correction button is doing: a rotating arc on its rim shows
/// it — yellow while the engine listens for the words of a replacement,
/// red while a recogniser or corrector works on the text. The text mark
/// follows the colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    None,
    Listening,
    Processing,
}

pub fn set_busy(busy: Busy) {
    platform::set_busy(busy);
}

/// Which half of the switch is lit: audio correction (true) or AI (false).
pub fn set_audio_mode(on: bool) {
    platform::set_audio_mode(on);
}

/// What the correction button corrects; its glyph shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeMode {
    /// From the cursor to the end of the text: a start mark, a track, dots.
    CursorToEnd,
    /// The whole text: lines of text.
    WholeText,
    /// Between two clicks: a start mark, a track, an end mark.
    ClickToClick,
    /// The word under one click: a short word bar with a click dot above.
    Word,
}

impl RangeMode {
    /// From the settings value ("cursor_to_end", "whole_text", "click_to_click").
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "whole_text" => Self::WholeText,
            "click_to_click" => Self::ClickToClick,
            "single_word" => Self::Word,
            _ => Self::CursorToEnd,
        }
    }

    /// The settings value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CursorToEnd => "cursor_to_end",
            Self::WholeText => "whole_text",
            Self::ClickToClick => "click_to_click",
            Self::Word => "single_word",
        }
    }
}

/// A key steps through the ranges while the buttons show (Windows: F2);
/// `on_range` hears the new one and saves it. Call before [`show`].
pub fn on_range_key(on_range: Option<OnRange>) {
    platform::on_range_key(on_range);
}

/// Tell the correction button which range it corrects (its glyph changes).
pub fn set_range_mode(mode: RangeMode) {
    platform::set_range_mode(mode);
}

/// Replace by voice is armed (the correction button is yellow): the next
/// click names the word, or a second press names the range.
pub fn set_replace_armed(on: bool) {
    platform::set_replace_armed(on);
}

/// A translucent yellow mark over the text that is being replaced
/// (screen points, origin top-left of the main display).
pub fn highlight(x: f64, y: f64, w: f64, h: f64) {
    platform::highlight(x, y, w, h);
}

pub fn clear_highlight() {
    platform::clear_highlight();
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
/// `on_done` gets their positions. Gives up after `timeout`: `on_done` then
/// gets the clicks that did come, fewer than `count`. Returns false
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
