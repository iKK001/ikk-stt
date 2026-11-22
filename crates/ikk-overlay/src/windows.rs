//! Win32 implementation: a round, topmost tool window that never activates.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, InvalidateRect, MonitorFromPoint,
    SelectObject, MONITORINFO, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT,
};
use windows::Win32::Graphics::GdiPlus::{GdiplusStartup, GdiplusStartupInput};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowRect, KillTimer, LoadCursorW, PostMessageW, RegisterClassW, SetTimer, SetWindowPos, SetWindowsHookExW,
    ShowWindow, TranslateMessage, UnhookWindowsHookEx, HHOOK, HWND_TOPMOST, IDC_HAND, MA_NOACTIVATE, MSG,
    MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNOACTIVATE, WH_MOUSE_LL, WM_APP, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::{Busy, OnClicks, OnMode, OnMoved, OnTap, RangeMode};

const SIZE: i32 = 64;
const RING: i32 = 3;
/// Space between the microphone and the correction button.
const GAP: i32 = 10;
const DRAG_THRESHOLD: i32 = 4;
// Shrinking buttons: sizes come from the settings (crate::set_size).
use crate::{base_scale, hover_scale, idle_scale};
/// How much darker the idle buttons are drawn (multiplied into the colour).
const DIM: f64 = 0.8;
/// Growing and shrinking take this long, eased at both ends; redrawn
/// every 16 ms by a window timer.
const ANIM_MS: f64 = 340.0;
const TIMER_ANIM: usize = 8;
/// Redraws the rotating arc while the correction button is busy.
const TIMER_SPIN: usize = 9;
/// Looks where the mouse is: over the drawn buttons or not.
const TIMER_HOVER: usize = 10;
const MSG_SPIN: u32 = WM_APP + 10;
// COLORREF is 0x00BBGGRR.
const BLUE: COLORREF = COLORREF(0x00B3_8600); // #0086B3, brand primary: listening for the start word
const BRAND: COLORREF = COLORREF(0x00CC_9900); // #0099CC: the correction button
const ORANGE: COLORREF = COLORREF(0x0000_6CEF); // #EF6C00: waiting for the two clicks
const YELLOW: COLORREF = COLORREF(0x0000_C7F9); // #F9C700: replace by voice armed
const INK: COLORREF = COLORREF(0x0000_2E3A); // #3A2E00: glyph on the yellow button
const COLOR_LISTEN_ARC: COLORREF = COLORREF(0x00AD_7800); // #0078AD
const RED: COLORREF = COLORREF(0x002F_2FD3); // #D32F2F: dictating
const GRAY: COLORREF = COLORREF(0x0042_4242); // #424242: off
const DISABLED: COLORREF = COLORREF(0x009E_9E9E); // #9E9E9E: the start button when its mode is not available

const MSG_REDRAW: u32 = WM_APP + 1;
const MSG_HIDE: u32 = WM_APP + 2;
const MSG_SHOW: u32 = WM_APP + 3;
/// Install / remove the low-level mouse hook (window thread only).
const MSG_ARM: u32 = WM_APP + 4;
const MSG_DISARM: u32 = WM_APP + 5;
/// Move the window to `State::wanted`.
const MSG_MOVE: u32 = WM_APP + 6;
/// Shrinking switched on or off: window size and shape follow.
const MSG_SHRINK: u32 = WM_APP + 7;
/// Show / hide the yellow mark over the text being replaced.
const MSG_HIGHLIGHT: u32 = WM_APP + 8;
const MSG_UNHIGHLIGHT: u32 = WM_APP + 9;
const TIMER_CAPTURE: usize = 7;

/// The running click capture.
struct Capture {
    wanted: usize,
    points: Vec<(f64, f64)>,
    on_done: OnClicks,
    generation: u64,
    timeout_ms: u32,
}
static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
static CAPTURE_GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The mouse hook while a capture runs (window thread only).
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

pub fn capture_clicks(count: usize, timeout: Duration, on_done: OnClicks) -> bool {
    let generation = CAPTURE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut c) = CAPTURE.lock() {
        *c = Some(Capture {
            wanted: count.max(1),
            points: Vec::new(),
            on_done,
            generation,
            timeout_ms: timeout.as_millis().min(u32::MAX as u128) as u32,
        });
    }
    post(MSG_ARM);
    post(MSG_REDRAW);
    true
}

pub fn cancel_capture() -> bool {
    let had = CAPTURE.lock().map(|mut c| c.take().is_some()).unwrap_or(false);
    if let Ok(mut s) = state().lock() {
        s.replace_armed = false;
    }
    clear_highlight();
    post(MSG_DISARM);
    post(MSG_REDRAW);
    had
}

pub fn is_capturing() -> bool {
    CAPTURE.lock().map(|c| c.is_some()).unwrap_or(false)
}

