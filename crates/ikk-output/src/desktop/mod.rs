//! Desktop sink: one hardened injection path per platform behind one
//! struct. Order on every write: focus guard (remembered target must be in
//! front, else restore it and settle), chunked typing with a minimum
//! per-character delay, retry, clipboard paste as last resort.

pub mod windows;

use windows as platform;

use std::time::Duration;

use tracing::{info, warn};

use crate::budget::{with_budget, PROBE_BUDGET};
use crate::chunk::{chunk_plan, is_typewriter, MIN_SAFE_CHAR_DELAY_MS};
use crate::rhythm::Rhythm;
use crate::{AccessLevel, InsertStyle, OutputError, TargetStatus, TextSink, Verification};

/// Special keys the platform layer must be able to press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Backspace,
    Left,
    Right,
    Return,
}

const RETRIES: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(60);

pub struct DesktopSink {
    /// Per-character delay for typewriter mode; ≤ 1 = burst per chunk.
    typing_delay_ms: u32,
    /// Allow clipboard paste when keystrokes keep failing.
    clipboard_fallback: bool,
    /// Wait for the target to process each character before the next.
    sync_typing: bool,
    /// Insert text through the clipboard instead of keystrokes.
    paste_mode: bool,
    /// Type character by character and send the next only once the target
    /// shows the previous one (read back through the accessibility API).
    confirm_typing: bool,
    last_level: AccessLevel,
    /// Pauses between the characters of a whole segment (the per-character
    /// flow sets its own pauses before calling in).
    rhythm: Rhythm,
}

impl Default for DesktopSink {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopSink {
    pub fn new() -> Self {
        platform::start_foreground_tracker();
        Self {
            typing_delay_ms: MIN_SAFE_CHAR_DELAY_MS,
            clipboard_fallback: true,
            sync_typing: false,
            paste_mode: false,
            // Windows: unconfirmed keystrokes get garbled in slow targets
            // (Windows 11 Notepad turns bursts into repeats of one
            // character, measured 2026-10-24 even at 20 ms per character).
            confirm_typing: cfg!(target_os = "windows"),
            last_level: AccessLevel::BasicTyping,
            rhythm: Rhythm::default(),
        }
    }

    pub fn with_typing_delay(mut self, ms: u32) -> Self {
        self.typing_delay_ms = ms;
        self
    }

    pub fn with_sync_typing(mut self, on: bool) -> Self {
        self.sync_typing = on;
        self
    }

    pub fn with_paste_mode(mut self, on: bool) -> Self {
        self.paste_mode = on;
        self
    }

    pub fn with_confirmed_typing(mut self, on: bool) -> Self {
        self.confirm_typing = on;
        self
    }

    /// Visible typing rhythm without lost characters: each character waits
    /// for the target to show it. Targets that cannot be read back (or whose
    /// caret is not at the end) are typed at the paced delay instead. If a
    /// character never shows up, the typed part is deleted and the whole
    /// text is pasted, so the result is still exact.
    fn type_confirmed(&mut self, text: &str) -> Result<(), OutputError> {
        let total = text.chars().count();
        let mut typed = String::new();
        let mut verifiable = true;
        for (i, ch) in text.chars().enumerate() {
            let sent = std::time::Instant::now();
            platform::type_text(&ch.to_string())?;
            typed.push(ch);
            if verifiable {
                match wait_until_shown(&typed) {
                    Verification::Confirmed => {}
                    // Nothing to read, or other text after the caret: pace instead.
                    _ if i == 0 => verifiable = false,
                    _ => {
                        warn!(typed = i + 1, "typing not confirmed; replacing it by a paste");
                        platform::press(Key::Backspace, i + 1)?;
                        return platform::paste_clipboard(text);
                    }
                }
            }
            // The rhythm's pause, less what the confirmation already took.
            let pause = self.rhythm.delay_after(ch, total - i - 1);
            std::thread::sleep(pause.saturating_sub(sent.elapsed()).max(self.min_delay()));
        }
        Ok(())
    }

    fn min_delay(&self) -> Duration {
        Duration::from_millis(self.typing_delay_ms.max(MIN_SAFE_CHAR_DELAY_MS) as u64)
    }

