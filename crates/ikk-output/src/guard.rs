//! Focus guard: never type into a window the user did not focus when the
//! session started. The pure decision is here; platform guards compare the
//! remembered target with the current foreground window/app.

/// No remembered target → allow (nothing to protect). Remembered but not in
/// front → block, fail-closed: text in the wrong window is irreversible.
pub fn should_inject(has_target: bool, foreground_is_target: bool) -> bool {
    !has_target || foreground_is_target
}

pub trait ForegroundGuard: Send {
    fn may_inject(&self) -> bool;
}

/// Platforms without focus tracking: always allow.
pub struct AllowAllGuard;

impl ForegroundGuard for AllowAllGuard {
    fn may_inject(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_only_known_wrong_target() {
        assert!(should_inject(false, false));
        assert!(should_inject(true, true));
        assert!(!should_inject(true, false));
        assert!(AllowAllGuard.may_inject());
    }
}
