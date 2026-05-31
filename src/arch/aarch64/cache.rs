//! Kernel-side cache-maintenance primitives.
//!
//! Hosts the `dc civac` (clean-and-invalidate to point-of-coherency)
//! and `dc cvac` (clean to point-of-coherency) asm loops the
//! cacheable-DMA migration relies on (see
//! `docs/history/cacheable-dma-migration-plan.md` and
//! `docs/history/post-c1-fix-plan.md` §B2.1).
//!
//! Post C1 of the migration these primitives back the
//! `sys_dma_sync_for_cpu` / `sys_dma_sync_for_device` handlers in
//! `syscall::handler::dma_sync_common`. The kernel TTBR1 direct
//! map covers DmaPool pages cacheably (the pre-C1
//! `mmu::exclude_dma_pool_from_direct_map` exclusion was deleted
//! in C1), so the cache ops operate on real cache lines at the
//! pool's direct-map KVAs.
//!
//! **Device DMA-write drain is the device's responsibility**,
//! signalled by the device's completion interrupt / status (e.g.
//! SDHCI's `TRANSFER_COMPLETE`) — these CPU-side cache primitives
//! do NOT drain a device's AXI write buffer. The kernel cache op
//! invalidates / cleans PE-local lines so the CPU sees DRAM (or
//! pushes its writes there) at the moment the driver believes the
//! handoff is complete; it does nothing about whether the device's
//! transfer has actually settled. Drivers must observe the
//! device's completion before calling `sys_dma_sync_for_cpu`.
//!
//! Coherence at device-handoff points: drivers call
//! `sys_dma_sync_for_cpu` after a device DMA-write completes (so
//! the CPU's next load sees fresh DRAM) and
//! `sys_dma_sync_for_device` before a device DMA-read starts (so
//! the controller sees the CPU's writes). B2.1 upgraded the
//! invalidate path from `dc ivac` to `dc civac` because `dc ivac`
//! on a dirty line is UNPREDICTABLE per ARM DDI 0487 §D7.4.2;
//! `dc civac` is deterministic on any line state (clean-phase is
//! a no-op on clean lines; dirty lines write back before being
//! invalidated). The driver-side `sys_dma_sync_for_device` call
//! before a read-direction DMA (B2.2) still owns ensuring the
//! buffer is clean BEFORE the device writes it — civac AFTER the
//! device writes cannot recover from dirty pre-DMA lines, since
//! the write-back step would overwrite the device's bytes with
//! the stale CPU data.
//!
//! `init_and_check()` reads `CTR_EL0.DminLine` at boot and panics
//! if the silicon's actual data cache line size disagrees with
//! `lockjaw_types::cache::CACHE_LINE_BYTES`. The const is the
//! source of truth for the range math the syscalls use; the boot
//! check catches platform mismatch before any DMA is in flight.
//! Also called once at boot from `main.rs` after the pool's
//! direct-map mapping is established, immediately before the
//! pool-wide `dc civac` sweep that forecloses firmware-era stale
//! cache lines.

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

/// Clean-and-invalidate the data cache for the byte range
/// `[start_kva, start_kva + len)` to the point of coherency, then
/// `dsb sy` so the operation is observable system-wide before the
/// caller proceeds.
///
/// Despite the name "invalidate_range" this primitive issues
/// `dc civac` (clean-and-invalidate), not `dc ivac`. `dc ivac` on
/// a dirty cache line is UNPREDICTABLE per ARM DDI 0487 §D7.4.2;
/// `dc civac` is deterministic on every line state — clean lines
/// take a no-op writeback, dirty lines write back before being
/// invalidated. The name stays "invalidate_range" because that is
/// the operation's *semantics* (post-DMA-write: the caller wants
/// the next load to come from DRAM, not from stale cache); the
/// `civac` choice is the safe-by-default mechanism that gives
/// those semantics regardless of caller cache state.
///
/// The range is expanded to whole cache lines via the host-tested
/// `lockjaw_types::cache::lines_covering` helper.
///
/// # Safety
///
/// The primitive itself is safe regardless of caller cache state
/// (B2.1 — see module doc). BUT the post-DMA-read semantic
/// callers want — "the buffer reads back as the device's bytes" —
/// requires that the buffer's cache lines were CLEAN of pre-DMA
/// dirty CPU writes before the device started the DMA. If they
/// weren't, the clean phase here writes the stale CPU bytes back
/// over the device-deposited bytes in DRAM. Drivers must use
/// `sys_dma_sync_for_device` before kicking a read-direction DMA
/// (B2.2) to maintain that invariant.
///
/// `start_kva` must be a valid kernel direct-map address for the
/// duration of the call.
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
        // `dc civac, xN` — clean-and-invalidate data cache by VA
        // to point-of-coherency, on THIS PE's local cache.
        //
        // What this does: dirty lines write back to DRAM, then
        // every line in the range is invalidated, so a subsequent
        // load from `addr` misses cache and reads from DRAM.
        //
        // What this DOES NOT do: drain the AXI write FIFO of any
        // *device* that owns the buffer. A pre-B2.1 comment here
        // wrongly claimed `dc ivac`'s outer-shareable broadcast
        // somehow drained remote masters' AXI writes. It does
        // not — that bus transaction targets PE caches, not
        // device write buffers. Device DMA-write drain is the
        // device's own job, signalled by its completion interrupt
        // / status (e.g. SDHCI's `TRANSFER_COMPLETE`). Drivers
        // must observe that completion BEFORE calling
        // `sys_dma_sync_for_cpu`.
        //
        // Why `civac` not `ivac`: `dc ivac` on a dirty line is
        // UNPREDICTABLE per ARM DDI 0487 §D7.4.2 (the dirty
        // bytes may evict over freshly DMA-written DRAM, the
        // post-condition where the CPU reads device bytes is not
        // guaranteed). `dc civac` is well-defined on any line
        // state. See module doc and `docs/history/post-c1-fix-plan.md`
        // §B2.1 for the full rationale.
        asm!(
            "dc civac, {addr}",
            addr = in(reg) addr,
            options(nostack, preserves_flags)
        );
        addr += CACHE_LINE_BYTES;
    }
    // `dsb sy` — full system barrier; ensures the clean-and-
    // invalidates above complete and are observable system-wide
    // before the caller's subsequent loads.
    asm!("dsb sy", options(nomem, nostack, preserves_flags));
}

/// Clean (write back) the data cache for the byte range
/// `[start_kva, start_kva + len)` to the point of coherency, then
/// `dsb sy`. Used before a device's DMA read to flush pending CPU
/// writes so the device sees what the CPU wrote.
///
/// Dual to `invalidate_range`: `invalidate_range` is "clean +
/// invalidate" in implementation (via `dc civac`) but
/// "invalidate-for-CPU-consumer" in semantics — post-DMA-write,
/// drop cached lines so a fresh load misses cache. `clean_range`
/// is "clean only" (no invalidate, via `dc cvac`) — pre-DMA-read,
/// push CPU writes to DRAM so the device sees them, but the line
/// can stay cached (the CPU can read it back fast).
///
/// Always safe to call: clean does not drop data, it only writes
/// it back. Repeating a clean is harmless. `start_kva` must be a
/// valid kernel direct-map address for the duration of the call.
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
