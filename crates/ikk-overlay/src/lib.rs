//! Floating buttons over all apps, like the Android service's button: the
//! microphone (red while dictating, brand blue while listening for the
//! start word, dark gray when off; a click toggles dictation) and, next to
//! it, the correction column: the range button above (a click moves on to
//! the next range) and the start button below (a click corrects that range).
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

/// Show the buttons (created on first use): `on_tap` for the microphone,
/// `on_correct` for the start button of the correction column,
/// `on_next_range` for the range button above it (the range moves on),
/// `on_mode` for the switch between AI and audio correction (none: fewer
/// buttons), `on_moved` after every drag.
pub fn show(
    on_tap: OnTap,
    on_correct: Option<OnTap>,
    on_mode: Option<OnMode>,
    on_moved: Option<OnMoved>,
    on_next_range: Option<OnTap>,
) {
    platform::show(on_tap, on_correct, on_mode, on_moved, on_next_range);
}

/// What the start button is doing: a rotating arc on its rim shows
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

/// What the start button corrects; the range button's glyph shows it.
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

/// Tell the range button which range is chosen (its glyph changes).
pub fn set_range_mode(mode: RangeMode) {
    platform::set_range_mode(mode);
}

/// Replace by voice is armed (the start button is yellow): the next
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

/// Where the buttons sit on their screen, each direction on its own.
/// Close to an edge (less than one panel width of free room): the distance
/// of their top-left corner to that edge — left or right, top or bottom —
/// so they hug the edge on any screen. Further in: the share of the screen
/// at which their centre lies (`rx`, `ry`), so a third of the way across
/// stays a third of the way across on a larger screen. Distances are
/// screen points (Windows: pixels at 100 % scaling).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    /// Measured from the right edge (else from the left).
    pub right: bool,
    pub dx: f64,
    /// Measured from the bottom edge (else from the top).
    pub bottom: bool,
    pub dy: f64,
    /// Further in horizontally: the centre's share of the width (0 … 1).
    pub rx: Option<f64>,
    /// Further in vertically: the centre's share of the height (0 … 1).
    pub ry: Option<f64>,
}

/// A screen's usable area: x, y, width, height (origin top-left).
pub(crate) type Area = (f64, f64, f64, f64);

/// The anchor of buttons of `size` whose top-left corner is at `pos`.
#[allow(dead_code)]
pub(crate) fn anchor_for(pos: (f64, f64), size: (f64, f64), screen: Area) -> Anchor {
    let (sx, sy, sw, sh) = screen;
    let right = pos.0 + size.0 / 2.0 > sx + sw / 2.0;
    let bottom = pos.1 + size.1 / 2.0 > sy + sh / 2.0;
    // Free room between the buttons and the edge they are nearer to.
    let room_x = if right { sx + sw - pos.0 - size.0 } else { pos.0 - sx };
    let room_y = if bottom { sy + sh - pos.1 - size.1 } else { pos.1 - sy };
    // "Close" is one panel width, in both directions.
    let close = size.0;
    Anchor {
        right,
        dx: if right { sx + sw - pos.0 } else { pos.0 - sx },
        bottom,
        dy: if bottom { sy + sh - pos.1 } else { pos.1 - sy },
        rx: (room_x >= close && sw > 0.0).then(|| (pos.0 + size.0 / 2.0 - sx) / sw),
        ry: (room_y >= close && sh > 0.0).then(|| (pos.1 + size.1 / 2.0 - sy) / sh),
    }
}

/// The top-left corner for `anchor` on `screen`, the whole of the buttons
/// kept inside it.
#[allow(dead_code)]
pub(crate) fn place(anchor: Anchor, size: (f64, f64), screen: Area) -> (f64, f64) {
    let (sx, sy, sw, sh) = screen;
    let x = match anchor.rx {
        Some(share) => sx + share * sw - size.0 / 2.0,
        None if anchor.right => sx + sw - anchor.dx,
        None => sx + anchor.dx,
    };
    let y = match anchor.ry {
        Some(share) => sy + share * sh - size.1 / 2.0,
        None if anchor.bottom => sy + sh - anchor.dy,
        None => sy + anchor.dy,
    };
    (
        x.clamp(sx, (sx + sw - size.0).max(sx)),
        y.clamp(sy, (sy + sh - size.1).max(sy)),
    )
}

