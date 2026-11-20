//! Win32 implementation: a round, topmost tool window that never activates.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, SelectObject, PAINTSTRUCT,
};
use windows::Win32::Graphics::GdiPlus::{GdiplusStartup, GdiplusStartupInput};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, ReleaseCapture, SetCapture, TrackMouseEvent, UnregisterHotKey, MOD_NOREPEAT, TME_LEAVE,
    TRACKMOUSEEVENT, VK_F8, VK_F9,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowRect, KillTimer, LoadCursorW, PostMessageW, RegisterClassW, SetTimer, SetWindowPos, SetWindowsHookExW,
    ShowWindow, TranslateMessage, UnhookWindowsHookEx, HHOOK, HWND_TOPMOST, IDC_HAND, MA_NOACTIVATE, MSG,
    MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNOACTIVATE, WH_MOUSE_LL, WM_APP, WM_ERASEBKGND, WM_HOTKEY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::{Busy, OnClicks, OnMode, OnMoved, OnTap, RangeMode};

const SIZE: i32 = 64;
const RING: i32 = 3;
/// Space between the microphone and the correction button.
const GAP: i32 = 10;
const DRAG_THRESHOLD: i32 = 4;
/// Sent once after `TrackMouseEvent(TME_LEAVE)` when the mouse has left
/// (the `windows` crate files it under the common controls).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Shrinking buttons: drawn this much larger than [`SIZE`] under the
/// mouse, and this much smaller (and darker) when the mouse is elsewhere.
const HOVER_SCALE: f64 = 1.15;
const IDLE_SCALE: f64 = 0.72;
/// How much darker the idle buttons are drawn (multiplied into the colour).
const DIM: f64 = 0.8;
/// Growing and shrinking take this long, eased at both ends; redrawn
/// every 16 ms by a window timer.
const ANIM_MS: f64 = 340.0;
const TIMER_ANIM: usize = 8;
/// Redraws the rotating arc while the correction button is busy.
const TIMER_SPIN: usize = 9;
const MSG_SPIN: u32 = WM_APP + 10;
/// While the panel shows, in any application: F8 steps through the four
/// correction ranges, F9 flips the AI/Audio switch (the keys do not reach
/// the application).
const HOTKEY_RANGE: i32 = 1;
const HOTKEY_MODE: i32 = 2;
// COLORREF is 0x00BBGGRR.
const BLUE: COLORREF = COLORREF(0x00B3_8600); // #0086B3, brand primary: listening for the start word
const BRAND: COLORREF = COLORREF(0x00CC_9900); // #0099CC: the correction button
const ORANGE: COLORREF = COLORREF(0x0000_6CEF); // #EF6C00: waiting for the two clicks
const YELLOW: COLORREF = COLORREF(0x0000_C7F9); // #F9C700: replace by voice armed
const INK: COLORREF = COLORREF(0x0000_2E3A); // #3A2E00: glyph on the yellow button
const COLOR_LISTEN_ARC: COLORREF = COLORREF(0x00AD_7800); // #0078AD
const RED: COLORREF = COLORREF(0x002F_2FD3); // #D32F2F: dictating
const GRAY: COLORREF = COLORREF(0x0042_4242); // #424242: off

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
            let hwnd = state().lock().map(|s| s.hwnd).unwrap_or(0);
            let mut rect = RECT::default();
            hwnd > 0
                && GetWindowRect(HWND(hwnd as *mut _), &mut rect).is_ok()
                && x >= rect.left
                && x <= rect.right
                && y >= rect.top
                && y <= rect.bottom
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
    /// F8 anywhere while the buttons are shown.
    on_next_range: Option<OnTap>,
    /// The switch: audio correction (true) or AI correction (false).
    audio_mode: bool,
    /// 0 idle, 1 listening (yellow arc), 2 processing (red arc).
    busy: u8,
    busy_since: Option<Instant>,
    /// Replace by voice armed: the replace button is yellow.
    replace_armed: bool,
    /// The correction button's range: 0 cursor to end, 1 whole text, 2 click to click.
    range_mode: u8,
    /// The highlight window (0 = none) and where it should be.
    highlight_hwnd: isize,
    highlight_rect: Option<(i32, i32, i32, i32)>,
    /// Last known top-left corner (screen pixels).
    position: Option<(f64, f64)>,
    /// Where MSG_MOVE should put the window.
    wanted: Option<(i32, i32)>,
    /// The buttons shrink when the mouse is not over them.
    shrink: bool,
    /// The mouse is over the window.
    hover: bool,
    /// The running size change: from, to, when it started.
    anim: Option<(f64, f64, Instant)>,
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
    /// Number of buttons (1–3).
    count: usize,
    /// An animation is still running.
    animating: bool,
}