/// Low-level mouse hook: left button down anywhere outside our own window
/// is one captured click.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 as u32 == WM_LBUTTONDOWN {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let (x, y) = (info.pt.x, info.pt.y);
        let own = {
            let (hwnd, list) = state().lock().map(|s| (s.hwnd, s.choices_hwnd)).unwrap_or((0, 0));
            let inside = |hwnd: isize| {
                let mut rect = RECT::default();
                hwnd > 0
                    && windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(HWND(hwnd as *mut _)).as_bool()
                    && GetWindowRect(HWND(hwnd as *mut _), &mut rect).is_ok()
                    && x >= rect.left
                    && x <= rect.right
                    && y >= rect.top
                    && y <= rect.bottom
            };
            // The list of candidates is ours as well: a click on it picks.
            inside(hwnd) || inside(list)
        };
        if !own {
            let done = CAPTURE.lock().ok().and_then(|mut c| {
                let cap = c.as_mut()?;
                cap.points.push((x as f64, y as f64));
                (cap.points.len() >= cap.wanted).then(|| c.take()).flatten()
            });
            if let Some(cap) = done {
                post(MSG_DISARM);
                post(MSG_REDRAW);
                let (points, on_done) = (cap.points, cap.on_done);
                std::thread::spawn(move || on_done(points));
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[derive(Default)]
struct State {
    /// HWND as integer (0 = not created yet).
    hwnd: isize,
    listening: bool,
    standby: bool,
    on_tap: Option<OnTap>,
    on_correct: Option<OnTap>,
    on_mode: Option<OnMode>,
    on_moved: Option<OnMoved>,
    /// The range button (above the start button) was pressed.
    on_next_range: Option<OnTap>,
    /// The switch: audio correction (true) or AI correction (false).
    audio_mode: bool,
    /// 0 idle, 1 listening (yellow arc), 2 processing (red arc).
    busy: u8,
    busy_since: Option<Instant>,
    /// Replace by voice armed: the replace button is yellow.
    replace_armed: bool,
    /// The start button is drawn grey (what it starts is not available).
    start_disabled: bool,
    /// The range button's range: 0 cursor to end, 1 whole text, 2 click to click, 3 single word.
    range_mode: u8,
    /// The highlight window (0 = none) and where it should be.
    highlight_hwnd: isize,
    highlight_rect: Option<(i32, i32, i32, i32)>,
    /// Last known top-left corner (screen pixels).
    position: Option<(f64, f64)>,
    /// Where MSG_MOVE should put the window.
    wanted: Option<(i32, i32)>,
    /// MSG_MOVE places by this anchor (on the screen of `wanted`).
    wanted_anchor: Option<crate::Anchor>,
    /// The window's anchor on its screen.
    anchor: Option<crate::Anchor>,
    /// The buttons' size with the mouse away at the last resize.
    last_shown: Option<(f64, f64)>,
    /// The line of help under the buttons and its window (0 = none).
    hint: Option<String>,
    hint_hwnd: isize,
    /// The list of candidates under the buttons (empty = none), its
    /// window (0 = not created yet) and who hears the pick.
    choices: Vec<String>,
    /// The click on the word the candidates belong to (screen pixels).
    choices_at: Option<(f64, f64)>,
    choices_hwnd: isize,
    on_choice: Option<crate::OnChoice>,
    /// The window's exact centre: what a size change keeps in place.
    centre: Option<(f64, f64)>,
    /// The buttons shrink when the mouse is not over them.
    shrink: bool,
    /// The mouse is over the window.
    hover: bool,
    /// The running size change: from, to, when it started.
    anim: Option<(f64, f64, Instant)>,
    /// Growing from one button: the middle of the button the mouse came
    /// to first, in the drawing's own measure (scale 1, origin top-left).
    pivot: (f64, f64),
    /// How far the window is moved right now (pixels) so that this button
    /// stays where it is while the buttons are grown; zero at rest. The
    /// window's own place is its rectangle without this.
    shift: (i32, i32),
}

/// The current layout: window size, drawing scale and offset, dimmed.
#[derive(Clone, Copy)]
struct Layout {
    width: i32,
    height: i32,
    scale: f64,
    dx: f64,
    dy: f64,
    /// 0 = resting (small, dark), 1 = full size and bright.
    bright: f64,
    with_correct: bool,
    /// Number of button columns (1–3).
    count: usize,
    /// An animation is still running.
    animating: bool,
}

fn target_scale(shrink: bool, hover: bool) -> f64 {
    if !shrink {
        base_scale()
    } else if hover {
        hover_scale()
    } else {
        idle_scale()
    }
}

fn layout() -> Layout {
    let (count, with_correct, shrink, hover, anim) = state()
        .lock()
        .map(|s| (button_count(&s), s.on_correct.is_some(), s.shrink, s.hover, s.anim))
        .unwrap_or((1, false, false, false, None));
    let content = width(count) as f64;
    let content_h = height(with_correct) as f64;
    let panel_scale = if shrink { hover_scale() } else { base_scale() };
    let target = target_scale(shrink, hover);
    let mut scale = target;
    let mut animating = false;
    if let Some((from, to, start)) = anim {
        let t = start.elapsed().as_secs_f64() * 1000.0 / ANIM_MS;
        if t < 1.0 && to == target {
            let eased = t * t * (3.0 - 2.0 * t);
            scale = from + (to - from) * eased;
            animating = true;
        }
    }
    let bright = if shrink && hover_scale() - idle_scale() < 1e-6 {
        // No growth: bright under the mouse, dark away from it.
        if hover {
            1.0
        } else {
            0.0
        }
    } else if shrink {
        ((scale - idle_scale()) / (hover_scale() - idle_scale()).max(1e-6)).clamp(0.0, 1.0)
    } else {
        1.0
    };
    // Two pixels of room: the disc regions are one pixel wider than the
    // drawing and must never touch the window's edge.
    let w = (content * panel_scale).ceil() + 2.0;
    let h = (content_h * panel_scale).ceil() + 2.0;
    Layout {
        width: w as i32,
        height: h as i32,
        scale,
        dx: (w - content * scale) / 2.0,
        dy: (h - content_h * scale) / 2.0,
        bright,
        with_correct,
        count,
        animating,
    }
}

/// Where the window belongs at the size drawn right now: the buttons grow
/// from the button under the mouse, not from their middle, so the window
/// moves with the growth. `None` when its place is not known.
unsafe fn follow_growth(hwnd: HWND, l: &Layout) -> Option<POINT> {
    let mut rect = RECT::default();
    GetWindowRect(hwnd, &mut rect).ok()?;
    let (rest, pivot, old) = state()
        .lock()
        .map(|s| (target_scale(s.shrink, false), s.pivot, s.shift))
        .ok()?;
    // The window's own place and the middle of the buttons at rest.
    let home = (rect.left - old.0, rect.top - old.1);
    let size = ((rect.right - rect.left) as f64, (rect.bottom - rect.top) as f64);
    let centre = (home.0 as f64 + size.0 / 2.0, home.1 as f64 + size.1 / 2.0);
    let screen = work_area(centre.0.round() as i32, centre.1.round() as i32, true).map(|(area, _)| area);
    let dx = crate::grow_shift(
        pivot.0,
        width(l.count) as f64,
        rest,
        l.scale,
        centre.0,
        screen.map(|a| (a.0, a.0 + a.2)),
    );
    let dy = crate::grow_shift(
        pivot.1,
        height(l.with_correct) as f64,
        rest,
        l.scale,
        centre.1,
        screen.map(|a| (a.1, a.1 + a.3)),
    );
    let shift = (dx.round() as i32, dy.round() as i32);
    if let Ok(mut s) = state().lock() {
        s.shift = shift;
    }
    Some(POINT {
        x: home.0 + shift.0,
        y: home.1 + shift.1,
    })
}

/// Start moving from the size drawn now to the current target (window
/// thread): a timer redraws every frame until the time is up.
unsafe fn start_animation(hwnd: HWND, from: f64) {
    let to = state().lock().map(|s| target_scale(s.shrink, s.hover)).unwrap_or(1.0);
    if (to - from).abs() < 1e-6 {
        return;
    }
    if let Ok(mut s) = state().lock() {
        s.anim = Some((from, to, Instant::now()));
    }
    SetTimer(Some(hwnd), TIMER_ANIM, 16, None);
    apply_shape(hwnd, layout());
    render(hwnd);
}

/// The layered bitmap's alpha is the window's shape now; drawing it anew
/// is all a size or state change needs.
unsafe fn apply_shape(hwnd: HWND, _l: Layout) {
    render(hwnd);
}

/// The plain size or the growth changed: the window takes its new size
/// (around its centre) and is drawn anew.
pub fn refresh_size() {
    post(MSG_SHRINK);
}

pub fn set_shrink(on: bool) {
    // The size drawn now is where the animation to the new size starts.
    let from = layout().scale;
    let changed = state()
        .lock()
        .map(|mut s| {
            if s.shrink == on {
                return false;
            }
            s.shrink = on;
            let to = target_scale(on, s.hover);
            s.anim = Some((from, to, Instant::now()));
            true
        })
        .unwrap_or(false);
    if changed {
        post(MSG_SHRINK);
    }
}

pub fn position() -> Option<(f64, f64)> {
    state().lock().ok().and_then(|s| s.position)
}

pub fn set_position(x: f64, y: f64) {
    if let Ok(mut s) = state().lock() {
        s.wanted = Some((x.round() as i32, y.round() as i32));
        s.wanted_anchor = None;
    }
    post(MSG_MOVE);
}

pub fn anchor() -> Option<crate::Anchor> {
    state().lock().ok().and_then(|s| s.anchor)
}

pub fn set_position_anchored(x: f64, y: f64, anchor: crate::Anchor) {
    if let Ok(mut s) = state().lock() {
        s.wanted = Some((x.round() as i32, y.round() as i32));
        s.wanted_anchor = Some(anchor);
    }
    post(MSG_MOVE);
}

/// The work area (without the taskbar) of the screen that holds the
/// point, and that screen's scaling (1.0 = 100 %); `or_primary`: of the
/// primary screen when the point is on none.
unsafe fn work_area(x: i32, y: i32, or_primary: bool) -> Option<(crate::Area, f64)> {
    let flags = if or_primary {
        MONITOR_DEFAULTTOPRIMARY
    } else {
        MONITOR_DEFAULTTONULL
    };
    let monitor = MonitorFromPoint(POINT { x, y }, flags);
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    let scale = if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() && dpi_x > 0 {
        dpi_x as f64 / 96.0
    } else {
        1.0
    };
    let r = info.rcWork;
    Some((
        (
            r.left as f64,
            r.top as f64,
            (r.right - r.left) as f64,
            (r.bottom - r.top) as f64,
        ),
        scale,
    ))
}

/// Remember the window's corner and tell the shell when it was a drag.
unsafe fn note_position(hwnd: HWND, dragged: bool) {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    // The window's own place, without the move that follows the growth.
    let shift = state().lock().map(|s| s.shift).unwrap_or((0, 0));
    rect.left -= shift.0;
    rect.right -= shift.0;
    rect.top -= shift.1;
    rect.bottom -= shift.1;
    let p = (rect.left as f64, rect.top as f64);
    let size = ((rect.right - rect.left) as f64, (rect.bottom - rect.top) as f64);
    let anchor = work_area((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2, true).map(|(screen, scale)| {
        // Edge distances are kept at 100 % scaling, so they look the
        // same on a screen that is scaled differently.
        let mut a = crate::anchor_for(p, size, screen);
        a.dx /= scale;
        a.dy /= scale;
        a
    });
    let moved = state().lock().ok().and_then(|mut s| {
        s.position = Some(p);
        s.centre = Some((p.0 + size.0 / 2.0, p.1 + size.1 / 2.0));
        s.anchor = anchor;
        if dragged {
            s.on_moved.clone()
        } else {
            None
        }
    });
    if let Some(moved) = moved {
        std::thread::spawn(move || moved(p.0, p.1));
    }
}

/// Window width at scale 1 for `n` buttons.
fn width(n: usize) -> i32 {
    SIZE * n as i32 + GAP * (n as i32 - 1)
}

/// Window height at scale 1: the correction column holds two buttons, the
/// range button above the start button; the others sit beside its middle.
fn height(with_correct: bool) -> i32 {
    if with_correct {
        2 * SIZE + GAP
    } else {
        SIZE
    }
}

/// How many button columns the window holds (read under the state lock).
fn button_count(s: &State) -> usize {
    1 + usize::from(s.on_correct.is_some()) + usize::from(s.on_mode.is_some())
}

pub fn set_range_mode(mode: RangeMode) {
    let v = match mode {
        RangeMode::CursorToEnd => 0,
        RangeMode::WholeText => 1,
        RangeMode::ClickToClick => 2,
        RangeMode::Word => 3,
        RangeMode::Suggest => 4,
    };
    if let Ok(mut s) = state().lock() {
        s.range_mode = v;
    }
    post(MSG_REDRAW);
}

pub fn set_busy(busy: Busy) {
    let v = match busy {
        Busy::None => 0,
        Busy::Listening => 1,
        Busy::Processing => 2,
    };
    if let Ok(mut s) = state().lock() {
        if s.busy == 0 && v != 0 {
            s.busy_since = Some(Instant::now());
        }
        s.busy = v;
    }
    post(MSG_SPIN);
    // The highlight window takes the colour too.
    post(MSG_HIGHLIGHT_REPAINT);
}

const MSG_HIGHLIGHT_REPAINT: u32 = WM_APP + 11;
/// Show, move or hide the line of help (`State::hint`).
const MSG_HINT: u32 = WM_APP + 12;
/// Show or hide the list of candidates (`State::choices`).
const MSG_CHOICES: u32 = WM_APP + 13;

pub fn set_audio_mode(on: bool) {
    if let Ok(mut s) = state().lock() {
        s.audio_mode = on;
    }
    post(MSG_REDRAW);
}

pub fn set_start_disabled(on: bool) {
    if let Ok(mut s) = state().lock() {
        s.start_disabled = on;
    }
    post(MSG_REDRAW);
}

pub fn set_replace_armed(on: bool) {
    if let Ok(mut s) = state().lock() {
        s.replace_armed = on;
    }
    if !on {
        clear_highlight();
    }
    post(MSG_REDRAW);
}

/// A translucent yellow mark at (`x`, `y`, `w`, `h`) screen pixels.
pub fn highlight(x: f64, y: f64, w: f64, h: f64) {
    if let Ok(mut s) = state().lock() {
        s.highlight_rect = Some((
            x.round() as i32 - 3,
            y.round() as i32 - 3,
            w.round() as i32 + 6,
            h.round() as i32 + 6,
        ));
    }
    post(MSG_HIGHLIGHT);
}

pub fn clear_highlight() {
    post(MSG_UNHIGHLIGHT);
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

fn post(msg: u32) {
    // 0: no window yet; -1: being created on its thread (that thread reads
    // the state itself once the window exists).
    let hwnd = state().lock().map(|s| s.hwnd).unwrap_or(0);
    if hwnd > 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn show(
    on_tap: OnTap,
    on_correct: Option<OnTap>,
    on_mode: Option<OnMode>,
    on_moved: Option<OnMoved>,
    on_next_range: Option<OnTap>,
) {
    let create = {
        let Ok(mut s) = state().lock() else { return };
        s.on_tap = Some(on_tap);
        s.on_correct = on_correct;
        s.on_mode = on_mode;
        s.on_moved = on_moved;
        s.on_next_range = on_next_range;
        s.hwnd == 0
    };
    if create {
        // Mark as being created so a second call does not start another thread.
        if let Ok(mut s) = state().lock() {
            s.hwnd = -1;
        }
        std::thread::Builder::new()
            .name("ikk-floating-mic".into())
            .spawn(run)
            .ok();
    } else {
        post(MSG_SHOW);
    }
}

pub fn hide() {
    post(MSG_HIDE);
}

pub fn set_listening(listening: bool) {
    if let Ok(mut s) = state().lock() {
        s.listening = listening;
    }
    post(MSG_REDRAW);
}

pub fn set_standby(standby: bool) {
    if let Ok(mut s) = state().lock() {
        s.standby = standby;
    }
    post(MSG_REDRAW);
}

/// The window's own thread: create, then pump messages.
fn run() {
    unsafe {
        let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
            return;
        };
        gdiplus_ready();
        let class = w!("ikk_floating_mic");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let l = layout();
        let w_px = l.width;
        // Right edge, upper third: out of the way of most text.
        let x = GetSystemMetrics(SM_CXSCREEN) - w_px - 32;
        let y = GetSystemMetrics(SM_CYSCREEN) / 3;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            class,
            w!("Dictation"),
            WS_POPUP,
            x,
            y,
            w_px,
            l.height,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            tracing::warn!("floating microphone button could not be created");
            if let Ok(mut s) = state().lock() {
                s.hwnd = 0;
            }
            return;
        };
        if let Ok(mut s) = state().lock() {
            s.hwnd = hwnd.0 as isize;
        }
        let _ = l;
        render(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        note_position(hwnd, false);
        SetTimer(Some(hwnd), TIMER_HOVER, 50, None);
        // Settings that arrived while the window was being created (the
        // shell switches shrinking on right after `show`): size and shape
        // follow the state as it is now.
        let _ = PostMessageW(Some(hwnd), MSG_SHRINK, WPARAM(0), LPARAM(0));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

thread_local! {
    /// Drag in progress: cursor and window position at mouse-down, moved yet.
    static DRAG: Cell<Option<(POINT, RECT, bool)>> = const { Cell::new(None) };
}

/// The switch was pressed: flip it and tell the shell.
unsafe fn flip_mode(hwnd: HWND) {
    let (audio, on_mode) = state()
        .lock()
        .ok()
        .map(|mut s| {
            s.audio_mode = !s.audio_mode;
            (s.audio_mode, s.on_mode.clone())
        })
        .unwrap_or((false, None));
    render(hwnd);
    if let Some(on_mode) = on_mode {
        std::thread::spawn(move || on_mode(audio));
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Clicking must not take focus from the app being dictated into.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            let mut cursor = POINT::default();
            let mut rect = RECT::default();
            if GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut rect).is_ok() {
                DRAG.set(Some((cursor, rect, false)));
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some((start, rect, moved)) = DRAG.get() {
                let mut now = POINT::default();
                if GetCursorPos(&mut now).is_ok() {
                    let (dx, dy) = (now.x - start.x, now.y - start.y);
                    if moved || dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD {
                        DRAG.set(Some((start, rect, true)));
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            rect.left + dx,
                            rect.top + dy,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_HOVER => {
            // Over the buttons as they are drawn right now? The window is
            // larger (invisible room for the growth) and fully transparent
            // there, so it is the drawing that counts, not the window.
            let l = layout();
            let mut cursor = POINT::default();
            let mut rect = RECT::default();
            if DRAG.get().is_none() && GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut rect).is_ok() {
                let (cx, cy) = (
                    (rect.left + rect.right) as f64 / 2.0,
                    (rect.top + rect.bottom) as f64 / 2.0,
                );
                let (hw, hh) = (
                    width(l.count) as f64 * l.scale / 2.0,
                    height(l.with_correct) as f64 * l.scale / 2.0,
                );
                let over = (cursor.x as f64 - cx).abs() <= hw && (cursor.y as f64 - cy).abs() <= hh;
                let changed = state()
                    .lock()
                    .map(|mut s| {
                        let changed = s.shrink && std::mem::replace(&mut s.hover, over) != over;
                        // Coming to buttons at rest: they grow from the
                        // button the mouse is on, so that one stays under
                        // the mouse. Buttons still on their way back keep
                        // the button they grew from.
                        if changed && over && (l.scale - idle_scale()).abs() < 1e-3 {
                            let at = (
                                (cursor.x as f64 - (cx - hw)) / l.scale,
                                (cursor.y as f64 - (cy - hh)) / l.scale,
                            );
                            s.pivot = crate::button_centre(at, l.count, l.with_correct, SIZE as f64, GAP as f64);
                        }
                        changed
                    })
                    .unwrap_or(false);
                if changed {
                    start_animation(hwnd, l.scale);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let drag = DRAG.take();
            let _ = ReleaseCapture();
            if let Some((_, _, true)) = drag {
                note_position(hwnd, true);
            }
            if let Some((start, rect, false)) = drag {
                // Which button: the column by the x position inside the
                // window, in equal slots. In the correction column the
                // upper half is the range button, the lower half the start
                // button; the other buttons count on their own height only.
                let l = layout();
                // Measured on the buttons as drawn, which sit in the middle
                // of a window that is larger than they are.
                let across =
                    ((start.x - rect.left) as f64 - l.width as f64 / 2.0) / (width(l.count) as f64 * l.scale) + 0.5;
                if !(0.0..1.0).contains(&across) {
                    return LRESULT(0);
                }
                let slot = (across * l.count as f64).floor().clamp(0.0, (l.count - 1) as f64) as usize;
                let which = match (slot, l.with_correct) {
                    (0, _) => 0,
                    (1, true) => 1,
                    _ => 2,
                };
                let y = (start.y - rect.top) as f64;
                let mid = l.height as f64 / 2.0;
                if which != 1 && (y - mid).abs() > SIZE as f64 * l.scale / 2.0 {
                    return LRESULT(0);
                }
                let (replace_armed, busy) = state()
                    .lock()
                    .map(|s| (s.replace_armed, s.busy != 0))
                    .unwrap_or((false, false));
                if which == 1 && y < mid {
                    // The range button: on to the next range, unless a
                    // correction is under way.
                    if is_capturing() || replace_armed || busy {
                        return LRESULT(0);
                    }
                    let next = state().lock().ok().and_then(|s| s.on_next_range.clone());
                    if let Some(next) = next {
                        // Off the window thread: it talks to the engine.
                        std::thread::spawn(move || next());
                    }
                    return LRESULT(0);
                }
                // The start button while waiting for clicks: cancel.
                if which == 1 && is_capturing() && !replace_armed {
                    cancel_capture();
                    return LRESULT(0);
                }
                if which == 2 {
                    // The switch: any press flips it.
                    flip_mode(hwnd);
                    return LRESULT(0);
                }
                let action = state().lock().ok().and_then(|s| match which {
                    0 => s.on_tap.clone(),
                    _ => s.on_correct.clone(),
                });
                if let Some(action) = action {
                    // Off the window thread: it talks to the engine.
                    std::thread::spawn(move || action());
                }
            }
            LRESULT(0)
        }
        WM_PAINT => {
            // A layered window shows its bitmap; nothing to paint here.
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        MSG_REDRAW => {
            render(hwnd);
            LRESULT(0)
        }
        MSG_HIDE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        MSG_SHOW => {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            LRESULT(0)
        }
        MSG_MOVE => {
            let (wanted, anchor) = state()
                .lock()
                .map(|s| (s.wanted, s.wanted_anchor))
                .unwrap_or((None, None));
            if let Some((x, y)) = wanted {
                let l = layout();
                let (x, y) = match anchor {
                    // By the anchor: on the screen that holds the corner
                    // it was taken at, else on the primary screen.
                    Some(a) => {
                        let centre = (x + l.width / 2, y + l.height / 2);
                        match work_area(centre.0, centre.1, false).or_else(|| work_area(0, 0, true)) {
                            Some((screen, scale)) => {
                                let mut a = a;
                                a.dx *= scale;
                                a.dy *= scale;
                                let (nx, ny) = crate::place(a, (l.width as f64, l.height as f64), screen);
                                (nx.round() as i32, ny.round() as i32)
                            }
                            None => (x, y),
                        }
                    }
                    // Kept on the screen: at least a corner inside the desktop.
                    None => {
                        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                        (x.clamp(0, (sw - l.width).max(0)), y.clamp(0, (sh - l.height).max(0)))
                    }
                };
                // A place of its own again; the next drawing moves it with
                // the growth if the buttons are grown.
                if let Ok(mut s) = state().lock() {
                    s.shift = (0, 0);
                }
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                note_position(hwnd, false);
            }
            LRESULT(0)
        }
        MSG_SHRINK => {
            // Resized around the centre, so the middle of the buttons
            // stays where it is (until a button would leave the screen); the shape follows, and the animation set
            // up by `set_shrink` runs.
            let l = layout();
            let mut rect = RECT::default();
            let centre = state().lock().ok().and_then(|s| s.centre).or_else(|| {
                GetWindowRect(hwnd, &mut rect).is_ok().then(|| {
                    (
                        (rect.left + rect.right) as f64 / 2.0,
                        (rect.top + rect.bottom) as f64 / 2.0,
                    )
                })
            });
            match centre {
                Some(c) => {
                    let screen = work_area(c.0.round() as i32, c.1.round() as i32, true).map(|(area, _)| area);
                    // What is seen with the mouse away: the edge of the
                    // screen counts for that, not for the room kept for
                    // the growth.
                    let rest = state().lock().map(|s| target_scale(s.shrink, false)).unwrap_or(1.0);
                    let shown = (width(l.count) as f64 * rest, height(l.with_correct) as f64 * rest);
                    let before = state()
                        .lock()
                        .ok()
                        .and_then(|mut s| s.last_shown.replace(shown))
                        .unwrap_or(shown);
                    let (x, y) = crate::around_centre(c, (l.width as f64, l.height as f64), shown, before, screen);
                    if let Ok(mut s) = state().lock() {
                        s.shift = (0, 0);
                    }
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        x.round() as i32,
                        y.round() as i32,
                        l.width,
                        l.height,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    note_position(hwnd, false);
                    // The exact centre is kept over many small steps (a
                    // slider), unless an edge pushed the window aside.
                    let unmoved =
                        (x + l.width as f64 / 2.0 - c.0).abs() < 1e-6 && (y + l.height as f64 / 2.0 - c.1).abs() < 1e-6;
                    if unmoved {
                        if let Ok(mut s) = state().lock() {
                            s.centre = Some(c);
                        }
                    }
                }
                None => {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        l.width,
                        l.height,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            apply_shape(hwnd, l);
            render(hwnd);
            SetTimer(Some(hwnd), TIMER_ANIM, 16, None);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            let l = layout();
            apply_shape(hwnd, l);
            render(hwnd);
            if !l.animating {
                let _ = KillTimer(Some(hwnd), TIMER_ANIM);
            }
            LRESULT(0)
        }
        MSG_CHOICES => {
            show_choices(hwnd);
            LRESULT(0)
        }
        MSG_HINT => {
            show_hint(hwnd);
            LRESULT(0)
        }
        MSG_HIGHLIGHT => {
            show_highlight();
            LRESULT(0)
        }
        MSG_HIGHLIGHT_REPAINT => {
            let hl = state().lock().map(|s| s.highlight_hwnd).unwrap_or(0);
            if hl != 0 {
                let _ = InvalidateRect(Some(HWND(hl as *mut _)), None, true);
            }
            LRESULT(0)
        }
        MSG_SPIN => {
            let busy = state().lock().map(|s| s.busy).unwrap_or(0);
            if busy != 0 {
                SetTimer(Some(hwnd), TIMER_SPIN, 50, None);
            } else {
                let _ = KillTimer(Some(hwnd), TIMER_SPIN);
            }
            render(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_SPIN => {
            render(hwnd);
            LRESULT(0)
        }
        MSG_UNHIGHLIGHT => {
            let hl = state().lock().map(|s| s.highlight_hwnd).unwrap_or(0);
            if hl != 0 {
                let _ = ShowWindow(HWND(hl as *mut _), SW_HIDE);
            }
            LRESULT(0)
        }
        MSG_ARM => {
            if HOOK.get().is_none() {
                let instance = GetModuleHandleW(PCWSTR::null()).ok();
                if let Ok(hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance.map(|i| i.into()), 0) {
                    HOOK.set(Some(hook));
                }
            }
            let timeout = state_capture_timeout();
            SetTimer(Some(hwnd), TIMER_CAPTURE, timeout, None);
            LRESULT(0)
        }
        MSG_DISARM => {
            if let Some(hook) = HOOK.take() {
                let _ = UnhookWindowsHookEx(hook);
            }
            let _ = KillTimer(Some(hwnd), TIMER_CAPTURE);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_CAPTURE => {
            // Nobody clicked (often enough) in time. The shell hears it with
            // the clicks that did come — too few to act on — so it can let
            // go of what it armed; otherwise it would stay armed for good.
            let cap = CAPTURE.lock().ok().and_then(|mut c| c.take());
            cancel_capture();
            if let Some(cap) = cap {
                let (points, on_done) = (cap.points, cap.on_done);
                std::thread::spawn(move || on_done(points));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn state_capture_timeout() -> u32 {
    CAPTURE
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|c| c.timeout_ms))
        .unwrap_or(20_000)
        .max(1_000)
}

/// GDI+ once per process.
fn gdiplus_ready() -> bool {
    static TOKEN: OnceLock<usize> = OnceLock::new();
    *TOKEN.get_or_init(|| unsafe {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token = 0usize;
        if GdiplusStartup(&mut token, &input, std::ptr::null_mut()).0 == 0 {
            token
        } else {
            0
        }
    }) != 0
}

/// 0x00BBGGRR → GDI+ 0xAARRGGBB, darkened towards [`DIM`] by `bright`.
fn argb(c: COLORREF, bright: f64) -> u32 {
    let f = DIM + (1.0 - DIM) * bright;
    let ch = |shift: u32| (((c.0 >> shift) & 0xFF) as f64 * f) as u32;
    0xFF00_0000 | (ch(0) << 16) | (ch(8) << 8) | ch(16)
}

/// Draw the panel into a 32-bit bitmap with GDI+ (anti-aliased, scaled
/// by the shrink animation) and show it as the layered window's content:
/// no flicker, no window region, smooth edges. Everything but the buttons
/// is fully transparent and lets clicks through; whether the mouse is over
/// the buttons is looked up by a timer (`TIMER_HOVER`).
unsafe fn render(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, GetDC, ReleaseDC, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    };
    use windows::Win32::Graphics::GdiPlus::*;
    use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};
    if !gdiplus_ready() {
        return;
    }
    let l = layout();
    let (listening, standby, replace_armed, with_replace, range_mode, audio_mode, busy, since) = state()
        .lock()
        .map(|s| {
            (
                s.listening,
                s.standby,
                s.replace_armed,
                s.on_mode.is_some(),
                s.range_mode,
                s.audio_mode,
                s.busy,
                s.busy_since,
            )
        })
        .unwrap_or((false, false, false, false, 0, false, 0, None));
    let (w, h) = (l.width.max(1), l.height.max(1));
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bmp = match CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        std::result::Result::Ok(b) => b,
        Err(_) => {
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return;
        }
    };
    let old_bmp = SelectObject(mem, bmp.into());
    let mut g: *mut GpGraphics = std::ptr::null_mut();
    if GdipCreateFromHDC(mem, &mut g).0 == 0 && !g.is_null() {
        GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        GdipSetTextRenderingHint(g, TextRenderingHintAntiAlias);
        // Nothing behind the buttons, ever: fully transparent.
        GdipGraphicsClear(g, 0x0000_0000);
        GdipTranslateWorldTransform(g, l.dx as f32, l.dy as f32, MatrixOrderPrepend);
        GdipScaleWorldTransform(g, l.scale as f32, l.scale as f32, MatrixOrderPrepend);
        let bright = l.bright;
        let white = 0xFFFF_FFFFu32;
        let glyph = {
            let v = (0xE0 as f64 + (0xFF - 0xE0) as f64 * bright) as u32;
            0xFF00_0000 | (v << 16) | (v << 8) | v
        };
        // Drawing helpers over the graphics.
        let fill_ellipse = |x: f32, y: f32, d: f32, c: u32| {
            let mut b: *mut GpSolidFill = std::ptr::null_mut();
            if GdipCreateSolidFill(c, &mut b).0 == 0 {
                GdipFillEllipse(g, b as *mut GpBrush, x, y, d, d);
                GdipDeleteBrush(b as *mut GpBrush);
            }
        };
        let stroke_ellipse = |x: f32, y: f32, d: f32, width: f32, c: u32| {
            let mut p: *mut GpPen = std::ptr::null_mut();
            if GdipCreatePen1(c, width, UnitPixel, &mut p).0 == 0 {
                GdipDrawEllipse(g, p, x, y, d, d);
                GdipDeletePen(p);
            }
        };
        let line = |x1: f32, y1: f32, x2: f32, y2: f32, width: f32, c: u32| {
            let mut p: *mut GpPen = std::ptr::null_mut();
            if GdipCreatePen1(c, width, UnitPixel, &mut p).0 == 0 {
                GdipSetPenStartCap(p, LineCapRound);
                GdipSetPenEndCap(p, LineCapRound);
                GdipDrawLine(g, p, x1, y1, x2, y2);
                GdipDeletePen(p);
            }
        };
        let dot = |x: f32, y: f32, r: f32, c: u32| fill_ellipse(x - r, y - r, 2.0 * r, c);
        let star = |sx: f32, sy: f32, r: f32, c: u32| {
            let inner = r * 0.38;
            let pts: Vec<PointF> = (0..8)
                .map(|i| {
                    let a = std::f32::consts::FRAC_PI_4 * i as f32;
                    let rr = if i % 2 == 0 { r } else { inner };
                    PointF {
                        X: sx + rr * a.cos(),
                        Y: sy - rr * a.sin(),
                    }
                })
                .collect();
            let mut b: *mut GpSolidFill = std::ptr::null_mut();
            if GdipCreateSolidFill(c, &mut b).0 == 0 {
                GdipFillPolygon2(g, b as *mut GpBrush, pts.as_ptr(), pts.len() as i32);
                GdipDeleteBrush(b as *mut GpBrush);
            }
        };
        let text = |t: &str, family: PCWSTR, size: f32, bold: bool, rect: RectF, c: u32| {
            let mut fam: *mut GpFontFamily = std::ptr::null_mut();
            if GdipCreateFontFamilyFromName(family, std::ptr::null_mut(), &mut fam).0 != 0 || fam.is_null() {
                return;
            }
            let mut font: *mut GpFont = std::ptr::null_mut();
            let style = if bold { FontStyleBold.0 } else { 0 };
            if GdipCreateFont(fam, size, style, UnitPixel, &mut font).0 == 0 {
                let mut fmt: *mut GpStringFormat = std::ptr::null_mut();
                if GdipCreateStringFormat(0, 0, &mut fmt).0 == 0 {
                    GdipSetStringFormatAlign(fmt, StringAlignmentCenter);
                    GdipSetStringFormatLineAlign(fmt, StringAlignmentCenter);
                    let mut b: *mut GpSolidFill = std::ptr::null_mut();
                    if GdipCreateSolidFill(c, &mut b).0 == 0 {
                        let utf16: Vec<u16> = t.encode_utf16().chain(std::iter::once(0)).collect();
                        GdipDrawString(g, PCWSTR(utf16.as_ptr()), -1, font, &rect, fmt, b as *const GpBrush);
                        GdipDeleteBrush(b as *mut GpBrush);
                    }
                    GdipDeleteStringFormat(fmt);
                }
                GdipDeleteFont(font);
            }
            GdipDeleteFontFamily(fam);
        };
        let (size, ring) = (SIZE as f32, RING as f32);
        let inner = size - ring;
        // The microphone and the switch sit beside the middle of the
        // correction column (two buttons high).
        let oy = ((height(l.with_correct) - SIZE) / 2) as f32;
        // --- the microphone button
        GdipTranslateWorldTransform(g, 0.0, oy, MatrixOrderPrepend);
        let mic = if listening {
            RED
        } else if standby {
            BLUE
        } else {
            GRAY
        };
        fill_ellipse(ring / 2.0, ring / 2.0, inner, argb(mic, bright));
        stroke_ellipse(ring / 2.0, ring / 2.0, inner, ring, glyph);
        text(
            "\u{E720}",
            w!("Segoe MDL2 Assets"),
            30.0,
            false,
            RectF {
                X: 0.0,
                Y: 0.0,
                Width: size,
                Height: size,
            },
            glyph,
        );
        GdipTranslateWorldTransform(g, 0.0, -oy, MatrixOrderPrepend);
        // --- the correction column: above, the range button (wand over
        // the range glyph; a click moves the range on); below, the start
        // button (a click corrects that range).
        if l.with_correct {
            let x0 = (SIZE + GAP) as f32;
            let y1 = (SIZE + GAP) as f32;
            let face = if replace_armed {
                YELLOW
            } else if is_capturing() {
                ORANGE
            } else if state().lock().map(|s| s.start_disabled).unwrap_or(false) {
                // Not available right now: light grey.
                DISABLED
            } else {
                BRAND
            };
            fill_ellipse(x0 + ring / 2.0, ring / 2.0, inner, argb(BRAND, bright));
            stroke_ellipse(x0 + ring / 2.0, ring / 2.0, inner, ring, glyph);
            fill_ellipse(x0 + ring / 2.0, y1 + ring / 2.0, inner, argb(face, bright));
            stroke_ellipse(x0 + ring / 2.0, y1 + ring / 2.0, inner, ring, glyph);
            if busy != 0 {
                let t = since.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                let start = ((t * 400.0) % 360.0) as f32;
                let inset = ring + 3.0;
                let mut p: *mut GpPen = std::ptr::null_mut();
                let c = argb(if busy == 1 { COLOR_LISTEN_ARC } else { RED }, 1.0);
                if GdipCreatePen1(c, 6.0, UnitPixel, &mut p).0 == 0 {
                    GdipSetPenStartCap(p, LineCapRound);
                    GdipSetPenEndCap(p, LineCapRound);
                    GdipDrawArc(
                        g,
                        p,
                        x0 + inset,
                        y1 + inset,
                        size - 2.0 * inset,
                        size - 2.0 * inset,
                        start,
                        120.0,
                    );
                    GdipDeletePen(p);
                }
            }
            let (cx, cy) = (x0 + size / 2.0, size / 2.0);
            // The start button's glyph: a mouse pointer — "now click in
            // the text". Its outline, in round strokes.
            {
                let ink = if replace_armed { argb(INK, 1.0) } else { glyph };
                let sy = y1 + size / 2.0;
                let pts = crate::POINTER;
                for i in 0..pts.len() {
                    let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                    line(
                        cx + a.0 as f32,
                        sy + a.1 as f32,
                        cx + b.0 as f32,
                        sy + b.1 as f32,
                        3.4,
                        ink,
                    );
                }
            }
            let ink = glyph;
            let (wx, wy) = (cx, cy - 6.0);
            line(wx - 10.0, wy + 9.0, wx + 3.0, wy - 4.0, 3.6, ink);
            star(wx + 8.0, wy - 9.0, 6.5, ink);
            star(wx + 1.0, wy - 13.0, 2.6, ink);
            star(wx + 13.0, wy - 2.0, 2.6, ink);
            let gy = cy + 17.0;
            match range_mode {
                2 => {
                    line(cx - 11.0, gy - 4.0, cx - 11.0, gy + 4.0, 2.4, ink);
                    line(cx - 11.0, gy, cx + 11.0, gy, 2.0, ink);
                    line(cx + 11.0, gy - 4.0, cx + 11.0, gy + 4.0, 2.4, ink);
                }
                1 => {
                    line(cx - 11.0, gy - 5.0, cx + 11.0, gy - 5.0, 2.0, ink);
                    line(cx - 11.0, gy, cx + 11.0, gy, 2.0, ink);
                    line(cx - 11.0, gy + 5.0, cx + 1.0, gy + 5.0, 2.0, ink);
                }
                // Single word: a word bar with an arrow pointing down at it.
                3 => {
                    line(cx - 8.0, gy + 4.0, cx + 8.0, gy + 4.0, 3.0, ink);
                    line(cx, gy - 8.0, cx, gy - 0.5, 2.2, ink);
                    line(cx - 4.0, gy - 4.5, cx, gy - 0.5, 2.2, ink);
                    line(cx + 4.0, gy - 4.5, cx, gy - 0.5, 2.2, ink);
                }
                // Word suggestions: a word bar and, under it, the wide arrow
                // of a list that drops down from the word.
                4 => {
                    line(cx - 12.0, gy - 4.5, cx + 12.0, gy - 4.5, 3.0, ink);
                    line(cx - 7.0, gy - 0.5, cx, gy + 5.5, 2.4, ink);
                    line(cx, gy + 5.5, cx + 7.0, gy - 0.5, 2.4, ink);
                }
                _ => {
                    line(cx - 12.0, gy - 4.0, cx - 12.0, gy + 4.0, 2.4, ink);
                    line(cx - 12.0, gy, cx + 1.0, gy, 2.0, ink);
                    dot(cx + 5.0, gy, 1.4, ink);
                    dot(cx + 9.0, gy, 1.4, ink);
                    dot(cx + 13.0, gy, 1.4, ink);
                }
            }
        }
        // --- the switch: a disc cut in two, "AI" above, "Audio" below
        if with_replace {
            GdipTranslateWorldTransform(g, 0.0, oy, MatrixOrderPrepend);
            let x0 = ((SIZE + GAP) * if l.with_correct { 2 } else { 1 }) as f32;
            fill_ellipse(x0 + ring / 2.0, ring / 2.0, inner, argb(GRAY, bright));
            let mut path: *mut GpPath = std::ptr::null_mut();
            if GdipCreatePath(FillModeAlternate, &mut path).0 == 0 {
                GdipAddPathEllipse(path, x0 + ring, ring, size - 2.0 * ring, size - 2.0 * ring);
                GdipSetClipPath(g, path, CombineModeReplace);
                let mut b: *mut GpSolidFill = std::ptr::null_mut();
                if GdipCreateSolidFill(argb(BRAND, bright), &mut b).0 == 0 {
                    let y = if audio_mode { size / 2.0 } else { 0.0 };
                    GdipFillRectangle(g, b as *mut GpBrush, x0, y, size, size / 2.0);
                    GdipDeleteBrush(b as *mut GpBrush);
                }
                GdipResetClip(g);
                GdipDeletePath(path);
            }
            line(
                x0 + ring + 2.0,
                size / 2.0,
                x0 + size - ring - 2.0,
                size / 2.0,
                1.0,
                0xFF70_7070,
            );
            stroke_ellipse(x0 + ring / 2.0, ring / 2.0, inner, ring, glyph);
            let off = 0xFFB8_B8B8;
            text(
                "AI",
                w!("Segoe UI"),
                12.0,
                true,
                RectF {
                    X: x0,
                    Y: 2.0,
                    Width: size,
                    Height: size / 2.0 - 2.0,
                },
                if audio_mode { off } else { white },
            );
            text(
                "Audio",
                w!("Segoe UI"),
                12.0,
                true,
                RectF {
                    X: x0,
                    Y: size / 2.0,
                    Width: size,
                    Height: size / 2.0 - 2.0,
                },
                if audio_mode { white } else { off },
            );
            GdipTranslateWorldTransform(g, 0.0, -oy, MatrixOrderPrepend);
        }
        GdipDeleteGraphics(g);
    }
    // Hand the bitmap to the window manager.
    let src = POINT { x: 0, y: 0 };
    let size = windows::Win32::Foundation::SIZE { cx: w, cy: h };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // Its place goes along with the picture, in one step: the button under
    // the mouse does not wobble while the others grow away from it.
    let place = follow_growth(hwnd, &l);
    let _ = UpdateLayeredWindow(
        hwnd,
        None,
        place.as_ref().map(|p| p as *const POINT),
        Some(&size),
        Some(mem),
        Some(&src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    SelectObject(mem, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}

/// The yellow mark: a layered, click-through, topmost window painted in
/// one colour and shown half transparent at the remembered rectangle.
unsafe fn show_highlight() {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetLayeredWindowAttributes, LWA_ALPHA, WS_EX_LAYERED, WS_EX_TRANSPARENT,
    };
    let (mut hl, rect) = state()
        .lock()
        .map(|s| (s.highlight_hwnd, s.highlight_rect))
        .unwrap_or((0, None));
    let Some((x, y, w, h)) = rect else { return };
    if hl == 0 {
        let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
            return;
        };
        let class = w!("ikk_highlight");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(highlight_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hbrBackground: CreateSolidBrush(YELLOW),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            class,
            w!("Replacing"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 110, LWA_ALPHA);
        hl = hwnd.0 as isize;
        if let Ok(mut s) = state().lock() {
            s.highlight_hwnd = hl;
        }
    }
    let hwnd = HWND(hl as *mut _);
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
}

unsafe extern "system" fn highlight_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            // Yellow while listening, red while processing.
            let busy = state().lock().map(|s| s.busy).unwrap_or(0);
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let brush = CreateSolidBrush(if busy == 2 { RED } else { YELLOW });
            let mut rect = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            let _ = FillRect(hdc, &rect, brush);
            let _ = DeleteObject(brush.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// A line of help under the buttons, or `None` to take it away.
pub fn set_hint(text: Option<String>) {
    if let Ok(mut s) = state().lock() {
        s.hint = text;
    }
    post(MSG_HINT);
}

/// A list of candidates under the buttons, or `None` to take it away.
pub fn set_choices(choices: Option<crate::Choices>) {
    if let Ok(mut s) = state().lock() {
        match choices {
            Some((items, at, on_pick)) => {
                s.choices = items;
                s.choices_at = at;
                s.on_choice = Some(on_pick);
            }
            None => {
                s.choices.clear();
                s.on_choice = None;
            }
        }
    }
    post(MSG_CHOICES);
}

/// Height of one candidate and the space beside its text, at scale 1.
const CHOICE_ROW: f64 = 34.0;
const CHOICE_PAD: f64 = 14.0;

/// Window thread: the white list with `State::choices`, one per row,
/// right under the buttons as they are drawn (above them when there is
/// no room below). It takes clicks without taking the focus.
unsafe fn show_choices(panel: HWND) {
    use windows::Win32::Graphics::Gdi::{DrawTextW, GetDC, ReleaseDC, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE};
    let (mut hw, items, at) = state()
        .lock()
        .map(|s| (s.choices_hwnd, s.choices.clone(), s.choices_at))
        .unwrap_or((0, Vec::new(), None));
    if items.is_empty() {
        if hw != 0 {
            let _ = ShowWindow(HWND(hw as *mut _), SW_HIDE);
        }
        return;
    }
    let mut pr = RECT::default();
    if GetWindowRect(panel, &mut pr).is_err() {
        return;
    }
    // Under the clicked word, or under the buttons when none was clicked.
    let (cx, cy) = match at {
        Some((x, y)) => (x.round() as i32, y.round() as i32),
        None => ((pr.left + pr.right) / 2, (pr.top + pr.bottom) / 2),
    };
    let (area, scale) = work_area(cx, cy, true).unwrap_or(((0.0, 0.0, 1e9, 1e9), 1.0));
    if hw == 0 {
        let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
            return;
        };
        let class = w!("ikk_choices");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(choices_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hbrBackground: CreateSolidBrush(COLORREF(0x00FF_FFFF)),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("Suggestions"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        hw = hwnd.0 as isize;
        if let Ok(mut s) = state().lock() {
            s.choices_hwnd = hw;
        }
    }
    let hwnd = HWND(hw as *mut _);
    // The widest candidate in the list's font.
    let dc = GetDC(Some(hwnd));
    let font = hint_font(scale);
    let old = SelectObject(dc, font.into());
    let mut widest = 0;
    for text in &items {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut r = RECT::default();
        DrawTextW(dc, &mut wide, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
        widest = widest.max(r.right - r.left);
    }
    SelectObject(dc, old);
    let _ = DeleteObject(font.into());
    ReleaseDC(Some(hwnd), dc);
    let row = (CHOICE_ROW * scale).round() as i32;
    let (w, h) = (
        (widest + (2.0 * CHOICE_PAD * scale) as i32).max((180.0 * scale) as i32),
        row * items.len() as i32,
    );
    // From the word's lower edge; from the middle of the buttons otherwise.
    let half = match at {
        Some(_) => 0,
        None => {
            let l = layout();
            (height(l.with_correct) as f64 * l.scale / 2.0).round() as i32
        }
    };
    let gap = (6.0 * scale) as i32;
    let mut y = cy + half + gap;
    if (y + h) as f64 > area.1 + area.3 {
        // No room below: above the word (a line of text up) or the buttons.
        let up = if at.is_some() {
            (26.0 * scale).round() as i32
        } else {
            half
        };
        y = cy - up - gap - h;
    }
    let x = ((cx - w / 2) as f64).clamp(area.0, (area.0 + area.2 - w as f64).max(area.0)) as i32;
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = InvalidateRect(Some(hwnd), None, true);
}

unsafe extern "system" fn choices_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::Graphics::Gdi::{
        DrawTextW, FrameRect, SetBkMode, SetTextColor, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, TRANSPARENT,
    };
    let scale_here = || {
        let mut here = RECT::default();
        if GetWindowRect(hwnd, &mut here).is_ok() {
            work_area(here.left, here.top, true).map_or(1.0, |(_, s)| s)
        } else {
            1.0
        }
    };
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let row = ((CHOICE_ROW * scale_here()).round() as i32).max(1);
            let (picked, on_pick) = state()
                .lock()
                .map(|s| (s.choices.get((y / row).max(0) as usize).cloned(), s.on_choice.clone()))
                .unwrap_or((None, None));
            set_choices(None);
            if let (Some(text), Some(on_pick)) = (picked, on_pick) {
                std::thread::spawn(move || on_pick(text));
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let items = state().lock().map(|s| s.choices.clone()).unwrap_or_default();
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rect = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            let _ = FillRect(hdc, &rect, white);
            let _ = DeleteObject(white.into());
            let scale = scale_here();
            let row = (CHOICE_ROW * scale).round() as i32;
            let pad = (CHOICE_PAD * scale).round() as i32;
            let font = hint_font(scale);
            let old = SelectObject(hdc, font.into());
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x0000_0000));
            let line = CreateSolidBrush(COLORREF(0x00E0_E0E0));
            for (i, text) in items.iter().enumerate() {
                let top = row * i as i32;
                if i > 0 {
                    let sep = RECT {
                        left: pad / 2,
                        top,
                        right: rect.right - pad / 2,
                        bottom: top + 1,
                    };
                    let _ = FillRect(hdc, &sep, line);
                }
                let mut r = RECT {
                    left: pad,
                    top,
                    right: rect.right - pad,
                    bottom: top + row,
                };
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut r,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
            let _ = DeleteObject(line.into());
            // Brand blue frame (#0099CC).
            let frame = CreateSolidBrush(COLORREF(0x00CC_9900));
            let _ = FrameRect(hdc, &rect, frame);
            let _ = DeleteObject(frame.into());
            SelectObject(hdc, old);
            let _ = DeleteObject(font.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

const HINT_BACK: COLORREF = COLORREF(0x0024_1F1F);

unsafe fn hint_font(scale: f64) -> windows::Win32::Graphics::Gdi::HFONT {
    use windows::Win32::Graphics::Gdi::{
        CreateFontW, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
    };
    CreateFontW(
        -((15.0 * scale).round() as i32),
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        0,
        w!("Segoe UI"),
    )
}

/// Window thread: the white label with a blue frame and `State::hint` in black right under the
/// buttons as they are drawn (above them when there is no room below);
/// clicks pass through it.
unsafe fn show_hint(panel: HWND) {
    use windows::Win32::Graphics::Gdi::{DrawTextW, GetDC, ReleaseDC, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE};
    use windows::Win32::UI::WindowsAndMessaging::{
        SetLayeredWindowAttributes, LWA_ALPHA, WS_EX_LAYERED, WS_EX_TRANSPARENT,
    };
    let (mut hw, text) = state()
        .lock()
        .map(|s| (s.hint_hwnd, s.hint.clone()))
        .unwrap_or((0, None));
    let Some(text) = text else {
        if hw != 0 {
            let _ = ShowWindow(HWND(hw as *mut _), SW_HIDE);
        }
        return;
    };
    let mut pr = RECT::default();
    if GetWindowRect(panel, &mut pr).is_err() {
        return;
    }
    let (cx, cy) = ((pr.left + pr.right) / 2, (pr.top + pr.bottom) / 2);
    let (area, scale) = work_area(cx, cy, true).unwrap_or(((0.0, 0.0, 1e9, 1e9), 1.0));
    if hw == 0 {
        let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
            return;
        };
        let class = w!("ikk_hint");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(hint_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hbrBackground: CreateSolidBrush(HINT_BACK),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            class,
            w!("Help"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        hw = hwnd.0 as isize;
        if let Ok(mut s) = state().lock() {
            s.hint_hwnd = hw;
        }
    }
    let hwnd = HWND(hw as *mut _);
    // How wide the text is in the label's font.
    let dc = GetDC(Some(hwnd));
    let font = hint_font(scale);
    let old = SelectObject(dc, font.into());
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT::default();
    DrawTextW(dc, &mut wide, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(dc, old);
    let _ = DeleteObject(font.into());
    ReleaseDC(Some(hwnd), dc);
    let (w, h) = (
        r.right - r.left + (28.0 * scale) as i32,
        r.bottom - r.top + (14.0 * scale) as i32,
    );
    let l = layout();
    let half = (height(l.with_correct) as f64 * l.scale / 2.0).round() as i32;
    let gap = (8.0 * scale) as i32;
    let mut y = cy + half + gap;
    if (y + h) as f64 > area.1 + area.3 {
        y = cy - half - gap - h;
    }
    let x = ((cx - w / 2) as f64).clamp(area.0, (area.0 + area.2 - w as f64).max(area.0)) as i32;
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = InvalidateRect(Some(hwnd), None, true);
}

unsafe extern "system" fn hint_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::Graphics::Gdi::{
        DrawTextW, SetBkMode, SetTextColor, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, TRANSPARENT,
    };
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let text = state().lock().ok().and_then(|s| s.hint.clone()).unwrap_or_default();
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rect = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            // White with a thin brand-blue frame and black text: the same
            // look as the list of word suggestions.
            let brush = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            let _ = FillRect(hdc, &rect, brush);
            let _ = DeleteObject(brush.into());
            let frame = CreateSolidBrush(COLORREF(0x00CC_9900));
            let _ = windows::Win32::Graphics::Gdi::FrameRect(hdc, &rect, frame);
            let _ = DeleteObject(frame.into());
            let scale = work_area(0, 0, true).map_or(1.0, |(_, s)| s);
            let mut here = RECT::default();
            let scale = if GetWindowRect(hwnd, &mut here).is_ok() {
                work_area(here.left, here.top, true).map_or(scale, |(_, s)| s)
            } else {
                scale
            };
            let font = hint_font(scale);
            let old = SelectObject(hdc, font.into());
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x0000_0000));
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(
                hdc,
                &mut wide,
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            let _ = DeleteObject(font.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
