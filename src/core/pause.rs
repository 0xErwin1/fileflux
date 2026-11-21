//! The pause gate of a long transfer: a worker checks it between chunks and
//! items and blocks while it is closed. Pausing never interrupts a write, so
//! whatever the worker has created stays exactly what a cancel would clean
//! up; a cancel while paused wakes the worker, which then stops the normal
//! way.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Safety net for a missed wake-up: a blocked worker re-reads the cancel flag
/// at least this often even if nobody called [`PauseGate::wake`].
const RECHECK: Duration = Duration::from_millis(50);

/// A closable gate shared by the worker and the UI.
#[derive(Debug, Default)]
pub struct PauseGate {
    paused: Mutex<bool>,
    changed: Condvar,
}

impl PauseGate {
    fn lock(&self) -> MutexGuard<'_, bool> {
        // A flag has no invariant a panic could break.
        self.paused.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Closes the gate: the worker blocks at its next check.
    pub fn pause(&self) {
        *self.lock() = true;
    }

    /// Opens the gate and releases a blocked worker.
    pub fn resume(&self) {
        *self.lock() = false;
        self.changed.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        *self.lock()
    }

    /// Releases a blocked worker so it re-reads its cancel flag. The gate
    /// stays closed.
    pub fn wake(&self) {
        // Taking the lock orders this after a worker's check of the flag.
        drop(self.lock());
        self.changed.notify_all();
    }

    /// Blocks while the gate is closed and `cancel` is not set.
    pub fn wait_while_paused(&self, cancel: &AtomicBool) {
        let mut paused = self.lock();
        while *paused && !cancel.load(Ordering::Relaxed) {
            paused = self
                .changed
                .wait_timeout(paused, RECHECK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    const PROMPT: Duration = Duration::from_secs(2);

    #[test]
    fn an_unpaused_gate_never_blocks() {
        let gate = PauseGate::default();
        let cancel = AtomicBool::new(false);
        assert!(!gate.is_paused());
        let started = Instant::now();
        gate.wait_while_paused(&cancel);
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn pausing_blocks_the_worker_and_resuming_continues_it() {
        let gate = Arc::new(PauseGate::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let passed = Arc::new(AtomicU32::new(0));
        gate.pause();
        assert!(gate.is_paused());
        let worker = {
            let (gate, cancel, passed) = (gate.clone(), cancel.clone(), passed.clone());
            std::thread::spawn(move || {
                gate.wait_while_paused(&cancel);
                passed.fetch_add(1, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(passed.load(Ordering::SeqCst), 0, "blocked while paused");
        gate.resume();
        assert!(!gate.is_paused());
        let started = Instant::now();
        assert!(worker.join().is_ok(), "worker panicked");
        assert!(started.elapsed() < PROMPT);
        assert_eq!(passed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelling_while_paused_wakes_the_worker_promptly() {
        let gate = Arc::new(PauseGate::default());
        let cancel = Arc::new(AtomicBool::new(false));
        gate.pause();
        let worker = {
            let (gate, cancel) = (gate.clone(), cancel.clone());
            std::thread::spawn(move || gate.wait_while_paused(&cancel))
        };
        std::thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Relaxed);
        gate.wake();
        let started = Instant::now();
        assert!(worker.join().is_ok(), "worker panicked");
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(gate.is_paused(), "cancel does not resume; it only wakes");
    }
}
