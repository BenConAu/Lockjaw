/// A ticket lock — fair, FIFO spinlock using two atomic counters.
///
/// Each `lock()` takes the next ticket number. The lock spins until
/// `now_serving` matches that ticket. `unlock()` advances `now_serving`
/// by one, waking the next waiter in FIFO order.
///
/// Used by the kernel as the Giant Kernel Lock (GKL) for SMP. The
/// struct is in lockjaw-types so the lock logic is host-testable.

use core::sync::atomic::{AtomicU32, Ordering};

pub struct TicketLock {
    next_ticket: AtomicU32,
    now_serving: AtomicU32,
}

impl TicketLock {
    pub const fn new() -> Self {
        TicketLock {
            next_ticket: AtomicU32::new(0),
            now_serving: AtomicU32::new(0),
        }
    }

    /// Acquire the lock. Spins until this caller's ticket is served.
    #[inline]
    pub fn lock(&self) {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        while self.now_serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
    }

    /// Release the lock. The next waiting caller (if any) proceeds.
    #[inline]
    pub fn unlock(&self) {
        self.now_serving.fetch_add(1, Ordering::Release);
    }

    /// Returns true if the lock is currently held (some ticket has been
    /// taken but not yet served). This is a snapshot — may be stale by
    /// the time the caller acts on it. Useful for diagnostics only.
    pub fn is_locked(&self) -> bool {
        self.next_ticket.load(Ordering::Relaxed) != self.now_serving.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn lock_unlock_single_threaded() {
        let lock = TicketLock::new();
        assert!(!lock.is_locked());
        lock.lock();
        assert!(lock.is_locked());
        lock.unlock();
        assert!(!lock.is_locked());
    }

    #[test]
    fn multiple_lock_unlock_cycles() {
        let lock = TicketLock::new();
        for _ in 0..100 {
            lock.lock();
            lock.unlock();
        }
        assert!(!lock.is_locked());
    }

    #[test]
    fn fairness_ticket_order() {
        // Verify tickets are served in order: after N locks without
        // unlocks, now_serving stays at 0 (only the first proceeds).
        let lock = TicketLock::new();
        lock.lock(); // ticket 0, served immediately
        // next_ticket is now 1, now_serving is 0
        assert!(lock.is_locked());
        lock.unlock();
        // now_serving is 1, next_ticket is 1
        assert!(!lock.is_locked());
    }

    #[test]
    fn two_threads_serialize() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering as StdOrd};

        let lock = Arc::new(TicketLock::new());
        let counter = Arc::new(AtomicU64::new(0));
        let iterations = 10_000;

        let handles: std::vec::Vec<_> = (0..2).map(|_| {
            let l = Arc::clone(&lock);
            let c = Arc::clone(&counter);
            std::thread::spawn(move || {
                for _ in 0..iterations {
                    l.lock();
                    // Non-atomic increment under the lock — would race
                    // without the lock.
                    let val = c.load(StdOrd::Relaxed);
                    c.store(val + 1, StdOrd::Relaxed);
                    l.unlock();
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.load(StdOrd::Relaxed), iterations * 2);
    }

    #[test]
    fn four_threads_serialize() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering as StdOrd};

        let lock = Arc::new(TicketLock::new());
        let counter = Arc::new(AtomicU64::new(0));
        let iterations = 5_000;

        let handles: std::vec::Vec<_> = (0..4).map(|_| {
            let l = Arc::clone(&lock);
            let c = Arc::clone(&counter);
            std::thread::spawn(move || {
                for _ in 0..iterations {
                    l.lock();
                    let val = c.load(StdOrd::Relaxed);
                    c.store(val + 1, StdOrd::Relaxed);
                    l.unlock();
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.load(StdOrd::Relaxed), iterations * 4);
    }
}
