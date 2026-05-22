//! Thread-local MMIO operation recorder for host tests.
//!
//! Cells call `recorder::log_read` / `log_write` on every volatile
//! access. In production (no `mock` feature), the functions are
//! `#[inline(always)]` no-ops — zero runtime cost; the compiler
//! drops the call entirely. Under the `mock` feature (test builds
//! of `lockjaw-mmio`, `lockjaw-regs`, and the codegen substrate),
//! the functions append to a thread-local `Vec<MmioOp>` so tests
//! can assert exactly-N writes, ordering, widths, and values.
//!
//! Why a separate module instead of inlining the recording into
//! `MockMmioRegion`: cells don't know about regions. A cell holds
//! an `UnsafeCell<T>` at a raw address; it has no back-reference
//! to the `MockMmioRegion` it lives inside. The recorder is the
//! one place every cell access can converge on — thread-local
//! state, no per-cell or per-region plumbing required.
//!
//! Why thread-local: cargo runs tests in parallel; each test
//! thread gets its own recorder. Recording state never leaks
//! across test boundaries.
//!
//! Op log is monotonic — `MockMmioRegion::take_ops()` drains it.
//! Tests that want a clean baseline call `take_ops()` once
//! pre-action to discard any prior log entries (e.g. from
//! construction-time writes).

/// One observed MMIO operation. Width is in BYTES (1 / 2 / 4 / 8)
/// — matching `size_of::<T>()` for the cell's word type — not bits.
/// Value is normalised to u64; smaller widths zero-extend.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MmioOp {
    /// Volatile read.
    Read {
        /// Absolute address read.
        addr: usize,
        /// Bytes read (1/2/4/8).
        width: usize,
        /// Value returned, zero-extended to u64.
        value: u64,
    },
    /// Volatile write.
    Write {
        /// Absolute address written.
        addr: usize,
        /// Bytes written (1/2/4/8).
        width: usize,
        /// Value written, zero-extended to u64.
        value: u64,
    },
}

// The recording machinery uses `std::thread_local!` so each parallel
// cargo-test thread gets its own log without inter-test contamination.
// `std` is only pulled in under the `mock` feature (or this crate's
// own `cfg(test)` builds); production driver builds stay no_std.
#[cfg(any(test, feature = "mock"))]
mod recording {
    extern crate std;
    use std::cell::RefCell;
    use std::vec::Vec;
    use super::MmioOp;

    std::thread_local! {
        /// Per-thread MMIO op log. `RefCell` so cells can borrow_mut
        /// without `&mut self`. The Vec grows monotonically until a
        /// test calls `MockMmioRegion::take_ops()` to drain it.
        static LOG: RefCell<Vec<MmioOp>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn push(op: MmioOp) {
        // Best-effort: if a re-entrant access (cell during cell)
        // ever borrowed the log, skip rather than panic — the
        // recorder is observability, not correctness. In practice
        // the cell methods don't recurse so this never trips, but
        // belt-and-braces.
        let _ = LOG.try_with(|log| {
            if let Ok(mut g) = log.try_borrow_mut() {
                g.push(op);
            }
        });
    }

    /// Take ownership of the recorded ops, resetting the log to empty.
    /// Tests typically call this immediately after the action whose
    /// MMIO ops they want to inspect; the first call clears any prior
    /// log entries (e.g. from `as_mapped_regs` construction).
    pub fn drain() -> Vec<MmioOp> {
        LOG.with(|log| core::mem::take(&mut *log.borrow_mut()))
    }
}

#[cfg(any(test, feature = "mock"))]
pub use recording::drain;

/// Log a volatile read. Inlined no-op in production builds.
#[inline(always)]
pub fn log_read(addr: usize, width: usize, value: u64) {
    #[cfg(any(test, feature = "mock"))]
    recording::push(MmioOp::Read { addr, width, value });
    // Without the `mock` feature: arguments are unused; LLVM
    // discards the call. Verified zero-cost via release-mode
    // codegen check on PL011 driver — `read_data()` compiles to
    // a single `ldr` with no surrounding bookkeeping.
    #[cfg(not(any(test, feature = "mock")))]
    let _ = (addr, width, value);
}

/// Log a volatile write. Inlined no-op in production builds.
#[inline(always)]
pub fn log_write(addr: usize, width: usize, value: u64) {
    #[cfg(any(test, feature = "mock"))]
    recording::push(MmioOp::Write { addr, width, value });
    #[cfg(not(any(test, feature = "mock")))]
    let _ = (addr, width, value);
}
