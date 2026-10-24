//! Where transcribed text goes.
//!
//! Desktop: keystroke injection into the focused field of another app.
//! Mobile: the core cannot inject; the host (iOS keyboard extension,
//! Android IME) receives the text over the FFI and inserts it itself.
//! Clipboard is the universal fallback.
//!
//! Every sink can be *probed* before dictation starts and *verified* while
//! typing, so the engine knows when direct injection cannot work and the
//! shell must show the fallback dictation window instead (ARCHITECTURE §10).
//!
//! Layers (pure → platform):
//! * [`chunk`], [`guard`], [`budget`], [`queue`], [`reconcile`], [`live`] —
//!   pure logic, unit-tested without any OS.
//! * [`desktop`] — the real sinks: SendInput on Windows, enigo on macOS and
//!   Linux, UI Automation / AppKit / Wayland probes, clipboard fallback.

pub mod budget;
pub mod chunk;
pub mod guard;
pub mod live;
pub mod queue;
pub mod reconcile;

pub mod desktop;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("accessibility permission missing")]
    PermissionMissing,
    #[error("target is a password field; refusing")]
    PasswordField,
    #[error("target window is not in the foreground; injection blocked")]
    FocusLost,
    #[error("injection failed after retries: {0}")]
    Failed(String),
    #[error("not available on this platform")]
    Unavailable,
}

/// How much the sink can do with the target field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessLevel {
    /// Only keystrokes; no knowledge of existing text.
    BasicTyping,
    /// Can read/replace text around the caret (Windows UI Automation).
    FullTextControl,
    /// Text is handed to a host that inserts it (mobile).
    Host,
}

/// Result of probing the currently focused target before dictation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetStatus {
    /// An editable field is focused and injection is expected to work.
    Injectable(AccessLevel),
    /// We cannot tell (BasicTyping platforms without element inspection).
    /// The engine types optimistically and relies on runtime verification.
    Unknown,
    /// Focused element is a password field: never type into it.
    PasswordField,
    /// Target window runs elevated; unsigned builds' keystrokes are dropped
    /// by Windows (UIPI). Needs the signed UIAccess build or the fallback.
    Elevated,
    /// OS permission for injection missing (macOS Accessibility, Android
    /// keyboard not active). Shell must ask the user, not show the window.
    PermissionMissing,
    /// Nothing editable has focus (desktop, read-only view, file manager).
    NoEditableFocus,
    /// Injection impossible by construction (iOS, Wayland without wtype).
    NoInjection(String),
}

/// Result of checking that typed text actually arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Confirmed,
    Mismatch,
    /// Sink cannot read back (BasicTyping, Host).
    Unavailable,
}

/// The one owner of the target field. Everything the engine and the live
/// typer need: probe, type, delete, move the caret, read back.
pub trait TextSink: Send {
    fn access_level(&self) -> AccessLevel;
    /// Inspect the focused target. Must be cheap and must not hang: desktop
    /// implementations run under [`budget::with_budget`].
    fn probe(&mut self) -> TargetStatus;
    fn type_text(&mut self, text: &str) -> Result<(), OutputError>;
    fn backspace(&mut self, n: usize) -> Result<(), OutputError>;
    /// Move the caret left without deleting (used to skip an unchanged
    /// suffix during reconcile). Sinks that cannot return `Unavailable`;
    /// the reconcile then falls back to backspace-and-retype.
    fn cursor_left(&mut self, _n: usize) -> Result<(), OutputError> {
        Err(OutputError::Unavailable)
    }
    fn cursor_right(&mut self, _n: usize) -> Result<(), OutputError> {
        Err(OutputError::Unavailable)
    }
    /// Read back the tail of the target and compare with what we typed.
    fn verify(&mut self, _expected_tail: &str) -> Verification {
        Verification::Unavailable
    }
}

/// Sink that collects text for a host to insert (mobile, tests).
#[derive(Default)]
pub struct HostSink {
    pub pending: Vec<String>,
}

impl TextSink for HostSink {
    fn access_level(&self) -> AccessLevel {
        AccessLevel::Host
    }
    fn probe(&mut self) -> TargetStatus {
        TargetStatus::Injectable(AccessLevel::Host)
    }
    fn type_text(&mut self, text: &str) -> Result<(), OutputError> {
        self.pending.push(text.to_string());
        Ok(())
    }
    fn backspace(&mut self, n: usize) -> Result<(), OutputError> {
        self.pending.push(format!("\u{8}{n}"));
        Ok(())
    }
}

/// Best sink for this platform.
pub fn default_sink() -> Box<dyn TextSink> {
    {
        Box::new(desktop::DesktopSink::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_sink_collects_and_is_injectable() {
        let mut s = HostSink::default();
        assert_eq!(s.probe(), TargetStatus::Injectable(AccessLevel::Host));
        s.type_text("hallo").unwrap();
        s.backspace(2).unwrap();
        assert_eq!(s.pending.len(), 2);
        assert_eq!(s.verify("hallo"), Verification::Unavailable);
        assert!(matches!(s.cursor_left(1), Err(OutputError::Unavailable)));
    }
}