fn target_scale(shrink: bool, hover: bool) -> f64 {
    if !shrink {
        1.0
    } else if hover {
        HOVER_SCALE
    } else {
        IDLE_SCALE
    }
}

fn layout() -> Layout {
    let (count, with_correct, shrink, hover, anim) = state()
        .lock()
        .map(|s| (button_count(&s), s.on_correct.is_some(), s.shrink, s.hover, s.anim))
        .unwrap_or((1, false, false, false, None));
    let content = width(count) as f64;
    let panel_scale = if shrink { HOVER_SCALE } else { 1.0 };
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
    let bright = if shrink {
        ((scale - IDLE_SCALE) / (HOVER_SCALE - IDLE_SCALE)).clamp(0.0, 1.0)
    } else {
        1.0
    };
    // Two pixels of room: the disc regions are one pixel wider than the
    // drawing and must never touch the window's edge.
    let w = (content * panel_scale).ceil() + 2.0;
    let h = (SIZE as f64 * panel_scale).ceil() + 2.0;
    Layout {
        width: w as i32,
        height: h as i32,
        scale,
        dx: (w - content * scale) / 2.0,
        dy: (h - SIZE as f64 * scale) / 2.0,
        bright,
        with_correct,
        count,
        animating,
    }
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
    }
    post(MSG_MOVE);
}

