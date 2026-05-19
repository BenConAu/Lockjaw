//! `DmaPage` — typed handle over a DMA-coherent page region.
//!
//! Bundles `(pageset, va, pa)` and a region size, then hands out
//! `DmaCell<T>` / `DmaSliceDyn<T>` views at byte offsets within the
//! region. The DmaPage owns the mapping (the underlying `unsafe`
//! `DmaCell::at` / `DmaSliceDyn::at` calls are concentrated here),
//! so driver crates with `#![forbid(unsafe_code)]` can build typed
//! DMA structures without writing `unsafe` themselves.
//!
//! Two construction paths:
//! - `alloc[_contiguous]` — driver-owned mapping (request headers,
//!   virtqueue allocations).
//! - `map_existing` — adopt a pageset already produced elsewhere
//!   (e.g. `BlockEngine::alloc_buffer` returns a PageSetHandle that
//!   the driver can locally map for self-test inspection).
//!
//! **Caller contract on `cell`/`slice`:** byte offsets across calls
//! must not overlap. The aliasing rule is documented, not type-
//! enforced; bounds-checking ensures each view fits in the page
//! region, but mutual disjointness is the caller's responsibility.

use crate::handle::{PageSetGuard, PageSetHandle};
use crate::syscall::{
    sys_alloc_pages, sys_alloc_pages_contiguous, sys_map_pages,
    sys_query_pageset_phys, sys_unmap_pages,
};
use crate::virtual_memory::VMEM;
use core::marker::PhantomData;
use lockjaw_mmio::dma::{DmaCell, DmaSliceDyn, DmaValue};
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_types::syscall::SyscallError;
use lockjaw_types::vmem::MapMemoryAttribute;

// ---------------------------------------------------------------------------
// Lifetime-bound DMA views
//
// The raw `DmaCell` / `DmaSliceDyn` from lockjaw-mmio are owned values
// with no lifetime parameter — that's appropriate for the substrate.
// At the userlib layer we wrap them in `CellRef` / `SliceRef` that
// borrow `&DmaPage`, so:
// - the typed view can never outlive its backing page (no
//   use-after-unmap in safe code, since `DmaPage::unmap(self)` would
//   conflict with an outstanding `&self`-bound view);
// - the substrate-level cell types stay unchanged.
// ---------------------------------------------------------------------------

/// A typed cell view borrowed from a `DmaPage`.
pub struct CellRef<'a, T: DmaValue> {
    inner: DmaCell<T>,
    _life: PhantomData<&'a DmaPage>,
}

impl<'a, T: DmaValue> CellRef<'a, T> {
    /// Volatile write through the underlying DMA cell.
    #[inline(always)]
    pub fn write(&self, value: T) { self.inner.write(value); }
    /// Volatile read through the underlying DMA cell.
    #[inline(always)]
    pub fn read(&self) -> T { self.inner.read() }
}

/// A typed runtime-length slice view borrowed from a `DmaPage`.
pub struct SliceRef<'a, T: DmaValue> {
    inner: DmaSliceDyn<T>,
    _life: PhantomData<&'a DmaPage>,
}

impl<'a, T: DmaValue> SliceRef<'a, T> {
    /// Volatile write at `idx` (panics on OOB).
    #[inline(always)]
    pub fn write(&self, idx: usize, value: T) { self.inner.write(idx, value); }
    /// Volatile read at `idx` (panics on OOB).
    #[inline(always)]
    pub fn read(&self, idx: usize) -> T { self.inner.read(idx) }
    /// Length of the slice.
    #[inline(always)]
    pub fn len(&self) -> usize { self.inner.len() }
    /// Whether the slice has zero length.
    #[inline(always)]
    pub fn is_empty(&self) -> bool { self.inner.is_empty() }
}

/// A DMA-coherent page region with typed cell/slice access.
pub struct DmaPage {
    pageset: PageSetHandle,
    va: u64,
    pa: u64,
    pages: u64,
}

