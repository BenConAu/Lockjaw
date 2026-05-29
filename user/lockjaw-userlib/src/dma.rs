//! Ownership-typed DMA mapping handles.
//!
//! Two concrete types replace the earlier `DmaPage`:
//!
//! - `OwnedDmaMapping` — produced by `alloc()` / `alloc_contiguous()`.
//!   Owns the underlying pageset. `Drop` unmaps, returns VA, AND
//!   closes the pageset handle.
//! - `BorrowedDmaMapping` — produced by `map_existing(pageset, pages)`.
//!   Adopts the mapping but the caller retains the pageset handle.
//!   `Drop` unmaps and returns VA only; the pageset is NOT closed.
//!
//! The type IS the ownership story: a reader can tell at field
//! declaration whether the wrapper will release the underlying
//! pageset. Both types share the same view-construction surface
//! (`cell<T>(off)`, `slice<T>(off, n)`, `pa()`, `va()`,
//! `size_bytes()`, `pa_offset(o)`, `zero()`) via the
//! `DmaMappingView` trait.
//!
//! The trait deliberately covers only view methods. Per reviewer
//! discipline: NO ownership transfer methods, NO sub-mapping
//! creation, NO blanket impls beyond these two structs. If a future
//! need pushes against this, add the method to both structs directly
//! rather than expanding the trait surface.
//!
//! `Drop` errors from `sys_unmap_pages` / `sys_close_handle` cannot
//! propagate — drop functions don't return — so they are swallowed.
//! Callers that want the error use the explicit `unmap(self) ->
//! Result<(), SyscallError>` consumer instead; calling it disarms
//! the Drop.
//!
//! **Caller contract on `cell` / `slice`:** byte offsets across
//! calls must not overlap when mixing distinct `T`s. The aliasing
//! rule is documented, not type-enforced; bounds-checking ensures
//! each view fits in the region, but mutual disjointness is the
//! caller's responsibility. (Two overlapping cells over the SAME
//! `T` are harmless — they alias to the same volatile cell.)

use crate::handle::{PageSetGuard, PageSetHandle};
use crate::syscall::{
    sys_alloc_dma_pages, sys_alloc_pages, sys_alloc_pages_contiguous,
    sys_close_handle, sys_map_pages, sys_query_pageset_phys,
};
use crate::virtual_memory::{unmap_pages_tracked, VMEM};
use core::marker::PhantomData;
use lockjaw_mmio::dma::{DmaCell, DmaSliceDyn, DmaValue};
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_types::syscall::SyscallError;
use lockjaw_types::vmem::MapMemoryAttribute;

// ---------------------------------------------------------------------------
// DMA allocation origin + cache-sync capability (type-level).
//
// The kernel's `sys_dma_sync_*` cache-maintenance syscalls accept ONLY
// DmaPool-origin pagesets (`src/syscall/handler.rs`: `origin != DmaPool
// -> INVALID_PARAMETER`). Buddy-origin pages are coherent only on a
// coherent bus (QEMU virtio) and the kernel rejects syncing them.
//
// The origin is encoded in the mapping's TYPE. Today (P9.10) this types
// the ALLOCATION: emmc2 allocates `DmaPoolOrigin` mappings, so its
// pagesets are accepted by the sync gate instead of failing with a
// runtime `INVALID_PARAMETER` only on a hardware flash. The sync surface
// itself (`sys_dma_sync_*`) still takes raw handles; P9.11 adds a
// `SyncCapable`-gated coherence envelope that makes handing a
// `BuddyOrigin` mapping to a sync a compile error. Until then the origin
// marker constrains allocation, not the sync call.
// ---------------------------------------------------------------------------

mod origin_sealed {
    pub trait Sealed {}
}

/// Marker: Buddy-allocator origin (general RAM). Coherent only on a
/// coherent bus (QEMU); NOT accepted by `sys_dma_sync_*`, so it is not
/// `SyncCapable`. Use for coherent-bus DMA.
pub struct BuddyOrigin;

