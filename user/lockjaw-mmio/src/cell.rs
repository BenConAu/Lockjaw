//! Typed MMIO register cells.
//!
//! Each cell type wraps an `UnsafeCell<T>` and exposes only `&self`
//! methods. Volatile load/store is the per-operation contract;
//! callers do not need to be `&mut`. Because the wrapped type is
//! `UnsafeCell`, the cells are `!Sync` by default — the substrate
//! makes no claim that a single cell can be accessed from multiple
//! threads concurrently. Drivers are single-threaded today; if one
//! ever needs cross-thread MMIO it must wrap the entire
//! `MappedRegs<T>` in its own synchronization primitive (see the
//! crate-level docs).
//!
//! Each cell type has a different surface intentionally:
//! - `Ro<T>` — `read()` only
//! - `Rw<T>` — `read()`, `write(v)`, `modify(F)`
//! - `Wo<T>` — `write(v)` only
//! - `W1c<T>` — `read()`, `clear(mask)` (writing `mask` clears those
//!   bits in hardware; no `with_*` setter, because the semantics are
//!   not ordinary RW)
//!
//! Codex consultation recommended exactly these four variants and
//! explicitly warned against a common `MmioCell` trait — different
//! access kinds have different APIs and a unified trait would push
//! everyone to the least-common-denominator (and re-introduce the
//! footgun the substrate exists to eliminate).

use core::cell::UnsafeCell;
use core::ptr;

/// Marker for sizes legal as a single MMIO transaction on AArch64.
/// Restricts cells to integer widths the architecture supports as
/// single load/store instructions.
///
/// # Safety
///
/// `unsafe` because implementing this trait asserts two things the
/// compiler cannot verify:
/// 1. `read_volatile::<Self>(ptr)` and `write_volatile::<Self>(ptr, v)`
///    compile to a SINGLE machine instruction matching the
///    architecture's native MMIO transaction widths (ldrb/ldrh/ldr/ldr64
///    on AArch64). Composite types or types wider than one register
///    would split into multiple accesses, which is not how MMIO works.
/// 2. Every bit pattern of `size_of::<Self>()` bytes is a valid
///    `Self` — so that a `read_volatile` of arbitrary device-written
///    bytes is sound. (This rules out `bool`, restricted-discriminant
///    enums, `&T`, `NonNull<T>`, etc.)
///
/// Implementations live in this crate only — `pub` traits are not
/// sealed in Rust but `unsafe` requires downstream implementors to
/// write `unsafe impl`, which a `#![forbid(unsafe_code)]` driver
/// crate cannot do. The audited corpus of MMIO word types is the
/// four primitive integer widths below.
pub unsafe trait MmioWord: Copy + 'static {}

// SAFETY: each primitive integer has a single matching AArch64 load/
// store instruction, and every bit pattern is a valid value.
unsafe impl MmioWord for u8 {}
unsafe impl MmioWord for u16 {}
unsafe impl MmioWord for u32 {}
unsafe impl MmioWord for u64 {}

/// Read-only MMIO register cell.
#[repr(transparent)]
pub struct Ro<T: MmioWord>(UnsafeCell<T>);

impl<T: MmioWord> Ro<T> {
    /// Volatile read of the register.
    #[inline(always)]
    pub fn read(&self) -> T {
        // SAFETY: `self.0` is an UnsafeCell<T> at a valid MMIO address
        // (precondition asserted by MappedRegs::new at the substrate
        // boundary). `read_volatile` is the only legal way to access
        // hardware registers; T: MmioWord restricts the width to an
        // architecture-supported single load.
        unsafe { ptr::read_volatile(self.0.get()) }
    }
}

/// Read-write MMIO register cell.
#[repr(transparent)]
pub struct Rw<T: MmioWord>(UnsafeCell<T>);

impl<T: MmioWord> Rw<T> {
    /// Volatile read.
    #[inline(always)]
    pub fn read(&self) -> T {
        // SAFETY: as Ro<T>::read.
        unsafe { ptr::read_volatile(self.0.get()) }
    }

    /// Volatile write.
    #[inline(always)]
    pub fn write(&self, value: T) {
        // SAFETY: as Ro<T>::read; write_volatile is the legal write
        // primitive for hardware registers.
        unsafe { ptr::write_volatile(self.0.get(), value) }
    }

    /// Read-modify-write. The closure receives the current value and
    /// returns the new value to write. Useful for setting/clearing
    /// individual fields while preserving the rest.
    #[inline(always)]
    pub fn modify<F: FnOnce(T) -> T>(&self, f: F) {
        self.write(f(self.read()));
    }
}

/// Write-only MMIO register cell. No `read()` — writing a value to a
/// register declared write-only and reading from it back are different
/// operations; the spec for many WO registers explicitly says read is
/// undefined. Provide no API surface for it.
#[repr(transparent)]
pub struct Wo<T: MmioWord>(UnsafeCell<T>);

