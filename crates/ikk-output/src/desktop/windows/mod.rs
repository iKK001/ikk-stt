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

fn vk_of(key: Key) -> u16 {
    match key {
        Key::Backspace => sendinput::VK_BACK,
        Key::Delete => sendinput::VK_DELETE,
        Key::Left => sendinput::VK_LEFT,
        Key::Right => sendinput::VK_RIGHT,
        Key::Up => sendinput::VK_UP,
        Key::Down => sendinput::VK_DOWN,
        Key::Home => sendinput::VK_HOME,
        Key::End => sendinput::VK_END,
        Key::PageUp => sendinput::VK_PRIOR,
        Key::PageDown => sendinput::VK_NEXT,
        Key::Return => sendinput::VK_RETURN,
        Key::Tab => sendinput::VK_TAB,
    }
}

pub fn press(key: Key, n: usize) -> Result<(), OutputError> {
    sendinput::press_vk(vk_of(key), n)
}

/// A chord: Ctrl is both the shortcut and the word-jump modifier here.
pub fn press_chord(chord: crate::Chord, n: usize) -> Result<(), OutputError> {
    let (vk, m) = match chord {
        crate::Chord::Key(k, m) => (vk_of(k), m),
        // Letter keys: the virtual-key code of a letter is its upper-case ASCII.
        crate::Chord::Char(c, m) => (c.to_ascii_uppercase() as u16, m),
    };
    let mut mods = Vec::new();
    if m.command || m.word {
        mods.push(sendinput::VK_CONTROL);
    }
    if m.shift {
        mods.push(sendinput::VK_SHIFT);
    }
    sendinput::press_vk_with(&mods, vk, n)
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

/// A left click at screen pixel (`x`, `y`); with `shift` it extends a selection.
pub fn click_at(x: f64, y: f64, shift: bool) -> Result<(), OutputError> {
    sendinput::click_at(x.round() as i32, y.round() as i32, shift)
}

/// A double-click at screen pixel (`x`, `y`): selects the word under it.
pub fn double_click_at(x: f64, y: f64) -> Result<(), OutputError> {
    sendinput::double_click_at(x.round() as i32, y.round() as i32)
}

/// Shift, Ctrl, Alt as the system sees them held right now (for the log).
pub fn modifiers_down() -> String {
    sendinput::modifiers_down()
}

/// The system's double-click time.
pub fn double_click_time() -> std::time::Duration {
    sendinput::double_click_time()
}

/// A drag with the left button from one screen pixel to another (tests).
pub fn drag(x1: f64, y1: f64, x2: f64, y2: f64) -> Result<(), OutputError> {
    sendinput::drag(
        x1.round() as i32,
        y1.round() as i32,
        x2.round() as i32,
        y2.round() as i32,
    )
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

/// The focused element's selected text and the text around it through UI
/// Automation's text pattern: copies of the selected range are collapsed
/// to its start and its end and widened by characters; nothing in the
/// control is selected or moved for it.
pub fn selection_around(before: usize, after: usize) -> Option<(String, String, String)> {
    use windows::Win32::UI::Accessibility::{
        TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start, TextUnit_Character,
    };
    uia::with_focused(|el| {
        let pattern = uia::text_pattern(el).ok_or(uia::UiaError::NoText)?;
        unsafe {
            let ranges = pattern.GetSelection().map_err(|_| uia::UiaError::NoText)?;
            if ranges.Length().unwrap_or(0) < 1 {
                return Err(uia::UiaError::NoText);
            }
            let range = ranges.GetElement(0).map_err(|_| uia::UiaError::NoText)?;
            let marked = range.GetText(400).map_err(|_| uia::UiaError::NoText)?.to_string();
            if marked.trim().is_empty() {
                return Err(uia::UiaError::NoText);
            }
            // In front: both ends at the selection's start, the start moved back.
            let front = range.Clone().map_err(|_| uia::UiaError::NoText)?;
            front
                .MoveEndpointByRange(TextPatternRangeEndpoint_End, &range, TextPatternRangeEndpoint_Start)
                .map_err(|_| uia::UiaError::NoText)?;
            let _ = front.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -(before as i32));
            let before_text = front.GetText(before as i32).map(|t| t.to_string()).unwrap_or_default();
            // Behind: both ends at the selection's end, the end moved on.
            let back = range.Clone().map_err(|_| uia::UiaError::NoText)?;
            back.MoveEndpointByRange(TextPatternRangeEndpoint_Start, &range, TextPatternRangeEndpoint_End)
                .map_err(|_| uia::UiaError::NoText)?;
            let _ = back.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, after as i32);
            let after_text = back.GetText(after as i32).map(|t| t.to_string()).unwrap_or_default();
            Ok((
                unify_newlines(&before_text),
                unify_newlines(&marked),
                unify_newlines(&after_text),
            ))
        }
    })
    .ok()
}

/// The focused element's selection rectangle through UI Automation's text
/// pattern (the first rectangle of the first selected range), in screen
/// pixels. `None` when the control has no text pattern or no selection.
pub fn selection_bounds() -> Option<(f64, f64, f64, f64)> {
    use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayGetUBound, SafeArrayUnaccessData};
    uia::with_focused(|el| {
        let pattern = uia::text_pattern(el).ok_or(uia::UiaError::NoText)?;
        unsafe {
            let ranges = pattern.GetSelection().map_err(|_| uia::UiaError::NoText)?;
            if ranges.Length().unwrap_or(0) < 1 {
                return Err(uia::UiaError::NoText);
            }
            let range = ranges.GetElement(0).map_err(|_| uia::UiaError::NoText)?;
            let arr = range.GetBoundingRectangles().map_err(|_| uia::UiaError::NoText)?;
            if arr.is_null() {
                return Err(uia::UiaError::NoText);
            }
            let upper = SafeArrayGetUBound(arr, 1).map_err(|_| uia::UiaError::NoText)?;
            if upper < 3 {
                return Err(uia::UiaError::NoText);
            }
            let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
            SafeArrayAccessData(arr, &mut data).map_err(|_| uia::UiaError::NoText)?;
            let v = std::slice::from_raw_parts(data as *const f64, (upper + 1) as usize);
            let rect = (v[0], v[1], v[2], v[3]);
            let _ = SafeArrayUnaccessData(arr);
            if rect.2 <= 0.0 || rect.3 <= 0.0 {
                return Err(uia::UiaError::NoText);
            }
            Ok(rect)
        }
    })
    .ok()
}
