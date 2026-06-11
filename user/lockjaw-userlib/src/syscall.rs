use core::arch::asm;
use lockjaw_types::syscall::*;
use lockjaw_types::syscall::SyscallError;
use crate::handle::*;

/// Emit a byte slice to the kernel UART. Atomic w.r.t. other threads'
/// debug output — the kernel holds the GKL for the entire emit loop
/// so output never interleaves character-by-character.
///
/// This is the only userspace path to the kernel UART: there is no
/// per-character `putc` syscall. Callers that need to emit a few bytes
/// should build a small stack buffer and call this once.
pub fn sys_debug_puts(buf: &[u8]) {
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") buf.as_ptr() as u64 => _,
            inlateout("x1") buf.len() as u64 => _,
            in("x8") SYS_DEBUG_PUTS,
        );
    }
}

pub fn sys_yield() {
    unsafe {
        asm!(
            "svc #0",
            in("x8") SYS_YIELD,
            lateout("x0") _,
        );
    }
}

/// Allocate physical pages. Returns a PageSet handle on success.
pub fn sys_alloc_pages(count: u64) -> Result<PageSetHandle, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") count => err,
            inlateout("x1") 0u64 => val,
            in("x8") SYS_ALLOC_PAGES,
        );
    }
    if err == 0 { Ok(PageSetHandle(val)) } else { Err(SyscallError(err)) }
}

/// Allocate physically-contiguous DMA-safe pages from the kernel's DMA
/// pool. Returns a PageSet handle on success; the resulting PageSet
/// has `origin = DmaPool` and (post C1 of the cacheable-DMA migration)
/// must be mapped via `MapMemoryAttribute::Normal` — the kernel
/// rejects `NormalNonCacheable` and `Device` for DmaPool origin.
/// Use `sys_query_pageset_phys(handle, 0)` to obtain the first
/// page's physical address (needed by drivers programming ADMA /
/// SDMA descriptor targets).
///
/// Pool pages are physically reserved at kernel boot and never
/// registered with buddy. Coherence with devices is maintained via
/// `sys_dma_sync_for_cpu` / `sys_dma_sync_for_device` at handoff
/// points (see `lockjaw_userlib::dma_sync`); the single-attribute
/// invariant (Cacheable everywhere) prevents the mixed-attribute
/// alias bug.
pub fn sys_alloc_dma_pages(count: u64) -> Result<PageSetHandle, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") count => err,
            lateout("x1") val,
            in("x8") SYS_ALLOC_DMA_PAGES,
        );
    }
    if err == 0 { Ok(PageSetHandle(val)) } else { Err(SyscallError(err)) }
}

/// Allocate physically contiguous pages. Returns a PageSet handle on success.
/// The pages are guaranteed to be sequential in physical memory,
/// suitable for DMA buffers. The block size is rounded up to the next
/// power of two internally.
pub fn sys_alloc_pages_contiguous(count: u64) -> Result<PageSetHandle, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") count => err,
            inlateout("x1") ALLOC_FLAG_CONTIGUOUS => val,
            in("x8") SYS_ALLOC_PAGES,
        );
    }
    if err == 0 { Ok(PageSetHandle(val)) } else { Err(SyscallError(err)) }
}

/// Map a PageSet's pages into the caller's address space.
/// `attr` selects MAIR regime: `Normal` for RAM, `Device` for MMIO.
/// `attr as u64` discriminant lands in x2; the kernel decodes via
/// `MapMemoryAttribute::from_raw`.
pub fn sys_map_pages(
    ps: PageSetHandle, virt_addr: u64, attr: lockjaw_types::vmem::MapMemoryAttribute,
) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ps.0 => err,
            in("x1") virt_addr,
            in("x2") attr as u64,
            in("x8") SYS_MAP_PAGES,
        );
    }
    SyscallError(err)
}

/// Create a new process from a `ProcessCreateInfo` argument-pack.
/// The kernel reads the struct from caller memory via the user-VA
/// passed in x0 — single source of truth for the syscall ABI.
/// See `lockjaw_types::process::ProcessCreateInfo` and the
/// `docs/architecture/no-kernel-alloc.md` migration plan.
pub fn sys_create_process(info: &lockjaw_types::process::ProcessCreateInfo) -> SyscallError {
    let err: u64;
    let info_va = info as *const _ as u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") info_va => err,
            in("x8") SYS_CREATE_PROCESS,
        );
    }
    SyscallError(err)
}