impl DmaPage {
    /// Allocate one Normal-mapped page (DMA-coherent on the platforms
    /// Lockjaw targets), map it, and return the handle.
    pub fn alloc() -> Result<Self, SyscallError> {
        let pageset = sys_alloc_pages(1)?;
        Self::finish_alloc(pageset, 1)
    }

    /// Allocate `pages` physically-contiguous pages, map them, and
    /// return the handle. Used for virtqueue allocations.
    pub fn alloc_contiguous(pages: u64) -> Result<Self, SyscallError> {
        let pageset = sys_alloc_pages_contiguous(pages)?;
        Self::finish_alloc(pageset, pages)
    }

    fn finish_alloc(pageset: PageSetHandle, pages: u64) -> Result<Self, SyscallError> {
        // Guard closes the pageset if any subsequent step fails;
        // `take()` on the success path passes ownership to DmaPage.
        let guard = PageSetGuard::new(pageset);
        let pa = sys_query_pageset_phys(guard.handle(), 0)?;
        let va = VMEM.alloc(pages as usize).ok_or(SyscallError::OUT_OF_MEMORY)?;
        let err = sys_map_pages(guard.handle(), va, MapMemoryAttribute::Normal);
        if !err.is_ok() {
            // Reserved-but-unmapped VA range must go back to the pool.
            VMEM.free(va, pages as usize);
            return Err(err);
        }
        let pageset = guard.take();
        let page = Self { pageset, va, pa, pages };
        page.zero();
        Ok(page)
    }

    /// Map an existing pageset (e.g. produced by an external
    /// allocator) Normal and adopt it for typed access. Does NOT
    /// zero the page — the existing contents are preserved. Caller
    /// retains ownership of the pageset handle on failure; on
    /// success ownership moves into the returned DmaPage (released
    /// by `unmap()` or when the DmaPage is forgotten at process
    /// exit).
    pub fn map_existing(pageset: PageSetHandle, pages: u64) -> Result<Self, SyscallError> {
        // Reserve VA + map. On failure we MUST NOT close the caller's
        // pageset (we didn't allocate it), but we DO release any VA we
        // reserved.
        let pa = sys_query_pageset_phys(pageset, 0)?;
        let va = VMEM.alloc(pages as usize).ok_or(SyscallError::OUT_OF_MEMORY)?;
        let err = sys_map_pages(pageset, va, MapMemoryAttribute::Normal);
        if !err.is_ok() {
            VMEM.free(va, pages as usize);
            return Err(err);
        }
        Ok(Self { pageset, va, pa, pages })
    }

    /// Unmap and release the VA. Does NOT close the underlying
    /// pageset — caller owns the handle's lifetime. Use this for
    /// pages adopted via `map_existing`; for `alloc`-allocated pages
    /// process-lifetime is normal.
    pub fn unmap(self) -> Result<(), SyscallError> {
        let err = sys_unmap_pages(self.pageset, self.va);
        if !err.is_ok() { return Err(err); }
        VMEM.free(self.va, self.pages as usize);
        Ok(())
    }

    /// Underlying pageset handle (for export, IRQ binding, etc.).
    pub fn pageset(&self) -> PageSetHandle { self.pageset }
    /// First-page virtual address.
    pub fn va(&self) -> u64 { self.va }
    /// First-page physical address.
    pub fn pa(&self) -> u64 { self.pa }
    /// Total bytes in the region.
    pub fn size_bytes(&self) -> usize { self.region_bytes() as usize }

    /// PA at a byte offset within the region (for device-register
    /// programming on multi-page allocations).
    pub fn pa_offset(&self, byte_offset: u64) -> u64 {
        assert!(
            byte_offset < self.region_bytes(),
            "DmaPage pa_offset OOB: offset 0x{:x} >= region 0x{:x}",
            byte_offset, self.region_bytes()
        );
        self.pa + byte_offset
    }

