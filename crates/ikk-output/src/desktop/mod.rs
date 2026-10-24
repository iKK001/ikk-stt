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
use crate::{AccessLevel, OutputError, TargetStatus, TextSink, Verification};

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
    last_level: AccessLevel,
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
            last_level: AccessLevel::BasicTyping,
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

    fn type_chunked(&self, text: &str) -> Result<(), OutputError> {
        if self.paste_mode {
            return platform::paste_clipboard(text);
        }
        let delay = self.typing_delay_ms.max(MIN_SAFE_CHAR_DELAY_MS);
        for chunk in chunk_plan(text) {
            if is_typewriter(delay) {
                for ch in chunk.chars() {
                    platform::type_text(&ch.to_string())?;
                    if self.sync_typing {
                        platform::wait_input_idle();
                    }
                    std::thread::sleep(Duration::from_millis(delay as u64));
                }
            } else {
                platform::type_text(&chunk)?;
            }
        }
        Ok(())
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
}
