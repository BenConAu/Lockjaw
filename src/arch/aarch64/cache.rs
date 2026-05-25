// C0 of the cacheable-DMA migration ships `invalidate_range` and
// `clean_range` compiled but unreferenced from any caller. They go
// live at C1 along with the sync syscall handlers. `dead_code`
// allowed at the module level here only until C1.
#![allow(dead_code)]

//! Kernel-side cache-maintenance primitives.
//!
//! Hosts the `dc ivac` (invalidate to point-of-coherency) and
//! `dc cvac` (clean to point-of-coherency) asm loops the
//! cacheable-DMA migration relies on (see
//! `docs/cacheable-dma-migration-plan.md`).
//!
//! At C0 these primitives are compiled but unreferenced from any
//! dispatch path — `sys_dma_sync_for_cpu` / `sys_dma_sync_for_device`
//! return `NOT_SUPPORTED` at C0. They go live in C1, when the
//! `mmu::exclude_dma_pool_from_direct_map` exclusion is removed and
//! the pool's KVAs become reachable for `dc ivac` to operate on.
//!
//! `init_and_check()` reads `CTR_EL0.DminLine` at boot and panics
//! if the silicon's actual data cache line size disagrees with
//! `lockjaw_types::cache::CACHE_LINE_BYTES`. The const is the
//! source of truth for the range math the syscalls use; the boot
//! check catches platform mismatch before any DMA is in flight.

use core::arch::asm;
use lockjaw_types::cache::CACHE_LINE_BYTES;

/// Read `CTR_EL0.DminLine` and confirm the silicon's data cache
/// line size matches `CACHE_LINE_BYTES`. Called once at boot from
/// `main.rs` after MMU + higher-half are active.
///
/// `CTR_EL0.DminLine` (bits 16-19) encodes the log2 of the line
/// size in **words** (4-byte units), so the byte size is
/// `4 << DminLine`. We assert this equals the constant; mismatch
/// panics because the range math (`lockjaw_types::cache`) is built
/// against the constant and operating on the wrong line size would
/// either miss lines (correctness bug) or operate on extra lines
/// (performance, but also potentially trampling adjacent buffers
/// outside the DMA range — silent data corruption).
pub fn init_and_check() {
    // SAFETY: `mrs xN, CTR_EL0` is a read-only system register read
    // unconditionally available at EL1.
    let ctr_el0: u64;
    unsafe {
        asm!(
            "mrs {ctr}, CTR_EL0",   // CTR_EL0 → general-purpose reg
            ctr = out(reg) ctr_el0,
            options(nomem, nostack, preserves_flags)
        );
    }
    let dminline = (ctr_el0 >> 16) & 0xF;
    let line_bytes = 4u64 << dminline;
    if line_bytes != CACHE_LINE_BYTES {
        crate::kprintln!(
            "  CACHE: CTR_EL0=", ctr_el0,
            " DminLine=", dminline as u32,
            " line_bytes=", line_bytes,
            " (expected ", CACHE_LINE_BYTES, ")"
        );
        panic!("cache line size mismatch — see CTR_EL0 above");
    }
    crate::kprintln!("  Cache: DminLine=", dminline as u32, " line=", line_bytes, " bytes (matches CACHE_LINE_BYTES)");
}

/// Invalidate the data cache for the byte range `[start_kva,
/// start_kva + len)` to the point of coherency, then `dsb sy` so
/// the invalidation is observable system-wide before the caller
/// proceeds.
///
/// The range is expanded to whole cache lines via the host-tested
/// `lockjaw_types::cache::lines_covering` helper.
///
/// # Safety
///
/// Invalidation DROPS any dirty CPU cache lines in the range
/// without writing them back. Callers must ensure no in-flight
/// CPU write to the range exists, otherwise that write is lost.
/// For the DMA-completion path this is always true (the device
/// just finished writing the buffer; the CPU has not touched it
/// yet). `start_kva` must be a valid kernel direct-map address
/// for the duration of the call.
pub unsafe fn invalidate_range(start_kva: u64, len: u64) {
    let (line_start, line_count) = match
        lockjaw_types::cache::lines_covering(start_kva, len)
    {
        Some(v) => v,
        None => {
            // Range arithmetic overflows. Silent-wrap would
            // invalidate the wrong lines (likely near zero) and
            // corrupt unrelated memory's cached state — strictly
            // worse than "the operation didn't happen." Panic so
            // the calling syscall handler is forced to validate
            // before us. Should be unreachable in production code
            // paths: kernel-side direct-map KVAs are far from
            // u64::MAX, and the syscall handler will pre-validate.
            crate::kprintln!(
                "  CACHE: invalidate_range overflow: start_kva=", start_kva,
                " len=", len
            );
            panic!("cache::invalidate_range: range overflows u64");
        }
    };
    if line_count == 0 {
        // Empty range; still issue a barrier so callers can rely
        // on dsb-sy ordering after the call regardless of length.
        unsafe {
            asm!("dsb sy", options(nomem, nostack, preserves_flags));
        }
        return;
    }
    let mut addr = line_start;
    for _ in 0..line_count {
        // `dc ivac, xN` — invalidate data cache by VA to PoC.
        // Outer-shareable; the cache controller participates in
        // the bus protocol so pending writes from other coherent
        // masters drain to the PoC before the invalidate
        // completes. This is the drain mechanism the migration
        // relies on (see docs/cacheable-dma-migration-plan.md).
        asm!(
            "dc ivac, {addr}",
            addr = in(reg) addr,
            options(nostack, preserves_flags)
        );
        addr += CACHE_LINE_BYTES;
    }
    // `dsb sy` — full system barrier; ensures the invalidates
    // above complete and are observable system-wide before the
    // caller's subsequent loads.
    asm!("dsb sy", options(nomem, nostack, preserves_flags));
}

/// Clean (write back) the data cache for the byte range
/// `[start_kva, start_kva + len)` to the point of coherency, then
/// `dsb sy`. Used before a device's DMA read to flush pending CPU
/// writes so the device sees what the CPU wrote.
///
/// Safe (unlike `invalidate_range`): clean does not drop data, it
/// only writes it back. Repeating a clean is harmless. `start_kva`
/// must be a valid kernel direct-map address for the duration of
/// the call.
pub fn clean_range(start_kva: u64, len: u64) {
    let (line_start, line_count) = match
        lockjaw_types::cache::lines_covering(start_kva, len)
    {
        Some(v) => v,
        None => {
            crate::kprintln!(
                "  CACHE: clean_range overflow: start_kva=", start_kva,
                " len=", len
            );
            panic!("cache::clean_range: range overflows u64");
        }
    };
    if line_count == 0 {
        // SAFETY: dsb sy is always safe to issue at EL1.
        unsafe {
            asm!("dsb sy", options(nomem, nostack, preserves_flags));
        }
        return;
    }
    let mut addr = line_start;
    for _ in 0..line_count {
        // `dc cvac, xN` — clean data cache by VA to PoC. Writes
        // dirty lines back to memory; no-op for clean lines.
        // SAFETY: clean is non-destructive; no precondition on
        // CPU/device write order.
        unsafe {
            asm!(
                "dc cvac, {addr}",
                addr = in(reg) addr,
                options(nostack, preserves_flags)
            );
        }
        addr += CACHE_LINE_BYTES;
    }
    // SAFETY: dsb sy is always safe to issue at EL1.
    unsafe {
        asm!("dsb sy", options(nomem, nostack, preserves_flags));
    }
}
