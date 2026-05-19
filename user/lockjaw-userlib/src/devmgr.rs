//! Typed device-claim helper for user-mode drivers.
//!
//! Drivers ask the device manager for an MMIO region and get back a
//! `MappedRegs<T>` already wrapping the right typed register layout
//! (from `lockjaw_regs`). The driver never sees a raw `PageSetHandle`
//! map step, never writes `unsafe` to construct the typed pointer,
//! and never hand-computes the intra-page offset for shared-page
//! devices like virtio-mmio.
//!
//! `claim_typed` is the only sanctioned driver-side claim path going
//! forward; Phase 8 will add an xtask check that flags any driver
//! still doing the raw `CMD_CLAIM_BY_ADDR` dance.

use crate::handle::{EndpointHandle, PageSetGuard, PageSetHandle, ReplyHandle};
use crate::syscall::{sys_call_ret4, sys_map_pages};
use crate::virtual_memory::VMEM;
use lockjaw_mmio::region::MappedRegs;
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_types::device::{CMD_CLAIM_BY_ADDR, CMD_RELEASE_BY_ADDR, CLAIM_OK};
use lockjaw_types::vmem::MapMemoryAttribute;

/// What the device manager handed us, packaged for driver use.
pub struct ClaimedDevice<T: 'static> {
    /// Typed view of the device's MMIO region.
    pub regs: MappedRegs<T>,
    /// Allocated IRQ interrupt ID. The driver binds this to a
    /// notification via `sys_bind_irq_flags` to receive completions.
    pub irq_intid: u32,
    /// Pageset handle backing the MMIO mapping. Most drivers ignore
    /// this; it's exposed for callers that want to participate in
    /// later revocation flows.
    pub mmio_pageset: PageSetHandle,
}

/// Errors `claim_typed` can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// `CMD_CLAIM_BY_ADDR` IPC itself failed.
    IpcFailed,
    /// Device manager declined the claim (busy, unknown address, etc.).
    Rejected,
    /// Out of virtual address space (`VMEM::alloc` returned None).
    OutOfVirtualMemory,
    /// `sys_map_pages` returned an error.
    MapFailed,
    /// `mmio_addr` is not a multiple of `align_of::<T>()` — reading
    /// through `MappedRegs::regs()` would be UB. Caught pre-IPC so
    /// no claim is taken.
    LayoutMisaligned,
    /// `size_of::<T>() + (mmio_addr & 0xfff)` exceeds one page —
    /// `MappedRegs<T>` would expose `&T` extending past the mapping.
    /// Pre-IPC failure. (`claim_typed` maps exactly one page today;
    /// multi-page support is future work.)
    LayoutTooLarge,
}

/// Claim an MMIO device by physical address and return a typed handle.
///
/// `T` is one of the generated layout types in `lockjaw_regs::*` (e.g.
/// `lockjaw_regs::virtio_mmio::VirtioMmio`). The caller is asserting
/// that the device at `mmio_addr` actually exposes this layout — pick
/// `T` to match the device-type the address probe found.
///
/// Sequence:
/// 1. `CMD_CLAIM_BY_ADDR` IPC to the device manager
/// 2. Allocate one VA page (devices are 4 KiB-bounded)
/// 3. Map the device's pageset Device-typed
/// 4. Compose the intra-page offset (virtio-mmio packs 8 devices per
///    page at 512-byte stride; the offset preserves device identity)
/// 5. Wrap the resulting VA in a `MappedRegs<T>`
///
/// The single `unsafe` block lives at step 5; the driver never has to
/// write `unsafe` itself.
///
/// On any error AFTER the IPC succeeds (`OutOfVirtualMemory`,
/// `MapFailed`), this helper closes the exported MMIO pageset AND
/// fires `CMD_RELEASE_BY_ADDR` to the device manager so the same
/// `mmio_addr` becomes claimable again on retry.
pub fn claim_typed<T: 'static>(
    devmgr_ep: EndpointHandle,
    reply_obj: ReplyHandle,
    mmio_addr: u64,
) -> Result<ClaimedDevice<T>, ClaimError> {
    // Layout sanity checks BEFORE we touch the device manager — a
    // wrong T for this address is a programming error in the driver,
    // not something that should claim then release.
    let t_size = core::mem::size_of::<T>() as u64;
    let t_align = core::mem::align_of::<T>() as u64;
    if mmio_addr % t_align != 0 {
        return Err(ClaimError::LayoutMisaligned);
    }
    let intra_page = mmio_addr & (PAGE_SIZE - 1);
    if intra_page + t_size > PAGE_SIZE {
        return Err(ClaimError::LayoutTooLarge);
    }

    let claim = sys_call_ret4(devmgr_ep, reply_obj, CMD_CLAIM_BY_ADDR, mmio_addr, 0, 0)
        .map_err(|_| ClaimError::IpcFailed)?;
    if claim[0] != CLAIM_OK {
        return Err(ClaimError::Rejected);
    }
    // Wrap the claim in a guard so a failure between IPC success and
    // map success closes the pageset (returning the claim to the
    // device manager). On the success path we `take()` the guard and
    // hand the pageset to ClaimedDevice. Without this, a transient
    // VMEM/map failure would leak the claim and any retry would hit
    // CLAIM_REJECTED because the device manager still thinks the
    // driver owns it.
    let guard = PageSetGuard::new(PageSetHandle(claim[1]));
    let irq_intid = claim[2] as u32;

    let page_va = match VMEM.alloc(1) {
        Some(va) => va,
        None => {
            // Order matters: close the exported pageset BEFORE releasing
            // the device-manager's claim, otherwise there's a window in
            // which a second driver could re-claim the device while we
            // still hold a live mapping handle to it. Dropping the guard
            // here calls sys_close_handle on the pageset.
            drop(guard);
            release_quietly(devmgr_ep, reply_obj, mmio_addr);
            return Err(ClaimError::OutOfVirtualMemory);
        }
    };
    let map_err = sys_map_pages(guard.handle(), page_va, MapMemoryAttribute::Device);
    if !map_err.is_ok() {
        // Return the reserved VA to the pool, close the exported
        // pageset (drop guard), THEN release the device-manager
        // claim. Same ordering rationale as above.
        VMEM.free(page_va, 1);
        drop(guard);
        release_quietly(devmgr_ep, reply_obj, mmio_addr);
        return Err(ClaimError::MapFailed);
    }
    let mmio_va = page_va + (mmio_addr & 0xFFF);
    let mmio_pageset = guard.take();

    // SAFETY: the device manager handed us a valid MMIO pageset at
    // `mmio_addr` and we just mapped it Device-typed at `page_va`.
    // `mmio_va` is the per-device offset within that page. The
    // mapping outlives this `MappedRegs` because the driver process
    // owns the pageset for its whole lifetime.
    let regs = unsafe { MappedRegs::<T>::new(mmio_va) };

    Ok(ClaimedDevice { regs, irq_intid, mmio_pageset })
}

/// Fire-and-forget `CMD_RELEASE_BY_ADDR`. Used on `claim_typed`
/// error paths to free the device-manager's claim so retries work.
/// We ignore the IPC reply because there's nothing useful to do if
/// release itself fails (we're already on an error path).
fn release_quietly(devmgr_ep: EndpointHandle, reply_obj: ReplyHandle, mmio_addr: u64) {
    let _ = sys_call_ret4(devmgr_ep, reply_obj, CMD_RELEASE_BY_ADDR, mmio_addr, 0, 0);
}