/// Create a Notification from a donated page. Returns a Notification handle.
pub fn sys_create_notification(ps: PageSetHandle) -> Result<NotificationHandle, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ps.0 => err,
            lateout("x1") val,
            in("x8") SYS_CREATE_NOTIFICATION,
        );
    }
    if err == 0 { Ok(NotificationHandle(val)) } else { Err(SyscallError(err)) }
}

/// Create an Endpoint from a donated page. Returns an Endpoint handle.
pub fn sys_create_endpoint(ps: PageSetHandle) -> Result<EndpointHandle, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ps.0 => err,
            lateout("x1") val,
            in("x8") SYS_CREATE_ENDPOINT,
        );
    }
    if err == 0 { Ok(EndpointHandle(val)) } else { Err(SyscallError(err)) }
}

/// Create a Reply object from a 1-page PageSet. Each client thread
/// allocates one of these at startup and reuses it across sys_call
/// invocations. The Reply is the per-call "who to wake" marker that
/// replaces the single caller_tcb slot on endpoints.
pub fn sys_create_reply(ps: PageSetHandle) -> Result<ReplyHandle, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ps.0 => err,
            lateout("x1") val,
            in("x8") SYS_CREATE_REPLY,
        );
    }
    if err == 0 { Ok(ReplyHandle(val)) } else { Err(SyscallError(err)) }
}

/// Send a message on `ep` and block for a reply bound to `reply`.
/// Returns the first reply word on success.
///
/// The Reply object must be Fresh (either never used, or released by a
/// prior completed call). Each client thread typically owns one Reply and
/// reuses it. A second concurrent call with the same Reply returns
/// `REPLY_BOUND`.
pub fn sys_call(ep: EndpointHandle, reply: ReplyHandle, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ep.0 => err,       // in: endpoint handle, out: error
            inlateout("x1") reply.0 => val,     // in: reply handle, out: reply[0]
            inlateout("x2") msg0 => _,
            inlateout("x3") msg1 => _,
            inlateout("x4") msg2 => _,
            inlateout("x5") msg3 => _,
            in("x8") SYS_CALL,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Send a message and block for reply. Returns all 4 reply words.
pub fn sys_call_ret4(ep: EndpointHandle, reply: ReplyHandle, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> Result<[u64; 4], SyscallError> {
    let err: u64;
    let r1: u64;
    let r2: u64;
    let r3: u64;
    let r4: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ep.0 => err,
            inlateout("x1") reply.0 => r1,
            inlateout("x2") msg0 => r2,
            inlateout("x3") msg1 => r3,
            inlateout("x4") msg2 => r4,
            inlateout("x5") msg3 => _,
            in("x8") SYS_CALL,
        );
    }
    if err == 0 { Ok([r1, r2, r3, r4]) } else { Err(SyscallError(err)) }
}

/// Reply to the call currently bound on this thread's TCB (set by the
/// preceding sys_receive). No endpoint handle is needed — the Reply
/// object carries caller identity.
///
/// Returns `NO_CALLER` if the thread has no bound call.
pub fn sys_reply(msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") msg0 => err,
            in("x1") msg1,
            in("x2") msg2,
            in("x3") msg3,
            in("x8") SYS_REPLY,
        );
    }
    SyscallError(err)
}

/// Blocking receive on an endpoint.
/// Returns the first message word on success. Blocks until a sender is waiting.
pub fn sys_receive(ep: EndpointHandle) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ep.0 => err,
            lateout("x1") val,
            lateout("x2") _,
            lateout("x3") _,
            lateout("x4") _,
            in("x8") SYS_RECEIVE,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Blocking receive on an endpoint.
/// Returns all 4 message words on success. Blocks until a sender is waiting.
pub fn sys_receive_ret4(ep: EndpointHandle) -> Result<[u64; 4], SyscallError> {
    let err: u64;
    let r1: u64;
    let r2: u64;
    let r3: u64;
    let r4: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ep.0 => err,
            lateout("x1") r1,
            lateout("x2") r2,
            lateout("x3") r3,
            lateout("x4") r4,
            in("x8") SYS_RECEIVE,
        );
    }
    if err == 0 { Ok([r1, r2, r3, r4]) } else { Err(SyscallError(err)) }
}

/// Non-blocking receive on an endpoint.
/// Returns the first message word if a sender is waiting,
/// or Err(SyscallError::WOULD_BLOCK) if no message is pending.
pub fn sys_recv_nb(ep: EndpointHandle) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ep.0 => err,
            lateout("x1") val,
            lateout("x2") _,
            lateout("x3") _,
            lateout("x4") _,
            in("x8") SYS_RECV_NB,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Bind a hardware IRQ to a notification.