/// Remember the window's corner and tell the shell when it was a drag.
unsafe fn note_position(hwnd: HWND, dragged: bool) {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    let p = (rect.left as f64, rect.top as f64);
    let moved = state().lock().ok().and_then(|mut s| {
        s.position = Some(p);
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

/// How many buttons the window holds (read under the state lock).
fn button_count(s: &State) -> usize {
    1 + usize::from(s.on_correct.is_some()) + usize::from(s.on_mode.is_some())
}

pub fn set_range_mode(mode: RangeMode) {
    let v = match mode {
        RangeMode::CursorToEnd => 0,
        RangeMode::WholeText => 1,
        RangeMode::ClickToClick => 2,
        RangeMode::Word => 3,
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

pub fn set_audio_mode(on: bool) {
    if let Ok(mut s) = state().lock() {
        s.audio_mode = on;
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
        claim_keys(hwnd);
        note_position(hwnd, false);
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

/// Window thread only. Takes the panel's keys system-wide: F8 for the
/// correction range, F9 for the switch (each only when the panel has that
/// part). Another program may hold a key already: then that part stays as
/// it was, by click or in the settings.
unsafe fn claim_keys(hwnd: HWND) {
    let (range, mode) = state()
        .lock()
        .map(|s| (s.on_correct.is_some() && s.on_next_range.is_some(), s.on_mode.is_some()))
        .unwrap_or((false, false));
    // A second registration of the same id fails; drop the old ones first.
    release_keys(hwnd);
    if range {
        if let Err(e) = RegisterHotKey(Some(hwnd), HOTKEY_RANGE, MOD_NOREPEAT, VK_F8.0 as u32) {
            tracing::warn!(%e, "F8 is taken by another program; the range is chosen in the settings only");
        }
    }
    if mode {
        if let Err(e) = RegisterHotKey(Some(hwnd), HOTKEY_MODE, MOD_NOREPEAT, VK_F9.0 as u32) {
            tracing::warn!(%e, "F9 is taken by another program; the switch works by click only");
        }
    }
}

/// Window thread only. Unregistering what is not registered is harmless.
unsafe fn release_keys(hwnd: HWND) {
    let _ = UnregisterHotKey(Some(hwnd), HOTKEY_RANGE);
    let _ = UnregisterHotKey(Some(hwnd), HOTKEY_MODE);
}

/// The switch was pressed (click or F9): flip it and tell the shell.
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
            // Shrinking buttons: grow under the mouse, and be told when it leaves.
            let from = layout().scale;
            let entered = state()
                .lock()
                .map(|mut s| s.shrink && !std::mem::replace(&mut s.hover, true))
                .unwrap_or(false);
            if entered {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut track);
                start_animation(hwnd, from);
            }
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
        WM_MOUSELEAVE => {
            let from = layout().scale;
            let left = state()
                .lock()
                .map(|mut s| std::mem::replace(&mut s.hover, false))
                .unwrap_or(false);
            if left {
                start_animation(hwnd, from);
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
                // Which button: by the x position inside the window, in
                // equal slots.
                let l = layout();
                let slot = (((start.x - rect.left) as f64 / l.width as f64) * l.count as f64)
                    .floor()
                    .clamp(0.0, (l.count - 1) as f64) as usize;
                let which = match (slot, l.with_correct) {
                    (0, _) => 0,
                    (1, true) => 1,
                    _ => 2,
                };
                let replace_armed = state().lock().map(|s| s.replace_armed).unwrap_or(false);
                // The correction button while waiting for clicks: cancel.
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
            // No panel: F8 and F9 belong to the other programs again.
            release_keys(hwnd);
            LRESULT(0)
        }
        MSG_SHOW => {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            claim_keys(hwnd);
            LRESULT(0)
        }
        WM_HOTKEY if wparam.0 == HOTKEY_RANGE as usize => {
            // F8: the next correction range; the bridge steps and saves it.
            tracing::info!("F8: next correction range");
            let next = state().lock().ok().and_then(|s| s.on_next_range.clone());
            if let Some(next) = next {
                // Off the window thread: it talks to the engine.
                std::thread::spawn(move || next());
            }
            LRESULT(0)
        }
        WM_HOTKEY if wparam.0 == HOTKEY_MODE as usize => {
            tracing::info!("F9: correction switch");
            flip_mode(hwnd);
            LRESULT(0)
        }
        MSG_MOVE => {
            let wanted = state().lock().ok().and_then(|s| s.wanted);
            if let Some((x, y)) = wanted {
                // Kept on the screen: at least a corner inside the desktop.
                let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                let l = layout();
                let x = x.clamp(0, (sw - l.width).max(0));
                let y = y.clamp(0, (sh - l.height).max(0));
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
            // Resized around the top-left corner; the shape follows, and
            // the animation set up by `set_shrink` runs.
            let l = layout();
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                l.width,
                l.height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
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
/// no flicker, no window region, smooth edges. The whole panel is left at
/// alpha 1, so the mouse counts as over it everywhere (hover, drag).
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
        GdipGraphicsClear(g, 0x0100_0000);
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
        // --- the microphone button
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
        // --- the correction button: wand over the range glyph
        if l.with_correct {
            let x0 = (SIZE + GAP) as f32;
            let face = if replace_armed {
                YELLOW
            } else if is_capturing() {
                ORANGE
            } else {
                BRAND
            };
            fill_ellipse(x0 + ring / 2.0, ring / 2.0, inner, argb(face, bright));
            stroke_ellipse(x0 + ring / 2.0, ring / 2.0, inner, ring, glyph);
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
                        inset,
                        size - 2.0 * inset,
                        size - 2.0 * inset,
                        start,
                        120.0,
                    );
                    GdipDeletePen(p);
                }
            }
            let ink = if replace_armed { argb(INK, 1.0) } else { glyph };
            let (cx, cy) = (x0 + size / 2.0, size / 2.0);
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
                3 => {
                    line(cx - 7.0, gy + 2.0, cx + 7.0, gy + 2.0, 3.0, ink);
                    dot(cx, gy - 4.5, 2.0, ink);
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
    let _ = UpdateLayeredWindow(
        hwnd,
        None,
        None,
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
