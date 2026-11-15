//! Win32 implementation: a round, topmost tool window that never activates.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateEllipticRgn, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW,
    Ellipse, EndPaint, InvalidateRect, LineTo, MoveToEx, Polygon, SelectObject, SetBkMode, SetGraphicsMode,
    SetTextColor, SetWindowRgn, SetWorldTransform, DT_CENTER, DT_SINGLELINE, DT_VCENTER, GM_ADVANCED, PAINTSTRUCT,
    PS_SOLID, RGN_OR, TRANSPARENT, XFORM,
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

use crate::{OnClicks, OnMoved, OnTap};

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
const IDLE_SCALE: f64 = 0.6;
/// How much darker the idle buttons are drawn (multiplied into the colour).
const DIM: f64 = 0.55;
// COLORREF is 0x00BBGGRR.
const BLUE: COLORREF = COLORREF(0x00B3_8600); // #0086B3, brand primary: listening for the start word
const BRAND: COLORREF = COLORREF(0x00CC_9900); // #0099CC: the correction button
const ORANGE: COLORREF = COLORREF(0x0000_6CEF); // #EF6C00: waiting for the two clicks
const RED: COLORREF = COLORREF(0x002F_2FD3); // #D32F2F: dictating
const GRAY: COLORREF = COLORREF(0x0042_4242); // #424242: off
const WHITE: COLORREF = COLORREF(0x00FF_FFFF);
const LIGHT_GRAY: COLORREF = COLORREF(0x00B8_B8B8);

/// `c` darkened by [`DIM`].
fn dimmed(c: COLORREF) -> COLORREF {
    let ch = |shift: u32| (((c.0 >> shift) & 0xFF) as f64 * DIM) as u32;
    COLORREF(ch(0) | (ch(8) << 8) | (ch(16) << 16))
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
    on_moved: Option<OnMoved>,
    /// Last known top-left corner (screen pixels).
    position: Option<(f64, f64)>,
    /// Where MSG_MOVE should put the window.
    wanted: Option<(i32, i32)>,
    /// The buttons shrink when the mouse is not over them.
    shrink: bool,
    /// The mouse is over the window.
    hover: bool,
}

/// The current layout: window size, drawing scale and offset, dimmed.
#[derive(Clone, Copy)]
struct Layout {
    width: i32,
    height: i32,
    scale: f64,
    dx: f64,
    dy: f64,
    dim: bool,
    with_correct: bool,
}

fn layout() -> Layout {
    let (with_correct, shrink, hover) = state()
        .lock()
        .map(|s| (s.on_correct.is_some(), s.shrink, s.hover))
        .unwrap_or((false, false, false));
    let content = width(with_correct) as f64;
    let panel_scale = if shrink { HOVER_SCALE } else { 1.0 };
    let scale = if !shrink {
        1.0
    } else if hover {
        HOVER_SCALE
    } else {
        IDLE_SCALE
    };
    let w = (content * panel_scale).ceil();
    let h = (SIZE as f64 * panel_scale).ceil();
    Layout {
        width: w as i32,
        height: h as i32,
        scale,
        dx: (w - content * scale) / 2.0,
        dy: (h - SIZE as f64 * scale) / 2.0,
        dim: shrink && !hover,
        with_correct,
    }
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
    if l.with_correct {
        let second = disc((SIZE + GAP) as f64);
        let _ = CombineRgn(Some(region), Some(region), Some(second), RGN_OR);
        let _ = DeleteObject(second.into());
    }
    let _ = SetWindowRgn(hwnd, Some(region), true);
}

pub fn set_shrink(on: bool) {
    let changed = state()
        .lock()
        .map(|mut s| std::mem::replace(&mut s.shrink, on) != on)
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

/// Window width: one or two buttons.
fn width(with_correct: bool) -> i32 {
    if with_correct {
        SIZE * 2 + GAP
    } else {
        SIZE
    }
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

fn post(msg: u32) {
    let hwnd = state().lock().map(|s| s.hwnd).unwrap_or(0);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn show(on_tap: OnTap, on_correct: Option<OnTap>, on_moved: Option<OnMoved>) {
    let create = {
        let Ok(mut s) = state().lock() else { return };
        s.on_tap = Some(on_tap);
        s.on_correct = on_correct;
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
                apply_shape(hwnd, layout());
                let _ = InvalidateRect(Some(hwnd), None, true);
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
            let left = state()
                .lock()
                .map(|mut s| std::mem::replace(&mut s.hover, false))
                .unwrap_or(false);
            if left {
                apply_shape(hwnd, layout());
                let _ = InvalidateRect(Some(hwnd), None, true);
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
                // Which button: by the x position inside the window.
                let l = layout();
                let correct = l.with_correct && start.x - rect.left >= l.width / 2;
                // The correction button while waiting for clicks: cancel.
                if correct && is_capturing() {
                    cancel_capture();
                    return LRESULT(0);
                }
                let action = state().lock().ok().and_then(|s| {
                    if correct {
                        s.on_correct.clone()
                    } else {
                        s.on_tap.clone()
                    }
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
            // Resized around the top-left corner; the shape follows.
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
    let colour = |c: COLORREF| if l.dim { dimmed(c) } else { c };
    let white = if l.dim { LIGHT_GRAY } else { WHITE };
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
    if with_correct {
        // The correction button: brand blue, orange while waiting for clicks.
        let brush2 = CreateSolidBrush(colour(if is_capturing() { ORANGE } else { BRAND }));
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
    if with_correct {
        // A wand with a sparkle: the text gets checked and improved.
        let (cx, cy) = (SIZE + GAP + SIZE / 2, SIZE / 2);
        let wand_pen = CreatePen(PS_SOLID, 4, white);
        let old = SelectObject(hdc, wand_pen.into());
        let _ = MoveToEx(hdc, cx - 14, cy + 14, None);
        let _ = LineTo(hdc, cx + 4, cy - 4);
        SelectObject(hdc, old);
        let _ = DeleteObject(wand_pen.into());
        let star = |sx: i32, sy: i32, r: f64| {
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
            let _ = Polygon(hdc, &pts);
        };
        let thin = CreatePen(PS_SOLID, 1, white);
        let old_pen2 = SelectObject(hdc, thin.into());
        let white_brush = CreateSolidBrush(white);
        let old_brush2 = SelectObject(hdc, white_brush.into());
        star(cx + 11, cy - 11, 9.0);
        star(cx + 1, cy - 17, 3.5);
        star(cx + 17, cy - 1, 3.5);
        SelectObject(hdc, old_brush2);
        SelectObject(hdc, old_pen2);
        let _ = DeleteObject(white_brush.into());
        let _ = DeleteObject(thin.into());
    }
    SelectObject(hdc, old_font);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(font.into());
    let _ = DeleteObject(pen.into());
    let _ = DeleteObject(brush.into());
    let _ = EndPaint(hwnd, &ps);
}