/// Bind a hardware IRQ to a notification (level-triggered, the default).
pub fn sys_bind_irq(intid: u64, notif: NotificationHandle) -> SyscallError {
    sys_bind_irq_flags(intid, notif, 0)
}

/// Bind a hardware IRQ to a notification with explicit trigger flags.
/// flags bit 0: 1 = edge-triggered, 0 = level-triggered.
pub fn sys_bind_irq_flags(intid: u64, notif: NotificationHandle, flags: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") intid => err,
            in("x1") notif.0,
            in("x2") flags,
            in("x8") SYS_BIND_IRQ,
        );
    }
    SyscallError(err)
}

/// IRQ trigger flag: edge-triggered delivery.
pub const IRQ_FLAG_EDGE: u64 = 1;

/// Re-enable a previously-masked level-triggered IRQ.
///
/// The kernel masks level-triggered IRQs in the GIC distributor
/// immediately after signaling the bound notification (otherwise
/// the still-asserted source line refires the IRQ across EOIR).
/// User-space drivers handling level IRQs must:
///   1. Wake from `sys_wait_notification`.
///   2. Read the device's interrupt-status register.
///   3. Clear the latched status bits via the device's W1C path
///      (e.g. SDHCI: write 1s to NORMAL_INT_STATUS).
///   4. Call `sys_unmask_irq(intid)` to re-arm the GIC.
///
/// No-op for edge-triggered IRQs (the kernel doesn't mask those),
/// but the syscall still requires the intid to be a previously-
/// bound binding so it can't re-enable arbitrary SPIs.
pub fn sys_unmask_irq(intid: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") intid => err,
            in("x8") SYS_UNMASK_IRQ,
        );
    }
    SyscallError(err)
}

/// Wait on a notification until the timeline value reaches the threshold.
/// Returns the current counter value on success, or blocks until it does.
pub fn sys_wait_notification(notif: NotificationHandle, threshold: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") notif.0 => err,
            inlateout("x1") threshold => val,
            in("x8") SYS_WAIT_NOTIFICATION,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Export a handle into the caller currently bound on this thread's TCB
/// (set by the preceding sys_receive). Any handle type can be exported.
/// Returns the new handle index in the caller's handle table.
///
/// Returns `NO_CALLER` if there is no bound call.
pub fn sys_export_handle(h: impl Exportable) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") h.raw() => err,
            lateout("x1") val,
            in("x8") SYS_EXPORT_HANDLE,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Wait until any of the given objects is ready.
/// entries = array of WaitEntry (handle + threshold pairs).
/// Returns a bitmask: bit N set = entry N is ready.
///
/// Internally passes `NO_DEADLINE` (= u64::MAX) in x2 to the kernel's
/// 3-arg `sys_wait_any` ABI — this wrapper is the no-timeout form.
/// For the deadline-aware form, use `lockjaw_userlib::time::sleep_until`
/// or call the syscall directly with x2 = absolute CNTVCT_EL0 ticks.
pub fn sys_wait_any(entries: &[crate::WaitEntry]) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") entries.as_ptr() => err,
            inlateout("x1") entries.len() => val,
            in("x2") u64::MAX,                  // NO_DEADLINE — no timeout
            in("x8") SYS_WAIT_ANY,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Wait until any of the given objects is ready or the absolute
/// monotonic deadline expires. `entries` may be empty — that is the
/// "pure sleep" form (only the deadline can wake us). Returns the
/// readiness bitmask: bit N set = entry N became ready; mask == 0
/// = deadline expired before any object fired (timeout encoding).
pub fn sys_wait_any_until(
    entries: &[crate::WaitEntry],
    deadline: lockjaw_types::time::MonoTicks,
) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") entries.as_ptr() => err,
            inlateout("x1") entries.len() => val,
            in("x2") deadline.0,                // Absolute CNTVCT_EL0 deadline (NO_DEADLINE = u64::MAX)
            in("x8") SYS_WAIT_ANY,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Block the current thread forever. The kernel marks the thread
/// Blocked and parks it in a wfi loop with no wake source — no
/// IPC waiters, no deadline. Re-entry is impossible by construction;
/// the only way out is reset.
///
/// Use this when a thread has reached "done" but the process must
/// stay alive (init), or when a daemon has failed irrecoverably and
/// `sys_exit` would propagate the failure (e.g., CPU 0 boot TCB
/// cannot exit — its kernel stack lives in the linker image, not the
/// KVM pool, so finish_exit panics).
///
/// **Do not** spin on `sys_yield` or `loop { wfi }` instead. Those
/// keep the thread in `Running`/`Ready` state and contend for fair
/// round-robin slots — every other Ready thread on the CPU then
/// gets a 10ms timer-tick quantum even when there is no real work
/// to do, which inflates perf measurements unpredictably.
pub fn park_forever() -> ! {
    // sys_wait_any with empty entries + NO_DEADLINE → kernel marks
    // the thread Blocked, never wakes. The match arm covers
    // belt-and-braces: if a future kernel change does wake the
    // thread, we re-park instead of returning into caller code.
    loop {
        let _ = sys_wait_any_until(&[], lockjaw_types::time::MonoTicks::NO_DEADLINE);
    }
}

/// Unmap a PageSet from the caller's address space.
/// VA must match the address used in sys_map_pages. Validates that
/// every PTE maps to the expected physical page before clearing.
pub fn sys_unmap_pages(ps: PageSetHandle, va: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ps.0 => err,
            in("x1") va,
            in("x8") SYS_UNMAP_PAGES,
        );
    }
    SyscallError(err)
}

