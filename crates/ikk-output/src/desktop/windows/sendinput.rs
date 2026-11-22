//! Raw SendInput: one `KEYEVENTF_UNICODE` down/up pair per UTF-16 unit,
//! layout-free, no virtual-key mapping. Replaces enigo's `text()`, whose mix
//! of layout keys and Unicode events corrupted characters after word
//! boundaries in a field test ("ist"→"sst"). Plus secure-desktop
//! detection and the foreground-forcing workaround.

use std::ffi::c_void;

use windows::Win32::Foundation::HWND;

use crate::OutputError;

#[link(name = "user32")]
extern "system" {
    fn OpenInputDesktop(dw_flags: u32, f_inherit: i32, dw_desired_access: u32) -> *mut c_void;
    fn CloseDesktop(h_desktop: *mut c_void) -> i32;
    fn GetForegroundWindow() -> *mut c_void;
    fn SetForegroundWindow(h_wnd: *mut c_void) -> i32;
    fn GetWindowThreadProcessId(h_wnd: *mut c_void, pid: *mut u32) -> u32;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, f_attach: i32) -> i32;
    fn SetFocus(h_wnd: *mut c_void) -> *mut c_void;
    fn SendInput(c_inputs: u32, p_inputs: *const InputRecord, cb_size: i32) -> u32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetDoubleClickTime() -> u32;
    fn SendMessageTimeoutW(
        h_wnd: *mut c_void,
        msg: u32,
        w_param: usize,
        l_param: isize,
        flags: u32,
        timeout_ms: u32,
        result: *mut usize,
    ) -> isize;
}
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// Win32 `KEYBDINPUT`.
#[repr(C)]
#[derive(Clone, Copy)]
struct KeybdInput {
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

/// Win32 `MOUSEINPUT`; only here so the union gets the size Windows expects.
#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
union InputUnion {
    ki: KeybdInput,
    mi: MouseInput,
}

/// Win32 `INPUT`. The union is pointer-aligned, so on 64-bit Windows the
/// keyboard data starts at byte 8 and the record is 40 bytes. SendInput
/// rejects every event when `cbSize` does not match.
#[repr(C)]
struct InputRecord {
    r#type: u32,
    u: InputUnion,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<InputRecord>() == 40);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<InputRecord>() == 28);

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;

pub const VK_BACK: u16 = 0x08;
pub const VK_TAB: u16 = 0x09;
pub const VK_RETURN: u16 = 0x0D;
pub const VK_SHIFT: u16 = 0x10;
pub const VK_CONTROL: u16 = 0x11;
pub const VK_PRIOR: u16 = 0x21;
pub const VK_NEXT: u16 = 0x22;
pub const VK_END: u16 = 0x23;
pub const VK_HOME: u16 = 0x24;
pub const VK_LEFT: u16 = 0x25;
pub const VK_UP: u16 = 0x26;
pub const VK_RIGHT: u16 = 0x27;
pub const VK_DOWN: u16 = 0x28;
pub const VK_DELETE: u16 = 0x2E;
const VK_V: u16 = 0x56;

fn record(vk: u16, scan: u16, flags: u32) -> InputRecord {
    InputRecord {
        r#type: INPUT_KEYBOARD,
        u: InputUnion {
            ki: KeybdInput {
                w_vk: vk,
                w_scan: scan,
                dw_flags: flags,
                time: 0,
                dw_extra_info: 0,
            },
        },
    }
}

fn send(inputs: &[InputRecord]) -> Result<(), OutputError> {
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<InputRecord>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        return Err(OutputError::Failed(format!(
            "SendInput injected {sent}/{} events (UIPI/secure desktop?)",
            inputs.len()
        )));
    }
    Ok(())
}

const WM_NULL: u32 = 0;
const SMTO_ABORTIFHUNG: u32 = 0x0002;

