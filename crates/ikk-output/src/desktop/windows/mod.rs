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

pub fn press(key: Key, n: usize) -> Result<(), OutputError> {
    let vk = match key {
        Key::Backspace => sendinput::VK_BACK,
        Key::Left => sendinput::VK_LEFT,
        Key::Right => sendinput::VK_RIGHT,
        Key::Return => sendinput::VK_RETURN,
    };
    sendinput::press_vk(vk, n)
}

pub fn paste_clipboard(text: &str) -> Result<(), OutputError> {
    let mut cb = arboard::Clipboard::new().map_err(|e| OutputError::Failed(format!("clipboard: {e}")))?;
    let prior = cb.get_text().ok();
    cb.set_text(text.to_string())
        .map_err(|e| OutputError::Failed(format!("clipboard set: {e}")))?;
    sleep(Duration::from_millis(40));
    sendinput::ctrl_v()?;
    if let Some(prev) = prior {
        std::thread::spawn(move || {
            sleep(Duration::from_millis(2000));
            if let Ok(mut cb2) = arboard::Clipboard::new() {
                let _ = cb2.set_text(prev);
            }
        });
    }
    Ok(())
}

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

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