/// Boot info returned by the kernel: the DTB PageSet handle plus the
/// in-page offset of the DTB header within the first page of that
/// PageSet. The offset is nonzero on platforms whose firmware places
/// the DTB at an unaligned physical address (notably Pi 4B). To read
/// DTB bytes, map the PageSet at some VA and start reading at
/// `va + dtb_in_page_offset`.
///
/// See `lockjaw_types::dtb_layout` for the layout model.
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub dtb_pageset: PageSetHandle,
    pub dtb_in_page_offset: u32,
}

/// Get boot information from the kernel.
pub fn sys_get_boot_info() -> Result<BootInfo, SyscallError> {
    let err: u64;
    let dtb_handle: u64;
    let dtb_offset: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x8") SYS_GET_BOOT_INFO,
            lateout("x0") err,
            lateout("x1") dtb_handle,
            lateout("x2") dtb_offset,
        );
    }
    if err == 0 {
        Ok(BootInfo {
            dtb_pageset: PageSetHandle(dtb_handle),
            dtb_in_page_offset: dtb_offset as u32,
        })
    } else {
        Err(SyscallError(err))
    }
}

/// Register a physical MMIO address as a tracked PageSet.
/// Returns a PageSet handle for use with sys_map_pages.
pub fn sys_register_device_page(phys_addr: u64) -> Result<PageSetHandle, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") phys_addr => err,
            lateout("x1") val,
            in("x8") SYS_REGISTER_DEVICE_PAGE,
        );
    }
    if err == 0 { Ok(PageSetHandle(val)) } else { Err(SyscallError(err)) }
}

/// Query the physical address of a page within a PageSet.
/// Used by drivers that need to program DMA base registers.
pub fn sys_query_pageset_phys(ps: PageSetHandle, page_index: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") ps.0 => err,
            inlateout("x1") page_index => val,
            in("x8") SYS_QUERY_PAGESET_PHYS,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Create a new thread in the calling process via donate-and-claim
/// (NK3). The thread shares the caller's address space and handle
/// table. Starts at `entry` with SP=stack_top and x0=arg.
///
/// Caller MUST pre-allocate two 1-page Buddy-origin PageSets via
/// `sys_alloc_pages(1)` and pass their handles as `stack_ps` (for
/// the new thread's kernel stack) and `tcb_ps` (for its TCB). Both
/// PageSets are CONSUMED on success — the kernel takes ownership.
///
/// Asymmetric loss on certain failure paths: if `stack_ps` validates
/// ok and is consumed but `tcb_ps` validate fails, the stack PageSet
/// is permanently kernel-owned (one-way). Similarly if `tcb_ps`
/// validates ok but `tcb::create_tcb` rejects the donation, the
/// stack is gone but the TCB PageSet remains caller-owned (by
/// init-before-consume contract).
pub fn sys_create_thread(
    entry: u64,
    stack_top: u64,
    stack_base: u64,
    arg: u64,
    stack_ps: PageSetHandle,
    tcb_ps: PageSetHandle,
) -> Result<(), SyscallError> {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") entry => err,
            in("x1") stack_top,
            in("x2") stack_base,
            in("x3") arg,
            in("x4") stack_ps.0,
            in("x5") tcb_ps.0,
            in("x8") SYS_CREATE_THREAD,
        );
    }
    if err == 0 { Ok(()) } else { Err(SyscallError(err)) }
}