    /// View a typed cell at `byte_offset`. Bounds-checked AND
    /// alignment-checked: `byte_offset` must be a multiple of
    /// `align_of::<T>()` (else volatile load/store would be UB) and
    /// `byte_offset + size_of::<T>()` must fit in the region.
    /// Callers must keep cell offsets mutually disjoint within the
    /// page (this is a docs invariant, not type-enforced — two
    /// overlapping cells over the SAME `T` is harmless; mixing types
    /// at overlapping offsets is not).
    ///
    /// The returned `CellRef<'_, T>` borrows `&self`, so it cannot
    /// outlive this `DmaPage` — `DmaPage::unmap(self)` would not
    /// compile while any `CellRef` is live.
    pub fn cell<T: DmaValue>(&self, byte_offset: u64) -> CellRef<'_, T> {
        let region_bytes = self.region_bytes();
        let size = core::mem::size_of::<T>() as u64;
        let align = core::mem::align_of::<T>() as u64;
        assert!(
            byte_offset % align == 0,
            "DmaPage cell misaligned: offset 0x{:x} not a multiple of align {} for {}",
            byte_offset, align, core::any::type_name::<T>()
        );
        let end = byte_offset
            .checked_add(size)
            .expect("DmaPage cell offset+size overflow");
        assert!(
            end <= region_bytes,
            "DmaPage cell OOB: end 0x{:x} > region 0x{:x}",
            end, region_bytes
        );
        // SAFETY: alloc + map gave us exclusive ownership of the VA
        // range; offset is bounds-checked AND alignment-checked.
        // T: DmaValue makes volatile load/store sound. Lifetime
        // binding via CellRef prevents use-after-unmap.
        CellRef {
            inner: unsafe { DmaCell::<T>::at(self.va + byte_offset) },
            _life: PhantomData,
        }
    }

    /// View a runtime-length typed slice at `byte_offset`. Same
    /// alignment + bounds discipline as `cell()`. Uses checked
    /// arithmetic for `byte_offset + len * size_of::<T>()` so a
    /// pathological `len` cannot wrap the bound check. Returned
    /// `SliceRef<'_, T>` is lifetime-bound to `&self`.
    pub fn slice<T: DmaValue>(&self, byte_offset: u64, len: usize) -> SliceRef<'_, T> {
        let region_bytes = self.region_bytes();
        let size = core::mem::size_of::<T>() as u64;
        let align = core::mem::align_of::<T>() as u64;
        assert!(
            byte_offset % align == 0,
            "DmaPage slice misaligned: offset 0x{:x} not a multiple of align {} for {}",
            byte_offset, align, core::any::type_name::<T>()
        );
        let total = (len as u64)
            .checked_mul(size)
            .expect("DmaPage slice len*size_of::<T>() overflow");
        let end = byte_offset
            .checked_add(total)
            .expect("DmaPage slice offset+total overflow");
        assert!(
            end <= region_bytes,
            "DmaPage slice OOB: end 0x{:x} > region 0x{:x}",
            end, region_bytes
        );
        // SAFETY: as `cell()` above.
        SliceRef {
            inner: unsafe { DmaSliceDyn::<T>::at(self.va + byte_offset, len) },
            _life: PhantomData,
        }
    }

    #[inline]
    fn region_bytes(&self) -> u64 {
        // self.pages was created from a successful alloc; the kernel
        // bounds the page count well below u64::MAX / PAGE_SIZE, so
        // the multiplication is fine. Use checked_mul anyway as a
        // belt-and-braces invariant — overflow here would be a
        // catastrophic substrate bug.
        self.pages
            .checked_mul(PAGE_SIZE)
            .expect("DmaPage region size overflow (pages * PAGE_SIZE)")
    }

    /// Zero the entire region. Called automatically by `alloc[_contiguous]`.
    pub fn zero(&self) {
        // 64-bit volatile stores via DmaCell — keeps the substrate
        // contract that all writes go through the audited path. Slow
        // compared to a raw bzero but only runs once per allocation.
        let words = (self.region_bytes() / 8) as usize;
        let view = self.slice::<u64>(0, words);
        for i in 0..words {
            view.write(i, 0);
        }
    }
}