/// The buttons' anchor on their screen, once they exist.
pub fn anchor() -> Option<Anchor> {
    platform::anchor()
}

/// Move the buttons to `anchor`: on the screen that holds (`x`, `y`) —
/// the corner they had when the anchor was taken — or, when that point is
/// on no screen any more, on the main one.
pub fn set_position_anchored(x: f64, y: f64, anchor: Anchor) {
    platform::set_position_anchored(x, y, anchor);
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

/// The plain size of the buttons in percent, and their growth: how much
/// larger they are under the mouse than at rest (counts only while
/// shrinking is on; 0 % = one size, the plain one).
pub const DEFAULT_SIZE_PERCENT: u32 = 100;
/// How much larger the buttons are under the mouse than at rest.
pub const DEFAULT_GROWTH_PERCENT: u32 = 60;
pub const SIZE_PERCENT_RANGE: (u32, u32) = (50, 250);
pub const GROWTH_PERCENT_RANGE: (u32, u32) = (0, 150);
static SIZE_PERCENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(DEFAULT_SIZE_PERCENT);
static GROWTH_PERCENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(DEFAULT_GROWTH_PERCENT);
/// The share of the growth that lies above the plain size; the rest lies
/// below it. Chosen so that the default growth gives the sizes the
/// buttons always had: 115 % under the mouse, 72 % at rest.
const GROWTH_ABOVE: f64 = 0.2974;

/// The plain size as a factor (1.0 = 100 %).
#[allow(dead_code)]
pub(crate) fn base_scale() -> f64 {
    f64::from(SIZE_PERCENT.load(std::sync::atomic::Ordering::Relaxed)) / 100.0
}

/// Under the mouse over at rest (1.0 = no growth at all).
#[allow(dead_code)]
fn growth() -> f64 {
    1.0 + f64::from(GROWTH_PERCENT.load(std::sync::atomic::Ordering::Relaxed)) / 100.0
}

/// The size under the mouse while shrinking is on.
#[allow(dead_code)]
pub(crate) fn hover_scale() -> f64 {
    base_scale() * growth().powf(GROWTH_ABOVE)
}

/// The resting size while shrinking is on: `hover_scale` divided by the
/// growth, so with a growth of 0 % both are the plain size.
#[allow(dead_code)]
pub(crate) fn idle_scale() -> f64 {
    base_scale() * growth().powf(GROWTH_ABOVE - 1.0)
}

/// The top-left corner of a panel of size `panel` around `centre`. The
/// centre stays where it is until the buttons themselves (`shown`: their
/// size with the mouse away, smaller than the panel, which leaves room
/// for the growth) would reach over an edge of `screen`; only then is it
/// pushed back, just far enough.
#[allow(dead_code)]
pub(crate) fn around_centre(
    centre: (f64, f64),
    panel: (f64, f64),
    shown: (f64, f64),
    screen: Option<Area>,
) -> (f64, f64) {
    let (mut cx, mut cy) = centre;
    if let Some((sx, sy, sw, sh)) = screen {
        let keep = |c: f64, len: f64, from: f64, room: f64| {
            let lo = from + len / 2.0;
            c.clamp(lo, (from + room - len / 2.0).max(lo))
        };
        cx = keep(cx, shown.0, sx, sw);
        cy = keep(cy, shown.1, sy, sh);
    }
    (cx - panel.0 / 2.0, cy - panel.1 / 2.0)
}

/// Set the plain size and the growth under the mouse (both in percent,
/// kept inside their ranges); the buttons are resized around their
/// centre at once.
pub fn set_size(size_percent: u32, growth_percent: u32) {
    use std::sync::atomic::Ordering::Relaxed;
    SIZE_PERCENT.store(size_percent.clamp(SIZE_PERCENT_RANGE.0, SIZE_PERCENT_RANGE.1), Relaxed);
    GROWTH_PERCENT.store(
        growth_percent.clamp(GROWTH_PERCENT_RANGE.0, GROWTH_PERCENT_RANGE.1),
        Relaxed,
    );
    platform::refresh_size();
}

/// Click to click: the start button turns orange and the next `count`
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

#[cfg(test)]
mod anchor_tests {
    use super::*;

    const SIZE: (f64, f64) = (244.0, 138.0);
    const BIG: Area = (0.0, 25.0, 2560.0, 1415.0);
    const SMALL: Area = (0.0, 25.0, 1440.0, 875.0);

    #[test]
    fn close_to_an_edge_the_distance_is_kept() {
        // Near the right and the bottom edge.
        let a = anchor_for((2284.0, 1270.0), SIZE, BIG);
        assert_eq!(
            a,
            Anchor {
                right: true,
                dx: 276.0,
                bottom: true,
                dy: 170.0,
                rx: None,
                ry: None
            }
        );
        // Same screen: the same corner. A smaller one: the same distances.
        assert_eq!(place(a, SIZE, BIG), (2284.0, 1270.0));
        assert_eq!(place(a, SIZE, SMALL), (1164.0, 730.0));
        // Near the left and the top edge: measured from there.
        let b = anchor_for((40.0, 60.0), SIZE, BIG);
        assert_eq!(
            (b.right, b.dx, b.bottom, b.dy, b.rx, b.ry),
            (false, 40.0, false, 35.0, None, None)
        );
        assert_eq!(place(b, SIZE, SMALL), (40.0, 60.0));
        // Never outside the screen.
        let far = Anchor {
            right: false,
            dx: 5000.0,
            bottom: true,
            dy: 10.0,
            rx: None,
            ry: None,
        };
        assert_eq!(place(far, SIZE, SMALL), (1196.0, 762.0));
    }

    #[test]
    fn further_in_the_share_of_the_screen_is_kept() {
        // Centre at 40 % of the width, close to the top edge.
        let pos = (0.4 * 2560.0 - 122.0, 60.0);
        let a = anchor_for(pos, SIZE, BIG);
        assert_eq!(a.rx, Some(0.4));
        assert_eq!(a.ry, None);
        assert_eq!(place(a, SIZE, BIG), pos);
        // The smaller screen: again 40 % across, the same distance to the top.
        assert_eq!(place(a, SIZE, SMALL), (0.4 * 1440.0 - 122.0, 60.0));
        // In the middle both ways.
        let m = anchor_for((1158.0, 663.5), SIZE, BIG);
        assert_eq!((m.rx, m.ry), (Some(0.5), Some(0.5)));
        assert_eq!(place(m, SIZE, SMALL), (598.0, 393.5));
    }

    #[test]
    fn a_resize_keeps_the_centre_until_a_button_would_leave_the_screen() {
        let screen = Some((0.0, 0.0, 1000.0, 800.0));
        let (panel, shown) = ((200.0, 100.0), (120.0, 60.0));
        assert_eq!(around_centre((500.0, 400.0), panel, shown, screen), (400.0, 350.0));
        // The panel reaches over the edge, the buttons do not: unmoved.
        assert_eq!(around_centre((930.0, 760.0), panel, shown, screen), (830.0, 710.0));
        // The buttons would: pushed back until they touch the edge.
        assert_eq!(around_centre((980.0, 790.0), panel, shown, screen), (840.0, 720.0));
        assert_eq!(around_centre((980.0, 790.0), panel, shown, None), (880.0, 740.0));
    }

    #[test]
    fn growth_is_the_ratio_of_the_two_sizes() {
        use std::sync::atomic::Ordering::Relaxed;
        // The default gives the sizes the buttons always had.
        assert!((hover_scale() - 1.15).abs() < 0.005, "{}", hover_scale());
        assert!((idle_scale() - 0.72).abs() < 0.005, "{}", idle_scale());
        // No growth: one size, the plain one.
        GROWTH_PERCENT.store(0, Relaxed);
        assert_eq!(hover_scale(), base_scale());
        assert_eq!(idle_scale(), base_scale());
        GROWTH_PERCENT.store(DEFAULT_GROWTH_PERCENT, Relaxed);
    }
}
