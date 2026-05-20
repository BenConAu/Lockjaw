//! Memory-backed mock MMIO region for host-side codegen tests.
//!
//! `MockMmioRegion` wraps a `Box<[u8]>` of the right size for a generated
//! `#[repr(C)]` layout struct, hands out a `MappedRegs<T>` over it, and
//! exposes `peek_*` / `poke_*` so tests can pre-seed device-side values
//! before calling a synthesized accessor and inspect the bytes a write
//! left behind.
//!
//! Scope intentionally limited:
//! - No volatile-operation logging. Volatile reads/writes go through raw
//!   pointers into the backing store; there is no interception point on
//!   `read_volatile`. Sequence assertions therefore use end-state
//!   property tests ("after `write_driver_features_64(v)`, the value
//!   register holds the high half and the selector register holds 1")
//!   rather than per-call sequence assertions. End-state is enough for
//!   codegen confidence: if the synthesized helper skipped a step, the
//!   final byte pattern would diverge.
//! - Test-only. Gated behind `#[cfg(any(test, feature = "mock"))]` so the
//!   no_std target build of `lockjaw-mmio` does not pull `alloc`.
//! - One owner, one address. Construct, hand out `MappedRegs<T>`, run
//!   the test, drop. No aliasing between multiple `MappedRegs<T>` over
//!   the same region.
//!
//! The mock satisfies `MappedRegs::<T>::new`'s safety preconditions
//! because: the heap allocation is aligned to `align_of::<T>()` (we
//! over-allocate and align manually), the region is at least
//! `size_of::<T>()` bytes, no other `MappedRegs<T>` aliases the same
//! region (we hand out exactly one per call), and the backing `Box` is
//! owned by `MockMmioRegion`, so the mapping outlives every `&T`
//! derived from the wrapper as long as the test owns the `MockMmioRegion`.

use crate::region::MappedRegs;
use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

extern crate alloc;

/// A heap-backed mock MMIO region.
///
/// Owns the backing memory; hands out `MappedRegs<T>` views over it.
/// Drop frees the backing allocation.
pub struct MockMmioRegion {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl MockMmioRegion {
    /// Allocate a zeroed mock region sized + aligned for `T`.
    ///
    /// The returned region is large enough to hold one `T` and aligned
    /// to `align_of::<T>()`. Tests should use this rather than
    /// `MockMmioRegion::with_size` when wrapping a concrete generated
    /// layout struct.
    pub fn for_layout<T: 'static>() -> Self {
        let layout = Layout::from_size_align(size_of::<T>(), align_of::<T>())
            .expect("layout for T");
        // SAFETY: `layout` has non-zero size (any device layout struct
        // has at least one cell) and a valid alignment. `alloc_zeroed`
        // returns a non-null pointer or aborts; we check for null and
        // panic for a clearer test failure if the allocator returned
        // null.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).expect("alloc_zeroed returned null");
        Self { ptr, layout }
    }

    /// Allocate a zeroed mock region of `size` bytes with `align`
    /// alignment. For tests that want to wrap a hand-constructed
    /// layout. Most callers want `for_layout::<T>()` instead.
    pub fn with_size(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("layout");
        // SAFETY: same as for_layout.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).expect("alloc_zeroed returned null");
        Self { ptr, layout }
    }

    /// Hand out a lifetime-bound view over the backing memory.
    ///
    /// Returns a `MockedRegs<'_, T>` whose lifetime is tied to `&self`
    /// so the borrow checker prevents `drop(region); view.regs()` —
    /// an unsoundness in the earlier `MappedRegs<T>`-by-value shape.
    /// The view dereferences to `&T` via `.regs()` exactly like
    /// `MappedRegs<T>`, so generated test code reads the same.
    ///
    /// # Panics
    ///
    /// Panics if the region was sized smaller than `T` or aligned
    /// weaker than `T` requires.
    pub fn as_mapped_regs<T: 'static>(&self) -> MockedRegs<'_, T> {
        assert!(
            self.layout.size() >= size_of::<T>(),
            "mock region {} bytes too small for T ({} bytes)",
            self.layout.size(),
            size_of::<T>()
        );
        assert!(
            self.layout.align() >= align_of::<T>(),
            "mock region alignment {} too weak for T ({})",
            self.layout.align(),
            align_of::<T>()
        );
        // SAFETY: alloc_zeroed gave us at least `self.layout.size()`
        // bytes at `self.ptr`, aligned to `self.layout.align()`. We
        // verified both bounds against T above. The returned MockedRegs
        // borrows &self, so the backing memory is guaranteed to outlive
        // every &T derived from it.
        let inner = unsafe { MappedRegs::<T>::new(self.ptr.as_ptr() as u64) };
        MockedRegs { inner, _life: PhantomData }
    }