/// Marker: DmaPool origin (the cache-maintenance-managed pool). The only
/// origin `sys_dma_sync_*` accepts; required for real-hardware
/// (non-coherent) DMA. Implements `SyncCapable`.
pub struct DmaPoolOrigin;

impl origin_sealed::Sealed for BuddyOrigin {}
impl origin_sealed::Sealed for DmaPoolOrigin {}

/// A DMA allocation origin. Sealed — drivers cannot define new origins.
pub trait DmaOrigin: origin_sealed::Sealed {}
impl DmaOrigin for BuddyOrigin {}
impl DmaOrigin for DmaPoolOrigin {}

/// Origins whose pagesets the kernel's `sys_dma_sync_*` syscalls accept
/// (DmaPool only). P9.11's cache-coherence envelope will require this
/// bound, making a `BuddyOrigin` mapping handed to a sync a compile
/// error; until that lands the bound is declared but not yet enforced on
/// the sync surface. `BuddyOrigin` deliberately does NOT implement it.
pub trait SyncCapable: DmaOrigin {}
impl SyncCapable for DmaPoolOrigin {}

// ---------------------------------------------------------------------------
// Lifetime-bound DMA views.
//
// The raw `DmaCell` / `DmaSliceDyn` from lockjaw-mmio are owned values
// with no lifetime parameter — that's appropriate for the substrate.
// At the userlib layer we wrap them in `CellRef` / `SliceRef` that
// borrow `&self` of whatever mapping handed them out, so:
// - the typed view can never outlive its backing mapping (no
//   use-after-unmap in safe code, since `unmap(self)` would conflict
//   with an outstanding `&self`-bound view, and Drop runs only after
//   the last borrow ends);
// - the substrate-level cell types stay unchanged;
// - PhantomData carries the lifetime only (no concrete owner type),
//   so both `OwnedDmaMapping` and `BorrowedDmaMapping` produce the
//   same view wrappers.
// ---------------------------------------------------------------------------

/// A typed cell view borrowed from a DMA mapping.
pub struct CellRef<'a, T: DmaValue> {
    inner: DmaCell<T>,
    _life: PhantomData<&'a ()>,
}

impl<'a, T: DmaValue> CellRef<'a, T> {
    /// Volatile write through the underlying DMA cell.
    #[inline(always)]
    pub fn write(&self, value: T) { self.inner.write(value); }
    /// Volatile read through the underlying DMA cell.
    #[inline(always)]
    pub fn read(&self) -> T { self.inner.read() }
}

/// A typed runtime-length slice view borrowed from a DMA mapping.
pub struct SliceRef<'a, T: DmaValue> {
    inner: DmaSliceDyn<T>,
    _life: PhantomData<&'a ()>,
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

// ---------------------------------------------------------------------------
// View trait — minimal surface; default implementations only.
// ---------------------------------------------------------------------------

/// Shared view-construction surface for DMA mappings.
///
/// Required methods are the three primitive accessors the view default
/// impls need (`va`, `pa`, `size_bytes`). Everything else is a default
/// implementation so the two concrete mapping types share one body.
///
/// **Discipline:** this trait covers VIEW methods only. No allocation,
/// no mapping-lifecycle methods (unmap, close_pageset), no sub-mapping
/// creation, no blanket impls. If a future need pushes against this —
/// add the method to each struct directly rather than expanding the
/// trait surface.
pub trait DmaMappingView {
    /// First-page virtual address.
    fn va(&self) -> u64;
    /// First-page physical address.
    fn pa(&self) -> u64;
    /// Total bytes in the region.
    fn size_bytes(&self) -> usize;

    /// PA at a byte offset within the region (for device-register
    /// programming on multi-page allocations).
    fn pa_offset(&self, byte_offset: u64) -> u64 {
        let bytes = self.size_bytes() as u64;
        assert!(
            byte_offset < bytes,
            "DMA mapping pa_offset OOB: offset 0x{:x} >= region 0x{:x}",
            byte_offset, bytes
        );
        self.pa() + byte_offset
    }

