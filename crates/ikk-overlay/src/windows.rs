//! Win32 implementation: a round, topmost tool window that never activates.

use std::cell::Cell;
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateEllipticRgn, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
    EndPaint, InvalidateRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowRect,
    LoadCursorW, PostMessageW, RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, IDC_HAND,
    MA_NOACTIVATE, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
    WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::OnTap;

const SIZE: i32 = 64;
const RING: i32 = 3;
const DRAG_THRESHOLD: i32 = 4;
// COLORREF is 0x00BBGGRR.
const BLUE: COLORREF = COLORREF(0x00B3_8600); // #0086B3, brand primary: listening for the start word
const RED: COLORREF = COLORREF(0x002F_2FD3); // #D32F2F: dictating
const GRAY: COLORREF = COLORREF(0x0042_4242); // #424242: off
const WHITE: COLORREF = COLORREF(0x00FF_FFFF);

const MSG_REDRAW: u32 = WM_APP + 1;
const MSG_HIDE: u32 = WM_APP + 2;
const MSG_SHOW: u32 = WM_APP + 3;

#[derive(Default)]
struct State {
    /// HWND as integer (0 = not created yet).
    hwnd: isize,
    listening: bool,
    standby: bool,
    on_tap: Option<OnTap>,
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

pub fn show(on_tap: OnTap) {
    let create = {
        let Ok(mut s) = state().lock() else { return };
        s.on_tap = Some(on_tap);
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
        // Right edge, upper third: out of the way of most text.
        let x = GetSystemMetrics(SM_CXSCREEN) - SIZE - 32;
        let y = GetSystemMetrics(SM_CYSCREEN) / 3;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("Dictation"),
            WS_POPUP,
            x,
            y,
            SIZE,
            SIZE,
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
        let _ = SetWindowRgn(hwnd, Some(CreateEllipticRgn(0, 0, SIZE + 1, SIZE + 1)), true);
        if let Ok(mut s) = state().lock() {
            s.hwnd = hwnd.0 as isize;
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
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
        WM_LBUTTONUP => {
            let drag = DRAG.take();
            let _ = ReleaseCapture();
            if let Some((_, _, false)) = drag {
                let tap = state().lock().ok().and_then(|s| s.on_tap.clone());
                if let Some(tap) = tap {
                    // Off the window thread: toggling talks to the engine.
                    std::thread::spawn(move || tap());
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
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint(hwnd: HWND) {
    let (listening, standby) = state()
        .lock()
        .map(|s| (s.listening, s.standby))
        .unwrap_or((false, false));
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let brush = CreateSolidBrush(if listening {
        RED
    } else if standby {
        BLUE
    } else {
        GRAY
    });
    let pen = CreatePen(PS_SOLID, RING, WHITE);
    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());
    let _ = Ellipse(hdc, RING / 2, RING / 2, SIZE - RING / 2, SIZE - RING / 2);
    // Microphone glyph from the Windows symbol font.
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
    SetTextColor(hdc, WHITE);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: SIZE,
        bottom: SIZE,
    };
    let mut glyph: Vec<u16> = "\u{E720}".encode_utf16().collect();
    DrawTextW(hdc, &mut glyph, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(hdc, old_font);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(font.into());
    let _ = DeleteObject(pen.into());
    let _ = DeleteObject(brush.into());
    let _ = EndPaint(hwnd, &ps);
}