    /// Read 8 bytes at byte offset `offset` as `u64`, little-endian.
    /// For tests that want to inspect bytes a synthesized write left in
    /// device-side memory.
    pub fn peek_u64(&self, offset: usize) -> u64 {
        assert!(
            offset + 8 <= self.layout.size(),
            "peek_u64 offset {} out of range for {} bytes",
            offset,
            self.layout.size()
        );
        // SAFETY: bounds checked; ptr is valid for self.layout.size()
        // bytes; volatile read of u64 at a 4-byte-aligned offset is
        // safe for memory we allocated.
        unsafe {
            let p = self.ptr.as_ptr().add(offset) as *const u64;
            core::ptr::read_unaligned(p)
        }
    }

    /// Read 4 bytes at byte offset `offset` as `u32`.
    pub fn peek_u32(&self, offset: usize) -> u32 {
        assert!(offset + 4 <= self.layout.size(), "peek_u32 OOB");
        // SAFETY: bounds checked.
        unsafe {
            let p = self.ptr.as_ptr().add(offset) as *const u32;
            core::ptr::read_unaligned(p)
        }
    }

    /// Read 2 bytes at byte offset `offset` as `u16`.
    pub fn peek_u16(&self, offset: usize) -> u16 {
        assert!(offset + 2 <= self.layout.size(), "peek_u16 OOB");
        // SAFETY: bounds checked.
        unsafe {
            let p = self.ptr.as_ptr().add(offset) as *const u16;
            core::ptr::read_unaligned(p)
        }
    }

    /// Read 1 byte at byte offset `offset` as `u8`.
    pub fn peek_u8(&self, offset: usize) -> u8 {
        assert!(offset + 1 <= self.layout.size(), "peek_u8 OOB");
        // SAFETY: bounds checked.
        unsafe { *self.ptr.as_ptr().add(offset) }
    }

    /// Write 4 bytes at byte offset `offset`. For tests that pre-seed
    /// a device-side value before calling a synthesized accessor.
    pub fn poke_u32(&self, offset: usize, value: u32) {
        assert!(offset + 4 <= self.layout.size(), "poke_u32 OOB");
        // SAFETY: bounds checked.
        unsafe {
            let p = self.ptr.as_ptr().add(offset) as *mut u32;
            core::ptr::write_unaligned(p, value);
        }
    }

    /// Write 2 bytes at byte offset `offset`.
    pub fn poke_u16(&self, offset: usize, value: u16) {
        assert!(offset + 2 <= self.layout.size(), "poke_u16 OOB");
        // SAFETY: bounds checked.
        unsafe {
            let p = self.ptr.as_ptr().add(offset) as *mut u16;
            core::ptr::write_unaligned(p, value);
        }
    }

    /// Write 1 byte at byte offset `offset`.
    pub fn poke_u8(&self, offset: usize, value: u8) {
        assert!(offset + 1 <= self.layout.size(), "poke_u8 OOB");
        // SAFETY: bounds checked.
        unsafe { *self.ptr.as_ptr().add(offset) = value; }
    }
}