    /// View a typed cell at `byte_offset`. Bounds-checked AND
    /// alignment-checked: `byte_offset` must be a multiple of
    /// `align_of::<T>()` (else volatile load/store would be UB) and
    /// `byte_offset + size_of::<T>()` must fit in the region.
    /// Callers must keep cell offsets mutually disjoint within the
    /// region (docs invariant, not type-enforced).
    ///
    /// The returned `CellRef<'_, T>` borrows `&self`, so it cannot
    /// outlive this mapping.
    fn cell<T: DmaValue>(&self, byte_offset: u64) -> CellRef<'_, T> {
        let region_bytes = self.size_bytes() as u64;
        let size = core::mem::size_of::<T>() as u64;
        let align = core::mem::align_of::<T>() as u64;
        assert!(
            byte_offset % align == 0,
            "DMA cell misaligned: offset 0x{:x} not a multiple of align {} for {}",
            byte_offset, align, core::any::type_name::<T>()
        );
        let end = byte_offset
            .checked_add(size)
            .expect("DMA cell offset+size overflow");
        assert!(
            end <= region_bytes,
            "DMA cell OOB: end 0x{:x} > region 0x{:x}",
            end, region_bytes
        );
        // SAFETY: alloc/map gave us exclusive ownership of the VA
        // range; offset is bounds-checked AND alignment-checked.
        // T: DmaValue makes volatile load/store sound. Lifetime
        // binding via CellRef prevents use-after-unmap (the
        // mapping's Drop runs only after every &self-borrowed view
        // has ended).
        CellRef {
            inner: unsafe { DmaCell::<T>::at(self.va() + byte_offset) },
            _life: PhantomData,
        }
    }

    /// View a runtime-length typed slice at `byte_offset`. Same
    /// alignment + bounds discipline as `cell()`. Returned
    /// `SliceRef<'_, T>` is lifetime-bound to `&self`.
    fn slice<T: DmaValue>(&self, byte_offset: u64, len: usize) -> SliceRef<'_, T> {
        let region_bytes = self.size_bytes() as u64;
        let size = core::mem::size_of::<T>() as u64;
        let align = core::mem::align_of::<T>() as u64;
        assert!(
            byte_offset % align == 0,
            "DMA slice misaligned: offset 0x{:x} not a multiple of align {} for {}",
            byte_offset, align, core::any::type_name::<T>()
        );
        let total = (len as u64)
            .checked_mul(size)
            .expect("DMA slice len*size_of::<T>() overflow");
        let end = byte_offset
            .checked_add(total)
            .expect("DMA slice offset+total overflow");
        assert!(
            end <= region_bytes,
            "DMA slice OOB: end 0x{:x} > region 0x{:x}",
            end, region_bytes
        );
        // SAFETY: as `cell()` above.
        SliceRef {
            inner: unsafe { DmaSliceDyn::<T>::at(self.va() + byte_offset, len) },
            _life: PhantomData,
        }
    }

    /// Zero the entire region via 64-bit volatile DMA writes.
    fn zero(&self) {
        let words = (self.size_bytes() / 8) as usize;
        let view = self.slice::<u64>(0, words);
        for i in 0..words {
            view.write(i, 0);
        }
    }

    /// Zero `len` bytes starting at `byte_offset` via volatile writes.
    /// Use this — not `zero()` — when only a sub-range of the mapping
    /// will be cache-synced. Zeroing (dirtying) cache lines beyond the
    /// range a driver later cleans/invalidates leaves them dirty and
    /// unmanaged; on a DmaPool page that is subsequently closed/freed,
    /// those lines can write back into a reallocated physical page.
    fn zero_range(&self, byte_offset: u64, len: usize) {
        let view = self.slice::<u8>(byte_offset, len);
        for i in 0..len {
            view.write(i, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// OwnedDmaMapping — alloc-backed; Drop closes the pageset.
// ---------------------------------------------------------------------------

/// A DMA-coherent mapping that owns its pageset.
///
/// `Drop` unmaps the VA range, returns it to the allocator, AND
/// closes the underlying pageset handle. Use for driver-owned
/// allocations (request headers, virtqueue backing).
pub struct OwnedDmaMapping<O: DmaOrigin> {
    pageset: PageSetHandle,
    va: u64,
    pa: u64,
    pages: u64,
    // Set to false by .unmap(self) so Drop is a no-op (the explicit
    // consumer already released the resources). Internal flag, not
    // exposed on the API.
    armed: bool,
    // Zero-size origin marker (Buddy vs DmaPool). Determines at the type
    // level whether this mapping is `SyncCapable`.
    _origin: PhantomData<O>,
}

impl OwnedDmaMapping<BuddyOrigin> {
    /// Allocate one Normal-mapped Buddy page (coherent on a coherent
    /// bus, e.g. QEMU), map it, and return the handle. NOT cache-sync
    /// capable; for DMA that issues `sys_dma_sync_*` (real hardware)
    /// use `OwnedDmaMapping::<DmaPoolOrigin>::alloc_dma_pool`.
    pub fn alloc() -> Result<Self, SyscallError> {
        let m = Self::alloc_and_map(sys_alloc_pages(1)?, 1)?;
        m.zero();
        Ok(m)
    }

    /// Allocate `pages` physically-contiguous Buddy pages, map them,
    /// zero them, and return the handle. Used for virtqueue allocations.
    pub fn alloc_contiguous(pages: u64) -> Result<Self, SyscallError> {
        let m = Self::alloc_and_map(sys_alloc_pages_contiguous(pages)?, pages)?;
        m.zero();
        Ok(m)
    }
}

impl OwnedDmaMapping<DmaPoolOrigin> {
    /// Allocate one Normal-mapped DmaPool page and return the handle.
    /// DmaPool origin is `SyncCapable` — required for DMA on a
    /// non-coherent bus (real hardware) whose driver issues
    /// `sys_dma_sync_*` around transfers.
    ///
    /// Deliberately NOT zeroed (unlike the Buddy `alloc`): whole-page
    /// zeroing would dirty cache lines for the whole page, but a driver
    /// only cleans/invalidates the range it transfers; the unused tail
    /// would stay dirty until the pageset is closed/freed, then write
    /// back into a reallocated page. Zero only the range you will sync,
    /// via `DmaMappingView::zero_range`.
    pub fn alloc_dma_pool() -> Result<Self, SyscallError> {
        Self::alloc_and_map(sys_alloc_dma_pages(1)?, 1)
    }
}

impl<O: DmaOrigin> OwnedDmaMapping<O> {
    /// Allocate + Normal-map the pageset. Does NOT zero the region —
    /// zeroing is the constructor's choice. Whole-page zeroing a DmaPool
    /// page via cacheable writes dirties every line; for the unused tail
    /// beyond a driver's per-transfer synced range those lines are never
    /// cleaned before the pageset is closed/freed and reused (a writeback
    /// hazard). Buddy constructors zero (coherent bus, no hazard);
    /// DmaPool constructors leave it unzeroed.
    fn alloc_and_map(pageset: PageSetHandle, pages: u64) -> Result<Self, SyscallError> {
        // PageSetGuard closes the pageset on early return; .take() on
        // the success path passes ownership into the mapping (whose
        // Drop is the new owner).
        let guard = PageSetGuard::new(pageset);
        let pa = sys_query_pageset_phys(guard.handle(), 0)?;
        let va = VMEM.alloc(pages as usize).ok_or(SyscallError::OUT_OF_MEMORY)?;
        let err = sys_map_pages(guard.handle(), va, MapMemoryAttribute::Normal);
        if !err.is_ok() {
            // No mapping was ever established — safe to return the
            // VA via the alloc-but-never-mapped path.
            VMEM.free_unused_allocation(va, pages as usize);
            return Err(err);
        }
        let pageset = guard.take();
        Ok(Self { pageset, va, pa, pages, armed: true, _origin: PhantomData })
    }

    /// Underlying pageset handle (for export, IRQ binding, etc.).
    pub fn pageset(&self) -> PageSetHandle { self.pageset }

    /// Consume the mapping, unmapping and freeing VA, AND closing the
    /// pageset. Returns the unmap error (if any).
    ///
    /// Type-level VA-leak-on-unmap-failure invariant: VMEM returns the
    /// VA range to the allocator only via `free_unmapped(VaUnmapped)`,
    /// and the `VaUnmapped` proof token is only constructible by a
    /// successful `unmap_pages_tracked`. On unmap failure we cannot
    /// construct the proof, so the VA is leaked by construction. The
    /// pageset is still closed because (a) closing is independent of
    /// the mapping state and (b) leaking the pageset compounds the
    /// failure without helping.
    ///
    /// After this returns, Drop is a no-op (`armed = false`).
    pub fn unmap(mut self) -> Result<(), SyscallError> {
        self.armed = false;
        match unmap_pages_tracked(self.pageset, self.va, self.pages as usize) {
            Ok(proof) => {
                VMEM.free_unmapped(proof);
                sys_close_handle(self.pageset);
                Ok(())
            }
            Err(e) => {
                // VA leaked by construction — no proof, no free.
                sys_close_handle(self.pageset);
                Err(e)
            }
        }
    }
}

impl<O: DmaOrigin> DmaMappingView for OwnedDmaMapping<O> {
    fn va(&self) -> u64 { self.va }
    fn pa(&self) -> u64 { self.pa }
    fn size_bytes(&self) -> usize {
        region_bytes(self.pages) as usize
    }
}

impl<O: DmaOrigin> Drop for OwnedDmaMapping<O> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Same construction-safe pattern as `unmap`: no proof →
        // no free. The unmap-failure branch leaks the VA, which is
        // the safer failure mode (vs. aliasing on reuse).
        if let Ok(proof) = unmap_pages_tracked(self.pageset, self.va, self.pages as usize) {
            VMEM.free_unmapped(proof);
        }
        sys_close_handle(self.pageset);
    }
}

// ---------------------------------------------------------------------------
// BorrowedDmaMapping — adopts an existing pageset; Drop does NOT close.
// ---------------------------------------------------------------------------

/// A DMA-coherent mapping that adopts an existing pageset.
///
/// The caller retains ownership of the pageset handle's lifetime.
/// `Drop` unmaps the VA range and returns it to the allocator; the
/// pageset is NOT closed. Use for adopting a pageset produced by
/// another subsystem (e.g. `BlockEngine::alloc_buffer` hands a
/// PageSetHandle to the driver that the driver locally maps for
/// inspection).
pub struct BorrowedDmaMapping {
    pageset: PageSetHandle,
    va: u64,
    pa: u64,
    pages: u64,
    armed: bool,
}

impl BorrowedDmaMapping {
    /// Map an existing pageset Normal and adopt it for typed access.
    /// Does NOT zero the page — existing contents are preserved.
    /// Caller retains the pageset handle and is responsible for
    /// closing it independently.
    pub fn map_existing(pageset: PageSetHandle, pages: u64)
        -> Result<Self, SyscallError>
    {
        let pa = sys_query_pageset_phys(pageset, 0)?;
        let va = VMEM.alloc(pages as usize).ok_or(SyscallError::OUT_OF_MEMORY)?;
        let err = sys_map_pages(pageset, va, MapMemoryAttribute::Normal);
        if !err.is_ok() {
            // No mapping was ever established — safe to return the
            // VA via the alloc-but-never-mapped path. Pageset not
            // closed (caller owns it).
            VMEM.free_unused_allocation(va, pages as usize);
            return Err(err);
        }
        Ok(Self { pageset, va, pa, pages, armed: true })
    }

    /// Underlying pageset handle. Caller owns its lifetime.
    pub fn pageset(&self) -> PageSetHandle { self.pageset }

    /// Consume the mapping, unmapping and freeing VA. Returns the
    /// unmap error if any. Does NOT close the pageset (caller owns).
    ///
    /// Type-level VA-leak-on-unmap-failure invariant: same as
    /// `OwnedDmaMapping::unmap` — `VMEM.free_unmapped` requires a
    /// `VaUnmapped` proof that only `unmap_pages_tracked` produces
    /// on success. The unmap-failure branch leaks the VA by
    /// construction.
    ///
    /// After this returns, Drop is a no-op.
    pub fn unmap(mut self) -> Result<(), SyscallError> {
        self.armed = false;
        match unmap_pages_tracked(self.pageset, self.va, self.pages as usize) {
            Ok(proof) => {
                VMEM.free_unmapped(proof);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl DmaMappingView for BorrowedDmaMapping {
    fn va(&self) -> u64 { self.va }
    fn pa(&self) -> u64 { self.pa }
    fn size_bytes(&self) -> usize {
        region_bytes(self.pages) as usize
    }
}

impl Drop for BorrowedDmaMapping {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Construction-safe: no proof → no free. Pageset is NOT
        // closed (caller still owns it).
        if let Ok(proof) = unmap_pages_tracked(self.pageset, self.va, self.pages as usize) {
            VMEM.free_unmapped(proof);
        }
    }
}

// ---------------------------------------------------------------------------
// DMA backing allocation without local mapping.
//
// Used when a driver allocates a DMA-coherent region to hand to a
// CLIENT (not to access locally). The client maps the pageset; the
// driver only needs the PA to program the device. The caller takes
// ownership of the returned pageset handle — typically tracking it
// in a slot table — and is responsible for closing it via
// `close_dma_backing`.
//
// Concrete users: virtio-blk-driver's `alloc_buffer` /
// `free_buffer` (block-engine helpers that allocate DMA buffers
// requested by the FAT32 filesystem server). Without these helpers
// the driver crate would have to call `sys_alloc_pages_contiguous`,
// `sys_query_pageset_phys`, and `sys_close_handle` directly —
// violating the "drivers don't touch the kernel syscall surface"
// gate.
// ---------------------------------------------------------------------------

/// Result of `alloc_dma_backing*`: the pageset handle (caller owns)
/// and the first-page physical address (for device programming). The
/// `O` marker records the origin (`BuddyOrigin` vs `DmaPoolOrigin`);
/// only `DmaPoolOrigin` backings are `SyncCapable`.
pub struct DmaBacking<O: DmaOrigin> {
    pub pageset: PageSetHandle,
    pub pa: u64,
    pub pages: u64,
    _origin: PhantomData<O>,
}

/// Allocate a contiguous Buddy-origin DMA backing region without
/// mapping it locally. Coherent-bus only (QEMU); NOT `SyncCapable`.
/// Returns the pageset handle (transfer of ownership to caller) and
/// the first-page PA.
pub fn alloc_dma_backing(pages: u64) -> Result<DmaBacking<BuddyOrigin>, SyscallError> {
    finish_dma_backing(sys_alloc_pages_contiguous(pages)?, pages)
}

/// Allocate a contiguous DmaPool-origin DMA backing region without
/// mapping it locally. `SyncCapable` — for non-coherent (real
/// hardware) DMA whose driver issues `sys_dma_sync_*` around transfers.
pub fn alloc_dma_backing_dma_pool(pages: u64) -> Result<DmaBacking<DmaPoolOrigin>, SyscallError> {
    finish_dma_backing(sys_alloc_dma_pages(pages)?, pages)
}

fn finish_dma_backing<O: DmaOrigin>(
    pageset: PageSetHandle,
    pages: u64,
) -> Result<DmaBacking<O>, SyscallError> {
    let guard = PageSetGuard::new(pageset);
    let pa = sys_query_pageset_phys(pageset, 0)?;
    let pageset = guard.take();
    Ok(DmaBacking { pageset, pa, pages, _origin: PhantomData })
}

/// Close a `DmaBacking`'s pageset handle. Use when the caller is
/// done with the buffer (no client still holds it). Errors swallowed.
pub fn close_dma_backing(pageset: PageSetHandle) {
    sys_close_handle(pageset);
}

// ---------------------------------------------------------------------------
// Internal helper.
// ---------------------------------------------------------------------------

#[inline]
fn region_bytes(pages: u64) -> u64 {
    // `pages` originated in a successful alloc/map; kernel bounds
    // page count well below u64::MAX / PAGE_SIZE. checked_mul as
    // belt-and-braces — overflow would be a catastrophic substrate
    // bug.
    pages
        .checked_mul(PAGE_SIZE)
        .expect("DMA mapping region size overflow (pages * PAGE_SIZE)")
}
