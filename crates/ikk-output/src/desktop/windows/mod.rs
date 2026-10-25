//! Windows: layout-free Unicode SendInput, foreground tracking with focus
//! restore, secure-desktop detection, UI Automation probe and read-back,
//! integrity-level (UIPI) check, clipboard fallback.
//!
//! Probe order matters: secure desktop first (Windows accepts no input),
//! then integrity (an elevated window may offer a TextPattern yet receive
//! nothing), then password field, then capabilities.

pub mod integrity;
pub mod sendinput;
pub mod uia;

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread::sleep;
use std::time::Duration;

use tracing::{debug, info, warn};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::Key;
use crate::guard::should_inject;
use crate::{AccessLevel, OutputError, TargetStatus, Verification};

/// Remembered foreign foreground window (HWND as isize), 0 = none.
static TARGET_HWND: AtomicIsize = AtomicIsize::new(0);
const TRACK_INTERVAL: Duration = Duration::from_millis(150);

fn foreign_foreground() -> Option<HWND> {
    let h = unsafe { GetForegroundWindow() };
    if h.0.is_null() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    (pid != unsafe { GetCurrentProcessId() }).then_some(h)
}

pub fn start_foreground_tracker() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("ikk-foreground-tracker".into())
        .spawn(|| loop {
            if let Some(h) = foreign_foreground() {
                TARGET_HWND.store(h.0 as isize, Ordering::SeqCst);
            }
            sleep(TRACK_INTERVAL);
        })
        .ok();
    info!("foreground tracker started");
}

pub fn remember_target() {
    if let Some(h) = foreign_foreground() {
        TARGET_HWND.store(h.0 as isize, Ordering::SeqCst);
        info!(hwnd = ?h, "typing target remembered");
    }
}

fn target_is_foreground() -> bool {
    let stored = TARGET_HWND.load(Ordering::SeqCst);
    stored != 0 && unsafe { GetForegroundWindow() }.0 as isize == stored
}

/// Guard + restore + settle (SetForegroundWindow is asynchronous).
pub fn ensure_target_foreground() -> Result<(), OutputError> {
    if sendinput::is_secure_desktop_active() {
        return Err(OutputError::Failed("secure desktop active (UAC / lock screen)".into()));
    }
    let stored = TARGET_HWND.load(Ordering::SeqCst);
    if stored == 0 {
        return Ok(());
    }
    if !target_is_foreground() {
        sendinput::force_foreground(HWND(stored as *mut _));
        let mut waited = 0u64;
        while waited < 250 && !target_is_foreground() {
            sleep(Duration::from_millis(20));
            waited += 20;
        }
    }
    if should_inject(true, target_is_foreground()) {
        Ok(())
    } else {
        warn!("typing target not in foreground; injection blocked");
        Err(OutputError::FocusLost)
    }
}

pub fn probe() -> TargetStatus {
    if sendinput::is_secure_desktop_active() {
        return TargetStatus::NoInjection("secure desktop active (lock screen, UAC, Ctrl+Alt+Del)".into());
    }
    let element = match uia::with_focused(|el| Ok(uia::ElementInfo::collect(el))) {
        Ok(info) => info,
        Err(e) => {
            debug!(%e, "no UI Automation info; basic typing");
            return TargetStatus::Unknown;
        }
    };
    let hwnd = if element.hwnd.0.is_null() {
        unsafe { GetForegroundWindow() }
    } else {
        element.hwnd
    };
    if hwnd.0.is_null() {
        return TargetStatus::NoEditableFocus;
    }
    let integ = integrity::compare_with_own(hwnd);
    debug!(detail = %integ.detail(), "integrity");
    if integ.blocks_input() {
        return TargetStatus::Elevated;
    }
    if element.is_password {
        return TargetStatus::PasswordField;
    }
    if element.has_text_pattern || element.value_writable {
        return TargetStatus::Injectable(AccessLevel::FullTextControl);
    }
    if element.is_static_control {
        return TargetStatus::NoEditableFocus;
    }
    TargetStatus::Injectable(AccessLevel::BasicTyping)
}

pub fn type_text(text: &str) -> Result<(), OutputError> {
    for ch in text.chars() {
        if ch == '\n' {
            sendinput::press_vk(sendinput::VK_RETURN, 1)?;
        } else {
            sendinput::send_unicode_char(ch)?;
        }
    }
    Ok(())
}

/// Wait until the target has processed the input sent so far.
pub fn wait_input_idle() {
    sendinput::wait_target_idle(250);
}