/// Query the mapping state at a page-aligned user VA.
/// Returns `(mapped, run_pages)`: whether the page is mapped and how
/// many consecutive pages share the same mapped/unmapped state.
pub fn sys_query_mapping(va: u64) -> Result<(bool, u64), SyscallError> {
    let err: u64;
    let mapped: u64;
    let run_pages: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") va => err,
            lateout("x1") mapped,
            lateout("x2") run_pages,
            in("x8") SYS_QUERY_MAPPING,
        );
    }
    if err == 0 { Ok((mapped != 0, run_pages)) } else { Err(SyscallError(err)) }
}

/// Query the caller token of the most recently dequeued sender/caller
/// on this thread. Set on every successful sys_receive or sys_recv_nb.
/// Returns 0 if this thread has never received.
pub fn sys_query_caller_token() -> u64 {
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            lateout("x0") _,
            lateout("x1") val,
            in("x8") SYS_QUERY_CALLER_TOKEN,
        );
    }
    val
}

/// Snapshot of scheduler-side counters for userspace perf diagnostics.
/// Each counter is monotonically non-decreasing; the four are NOT
/// snapshot-consistent (read with four separate atomic loads).
#[derive(Clone, Copy, Debug)]
pub struct SchedTelemetry {
    pub ticks: u64,
    pub ctx_switches: u64,
    pub ttbr0_writes: u64,
    /// High-water mark of tick-handler elapsed CNTVCT ticks since
    /// boot. Divide by `cntfreq_hz` to convert to microseconds.
    pub tick_max_cycles: u64,
}

impl SchedTelemetry {
    /// Per-counter deltas (saturating) from `from` to `self`.
    /// `tick_max_cycles` is a HIGH-WATER MARK, so the delta is
    /// "how much the max grew during this window" — 0 means the
    /// max-cost tick handler in the window was <= the prior max.
    pub fn delta_from(&self, from: &SchedTelemetry) -> SchedTelemetry {
        SchedTelemetry {
            ticks: self.ticks.saturating_sub(from.ticks),
            ctx_switches: self.ctx_switches.saturating_sub(from.ctx_switches),
            ttbr0_writes: self.ttbr0_writes.saturating_sub(from.ttbr0_writes),
            tick_max_cycles: self.tick_max_cycles.saturating_sub(from.tick_max_cycles),
        }
    }
}

/// Diagnostic snapshot of scheduler counters (tick_count, context
/// switches, TTBR0 writes, tick-handler max cycles). Read-only.
/// See `SchedTelemetry`.
pub fn sys_sched_telemetry() -> SchedTelemetry {
    let ticks: u64;
    let ctx_switches: u64;
    let ttbr0_writes: u64;
    let tick_max_cycles: u64;
    unsafe {
        asm!(
            "svc #0",                       // Trap to EL1; kernel reads x8 for syscall number.
            lateout("x0") _,                // x0: syscall error (always OK for read-only telemetry).
            lateout("x1") ticks,            // x1: TICK_COUNT snapshot.
            lateout("x2") ctx_switches,     // x2: CONTEXT_SWITCH_COUNT snapshot.
            lateout("x3") ttbr0_writes,     // x3: TTBR0_WRITE_COUNT snapshot.
            lateout("x4") tick_max_cycles,  // x4: TICK_MAX_CYCLES high-water mark (CNTVCT ticks).
            in("x8") SYS_SCHED_TELEMETRY,   // x8: syscall dispatch number.
        );
    }
    SchedTelemetry { ticks, ctx_switches, ttbr0_writes, tick_max_cycles }
}

/// Close a handle, freeing the slot for reuse. Does not free the
/// backing kernel object or its pages (no refcounting yet).
pub fn sys_close_handle(h: impl Exportable) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") h.raw() => err,
            in("x8") SYS_CLOSE_HANDLE,
        );
    }
    SyscallError(err)
}

/// Exit the current thread. Never returns. The kernel frees the thread's
/// TCB, kernel stack, and handle table pages.
pub fn sys_exit() -> ! {
    unsafe {
        asm!(
            "svc #0",
            in("x8") SYS_EXIT,
            options(noreturn),
        );
    }
}
