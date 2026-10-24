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
    _mi: MouseInput,
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

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;

pub const VK_BACK: u16 = 0x08;
pub const VK_RETURN: u16 = 0x0D;
pub const VK_CONTROL: u16 = 0x11;
pub const VK_LEFT: u16 = 0x25;
pub const VK_RIGHT: u16 = 0x27;
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

pub fn press_vk(vk: u16, n: usize) -> Result<(), OutputError> {
    for _ in 0..n {
        send(&[record(vk, 0, 0), record(vk, 0, KEYEVENTF_KEYUP)])?;
    }
    Ok(())
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
