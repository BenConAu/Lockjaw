//! Kernel adapter for the bootstrap-allocated PageSet header pool.
//!
//! Pure decision logic lives in
//! `lockjaw_types::pageset_header_pool::PageSetHeaderPoolState`.
//! This module owns the singleton, the `POOL_BASE` KVA, the
//! `UnsafeCell`-wrapped state, and the kernel-side claim/release
//! entry points that map slot indices to `KernelVa`s.
//!
//! ## SAFETY: GKL discipline
//!
//! The `UnsafeCell` is sound because every accessor runs under
//! the kernel's Global Kernel Lock. Same regime as
//! `KvmAllocator` (`src/mm/kvm.rs:54-56`) and `PageSetTable`
//! (`src/cap/pageset_table.rs:24-26`). When SMP lifts the GKL
//! (`docs/tracking/tech-debt.md:65` — `## UnsafeCell globals
//! serialized only by GKL`), this `UnsafeCell` needs to be
//! replaced with proper SMP synchronisation alongside the
//! other GKL-protected statics.
//!
//! ## NK1+NK2 sizing contract
//!
//! NK1 sized the KVM working pool to include
//! `KVM_BUDGET_PAGESETS = MAX_PAGESETS × MAX_HEADER_PAGES_PER_PAGESET
//! = 128 × 33 = 4224` pages
//! (`lockjaw-types/src/kvm.rs`). NK2's bootstrap allocation is
//! exactly that budget item realised. After this allocation the
//! KVM working pool has its `KVM_BUDGET_PAGESETS` slice
//! committed; subsequent `kvm::alloc_kernel_pages` callers
//! (NK1-A's `kvm_init`-time table allocation already happened
//! pre-init) consume from the remaining headroom.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lockjaw_types::addr::{KernelVa, PAGE_SIZE};
use lockjaw_types::pageset_header_pool::{
    PageSetHeaderPoolState, MAX_HEADER_PAGES_PER_PAGESET,
};
use lockjaw_types::pageset_table::MAX_PAGESETS;
use crate::mm::kernel_ptr::KernelMut;
use crate::mm::kvm;

/// Bytes per pool slot. Each slot owns `MAX_HEADER_PAGES_PER_PAGESET`
/// contiguous KVA pages.
const SLOT_STRIDE: u64 = (MAX_HEADER_PAGES_PER_PAGESET as u64) * PAGE_SIZE;

/// Total pages the pool reserves at bootstrap.
pub const POOL_TOTAL_PAGES: usize = MAX_PAGESETS * MAX_HEADER_PAGES_PER_PAGESET;

struct PageSetHeaderPool {
    /// Pool base KVA, set once by `init`. `slot_idx`'s KVA is
    /// `POOL_BASE + slot_idx * SLOT_STRIDE`.
    base: AtomicU64,
    /// Free-list state. GKL-serialized; see module SAFETY note.
    state: UnsafeCell<PageSetHeaderPoolState>,
    /// True once `init` has completed. claim/release before init
    /// panic — there is no fallback.
    initialized: AtomicBool,
}

// SAFETY: single accessor at a time under GKL; same regime as
// the other kernel-side singletons in src/mm/ and src/cap/.
unsafe impl Sync for PageSetHeaderPool {}

static POOL: PageSetHeaderPool = PageSetHeaderPool {
    base: AtomicU64::new(0),
    state: UnsafeCell::new(PageSetHeaderPoolState::new()),
    initialized: AtomicBool::new(false),
};

/// Pre-allocate the pool's backing KVA range and mark the
/// adapter live. Must be called exactly once at bootstrap,
/// after `mm::kvm::kvm_init` + `mm::kvm::boot_self_test`, and
/// BEFORE any caller of `claim()`.
///
/// # Safety
/// Must be called exactly once. Race-free under GKL.
pub unsafe fn init() {
    if POOL.initialized.load(Ordering::Acquire) {
        panic!("pageset_header_pool::init called twice");
    }

    let range = kvm::alloc_kernel_pages(POOL_TOTAL_PAGES).unwrap_or_else(|e| {
        panic!(
            "pageset_header_pool::init: alloc_kernel_pages({}) failed \
             (bootstrap memory pressure: {:?})",
            POOL_TOTAL_PAGES, e,
        )
    });

    POOL.base.store(range.kva.as_u64(), Ordering::Release);
    POOL.initialized.store(true, Ordering::Release);

    crate::kprintln!(
        "  PageSet header pool initialized: ",
        MAX_PAGESETS,
        " slots x ",
        MAX_HEADER_PAGES_PER_PAGESET,
        " pages, base ",
        crate::print::Hex(range.kva.as_u64()),
    );
}