    fn type_chunked(&mut self, text: &str) -> Result<(), OutputError> {
        if self.paste_mode {
            return platform::paste_clipboard(text);
        }
        if self.confirm_typing {
            return self.type_confirmed(text);
        }
        let delay = self.typing_delay_ms.max(MIN_SAFE_CHAR_DELAY_MS);
        // One character at a time from the engine's flow: it keeps the
        // rhythm itself (never below the floor), so no pause here.
        if text.chars().count() == 1 {
            return platform::type_text(text);
        }
        let total = text.chars().count();
        let mut done = 0;
        for chunk in chunk_plan(text) {
            if is_typewriter(delay) {
                for ch in chunk.chars() {
                    platform::type_text(&ch.to_string())?;
                    if self.sync_typing {
                        platform::wait_input_idle();
                    }
                    done += 1;
                    std::thread::sleep(self.rhythm.delay_after(ch, total - done).max(self.min_delay()));
                }
            } else {
                platform::type_text(&chunk)?;
            }
        }
        Ok(())
    }
}

/// How long one character may take to appear before typing is repaired.
const CONFIRM_TIMEOUT: Duration = Duration::from_millis(400);
const CONFIRM_POLL: Duration = Duration::from_millis(4);

fn wait_until_shown(expected: &str) -> Verification {
    let started = std::time::Instant::now();
    loop {
        match platform::confirm_tail(expected) {
            Verification::Mismatch if started.elapsed() < CONFIRM_TIMEOUT => std::thread::sleep(CONFIRM_POLL),
            v => return v,
        }
    }
}

impl Drop for DesktopSink {
    /// A pending clipboard restore must not be lost when the sink closes.
    fn drop(&mut self) {
        platform::restore_clipboard(true);
    }
}

impl TextSink for DesktopSink {
    fn access_level(&self) -> AccessLevel {
        self.last_level
    }

    fn probe(&mut self) -> TargetStatus {
        let status = match with_budget(PROBE_BUDGET, platform::probe) {
            Ok(s) => s,
            Err(_) => {
                warn!("target probe exceeded {:?}; assuming basic typing", PROBE_BUDGET);
                TargetStatus::Unknown
            }
        };
        if let TargetStatus::Injectable(level) = &status {
            self.last_level = *level;
        }
        platform::remember_target();
        info!(?status, "target probed");
        status
    }

    fn type_text(&mut self, text: &str) -> Result<(), OutputError> {
        if text.is_empty() {
            return Ok(());
        }
        platform::ensure_target_foreground()?;
        let mut last_err = None;
        for attempt in 0..RETRIES {
            match self.type_chunked(text) {
                Ok(()) => return Ok(()),
                Err(OutputError::FocusLost) => return Err(OutputError::FocusLost),
                Err(OutputError::PermissionMissing) => return Err(OutputError::PermissionMissing),
                Err(e) => {
                    warn!(attempt, %e, "typing failed");
                    last_err = Some(e);
                    std::thread::sleep(RETRY_BACKOFF * (attempt + 1));
                }
            }
        }
        if self.clipboard_fallback {
            warn!("keystrokes failed {RETRIES} times; pasting via clipboard");
            platform::paste_clipboard(text)?;
            return Ok(());
        }
        Err(last_err.unwrap_or(OutputError::Failed("typing failed".into())))
    }

    fn backspace(&mut self, n: usize) -> Result<(), OutputError> {
        if n == 0 {
            return Ok(());
        }
        platform::ensure_target_foreground()?;
        platform::press(Key::Backspace, n)
    }

    fn cursor_left(&mut self, n: usize) -> Result<(), OutputError> {
        platform::ensure_target_foreground()?;
        platform::press(Key::Left, n)
    }

    fn cursor_right(&mut self, n: usize) -> Result<(), OutputError> {
        platform::ensure_target_foreground()?;
        platform::press(Key::Right, n)
    }

    fn verify(&mut self, expected_tail: &str) -> Verification {
        platform::verify_tail(expected_tail)
    }

    fn set_insert_style(&mut self, style: InsertStyle) {
        self.paste_mode = style == InsertStyle::Paste;
    }

    fn wants_whole_segments(&self) -> bool {
        // Both modes need the whole segment: paste inserts it at once,
        // confirmed typing sets its own rhythm and can repair it.
        self.paste_mode || self.confirm_typing
    }
}