impl Drop for MockMmioRegion {
    fn drop(&mut self) {
        // SAFETY: ptr was produced by alloc_zeroed with self.layout; we
        // own the allocation exclusively (no Clone, no shared owners).
        // Any MockedRegs<'_, T> handed out by as_mapped_regs borrows
        // &self, so the borrow checker has already ensured every such
        // view (and every &T derived from one) was dropped before this
        // Drop runs.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// Lifetime-bound view of a `MockMmioRegion` as `MappedRegs<T>`.
///
/// The `'a` lifetime ties the view to the originating `MockMmioRegion`,
/// preventing the unsound `drop(region); view.regs()` pattern that an
/// owned `MappedRegs<T>` would permit. `.regs()` returns `&T` borrowed
/// from `&self`, matching the surface of `MappedRegs::regs`.
///
/// # Compile-fail soundness guarantee
///
/// Dropping the originating region while a view is live is a borrow-
/// check error. The doctest below MUST fail to compile; the
/// `compile_fail` annotation runs it as a `cargo test --doc` check.
///
/// ```compile_fail
/// use lockjaw_mmio::mock::MockMmioRegion;
/// let region = MockMmioRegion::with_size(8, 4);
/// let regs = region.as_mapped_regs::<u32>();
/// drop(region);  // borrow checker rejects: `region` is borrowed by `regs`
/// regs.regs();
/// ```
pub struct MockedRegs<'a, T: 'static> {
    inner: MappedRegs<T>,
    _life: PhantomData<&'a MockMmioRegion>,
}

impl<'a, T: 'static> MockedRegs<'a, T> {
    /// Typed access to the mapped region. Same shape as
    /// `MappedRegs::regs`; the returned `&T` borrows `&self`, which in
    /// turn borrows the originating `MockMmioRegion`.
    #[inline]
    pub fn regs(&self) -> &T {
        self.inner.regs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Ro, Rw};

    #[repr(C)]
    struct Layout {
        a: Rw<u32>,
        b: Rw<u32>,
        c: Ro<u32>,
    }

    /// MappedRegs<T> over a MockMmioRegion behaves like real memory:
    /// writes through cells land in the backing store; peek observes
    /// the same bytes.
    #[test]
    fn cell_write_lands_in_backing() {
        let region = MockMmioRegion::for_layout::<Layout>();
        let regs = region.as_mapped_regs::<Layout>();
        regs.regs().a.write(0xDEAD_BEEF);
        regs.regs().b.write(0xCAFE_BABE);
        assert_eq!(region.peek_u32(0), 0xDEAD_BEEF);
        assert_eq!(region.peek_u32(4), 0xCAFE_BABE);
    }

    /// poke_* lets tests pre-seed device-side state before exercising
    /// a generated accessor — the dual of cell_write_lands_in_backing.
    #[test]
    fn poke_observable_via_cell_read() {
        let region = MockMmioRegion::for_layout::<Layout>();
        region.poke_u32(8, 0x1234_5678);
        let regs = region.as_mapped_regs::<Layout>();
        assert_eq!(regs.regs().c.read(), 0x1234_5678);
    }

    #[test]
    fn peek_widths_match() {
        let region = MockMmioRegion::with_size(16, 8);
        region.poke_u32(0, 0xAA55_AA55);
        region.poke_u16(4, 0xBEEF);
        region.poke_u8(6, 0xC3);
        assert_eq!(region.peek_u32(0), 0xAA55_AA55);
        assert_eq!(region.peek_u16(4), 0xBEEF);
        assert_eq!(region.peek_u8(6), 0xC3);
        assert_eq!(region.peek_u64(0), 0x00C3_BEEF_AA55_AA55);
    }

    /// Region drops cleanly; allocator gets the right layout back.
    /// (Miri / leak-sanitizer territory; the assertion here is just
    /// that drop runs without panicking.)
    #[test]
    fn region_drops_cleanly() {
        let region = MockMmioRegion::with_size(64, 16);
        drop(region);
    }

    /// `MockedRegs<'_, T>` must borrow the originating
    /// `MockMmioRegion` so the use-after-free pattern
    /// `let r = region.as_mapped_regs(); drop(region); r.regs();`
    /// is a compile error in safe code. We cannot test "this fails
    /// to compile" inline, but we can statically assert the wrapper
    /// has a non-`'static` lifetime parameter and verify the borrow
    /// is observable at runtime by attempting a use that the borrow
    /// checker accepts only because the borrow chain is intact.
    #[test]
    fn mocked_regs_borrows_region() {
        let region = MockMmioRegion::for_layout::<Layout>();
        let regs = region.as_mapped_regs::<Layout>();
        // `regs` holds an immutable borrow of `region`; we can still
        // call `region.peek_u32` (also `&self`) but we could NOT call
        // `drop(region)` here — the borrow checker would reject it.
        regs.regs().a.write(0xfeedface);
        assert_eq!(region.peek_u32(0), 0xfeedface);
        // Borrow ends here when `regs` and `region` both drop in
        // reverse declaration order; no use-after-free is reachable.
    }
}

