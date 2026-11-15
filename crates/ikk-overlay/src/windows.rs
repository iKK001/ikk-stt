//! Win32 implementation: a round, topmost tool window that never activates.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateEllipticRgn, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW,
    Ellipse, EndPaint, FillRect, InvalidateRect, LineTo, MoveToEx, Polygon, SelectClipRgn, SelectObject, SetBkMode,
    SetGraphicsMode, SetTextColor, SetWindowRgn, SetWorldTransform, DT_CENTER, DT_SINGLELINE, DT_VCENTER, GM_ADVANCED,
    PAINTSTRUCT, PS_SOLID, RGN_OR, TRANSPARENT, XFORM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowRect, KillTimer, LoadCursorW, PostMessageW, RegisterClassW, SetTimer, SetWindowPos, SetWindowsHookExW,
    ShowWindow, TranslateMessage, UnhookWindowsHookEx, HHOOK, HWND_TOPMOST, IDC_HAND, MA_NOACTIVATE, MSG,
    MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNOACTIVATE, WH_MOUSE_LL, WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_PAINT,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
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
// COLORREF is 0x00BBGGRR.
const BLUE: COLORREF = COLORREF(0x00B3_8600); // #0086B3, brand primary: listening for the start word
const BRAND: COLORREF = COLORREF(0x00CC_9900); // #0099CC: the correction button
const ORANGE: COLORREF = COLORREF(0x0000_6CEF); // #EF6C00: waiting for the two clicks
const YELLOW: COLORREF = COLORREF(0x0000_C7F9); // #F9C700: replace by voice armed
const INK: COLORREF = COLORREF(0x0000_2E3A); // #3A2E00: glyph on the yellow button
const WHITE_FACE: COLORREF = COLORREF(0x00FF_FFFF);
const COLOR_LISTEN_ARC: COLORREF = COLORREF(0x00AD_7800); // #0078AD
const RED: COLORREF = COLORREF(0x002F_2FD3); // #D32F2F: dictating
const GRAY: COLORREF = COLORREF(0x0042_4242); // #424242: off

/// `c` darkened towards [`DIM`]: `bright` 1 = as is, 0 = fully dimmed.
fn dimmed(c: COLORREF, bright: f64) -> COLORREF {
    let f = DIM + (1.0 - DIM) * bright;
    let ch = |shift: u32| (((c.0 >> shift) & 0xFF) as f64 * f) as u32;
    COLORREF(ch(0) | (ch(8) << 8) | (ch(16) << 16))
}

/// The ring and glyph colour: light grey at rest, white at full size.
fn glyph_colour(bright: f64) -> COLORREF {
    let v = (0xE0 as f64 + (0xFF - 0xE0) as f64 * bright) as u32;
    COLORREF(v | (v << 8) | (v << 16))
}

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
    let _ = InvalidateRect(Some(hwnd), None, true);
}

