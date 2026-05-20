//! `lockjaw-mmio` — the audited substrate for user-mode driver access
//! to MMIO, DMA-shared memory, and memory barriers.
//!
//! This is the only crate in user-mode driver code that uses `unsafe`.
//! Drivers depend on it (and on the generated `lockjaw-regs`) and add
//! `#![forbid(unsafe_code)]` at their crate root. The `unsafe`
//! surface here is intentionally small and concentrated:
//!
//! - `cell` — typed MMIO register cells (`Ro`/`Rw`/`Wo`/`W1c`).
//!   Volatile load/store wrapped around `UnsafeCell<T>`. The cells
//!   are `!Sync` by default; the substrate makes no concurrency claim
//!   beyond that.
//! - `region` — `MappedRegs<T>` typed region wrapper. One `unsafe`
//!   constructor; safe `regs() -> &T` afterwards.
//! - `dma` — `DmaCell<T>` / `DmaSlice<T, N>` for typed access to
//!   DMA-shared memory. `T` must implement `lockjaw_types::dma::DmaValue`
//!   (a bit-pattern-safety marker, vetted by lockjaw-types).
//! - `barrier` — safe wrappers around `dsb sy` / `dmb ish*` / `isb`.
//!   Callers in `#![forbid(unsafe_code)]` crates can call these
//!   directly.
//!
//! Driver code never writes `unsafe`. The `unsafe` blocks here are
//! audited individually; each is one line, no business logic mixed in.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

pub mod barrier;
pub mod cell;
pub mod dma;
pub mod region;

// Host-side test substrate: memory-backed mock MMIO region for codegen
// tests. Gated behind `#[cfg(any(test, feature = "mock"))]` so the
// no_std target build does not pull `alloc` into production driver
// crates. `lockjaw-regs` enables the `mock` feature for its codegen
// tests.
#[cfg(any(test, feature = "mock"))]
pub mod mock;