/// Compute slot `i`'s base KVA. Constant arithmetic; no state
/// lookup beyond `POOL_BASE`.
#[inline]
fn slot_kva(slot_idx: usize) -> KernelVa {
    let base = POOL.base.load(Ordering::Acquire);
    KernelVa::new(base + (slot_idx as u64) * SLOT_STRIDE)
}

/// Compute `slot_idx` from a KVA the adapter previously returned.
/// Panics if `kva` is outside the pool's range or not aligned to
/// `SLOT_STRIDE`.
#[inline]
fn kva_to_slot(kva: KernelVa) -> usize {
    let base = POOL.base.load(Ordering::Acquire);
    let offset = kva.as_u64().checked_sub(base).unwrap_or_else(|| {
        panic!(
            "pageset_header_pool::release: kva {:#x} below pool base {:#x}",
            kva.as_u64(), base,
        )
    });
    assert!(
        offset % SLOT_STRIDE == 0,
        "pageset_header_pool::release: kva {:#x} not slot-aligned (stride {})",
        kva.as_u64(), SLOT_STRIDE,
    );
    let slot = (offset / SLOT_STRIDE) as usize;
    assert!(
        slot < MAX_PAGESETS,
        "pageset_header_pool::release: slot {} >= MAX_PAGESETS {}",
        slot, MAX_PAGESETS,
    );
    slot
}

/// Claim the lowest free slot for `header_pages` pages. Returns
/// the slot's base KVA. The first `header_pages * PAGE_SIZE`
/// bytes are zeroed before return so callers don't need their
/// own `write_bytes` loop.
///
/// Returns `None` if the pool is exhausted or `header_pages` is
/// out of range. Callers categorise the exhaustion into their
/// typed error (e.g. `AllocError::HeaderPoolExhausted` in
/// `src/cap/pageset_table.rs`).
pub fn claim(header_pages: usize) -> Option<KernelVa> {
    if !POOL.initialized.load(Ordering::Acquire) {
        panic!("pageset_header_pool::claim before init");
    }

    // SAFETY: GKL serializes access; see module SAFETY note.
    let state = unsafe { &mut *POOL.state.get() };
    let slot = state.claim(header_pages)?;
    let kva = slot_kva(slot);

    // Zero the claimed prefix so callers drop their own zeroing.
    // SAFETY: kva points into the bootstrap-mapped pool range;
    // the slot is exclusively owned by this caller post-claim.
    unsafe {
        let mut page = KernelMut::<u8>::from_kva(kva);
        core::ptr::write_bytes(
            page.as_mut_ptr(),
            0,
            header_pages * (PAGE_SIZE as usize),
        );
    }

    Some(kva)
}

/// RAII guard: on drop, calls `release(kva)` unless `take()` was
/// called first. Mirrors `OwnedKvmRangeGuard` (`src/mm/kvm.rs`)
/// — same shape, different release fn. Callers wrap a claim in
/// the guard, do downstream fallible work, then `take()` on
/// success to transfer ownership without releasing.
pub struct HeaderSlotGuard {
    kva: Option<KernelVa>,
}

impl HeaderSlotGuard {
    pub fn new(kva: KernelVa) -> Self {
        Self { kva: Some(kva) }
    }

    /// Claim the slot, preventing release on drop. Panics if
    /// already taken.
    pub fn take(&mut self) -> KernelVa {
        self.kva.take().expect("HeaderSlotGuard already taken")
    }

    /// The underlying KVA without releasing ownership. Panics if
    /// already taken.
    pub fn kva(&self) -> KernelVa {
        self.kva.expect("HeaderSlotGuard already taken")
    }
}

impl Drop for HeaderSlotGuard {
    fn drop(&mut self) {
        if let Some(kva) = self.kva.take() {
            release(kva);
        }
    }
}

/// Release a previously-claimed slot. Panics on double-release
/// or on a KVA outside the pool's range (Tier 1 #1 — these are
/// kernel bugs, not recoverable runtime conditions).
pub fn release(kva: KernelVa) {
    if !POOL.initialized.load(Ordering::Acquire) {
        panic!("pageset_header_pool::release before init");
    }
    let slot = kva_to_slot(kva);
    // SAFETY: GKL serializes access.
    let state = unsafe { &mut *POOL.state.get() };
    let _pages = state.release(slot);
}

/// Diagnostics: number of slots currently claimed.
pub fn used_count() -> usize {
    if !POOL.initialized.load(Ordering::Acquire) {
        return 0;
    }
    // SAFETY: GKL serializes access.
    let state = unsafe { &*POOL.state.get() };
    state.used_count()
}
