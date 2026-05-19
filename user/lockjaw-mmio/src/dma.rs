//! Typed access to DMA-shared memory.
//!
//! `DmaCell<T>` and `DmaSlice<T, N>` wrap raw pointers to memory the
//! driver has mapped for DMA (typically `NormalNonCacheable` for
//! emmc2 ADMA, or `Normal` for virtio rings on coherent hardware).
//! They provide typed volatile read/write, with the bit-pattern
//! safety contract enforced by the `T: DmaValue` bound from
//! `lockjaw-types`.
//!
//! # Scope of the abstraction (read before adding fields)
//!
//! - Provides typed *volatile* access to memory shared between CPU
//!   and device DMA.
//! - Does **NOT** model cache coherency. Each `DmaCell` / `DmaSlice`
//!   must be backed by a page whose mapping attribute matches the
//!   intended access pattern:
//!   - `NormalNonCacheable` (emmc2 ADMA buffers via the DMA pool) —
//!     device sees what CPU writes immediately; no cache maintenance
//!     needed.
//!   - `Normal` (virtio rings, virtio-blk request header, ramfb
//!     fw_cfg DMA scratch) — device-coherent on the hardware Lockjaw
//!     runs on; relies on hardware coherency, no explicit cache flush.
//! - Caller is responsible for: (a) ensuring the mapping attribute
//!   is appropriate; (b) issuing barriers around publication points
//!   (via `lockjaw_mmio::barrier::*`); (c) cache maintenance if a
//!   future driver mixes cacheable CPU access with non-coherent DMA
//!   (substrate would need `DC_CVAC` / `DC_IVAC` helpers — not yet).
//! - The substrate's claim: "removes the volatile-write/read
//!   mechanics and pointer arithmetic from drivers" — NOT "DMA is
//!   correct by construction."

use core::marker::PhantomData;
use core::ptr;

pub use lockjaw_types::dma::DmaValue;

/// Single typed value in DMA-mapped memory.
pub struct DmaCell<T: DmaValue> {
    ptr: *mut T,
    _phantom: PhantomData<T>,
}

impl<T: DmaValue> DmaCell<T> {
    /// Construct a `DmaCell` at a given virtual address.
    ///
    /// # Safety
    ///
    /// Caller asserts:
    /// - `va` points to a writable, properly-aligned `T`.
    /// - The mapping attribute is appropriate for the device's
    ///   access pattern (see the module-level scope notes).
    /// - No other `DmaCell<T>` instance aliases this address.
    /// - The mapping outlives this `DmaCell`.
    #[inline]
    pub const unsafe fn at(va: u64) -> Self {
        Self {
            ptr: va as *mut T,
            _phantom: PhantomData,
        }
    }

    /// Volatile write.
    #[inline(always)]
    pub fn write(&self, value: T) {
        // SAFETY: `self.ptr` valid per `at()`. T: DmaValue so any
        // value is legal in memory.
        unsafe { ptr::write_volatile(self.ptr, value) }
    }

    /// Volatile read. Sound for any `T: DmaValue` because every bit
    /// pattern of size `T` is a valid `T`.
    #[inline(always)]
    pub fn read(&self) -> T {
        // SAFETY: `self.ptr` valid per `at()`. T: DmaValue makes
        // reading arbitrary bytes back into a `T` sound.
        unsafe { ptr::read_volatile(self.ptr) }
    }
}

/// Bounded-length array in DMA-mapped memory (e.g. ADMA descriptor
/// table, virtqueue rings). Bounds-checked at index time.
pub struct DmaSlice<T: DmaValue, const N: usize> {
    ptr: *mut T,
    _phantom: PhantomData<T>,
}

impl<T: DmaValue, const N: usize> DmaSlice<T, N> {
    /// Construct a `DmaSlice` at a given virtual address.
    ///
    /// # Safety
    ///
    /// Caller asserts:
    /// - `va` points to a writable, properly-aligned region of at
    ///   least `N * size_of::<T>()` bytes.
    /// - Mapping attribute appropriate for the device's access
    ///   pattern.
    /// - No other `DmaSlice<T, N>` instance aliases this region.
    /// - The mapping outlives this `DmaSlice`.
    #[inline]
    pub const unsafe fn at(va: u64) -> Self {
        Self {
            ptr: va as *mut T,
            _phantom: PhantomData,
        }
    }

    /// Volatile write at index `idx`. Panics if `idx >= N`.
    #[inline(always)]
    pub fn write(&self, idx: usize, value: T) {
        assert!(idx < N, "DmaSlice write OOB: idx={} >= N={}", idx, N);
        // SAFETY: bounds checked; `self.ptr.add(idx)` is within the
        // mapped region per `at()`'s precondition.
        unsafe { ptr::write_volatile(self.ptr.add(idx), value) }
    }

    /// Volatile read at index `idx`. Panics if `idx >= N`.
    #[inline(always)]
    pub fn read(&self, idx: usize) -> T {
        assert!(idx < N, "DmaSlice read OOB: idx={} >= N={}", idx, N);
        // SAFETY: bounds checked; `T: DmaValue` per the trait bound.
        unsafe { ptr::read_volatile(self.ptr.add(idx)) }
    }

    /// Length of the slice (always `N`).
    #[inline(always)]
    pub const fn len(&self) -> usize {
        N
    }

    /// Whether the slice has zero length.
    #[inline(always)]
    pub const fn is_empty(&self) -> bool {
        N == 0
    }
}

