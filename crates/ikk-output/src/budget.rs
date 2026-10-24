//! Hard time budget for target probing. Foreign interfaces (COM/UI
//! Automation, AppKit) may hang on a non-responding app; over budget the
//! result is discarded and the caller falls back to basic typing rather
//! than freezing dictation. The worker keeps running to completion in the
//! background, so `f` must return plain values only, never OS handles.

use std::sync::mpsc;
use std::time::Duration;

pub const PROBE_BUDGET: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExceeded {
    pub budget: Duration,
}

pub fn with_budget<T, F>(budget: Duration, f: F) -> Result<T, BudgetExceeded>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(budget).map_err(|_| BudgetExceeded { budget })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_work_returns_slow_work_is_discarded() {
        assert_eq!(with_budget(Duration::from_millis(200), || 42), Ok(42));
        let r = with_budget(Duration::from_millis(20), || {
            std::thread::sleep(Duration::from_millis(200));
            1
        });
        assert!(r.is_err());
    }
}