/// Block until the foreground window's thread has pumped its messages once
/// (at most `timeout_ms`). Unicode key events carry their character in
/// per-thread state; when several queue up before the target reads them,
/// slow targets (Windows 11 Notepad) turn a burst into repeats of the last
/// character. Waiting for the target between characters prevents that.
pub fn wait_target_idle(timeout_ms: u32) {
    unsafe {
        let h = GetForegroundWindow();
        if h.is_null() {
            return;
        }
        let mut result = 0usize;
        SendMessageTimeoutW(h, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, timeout_ms, &mut result);
    }
}

pub fn send_unicode_char(ch: char) -> Result<(), OutputError> {
    let mut units = [0u16; 2];
    let encoded = ch.encode_utf16(&mut units);
    let mut inputs = Vec::with_capacity(encoded.len() * 2);
    for &u in encoded.iter() {
        inputs.push(record(0, u, KEYEVENTF_UNICODE));
        inputs.push(record(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send(&inputs)
}

/// The arrow keys, Home, End, Page Up, Page Down and Delete exist twice:
/// on the number pad and beside it. Without this flag Windows takes an
/// injected one for the number pad's, and with Num Lock on it lets go of
/// Shift for those: Shift+End or Shift+Left then move the cursor without
/// selecting anything. The flag names the keys beside the number pad.
fn key_flags(vk: u16) -> u32 {
    match vk {
        VK_PRIOR | VK_NEXT | VK_END | VK_HOME | VK_LEFT | VK_UP | VK_RIGHT | VK_DOWN | VK_DELETE => {
            KEYEVENTF_EXTENDEDKEY
        }
        _ => 0,
    }
}

pub fn press_vk(vk: u16, n: usize) -> Result<(), OutputError> {
    let flags = key_flags(vk);
    for _ in 0..n {
        send(&[record(vk, 0, flags), record(vk, 0, flags | KEYEVENTF_KEYUP)])?;
    }
    Ok(())
}

/// `vk` pressed `n` times while the modifier keys are held.
pub fn press_vk_with(mods: &[u16], vk: u16, n: usize) -> Result<(), OutputError> {
    let mut records = Vec::with_capacity(mods.len() * 2 + n * 2);
    for m in mods {
        records.push(record(*m, 0, 0));
    }
    let flags = key_flags(vk);
    for _ in 0..n {
        records.push(record(vk, 0, flags));
        records.push(record(vk, 0, flags | KEYEVENTF_KEYUP));
    }
    for m in mods.iter().rev() {
        records.push(record(*m, 0, KEYEVENTF_KEYUP));
    }
    send(&records)
}

fn mouse(flags: u32) -> InputRecord {
    InputRecord {
        r#type: INPUT_MOUSE,
        u: InputUnion {
            mi: MouseInput {
                dx: 0,
                dy: 0,
                mouse_data: 0,
                dw_flags: flags,
                time: 0,
                dw_extra_info: 0,
            },
        },
    }
}

/// Wait until the user has let go of the mouse buttons (at most 3 s): a
/// click sent while a button is held is taken as part of a drag.
fn wait_buttons_up() {
    const VK_LBUTTON: i32 = 0x01;
    const VK_RBUTTON: i32 = 0x02;
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } < 0;
    // Left and right are swapped for some users: wait for both.
    for _ in 0..300 {
        if !down(VK_LBUTTON) && !down(VK_RBUTTON) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The button that does the "left click" (users may have swapped them).
fn primary_button() -> (u32, u32) {
    const SM_SWAPBUTTON: i32 = 23;
    if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 {
        (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
    } else {
        (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
    }
}

/// Put the pointer on the pixel and give the program under it a moment to
/// notice; then once more, in case the user's hand moved it meanwhile.
fn place(x: i32, y: i32) -> Result<(), OutputError> {
    if unsafe { SetCursorPos(x, y) } == 0 {
        return Err(OutputError::Failed("SetCursorPos failed".into()));
    }
    std::thread::sleep(std::time::Duration::from_millis(40));
    let _ = unsafe { SetCursorPos(x, y) };
    Ok(())
}

/// A left click at screen pixel (`x`, `y`); with `shift` held it extends a
/// text selection.
pub fn click_at(x: i32, y: i32, shift: bool) -> Result<(), OutputError> {
    wait_buttons_up();
    place(x, y)?;
    let (down, up) = primary_button();
    if shift {
        send(&[
            record(VK_SHIFT, 0, 0),
            mouse(down),
            mouse(up),
            record(VK_SHIFT, 0, KEYEVENTF_KEYUP),
        ])
    } else {
        send(&[mouse(down), mouse(up)])
    }
}

/// A double-click at screen pixel (`x`, `y`), sent as one piece so that
/// nothing can come between its two clicks.
pub fn double_click_at(x: i32, y: i32) -> Result<(), OutputError> {
    wait_buttons_up();
    place(x, y)?;
    let (down, up) = primary_button();
    send(&[mouse(down), mouse(up), mouse(down), mouse(up)])
}

/// The system's double-click time.
pub fn double_click_time() -> std::time::Duration {
    std::time::Duration::from_millis(unsafe { GetDoubleClickTime() }.clamp(100, 5000) as u64)
}

/// Press at one point, release at another: selects the text between the
/// two in every text field, with the mouse alone and at once (no Shift:
/// a Shift pressed without a letter key is held on by the Sticky Keys aid
/// for the user's next click). The pointer goes to the end in one step;
/// it is placed there twice because a program only extends a selection
/// when it has seen the pointer move while the button is down.
pub fn drag(x1: i32, y1: i32, x2: i32, y2: i32) -> Result<(), OutputError> {
    wait_buttons_up();
    place(x1, y1)?;
    let (down, up) = primary_button();
    send(&[mouse(down)])?;
    let pause = std::time::Duration::from_millis(25);
    std::thread::sleep(pause);
    // One pixel short first, so that the second placing is a move too.
    let _ = unsafe { SetCursorPos(x2 - (x2 - x1).signum(), y2) };
    std::thread::sleep(pause);
    let _ = unsafe { SetCursorPos(x2, y2) };
    std::thread::sleep(pause);
    send(&[mouse(up)])
}

/// Which of Shift, Ctrl and Alt the system takes as held right now (for
/// the log: a held Shift turns every click into "select up to here").
pub fn modifiers_down() -> String {
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } < 0;
    let mut held = Vec::new();
    for (vk, name) in [(0x10, "shift"), (0x11, "ctrl"), (0x12, "alt")] {
        if down(vk) {
            held.push(name);
        }
    }
    held.join("+")
}

pub fn ctrl_v() -> Result<(), OutputError> {
    send(&[
        record(VK_CONTROL, 0, 0),
        record(VK_V, 0, 0),
        record(VK_V, 0, KEYEVENTF_KEYUP),
        record(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ])
}

/// UAC / lock screen / Ctrl+Alt+Del: Windows drops all SendInput silently.
pub fn is_secure_desktop_active() -> bool {
    let h = unsafe { OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP) };
    if h.is_null() {
        return true;
    }
    unsafe { CloseDesktop(h) };
    false
}

/// SetForegroundWindow is restricted to the current foreground process;
/// briefly attach to the target's input thread when the plain call fails.
pub fn force_foreground(hwnd: HWND) {
    let h = hwnd.0;
    unsafe {
        if SetForegroundWindow(h) != 0 {
            return;
        }
        let our = GetCurrentThreadId();
        let mut pid = 0u32;
        let target = GetWindowThreadProcessId(h, &mut pid);
        if target == 0 || target == our {
            return;
        }
        if AttachThreadInput(our, target, 1) != 0 {
            SetForegroundWindow(h);
            SetFocus(h);
            AttachThreadInput(our, target, 0);
        }
        let _ = GetForegroundWindow();
    }
}