/// Bounded-length DMA slice with **runtime** length. Same shape as
/// `DmaSlice<T, N>` but the length is fixed at construction time
/// rather than compile time. Use this when the bound comes from
/// device-reported state (e.g. virtio's `QUEUE_NUM_MAX` clamps the
/// virtqueue ring length at runtime).
pub struct DmaSliceDyn<T: DmaValue> {
    ptr: *mut T,
    len: usize,
    _phantom: PhantomData<T>,
}

impl<T: DmaValue> DmaSliceDyn<T> {
    /// Construct a `DmaSliceDyn` at a given virtual address.
    ///
    /// # Safety
    ///
    /// Caller asserts:
    /// - `va` points to a writable, properly-aligned region of at
    ///   least `len * size_of::<T>()` bytes.
    /// - Mapping attribute appropriate for the device's access pattern.
    /// - No other `DmaSliceDyn<T>` instance aliases this region.
    /// - The mapping outlives this slice.
    #[inline]
    pub const unsafe fn at(va: u64, len: usize) -> Self {
        Self {
            ptr: va as *mut T,
            len,
            _phantom: PhantomData,
        }
    }

    /// Volatile write at index `idx`. Panics if `idx >= len`.
    #[inline(always)]
    pub fn write(&self, idx: usize, value: T) {
        assert!(idx < self.len, "DmaSliceDyn write OOB: idx={} >= len={}", idx, self.len);
        // SAFETY: bounds checked; `self.ptr.add(idx)` is within the
        // mapped region per `at()`'s precondition.
        unsafe { ptr::write_volatile(self.ptr.add(idx), value) }
    }

    /// Volatile read at index `idx`. Panics if `idx >= len`.
    #[inline(always)]
    pub fn read(&self, idx: usize) -> T {
        assert!(idx < self.len, "DmaSliceDyn read OOB: idx={} >= len={}", idx, self.len);
        // SAFETY: bounds checked; `T: DmaValue` per the trait bound.
        unsafe { ptr::read_volatile(self.ptr.add(idx)) }
    }

    /// Length of the slice (runtime).
    #[inline(always)]
    pub const fn len(&self) -> usize { self.len }

    /// Whether the slice has zero length.
    #[inline(always)]
    pub const fn is_empty(&self) -> bool { self.len == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DmaCell roundtrip on backing memory. Same shape as the cell
    /// tests in `cell.rs` — we can't exercise actual device DMA on
    /// host, but the volatile read/write mechanics work on any
    /// pointer to a `DmaValue` type.
    #[test]
    fn dma_cell_roundtrip() {
        let mut backing: u32 = 0;
        // SAFETY: backing lives for the duration of this test;
        // DmaCell holds a raw pointer to it.
        let cell = unsafe { DmaCell::<u32>::at(&mut backing as *mut u32 as u64) };
        cell.write(0xdead_beef);
        assert_eq!(cell.read(), 0xdead_beef);
        assert_eq!(backing, 0xdead_beef);
    }

    #[test]
    fn dma_slice_roundtrip() {
        let mut backing: [u32; 4] = [0; 4];
        // SAFETY: backing lives for the duration of this test.
        let slice = unsafe { DmaSlice::<u32, 4>::at(backing.as_mut_ptr() as u64) };
        for i in 0..4 {
            slice.write(i, 0xa000_0000 | (i as u32));
        }
        for i in 0..4 {
            assert_eq!(slice.read(i), 0xa000_0000 | (i as u32));
            assert_eq!(backing[i], 0xa000_0000 | (i as u32));
        }
        assert_eq!(slice.len(), 4);
        assert!(!slice.is_empty());
    }

    #[test]
    #[should_panic(expected = "DmaSlice write OOB")]
    fn dma_slice_write_oob_panics() {
        let mut backing: [u32; 2] = [0; 2];
        let slice = unsafe { DmaSlice::<u32, 2>::at(backing.as_mut_ptr() as u64) };
        slice.write(2, 0); // 2 >= N=2 → panic
    }

    #[test]
    #[should_panic(expected = "DmaSlice read OOB")]
    fn dma_slice_read_oob_panics() {
        let mut backing: [u32; 2] = [0; 2];
        let slice = unsafe { DmaSlice::<u32, 2>::at(backing.as_mut_ptr() as u64) };
        let _ = slice.read(2);
    }

    #[test]
    fn dma_slice_dyn_roundtrip() {
        let mut backing: [u16; 6] = [0; 6];
        // SAFETY: backing lives for the duration of this test.
        let slice = unsafe { DmaSliceDyn::<u16>::at(backing.as_mut_ptr() as u64, 6) };
        assert_eq!(slice.len(), 6);
        assert!(!slice.is_empty());
        for i in 0..6 {
            slice.write(i, 0x1000 | (i as u16));
        }
        for i in 0..6 {
            assert_eq!(slice.read(i), 0x1000 | (i as u16));
            assert_eq!(backing[i], 0x1000 | (i as u16));
        }
    }

    #[test]
    #[should_panic(expected = "DmaSliceDyn write OOB")]
    fn dma_slice_dyn_write_oob_panics() {
        let mut backing: [u32; 3] = [0; 3];
        let slice = unsafe { DmaSliceDyn::<u32>::at(backing.as_mut_ptr() as u64, 3) };
        slice.write(3, 0);
    }

    #[test]
    #[should_panic(expected = "DmaSliceDyn read OOB")]
    fn dma_slice_dyn_read_oob_panics() {
        let mut backing: [u32; 3] = [0; 3];
        let slice = unsafe { DmaSliceDyn::<u32>::at(backing.as_mut_ptr() as u64, 3) };
        let _ = slice.read(3);
    }
}