/// Round buttons: the window's shape is the drawn discs (one, or two
/// joined), where they are at the current scale.
unsafe fn apply_shape(hwnd: HWND, l: Layout) {
    let disc = |x0: f64| {
        let left = (l.dx + x0 * l.scale).floor() as i32;
        let top = l.dy.floor() as i32;
        let d = (SIZE as f64 * l.scale).ceil() as i32 + 1;
        CreateEllipticRgn(left, top, left + d, top + d)
    };
    let region = disc(0.0);
    for i in 1..l.count {
        let next = disc(((SIZE + GAP) * i as i32) as f64);
        let _ = CombineRgn(Some(region), Some(region), Some(next), RGN_OR);
        let _ = DeleteObject(next.into());
    }
    let _ = SetWindowRgn(hwnd, Some(region), true);
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

pub fn show(on_tap: OnTap, on_correct: Option<OnTap>, on_mode: Option<OnMode>, on_moved: Option<OnMoved>) {
    let create = {
        let Ok(mut s) = state().lock() else { return };
        s.on_tap = Some(on_tap);
        s.on_correct = on_correct;
        s.on_mode = on_mode;
        s.on_moved = on_moved;
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
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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
        apply_shape(hwnd, l);
        if let Ok(mut s) = state().lock() {
            s.hwnd = hwnd.0 as isize;
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
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
                    let (audio, on_mode) = state()
                        .lock()
                        .ok()
                        .map(|mut s| {
                            s.audio_mode = !s.audio_mode;
                            (s.audio_mode, s.on_mode.clone())
                        })
                        .unwrap_or((false, None));
                    let _ = InvalidateRect(Some(hwnd), None, true);
                    if let Some(on_mode) = on_mode {
                        std::thread::spawn(move || on_mode(audio));
                    }
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
            paint(hwnd);
            LRESULT(0)
        }
        MSG_REDRAW => {
            let _ = InvalidateRect(Some(hwnd), None, true);
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
            let _ = InvalidateRect(Some(hwnd), None, true);
            SetTimer(Some(hwnd), TIMER_ANIM, 16, None);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ANIM => {
            let l = layout();
            apply_shape(hwnd, l);
            let _ = InvalidateRect(Some(hwnd), None, true);
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
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_SPIN => {
            let _ = InvalidateRect(Some(hwnd), None, true);
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
            // Nobody clicked twice in time.
            cancel_capture();
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

unsafe fn paint(hwnd: HWND) {
    let (listening, standby) = state()
        .lock()
        .map(|s| (s.listening, s.standby))
        .unwrap_or((false, false));
    let l = layout();
    let with_correct = l.with_correct;
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    // Everything below is drawn at scale 1 (SIZE × SIZE per button); the
    // world transform scales and centres it. Idle shrunk buttons: darker.
    SetGraphicsMode(hdc, GM_ADVANCED);
    let xform = XFORM {
        eM11: l.scale as f32,
        eM12: 0.0,
        eM21: 0.0,
        eM22: l.scale as f32,
        eDx: l.dx as f32,
        eDy: l.dy as f32,
    };
    let _ = SetWorldTransform(hdc, &xform);
    let colour = |c: COLORREF| dimmed(c, l.bright);
    let white = glyph_colour(l.bright);
    let brush = CreateSolidBrush(colour(if listening {
        RED
    } else if standby {
        BLUE
    } else {
        GRAY
    }));
    let pen = CreatePen(PS_SOLID, RING, white);
    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());
    let _ = Ellipse(hdc, RING / 2, RING / 2, SIZE - RING / 2, SIZE - RING / 2);
    let replace_armed = state().lock().map(|s| s.replace_armed).unwrap_or(false);
    if with_correct {
        // The correction button: brand blue, orange while waiting for
        // clicks (yellow when the range is for a replacement by voice).
        let c = if replace_armed {
            YELLOW
        } else if is_capturing() {
            ORANGE
        } else {
            BRAND
        };
        let brush2 = CreateSolidBrush(colour(c));
        let prev = SelectObject(hdc, brush2.into());
        let x0 = SIZE + GAP;
        let _ = Ellipse(hdc, x0 + RING / 2, RING / 2, x0 + SIZE - RING / 2, SIZE - RING / 2);
        SelectObject(hdc, prev);
        let _ = DeleteObject(brush2.into());
    }
    // Glyphs from the Windows symbol font: microphone, check mark.
    let font = CreateFontW(
        30,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        w!("Segoe MDL2 Assets"),
    );
    let old_font = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, white);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: SIZE,
        bottom: SIZE,
    };
    let mut glyph: Vec<u16> = "\u{E720}".encode_utf16().collect();
    DrawTextW(hdc, &mut glyph, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    let (replace_armed, with_replace, range_mode, audio_mode) = state()
        .lock()
        .map(|s| (s.replace_armed, s.on_mode.is_some(), s.range_mode, s.audio_mode))
        .unwrap_or((false, false, 0, false));
    let line = |x1: i32, y1: i32, x2: i32, y2: i32, w: i32, c: COLORREF| {
        let pen = CreatePen(PS_SOLID, w, c);
        let old = SelectObject(hdc, pen.into());
        let _ = MoveToEx(hdc, x1, y1, None);
        let _ = LineTo(hdc, x2, y2);
        SelectObject(hdc, old);
        let _ = DeleteObject(pen.into());
    };
    let dot = |x: i32, y: i32, r: i32, c: COLORREF| {
        let b = CreateSolidBrush(c);
        let p = CreatePen(PS_SOLID, 1, c);
        let ob = SelectObject(hdc, b.into());
        let op = SelectObject(hdc, p.into());
        let _ = Ellipse(hdc, x - r, y - r, x + r, y + r);
        SelectObject(hdc, op);
        SelectObject(hdc, ob);
        let _ = DeleteObject(p.into());
        let _ = DeleteObject(b.into());
    };
    let star = |sx: i32, sy: i32, r: f64, c: COLORREF| {
        let inner = r * 0.38;
        let pts: Vec<POINT> = (0..8)
            .map(|i| {
                let a = std::f64::consts::FRAC_PI_4 * i as f64;
                let rr = if i % 2 == 0 { r } else { inner };
                POINT {
                    x: sx + (rr * a.cos()).round() as i32,
                    y: sy - (rr * a.sin()).round() as i32,
                }
            })
            .collect();
        let thin = CreatePen(PS_SOLID, 1, c);
        let brush = CreateSolidBrush(c);
        let op = SelectObject(hdc, thin.into());
        let ob = SelectObject(hdc, brush.into());
        let _ = Polygon(hdc, &pts);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(brush.into());
        let _ = DeleteObject(thin.into());
    };
    if with_correct {
        // The rotating arc while busy: a quarter turn every 0.9 s.
        let (busy, since) = state().lock().map(|s| (s.busy, s.busy_since)).unwrap_or((0, None));
        if busy != 0 {
            use windows::Win32::Graphics::Gdi::{Arc, SetArcDirection, AD_CLOCKWISE};
            let t = since.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
            let a0 = (t * 400.0) % 360.0;
            let a1 = a0 + 110.0;
            let (cx, cy, r) = (
                (SIZE + GAP + SIZE / 2) as f64,
                (SIZE / 2) as f64,
                (SIZE / 2 - RING - 3) as f64,
            );
            let pt = |a: f64| POINT {
                x: (cx + r * a.to_radians().cos()).round() as i32,
                y: (cy - r * a.to_radians().sin()).round() as i32,
            };
            // Listening: brand blue (shows on the yellow armed button); processing: red.
            let colour = if busy == 1 { COLOR_LISTEN_ARC } else { RED };
            let pen = CreatePen(PS_SOLID, 6, colour);
            let old = SelectObject(hdc, pen.into());
            let _ = SetArcDirection(hdc, AD_CLOCKWISE);
            let (p1, p2) = (pt(a1), pt(a0));
            let inset = RING + 3;
            let _ = Arc(
                hdc,
                SIZE + GAP + inset,
                inset,
                SIZE + GAP + SIZE - inset,
                SIZE - inset,
                p1.x,
                p1.y,
                p2.x,
                p2.y,
            );
            SelectObject(hdc, old);
            let _ = DeleteObject(pen.into());
        }
        // The correction button: the wand, a little smaller and higher, and
        // the range it corrects as small lines beneath it.
        let (cx, cy) = (SIZE + GAP + SIZE / 2, SIZE / 2);
        let ink = if replace_armed { INK } else { white };
        let (wx, wy) = (cx, cy - 6);
        line(wx - 10, wy + 9, wx + 3, wy - 4, 4, ink);
        star(wx + 8, wy - 9, 6.5, ink);
        star(wx + 1, wy - 13, 2.6, ink);
        star(wx + 13, wy - 2, 2.6, ink);
        let gy = cy + 17;
        match range_mode {
            2 => {
                line(cx - 11, gy - 4, cx - 11, gy + 4, 2, ink);
                line(cx - 11, gy, cx + 11, gy, 2, ink);
                line(cx + 11, gy - 4, cx + 11, gy + 4, 2, ink);
            }
            1 => {
                line(cx - 11, gy - 5, cx + 11, gy - 5, 2, ink);
                line(cx - 11, gy, cx + 11, gy, 2, ink);
                line(cx - 11, gy + 5, cx + 1, gy + 5, 2, ink);
            }
            3 => {
                line(cx - 7, gy + 2, cx + 7, gy + 2, 3, ink);
                dot(cx, gy - 4, 2, ink);
            }
            _ => {
                line(cx - 12, gy - 4, cx - 12, gy + 4, 2, ink);
                line(cx - 12, gy, cx + 1, gy, 2, ink);
                dot(cx + 5, gy, 1, ink);
                dot(cx + 9, gy, 1, ink);
                dot(cx + 13, gy, 1, ink);
            }
        }
    }
    if with_replace {
        // The switch: a white disc, the lit half brand blue — "AI" above,
        // "Audio" below.
        let x0 = (SIZE + GAP) * if with_correct { 2 } else { 1 };
        let dark_brush = CreateSolidBrush(colour(GRAY));
        let prev = SelectObject(hdc, dark_brush.into());
        let _ = Ellipse(hdc, x0 + RING / 2, RING / 2, x0 + SIZE - RING / 2, SIZE - RING / 2);
        SelectObject(hdc, prev);
        let _ = DeleteObject(dark_brush.into());
        // The lit half: clip to the disc, fill a rectangle.
        let disc = CreateEllipticRgn(x0 + RING / 2, RING / 2, x0 + SIZE - RING / 2, SIZE - RING / 2);
        let _ = SelectClipRgn(hdc, Some(disc));
        let lit = CreateSolidBrush(colour(BRAND));
        let rect = if audio_mode {
            RECT {
                left: x0,
                top: SIZE / 2,
                right: x0 + SIZE,
                bottom: SIZE,
            }
        } else {
            RECT {
                left: x0,
                top: 0,
                right: x0 + SIZE,
                bottom: SIZE / 2,
            }
        };
        let _ = FillRect(hdc, &rect, lit);
        let _ = DeleteObject(lit.into());
        let _ = SelectClipRgn(hdc, None);
        let _ = DeleteObject(disc.into());
        line(
            x0 + RING + 2,
            SIZE / 2,
            x0 + SIZE - RING - 2,
            SIZE / 2,
            1,
            COLORREF(0x0070_7070),
        );
        // Labels.
        let small = CreateFontW(
            13,
            0,
            0,
            0,
            700,
            0,
            0,
            0,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            w!("Segoe UI"),
        );
        let old_small = SelectObject(hdc, small.into());
        let label = |text: &str, top: i32, bottom: i32, c: COLORREF| {
            SetTextColor(hdc, c);
            let mut r = RECT {
                left: x0,
                top,
                right: x0 + SIZE,
                bottom,
            };
            let mut t: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(hdc, &mut t, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        };
        let off = COLORREF(0x00B8_B8B8);
        label("AI", 2, SIZE / 2, if audio_mode { off } else { WHITE_FACE });
        label("Audio", SIZE / 2, SIZE - 2, if audio_mode { WHITE_FACE } else { off });
        SelectObject(hdc, old_small);
        let _ = DeleteObject(small.into());
    }
    SelectObject(hdc, old_font);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(font.into());
    let _ = DeleteObject(pen.into());
    let _ = DeleteObject(brush.into());
    let _ = EndPaint(hwnd, &ps);
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