impl<T: MmioWord> Wo<T> {
    /// Volatile write.
    #[inline(always)]
    pub fn write(&self, value: T) {
        // SAFETY: as Rw<T>::write.
        unsafe { ptr::write_volatile(self.0.get(), value) }
    }
}

/// Write-1-to-clear MMIO register cell. Reading observes the current
/// status bits. Writing a value clears only the bits set in that
/// value (the hardware semantics — "write 1 to clear"). There is no
/// `with_*`-style setter because the operation is not "set the
/// register to X" but "clear the bits I write 1 in"; an ordinary
/// setter would be misleading.
#[repr(transparent)]
pub struct W1c<T: MmioWord>(UnsafeCell<T>);

impl<T: MmioWord> W1c<T> {
    /// Volatile read of the status bits.
    #[inline(always)]
    pub fn read(&self) -> T {
        // SAFETY: as Ro<T>::read.
        unsafe { ptr::read_volatile(self.0.get()) }
    }

    /// Write `mask` to the register, clearing every bit set in `mask`.
    /// Bits not set in `mask` are unaffected.
    #[inline(always)]
    pub fn clear(&self, mask: T) {
        // SAFETY: as Wo<T>::write. W1C registers expect this write
        // semantics from the hardware spec; the volatile primitive is
        // the right way to deliver the write.
        unsafe { ptr::write_volatile(self.0.get(), mask) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells are intentionally `!Sync` (UnsafeCell makes them so).
    /// They *are* `Send` when `T: Send` — that's normal UnsafeCell
    /// behaviour and is fine; multi-threaded use would still require
    /// the caller to provide synchronization (Mutex around the
    /// containing `MappedRegs`), which is out of scope for this
    /// substrate. The test specifically guards the `!Sync` invariant.
    #[test]
    fn cells_are_not_sync() {
        use static_assertions::assert_not_impl_any;
        assert_not_impl_any!(Ro<u32>: Sync);
        assert_not_impl_any!(Rw<u32>: Sync);
        assert_not_impl_any!(Wo<u32>: Sync);
        assert_not_impl_any!(W1c<u32>: Sync);
    }

    /// Cells are `#[repr(transparent)]` over `UnsafeCell<T>`, which
    /// is `#[repr(transparent)]` over `T`. So the cell occupies
    /// exactly `size_of::<T>()` bytes and `align_of::<T>()` bytes —
    /// safe to embed at exact offsets in a `#[repr(C)]` layout struct.
    #[test]
    fn cell_layout_matches_inner() {
        use core::mem::{align_of, size_of};
        assert_eq!(size_of::<Ro<u8>>(), size_of::<u8>());
        assert_eq!(align_of::<Ro<u8>>(), align_of::<u8>());
        assert_eq!(size_of::<Rw<u16>>(), size_of::<u16>());
        assert_eq!(align_of::<Rw<u16>>(), align_of::<u16>());
        assert_eq!(size_of::<Wo<u32>>(), size_of::<u32>());
        assert_eq!(align_of::<Wo<u32>>(), align_of::<u32>());
        assert_eq!(size_of::<W1c<u64>>(), size_of::<u64>());
        assert_eq!(align_of::<W1c<u64>>(), align_of::<u64>());
    }

    /// Roundtrip Rw on a backing UnsafeCell — proves volatile read
    /// and write semantics work end-to-end on real memory (no MMIO
    /// involvement; just exercising the cell mechanics).
    #[test]
    fn rw_roundtrip() {
        // Allocate a u32 we can point a cell at. UnsafeCell can't be
        // constructed externally for Rw<T> (it's private), so we
        // transmute via the repr(transparent) guarantee.
        let backing: UnsafeCell<u32> = UnsafeCell::new(0);
        // SAFETY: Rw<u32> is #[repr(transparent)] over UnsafeCell<u32>.
        let cell: &Rw<u32> = unsafe { &*(&backing as *const UnsafeCell<u32> as *const Rw<u32>) };

        assert_eq!(cell.read(), 0);
        cell.write(0xdead_beef);
        assert_eq!(cell.read(), 0xdead_beef);

        cell.modify(|v| v.wrapping_add(1));
        assert_eq!(cell.read(), 0xdead_beef_u32.wrapping_add(1));
    }

    /// W1C clear() roundtrip — writing `mask` is observable as a
    /// volatile write, distinct from `Rw::write`. Backing memory
    /// doesn't model W1C hardware semantics (CPU memory accepts the
    /// write verbatim), but the *call* must succeed and the *write*
    /// must land.
    #[test]
    fn w1c_clear_writes_mask() {
        let backing: UnsafeCell<u16> = UnsafeCell::new(0);
        // SAFETY: W1c<u16> is #[repr(transparent)] over UnsafeCell<u16>.
        let cell: &W1c<u16> = unsafe { &*(&backing as *const UnsafeCell<u16> as *const W1c<u16>) };
        cell.clear(0xff00);
        assert_eq!(cell.read(), 0xff00);
    }
}
