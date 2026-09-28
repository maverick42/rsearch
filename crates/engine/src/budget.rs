//! Bounded byte budget for documents waiting for SQLite insertion.
//!
//! A pure item-count queue limit is insufficient: a handful of huge text
//! documents could otherwise occupy unbounded memory between the workers
//! and the SQLite writer. This budget bounds the total UTF-8 text bytes
//! waiting for insertion across the whole pipeline.
//!
//! A single document larger than the whole budget is still admitted
//! alone (when nothing else is in flight), so huge documents cannot
//! deadlock the build.
//!
//! Cancellation wakes all waiters.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Shared byte budget. `acquire` blocks until enough budget is available
/// or the budget is cancelled.
pub(crate) struct ByteBudget {
    total: usize,
    used: Mutex<usize>,
    cv: Condvar,
    cancelled: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetError {
    /// The build was cancelled while waiting for budget.
    Cancelled,
}

impl ByteBudget {
    pub fn new(total: usize) -> Self {
        ByteBudget {
            total,
            used: Mutex::new(0),
            cv: Condvar::new(),
            cancelled: AtomicBool::new(false),
        }
    }

    /// Acquires `bytes` of budget, blocking until available.
    ///
    /// A request larger than or equal to the total budget is admitted
    /// alone once nothing else is in flight.
    pub fn acquire(&self, bytes: usize) -> Result<(), BudgetError> {
        let mut used = self.used.lock().unwrap();
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(BudgetError::Cancelled);
            }
            let current = *used;
            let fits = if bytes >= self.total {
                current == 0
            } else {
                current + bytes <= self.total
            };
            if fits {
                *used = current + bytes;
                return Ok(());
            }
            let (guard, _timeout) = self
                .cv
                .wait_timeout(used, Duration::from_millis(100))
                .unwrap();
            used = guard;
        }
    }

    /// Releases `bytes` previously acquired. Called by the SQLite writer
    /// after it consumes a document.
    pub fn release(&self, bytes: usize) {
        let mut used = self.used.lock().unwrap();
        *used = used.saturating_sub(bytes);
        drop(used);
        self.cv.notify_all();
    }

    /// Wakes all waiters and makes every future `acquire` fail. Waiters
    /// blocked in `acquire` return promptly.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cv.notify_all();
    }

    /// Whether the budget has been cancelled. (Currently informational;
    /// waiters observe cancellation through the acquire error.)
    #[allow(dead_code)]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Bytes currently held by in-flight documents.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn in_flight(&self) -> usize {
        *self.used.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn acquire_and_release_within_budget() {
        let budget = ByteBudget::new(100);
        budget.acquire(60).unwrap();
        budget.acquire(40).unwrap();
        assert_eq!(budget.in_flight(), 100);
        budget.release(60);
        budget.acquire(60).unwrap();
    }

    #[test]
    fn oversized_document_is_admitted_alone() {
        let budget = ByteBudget::new(100);
        budget.acquire(500).unwrap();
        assert_eq!(budget.in_flight(), 500);
        budget.release(500);
        assert_eq!(budget.in_flight(), 0);
    }

    #[test]
    fn oversized_document_waits_while_budget_is_busy() {
        let budget = Arc::new(ByteBudget::new(100));
        budget.acquire(100).unwrap();
        let b2 = Arc::clone(&budget);
        let t = std::thread::spawn(move || {
            // Must block until the main thread releases.
            b2.acquire(500).unwrap();
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(budget.in_flight(), 100);
        budget.release(100);
        t.join().unwrap();
        assert_eq!(budget.in_flight(), 500);
    }

    #[test]
    fn cancel_unblocks_waiters() {
        let budget = Arc::new(ByteBudget::new(10));
        budget.acquire(10).unwrap();
        let b2 = Arc::clone(&budget);
        let start = Arc::new(Instant::now());
        let s2 = Arc::clone(&start);
        let t = std::thread::spawn(move || {
            let r = b2.acquire(5);
            assert!(matches!(r, Err(BudgetError::Cancelled)));
            assert!(s2.elapsed() < Duration::from_secs(5));
        });
        std::thread::sleep(Duration::from_millis(100));
        budget.cancel();
        t.join().unwrap();
    }
}