pub fn press(key: Key, n: usize) -> Result<(), OutputError> {
    let vk = match key {
        Key::Backspace => sendinput::VK_BACK,
        Key::Left => sendinput::VK_LEFT,
        Key::Right => sendinput::VK_RIGHT,
        Key::Return => sendinput::VK_RETURN,
    };
    sendinput::press_vk(vk, n)
}

#[link(name = "user32")]
extern "system" {
    fn GetClipboardSequenceNumber() -> u32;
}

/// The user's clipboard while our pastes are in flight, restored once the
/// last paste is RESTORE_AFTER old and nobody else changed the clipboard.
struct PendingRestore {
    prior: Option<String>,
    our_seq: u32,
    last_paste: std::time::Instant,
}

static PENDING: std::sync::Mutex<Option<PendingRestore>> = std::sync::Mutex::new(None);
const RESTORE_AFTER: Duration = Duration::from_millis(1500);

/// Kept out of the clipboard history (Win+V), the cloud clipboard and
/// clipboard tools, so dictated text leaves no copy behind.
fn set_private_text(cb: &mut arboard::Clipboard, text: &str) -> Result<(), OutputError> {
    use arboard::SetExtWindows;
    cb.set()
        .exclude_from_monitoring()
        .exclude_from_history()
        .exclude_from_cloud()
        .text(text.to_string())
        .map_err(|e| OutputError::Failed(format!("clipboard set: {e}")))
}

pub fn paste_clipboard(text: &str) -> Result<(), OutputError> {
    let mut cb = arboard::Clipboard::new().map_err(|e| OutputError::Failed(format!("clipboard: {e}")))?;
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    // A paste shortly after the previous one: the clipboard still holds our
    // own text, so keep the user's original from the first paste.
    let prior = match pending.take() {
        Some(p) if unsafe { GetClipboardSequenceNumber() } == p.our_seq => p.prior,
        _ => cb.get_text().ok(),
    };
    set_private_text(&mut cb, text)?;
    let our_seq = unsafe { GetClipboardSequenceNumber() };
    sleep(Duration::from_millis(40));
    let pasted = sendinput::ctrl_v();
    *pending = Some(PendingRestore {
        prior,
        our_seq,
        last_paste: std::time::Instant::now(),
    });
    drop(pending);
    std::thread::spawn(|| {
        sleep(RESTORE_AFTER);
        restore_clipboard(false);
    });
    pasted
}

/// Put the user's clipboard back once the last paste is old enough, or now
/// with `force` (the sink is closing; a short pause lets the target read
/// the clipboard first). Without earlier text the clipboard is cleared, so
/// dictated text never stays behind.
pub fn restore_clipboard(force: bool) {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(age) = pending.as_ref().map(|p| p.last_paste.elapsed()) else {
        return;
    };
    if !force && age < RESTORE_AFTER {
        return; // a newer paste will restore
    }
    if force && age < MIN_PASTE_SETTLE {
        sleep(MIN_PASTE_SETTLE - age);
    }
    let Some(p) = pending.take() else { return };
    // Only if the clipboard still holds our text: never overwrite
    // something the user copied in the meantime.
    if unsafe { GetClipboardSequenceNumber() } != p.our_seq {
        return;
    }
    let Ok(mut cb) = arboard::Clipboard::new() else { return };
    match p.prior {
        Some(prev) => {
            let _ = set_private_text(&mut cb, &prev);
        }
        None => {
            let _ = cb.clear();
        }
    }
}

/// Time the target gets to read the clipboard after Ctrl+V.
const MIN_PASTE_SETTLE: Duration = Duration::from_millis(400);

/// Read the focused field through UI Automation and compare its tail.
pub fn verify_tail(expected: &str) -> Verification {
    let expected = expected.trim_end();
    if expected.is_empty() {
        return Verification::Unavailable;
    }
    match uia::with_focused(uia::read_text) {
        Ok(text) => {
            let tail: String = text
                .chars()
                .rev()
                .take(expected.chars().count())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if normalize(&tail) == normalize(expected) {
                Verification::Confirmed
            } else {
                Verification::Mismatch
            }
        }
        Err(_) => Verification::Unavailable,
    }
}

/// Exact tail check for confirmed typing: only line endings are unified
/// (an editor may store a typed Return as CR or CRLF).
pub fn confirm_tail(expected: &str) -> Verification {
    if expected.is_empty() {
        return Verification::Unavailable;
    }
    match uia::with_focused(uia::read_text) {
        Ok(text) => {
            let text = unify_newlines(&text);
            let expected = unify_newlines(expected);
            if text.ends_with(&expected) {
                Verification::Confirmed
            } else {
                Verification::Mismatch
            }
        }
        Err(_) => Verification::Unavailable,
    }
}

fn unify_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
