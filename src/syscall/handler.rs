use crate::arch::aarch64::exceptions::ExceptionContext;
use crate::arch::aarch64::uart::Uart;
use crate::cap::handle_table;
use crate::cap::object::ObjectType;
use crate::cap::rights::{Rights, RIGHT_READ, RIGHT_WRITE};
use crate::ipc::endpoint;
use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::{KernelRef, KernelMut};
use crate::sched::current::CurrentThread;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;

// Syscall numbers and error type from lockjaw-types.
use lockjaw_types::syscall::*;
use lockjaw_types::syscall::SyscallError;

/// Result of a syscall dispatch.
/// x0 is ALWAYS the error (0 = success, nonzero = SyscallError).
/// x1 is the return value for value-returning syscalls.
/// x1-x4 are IPC message words for message-returning syscalls.
enum SyscallReturn {
    /// Syscall has no return value — x0=error, x1 unchanged.
    Void(SyscallError),
    /// Syscall returns a single value — x0=error, x1=value on success.
    Value(Result<u64, SyscallError>),
    /// Syscall returns IPC message words — x0=error, x1-x4=msg on success.
    /// The handler already wrote x1-x4; dispatch only writes x0.
    Message(SyscallError),
}

/// Dispatch a syscall from userspace.
///
/// Called from handle_exception_sync_lower when ESR_EL1.EC = 0x15 (SVC from AArch64).
/// Reads the syscall number from x8, dispatches to the handler, and writes
/// error/return values to the appropriate registers.
pub fn handle_syscall(ctx: &mut ExceptionContext) {
    let syscall_num = ctx.gpr[8]; // x8

    // Record syscall breadcrumb for crash diagnostics
    CurrentThread::set_breadcrumb(syscall_num, [ctx.gpr[0], ctx.gpr[1], ctx.gpr[2], ctx.gpr[3]]);

    // Dispatch. Void syscalls return SyscallError only.
    // Value-returning syscalls return Result<u64, SyscallError>.
    // Message syscalls return SyscallReturn::Message with x1-x4 pre-set.
    // x0 = error (always), x1 = value (Value), x1-x4 = msg (Message).
    let ret = match syscall_num {
        SYS_DEBUG_PUTS => SyscallReturn::Void(sys_debug_puts(ctx)),
        SYS_YIELD => SyscallReturn::Void(sys_yield()),
        SYS_SEND => SyscallReturn::Void(sys_send(ctx)),
        SYS_RECEIVE => sys_receive(ctx),
        SYS_CALL => sys_call(ctx),
        SYS_REPLY => SyscallReturn::Void(sys_reply(ctx)),
        SYS_ALLOC_PAGES => SyscallReturn::Value(sys_alloc_pages(ctx)),
        SYS_ALLOC_DMA_PAGES => SyscallReturn::Value(sys_alloc_dma_pages(ctx)),
        SYS_MAP_PAGES => SyscallReturn::Void(sys_map_pages(ctx)),
        SYS_CREATE_PROCESS => SyscallReturn::Void(sys_create_process(ctx)),
        SYS_CREATE_NOTIFICATION => SyscallReturn::Value(sys_create_notification(ctx)),
        SYS_SIGNAL_NOTIFICATION => SyscallReturn::Void(sys_signal_notification(ctx)),
        SYS_WAIT_NOTIFICATION => SyscallReturn::Value(sys_wait_notification(ctx)),
        SYS_BIND_IRQ => SyscallReturn::Void(sys_bind_irq(ctx)),
        SYS_CREATE_ENDPOINT => SyscallReturn::Value(sys_create_endpoint(ctx)),
        SYS_RECV_NB => sys_recv_nb(ctx),
        SYS_WAIT_ANY => SyscallReturn::Value(sys_wait_any(ctx)),
        SYS_EXPORT_HANDLE => SyscallReturn::Value(sys_export_handle(ctx)),
        SYS_GET_BOOT_INFO => sys_get_boot_info(ctx),
        SYS_REGISTER_DEVICE_PAGE => SyscallReturn::Value(sys_register_device_page(ctx)),
        SYS_QUERY_PAGESET_PHYS => SyscallReturn::Value(sys_query_pageset_phys(ctx)),
        SYS_CREATE_REPLY => SyscallReturn::Value(sys_create_reply(ctx)),
        SYS_CREATE_THREAD => SyscallReturn::Void(sys_create_thread(ctx)),
        SYS_QUERY_MAPPING => sys_query_mapping(ctx),
        SYS_CLOSE_HANDLE => SyscallReturn::Void(sys_close_handle(ctx)),
        SYS_UNMAP_PAGES => SyscallReturn::Void(sys_unmap_pages(ctx)),
        SYS_QUERY_CALLER_TOKEN => SyscallReturn::Value(Ok(sys_query_caller_token())),
        SYS_EXIT => {
            scheduler::exit_current(); // never returns
        }
        _ => {
            crate::kprintln!("Unknown syscall ", syscall_num);
            SyscallReturn::Void(SyscallError::INVALID_PARAMETER)
        }
    };
    // x0 = error (always). x1 = value (for Value), x1-x4 = msg (for Message).
    match ret {
        SyscallReturn::Void(err) => {
            ctx.gpr[0] = err.0;
        }
        SyscallReturn::Value(Ok(val)) => {
            ctx.gpr[0] = 0;
            ctx.gpr[1] = val;
        }
        SyscallReturn::Value(Err(err)) => {
            ctx.gpr[0] = err.0;
        }
        SyscallReturn::Message(err) => {
            // x1-x4 already written by the handler (IPC message words).
            // Only set x0 = error status.
            ctx.gpr[0] = err.0;
        }
    }

    // Clear syscall breadcrumb
    CurrentThread::clear_breadcrumb();
}

/// Common logic for sys_create_endpoint, sys_create_notification,
/// sys_create_reply. The new object is addressed through the KVM
/// pool: every fallible step (including `kvm::map_existing` for the
/// donated frame) runs **before** `consume_pageset_apply`, so a
/// KVM-OOM cannot strand the caller with their PageSet revoked.
///
/// The factory takes the donated page's KVA — already mapped — and
/// packages it into the appropriate `HandleKind` variant. The KVA
/// reservation is held under a `MappedKvmRangeGuard`; on
/// `ht.insert` failure the guard drops and unmaps, returning the
/// VA range to the KVM free list (the donated physical frame still
/// leaks in that path — same as the pre-migration leak when no
/// destroy path exists; tracked in `docs/tech-debt.md`).
fn create_kernel_object_kvm(
    ps_handle: u32,
    make_kind: fn(lockjaw_types::addr::KernelVa) -> lockjaw_types::object::HandleKind,
    init_fn: fn(crate::mm::addr::ObjectInitPage) -> Result<(), crate::cap::object::CreateError>,
) -> Result<u64, SyscallError> {
    let ht = CurrentThread::handle_table();
    // Require WRITE rights — destructive operation that consumes the PageSet.
    let entry = ht.lookup(ps_handle, Rights::from_bits(RIGHT_WRITE), ObjectType::PageSet)?;
    let header_kva = match entry.kind {
        lockjaw_types::object::HandleKind::PageSet { kva, .. } => kva,
        _ => return Err(SyscallError::INVALID_HANDLE),
    };

    // SAFETY: PageSet handle → registered header.
    let backed = unsafe { crate::cap::pageset_table::read_header_backed(header_kva) };
    // M6: donate-as-kernel-object remaps the donated frame as
    // cacheable Normal in KVM (build_kernel_page). A DmaPool-origin
    // PageSet would create the mixed-attribute alias — reject up-front.
    // Uninit origin (None from from_raw) is also rejected: a header
    // whose origin was never written is a bug at the alloc site.
    match backed.raw().origin() {
        Some(lockjaw_types::pageset_table::PageSetOrigin::DmaPool) =>
            return Err(SyscallError::INVALID_PARAMETER),
        None => return Err(SyscallError::INVALID_HANDLE),
        Some(lockjaw_types::pageset_table::PageSetOrigin::Buddy) => {}
    }
    if backed.data_page_count() != 1 {
        return Err(SyscallError::INVALID_PARAMETER);
    }
    let page_paddr = backed.get_page(0).ok_or(SyscallError::INVALID_HANDLE)?;
    // SAFETY: page came from a registered PageSet — valid kernel page.
    let init_page = unsafe { crate::mm::addr::ObjectInitPage::new(PhysAddr::new(page_paddr)) };

    // -- Validate phase (every step here is fallible, no state changes
    //    that the caller can't recover from). --
    //
    // Critical: `init_fn` writes the new object's bytes into the
    // donated page in place. That write is observable to the caller
    // through their PageSet handle until `consume_pageset_apply`
    // runs. So every fallible step — including the KVA reservation —
    // must complete BEFORE `init_fn`. If we ran init_fn first and
    // then map_existing failed, we'd return OUT_OF_MEMORY without
    // consuming the PageSet, leaving the caller with intact handle
    // ownership but a frame whose contents we already overwrote.

    if crate::cap::pageset_table::consume_pageset_validate(header_kva).is_err() {
        return Err(SyscallError::INVALID_HANDLE);
    }

    // Reserve the KVA mapping for the donated frame. PTE writes only —
    // does not touch the frame's contents. If this fails, the caller
    // still holds their PageSet handle and the page bytes are
    // unchanged.
    let mapped = crate::mm::kvm::map_existing(crate::mm::addr::PhysPage::containing(
        PhysAddr::new(page_paddr),
    )).map_err(|_| SyscallError::OUT_OF_MEMORY)?;
    let mut guard = crate::mm::kvm::MappedKvmRangeGuard::new(mapped);

    // Init writes the new object's bytes through the linear map; the
    // same bytes are visible through the KVA reserved above. This is
    // the first destructive step on the caller's frame, but the only
    // remaining work is consume_pageset_apply (infallible) and
    // ht.insert (residual fallible — leaks the donated frame on the
    // failure path same as before, but the KVA range is recovered by
    // the guard's Drop).
    if init_fn(init_page).is_err() {
        return Err(SyscallError::UNKNOWN);
    }

    // -- Apply phase: every step here must be infallible (or its
    //    failure must leave a self-cleaning footprint). --

    // Phase 2: revoke every cross-process handle, clear PTEs,
    // dec refcount/map_count, unlink and free the header.
    // Cannot fail under the validate→apply contract.
    crate::cap::pageset_table::consume_pageset_apply(header_kva);

    let kva = guard.kva();
    match ht.insert(Rights::from_bits(RIGHT_READ | RIGHT_WRITE), make_kind(kva)) {
        Ok(h) => {
            // Success: transfer KVA ownership to the handle.
            guard.take();
            Ok(h as u64)
        }
        Err(e) => {
            // ht.insert is the one residual fallible step after the
            // irreversible boundary — pre-migration this leaked the
            // donated page (no destroy path). Now the donated frame
            // still leaks, but the guard's Drop returns the KVA to
            // the KVM free list so the VA pool doesn't bleed.
            Err(e)
        }
    }
}

fn sys_debug_puts(ctx: &mut ExceptionContext) -> SyscallError {
    let buf_va = ctx.gpr[0];
    let len = ctx.gpr[1] as usize;

    if len == 0 {
        return SyscallError::OK;
    }
    if len > lockjaw_types::addr::PAGE_SIZE as usize {
        return SyscallError::INVALID_PARAMETER;
    }
    // Reject buffers that extend past the user VA range (also catches
    // overflow on buf_va + len). Done before the page-table walk so a
    // bad pointer can't even start a translation.
    if !lockjaw_types::wait::validate_user_buffer(buf_va, len as u64) {
        return SyscallError::INVALID_PARAMETER;
    }

    let addr_space = match CurrentThread::address_space() {
        Some(a) => a,
        None => return SyscallError::INVALID_PARAMETER,
    };
    let ttbr0 = addr_space.ttbr0();

    // Atomic-emit contract: sys_debug_puts is all-or-nothing. Userspace's
    // `puts`/`put_decimal`/`put_hex` wrappers depend on the kernel either
    // emitting the entire buffer or none of it (so concurrent writers'
    // output never interleaves mid-line). The buffer is capped at
    // PAGE_SIZE, so it spans at most 2 pages — translate both up front
    // and bail before writing any byte if either is unmapped.
    let page_size = lockjaw_types::addr::PAGE_SIZE as u64;
    let first_page_va = buf_va & !(page_size - 1);
    let last_page_va = (buf_va + len as u64 - 1) & !(page_size - 1);

    // SAFETY: ttbr0 is a live L0 from CurrentThread::address_space().
    let kva_first = match unsafe { crate::arch::aarch64::vmem::translate_user_va(ttbr0, first_page_va) } {
        Some(k) => k,
        None => return SyscallError::INVALID_PARAMETER,
    };
    let kva_last = if last_page_va == first_page_va {
        kva_first
    } else {
        // SAFETY: ttbr0 is a live L0 from CurrentThread::address_space().
        match unsafe { crate::arch::aarch64::vmem::translate_user_va(ttbr0, last_page_va) } {
            Some(k) => k,
            None => return SyscallError::INVALID_PARAMETER,
        }
    };

    // Both pages validated; emit cannot fault.
    let uart = Uart::new();
    for i in 0..len {
        let user_va = buf_va + i as u64;
        let kva_base = if user_va & !(page_size - 1) == first_page_va { kva_first } else { kva_last };
        let in_page_offset = user_va & (page_size - 1);
        // SAFETY: kva_base is the TTBR1 translation of the user page (validated above); in_page_offset is < PAGE_SIZE.
        let byte = unsafe { core::ptr::read((kva_base + in_page_offset) as *const u8) };
        uart.putc(byte);
    }
    SyscallError::OK
}

fn sys_yield() -> SyscallError {
    scheduler::tick();
    SyscallError::OK
}

/// sys_send(handle, msg0, msg1, msg2, msg3) — send a message on an endpoint.
/// x0 = endpoint handle, x1-x4 = message registers.
fn sys_send(ctx: &mut ExceptionContext) -> SyscallError {
    let handle = ctx.gpr[0] as u32;
    let msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];
    match crate::cap::object_ops::send(handle, msg) {
        Ok(Ok(())) => SyscallError::OK,
        Ok(Err(_)) => SyscallError::ENDPOINT_BUSY,
        Err(e) => e,
    }
}

/// sys_receive(handle) — receive a message from an endpoint.
/// x0 = endpoint handle. On success: x0=0, x1-x4 = message words.
fn sys_receive(ctx: &mut ExceptionContext) -> SyscallReturn {
    let handle = ctx.gpr[0] as u32;
    match crate::cap::object_ops::receive(handle) {
        Ok(Ok(msg)) => {
            ctx.gpr[1] = msg[0];
            ctx.gpr[2] = msg[1];
            ctx.gpr[3] = msg[2];
            ctx.gpr[4] = msg[3];
            SyscallReturn::Message(SyscallError::OK)
        }
        Ok(Err(_)) => SyscallReturn::Message(SyscallError::UNKNOWN),
        Err(e) => SyscallReturn::Message(e),
    }
}

/// sys_call(ep_handle, reply_handle, msg0, msg1, msg2, msg3) — send message
/// and block for reply. The Reply object identifies the caller; on return,
/// it's Fresh and ready to reuse on the next call.
/// x0 = endpoint handle, x1 = reply handle, x2-x5 = message.
/// On success: x0 = 0, x1-x4 = reply words.
fn sys_call(ctx: &mut ExceptionContext) -> SyscallReturn {
    let ep_handle = ctx.gpr[0] as u32;
    let reply_handle = ctx.gpr[1] as u32;
    let msg = [ctx.gpr[2], ctx.gpr[3], ctx.gpr[4], ctx.gpr[5]];
    match crate::cap::object_ops::call(ep_handle, reply_handle, msg) {
        Ok(Ok(reply_msg)) => {
            ctx.gpr[1] = reply_msg[0];
            ctx.gpr[2] = reply_msg[1];
            ctx.gpr[3] = reply_msg[2];
            ctx.gpr[4] = reply_msg[3];
            SyscallReturn::Message(SyscallError::OK)
        }
        Ok(Err(endpoint::IpcError::ReplyBound)) => SyscallReturn::Message(SyscallError::REPLY_BOUND),
        Ok(Err(_)) => SyscallReturn::Message(SyscallError::UNKNOWN),
        Err(e) => SyscallReturn::Message(e),
    }
}

/// sys_reply(msg0, msg1, msg2, msg3) — reply to the call currently bound on
/// the replier's TCB (set by the preceding sys_receive). No endpoint handle
/// is needed; the Reply object carries caller identity.
/// x0-x3 = reply message. Returns NO_CALLER if the TCB has no bound call.
fn sys_reply(ctx: &mut ExceptionContext) -> SyscallError {
    let reply_msg = [ctx.gpr[0], ctx.gpr[1], ctx.gpr[2], ctx.gpr[3]];
    match crate::ipc::reply::ipc_reply(reply_msg) {
        Ok(()) => SyscallError::OK,
        Err(endpoint::IpcError::NoCaller) => SyscallError::NO_CALLER,
        Err(_) => SyscallError::UNKNOWN,
    }
}

/// sys_alloc_pages(count, flags) — allocate physical pages.
/// x0 = number of pages to allocate.
/// x1 = flags (ALLOC_FLAG_CONTIGUOUS for physically contiguous pages).
/// Returns a PageSet handle in x1 on success.
fn sys_alloc_pages(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let count = ctx.gpr[0] as usize;
    let flags = ctx.gpr[1];

    if flags & !lockjaw_types::syscall::ALLOC_FLAG_CONTIGUOUS != 0 {
        return Err(SyscallError::INVALID_PARAMETER);
    }

    let id = if flags & lockjaw_types::syscall::ALLOC_FLAG_CONTIGUOUS != 0 {
        crate::cap::pageset_table::alloc_pages_contiguous(count)
    } else {
        crate::cap::pageset_table::alloc_pages(count)
    }.ok_or(SyscallError::OUT_OF_MEMORY)?;

    // Insert a PageSet handle into the caller's handle table.
    // The handle points to the header KVA so sys_export_handle can
    // transfer it to other processes.
    let (_, header_kva) = crate::cap::pageset_table::get_pageset(id)
        .ok_or(SyscallError::UNKNOWN)?;
    let ht = CurrentThread::handle_table();
    match ht.insert(
        Rights::from_bits(RIGHT_READ | RIGHT_WRITE),
        lockjaw_types::object::HandleKind::PageSet { kva: header_kva, mapped_va_page: 0 },
    ) {
        Ok(h) => {
            // Increment refcount — a new handle references this PageSet.
            unsafe { crate::cap::pageset_table::read_header_mut(header_kva).inc_refcount(); }
            Ok(h as u64)
        }
        Err(e) => {
            // Handle table full — free the pageset (global table slot,
            // data pages, and header range) to avoid leaking memory.
            crate::cap::pageset_table::free_by_header_kva(header_kva);
            Err(e)
        }
    }
}

/// sys_alloc_dma_pages(count) — allocate physically-contiguous DMA-safe pages.
/// x0 = number of pages to allocate (1..=MAX_PRACTICAL_PAGES_PER_SET).
/// Returns a PageSet handle in x1 with `origin = DmaPool`.
///
/// Pages come from the boot-reserved DMA pool, which is not registered
/// with buddy and (in a follow-up commit) is excluded from the kernel
/// direct map. The resulting PageSet can ONLY be mapped via
/// `MapMemoryAttribute::NormalNonCacheable`; cacheable mapping paths
/// reject DmaPool origin to prevent the mixed-attribute alias bug.
///
/// The phys address of the first data page is queryable via
/// `sys_query_pageset_phys(handle, 0)` — needed by drivers programming
/// ADMA descriptor targets.
fn sys_alloc_dma_pages(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let count = ctx.gpr[0] as usize;
    let id = crate::cap::pageset_table::alloc_dma_pages(count)
        .ok_or(SyscallError::OUT_OF_MEMORY)?;
    let (_, header_kva) = crate::cap::pageset_table::get_pageset(id)
        .ok_or(SyscallError::UNKNOWN)?;
    let ht = CurrentThread::handle_table();
    match ht.insert(
        Rights::from_bits(RIGHT_READ | RIGHT_WRITE),
        lockjaw_types::object::HandleKind::PageSet { kva: header_kva, mapped_va_page: 0 },
    ) {
        Ok(h) => {
            unsafe { crate::cap::pageset_table::read_header_mut(header_kva).inc_refcount(); }
            Ok(h as u64)
        }
        Err(e) => {
            crate::cap::pageset_table::free_by_header_kva(header_kva);
            Err(e)
        }
    }
}

/// sys_map_pages(handle, virt_addr, attr) — map pages into the caller's address space.
/// x0 = PageSet handle (from sys_alloc_pages, sys_alloc_dma_pages, or sys_register_device_page).
/// x1 = virtual address to map at (must be page-aligned, in user range).
/// x2 = `MapMemoryAttribute` discriminant (Normal=0, Device=1) — selects MAIR regime.
fn sys_map_pages(ctx: &mut ExceptionContext) -> SyscallError {
    let handle = ctx.gpr[0] as u32;
    let virt_addr = ctx.gpr[1];
    let attr = match lockjaw_types::vmem::MapMemoryAttribute::from_raw(ctx.gpr[2]) {
        Some(a) => a,
        None => return SyscallError::INVALID_PARAMETER,
    };

    // Reject VA 0 (mapped_va_page uses 0 as "not mapped" sentinel)
    // and unaligned VAs (would silently round down when stored as VA >> 12).
    if virt_addr == 0 || virt_addr & 0xFFF != 0 {
        return SyscallError::INVALID_PARAMETER;
    }

    let addr_space = match CurrentThread::address_space() {
        Some(a) => a,
        None => return SyscallError::INVALID_PARAMETER,
    };

    // Look up PageSet via handle table — enforces capability model.
    let ht = CurrentThread::handle_table();
    let entry = match ht.lookup(handle, Rights::from_bits(RIGHT_READ), ObjectType::PageSet) {
        Ok(e) => e,
        Err(e) => return e,
    };

    // Reject if this handle already has an active mapping.
    // One mapping per handle — alias mappings require duplicating the handle.
    match ht.get_mapped_va(handle) {
        Ok(va_page) if va_page != 0 => return SyscallError::INVALID_PARAMETER,
        Err(e) => return e,
        _ => {}
    }

    let header_kva = match entry.kind {
        lockjaw_types::object::HandleKind::PageSet { kva, .. } => kva,
        _ => return SyscallError::INVALID_HANDLE,
    };
    // SAFETY: kva from a PageSet handle — registered header.
    let header = unsafe { crate::cap::pageset_table::read_header_backed(header_kva) };

    // M6 attribute-vs-origin enforcement: the page-resident origin
    // field decides which mapping regimes are legal. Buddy pages can
    // be mapped Normal / Device; mapping them NormalNonCacheable would
    // create the mixed-attribute alias with the kernel direct map.
    // DmaPool pages can ONLY be mapped NormalNonCacheable; cacheable
    // mappings would reintroduce the alias from the other direction.
    // (See docs/ben_principles.md Tier 3 #13 — origin discriminants
    // start at 1 so a zero-init read is observably invalid.)
    use lockjaw_types::pageset_table::PageSetOrigin;
    use lockjaw_types::vmem::MapMemoryAttribute;
    let origin = match header.raw().origin() {
        Some(o) => o,
        None => return SyscallError::INVALID_HANDLE,
    };
    match (origin, attr) {
        (PageSetOrigin::Buddy,   MapMemoryAttribute::NormalNonCacheable) =>
            return SyscallError::INVALID_PARAMETER,
        (PageSetOrigin::DmaPool, MapMemoryAttribute::Normal) =>
            return SyscallError::INVALID_PARAMETER,
        (PageSetOrigin::DmaPool, MapMemoryAttribute::Device) =>
            return SyscallError::INVALID_PARAMETER,
        _ => {}
    }

    unsafe {
        match crate::arch::aarch64::vmem::map_pages_in_existing(addr_space.ttbr0(), virt_addr, &header, attr) {
            Ok(()) => {
                // Record the mapping on this handle and increment the
                // PageSet's global map count.
                if let Err(e) = ht.set_mapped_va(handle, (virt_addr >> 12) as u32) {
                    // Handle disappeared after mapping — shouldn't happen
                    // but if it does, the mapping is orphaned. Log and fail.
                    crate::kprintln!("WARNING: set_mapped_va failed after mapping");
                    return e;
                }
                crate::cap::pageset_table::read_header_mut(header_kva).inc_map_count();
                SyscallError::OK
            }
            Err(_) => SyscallError::UNKNOWN,
        }
    }
}

/// sys_create_process(mappings_ptr, mapping_count, entry_point, stack_handle, scratch_handle)
/// x0 = pointer to ProcessMapping array in caller's memory
/// x1 = number of mappings
/// x2 = entry point VA for the new process
/// x3 = PageSet handle for the stack page(s)
/// x4 = PageSet handle for a scratch page (kernel uses as Mapping buffer, caller keeps)
fn sys_create_process(ctx: &mut ExceptionContext) -> SyscallError {
    let mappings_va = ctx.gpr[0];
    let mapping_count = ctx.gpr[1] as usize;
    let entry_point = ctx.gpr[2];
    let stack_pageset_id = ctx.gpr[3];
    let scratch_pageset_id = ctx.gpr[4];
    let parent_handle_to_copy = ctx.gpr[5];
    let name_va = ctx.gpr[6];

    let addr_space = match CurrentThread::address_space() {
        Some(a) => a,
        None => return SyscallError::INVALID_PARAMETER,
    };

    // Read process name from user memory (16 bytes, NUL-padded)
    let name: [u8; 16] = addr_space.read(name_va).unwrap_or([0u8; 16]);

    match crate::process::create_process(&addr_space, mappings_va, mapping_count, entry_point, stack_pageset_id, scratch_pageset_id, parent_handle_to_copy, name) {
        Ok(()) => SyscallError::OK,
        Err(e) => e.to_syscall_error(),
    }
}

/// sys_create_notification(handle) — create a Notification from a donated page.
/// x0 = PageSet handle (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_notification(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    // Notification lives in the KVM pool — use the _kvm orchestrator
    // so map_existing runs in the validate phase before consume_apply.
    create_kernel_object_kvm(
        ctx.gpr[0] as u32,
        |kva| lockjaw_types::object::HandleKind::Notification { kva },
        crate::ipc::notification::create_notification,
    )
}

/// sys_signal_notification(handle, value) — signal a notification.
/// x0 = notification handle, x1 = new timeline value (must be > current).
/// Wakes any thread waiting with threshold <= value.
fn sys_signal_notification(ctx: &mut ExceptionContext) -> SyscallError {
    let handle = ctx.gpr[0] as u32;
    let new_value = ctx.gpr[1];
    match crate::cap::object_ops::signal_notification(handle, new_value) {
        Ok(Ok(())) => SyscallError::OK,
        Ok(Err(lockjaw_types::notification_state::NotificationError::ValueNotMonotonic)) => SyscallError::NOT_MONOTONIC,
        Ok(Err(_)) => SyscallError::UNKNOWN,
        Err(e) => e,
    }
}

/// sys_wait_notification(handle, threshold) — wait on a notification.
/// x0 = notification handle, x1 = threshold value to wait for.
/// Returns the current counter value in x1 when counter >= threshold.
/// Blocks if counter < threshold.
fn sys_wait_notification(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let handle = ctx.gpr[0] as u32;
    let threshold = ctx.gpr[1];
    match crate::cap::object_ops::wait_notification(handle, threshold) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(lockjaw_types::notification_state::NotificationError::AlreadyHasWaiter)) => Err(SyscallError::ALREADY_WAITING),
        Ok(Err(_)) => Err(SyscallError::UNKNOWN),
        Err(e) => Err(e),
    }
}

/// sys_bind_irq(intid, notification_handle, flags) — bind a hardware IRQ to a notification.
/// x0 = hardware INTID, x1 = notification handle, x2 = flags.
/// flags bit 0: 1 = edge-triggered, 0 = level-triggered (default).
/// When the IRQ fires, the kernel signals the notification (increments timeline by 1).
fn sys_bind_irq(ctx: &mut ExceptionContext) -> SyscallError {
    let intid = ctx.gpr[0] as u32;
    let notif_handle = ctx.gpr[1] as u32;
    let flags = ctx.gpr[2];
    let edge_triggered = flags & 1 != 0;

    // bind_irq needs the notification paddr for the arch/ binding layer,
    // not a mutable reference — use the existing typed lookup.
    let ht = CurrentThread::handle_table();
    let entry = match ht.lookup(notif_handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), crate::cap::object::ObjectType::Notification) {
        Ok(e) => e,
        Err(e) => return e,
    };
    let notif_kva = match entry.kind {
        lockjaw_types::object::HandleKind::Notification { kva } => kva,
        _ => return SyscallError::INVALID_HANDLE,
    };
    if crate::arch::aarch64::irq_bind::bind(intid, notif_kva) {
        // Enable SPI in the GIC distributor (PPIs are already enabled in gic::init)
        if intid >= 32 {
            // SAFETY: intid validated by irq_bind::bind; enable_spi is a GIC
            // MMIO write that is safe to execute for any valid SPI.
            unsafe { crate::arch::aarch64::gic::enable_spi(intid, edge_triggered) };
        }
        SyscallError::OK
    } else {
        SyscallError::INVALID_PARAMETER
    }
}

/// sys_create_endpoint(handle) — create an Endpoint from a donated page.
/// x0 = PageSet handle (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_endpoint(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    // Endpoint lives in the KVM pool — use the _kvm orchestrator so
    // map_existing runs in the validate phase before consume_apply.
    create_kernel_object_kvm(
        ctx.gpr[0] as u32,
        // Creator's handle is the master (no minted identity yet).
        // Send/call on caller_token=None is rejected; only sender
        // handles minted via sys_export_handle / create_process can
        // initiate IPC. The creator can receive on this handle.
        |kva| lockjaw_types::object::HandleKind::Endpoint { kva, caller_token: None },
        endpoint::create_endpoint,
    )
}

/// sys_create_reply(handle) — create a Reply object from a donated page.
/// x0 = PageSet handle (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_reply(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    // Reply lives in the KVM pool — use the _kvm orchestrator so
    // map_existing runs in the validate phase (before consume_apply).
    // The factory just packages the already-mapped KVA into the
    // HandleKind variant.
    create_kernel_object_kvm(
        ctx.gpr[0] as u32,
        |kva| lockjaw_types::object::HandleKind::Reply { kva },
        crate::ipc::reply::create_reply,
    )
}

/// sys_recv_nb(handle) — non-blocking receive on an endpoint.
/// x0 = endpoint handle. On success: x0=0, x1-x4 = message words.
/// Returns SyscallError::WOULD_BLOCK if no sender is waiting.
fn sys_recv_nb(ctx: &mut ExceptionContext) -> SyscallReturn {
    let handle = ctx.gpr[0] as u32;
    match crate::cap::object_ops::recv_nb(handle) {
        Ok(Ok(msg)) => {
            ctx.gpr[1] = msg[0];
            ctx.gpr[2] = msg[1];
            ctx.gpr[3] = msg[2];
            ctx.gpr[4] = msg[3];
            SyscallReturn::Message(SyscallError::OK)
        }
        Ok(Err(endpoint::IpcError::WouldBlock)) => SyscallReturn::Message(SyscallError::WOULD_BLOCK),
        Ok(Err(_)) => SyscallReturn::Message(SyscallError::UNKNOWN),
        Err(e) => SyscallReturn::Message(e),
    }
}

/// sys_wait_any(entries_ptr, count, deadline) — wait until any of N
/// objects is ready or `deadline` (absolute monotonic CNTVCT_EL0
/// ticks) expires, whichever comes first.
///
/// x0 = pointer to WaitEntry array in caller memory.
/// x1 = count (0..=MAX_WAIT_OBJECTS).
/// x2 = absolute monotonic deadline; `MonoTicks::NO_DEADLINE`
///      (= u64::MAX) means "no timeout".
///
/// Returns: standard 2-register syscall convention — x0 = SyscallError
/// code (0 on success), x1 = bitmask. Bit N set = entry N became ready;
/// mask == 0 = deadline expired before any object fired (timeout
/// encoding — see docs/syscalls.md S6 for the eternal-truth caveat).
///
/// Two roles deliberately collapse onto this one syscall:
/// - `count > 0`: wait on objects, optionally with a timeout.
/// - `count == 0`: pure sleep — no objects, only the deadline can
///   wake you. Used by lockjaw-userlib `sleep_until` / `sleep_for`.
fn sys_wait_any(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    use lockjaw_types::wait::{WaitEntry, MAX_WAIT_OBJECTS, validate_wait_count};
    use lockjaw_types::time::MonoTicks;
    use crate::ipc::notification;

    let entries_va = ctx.gpr[0];
    let count = ctx.gpr[1] as usize;
    let deadline = MonoTicks(ctx.gpr[2]);

    if !validate_wait_count(count) {
        return Err(SyscallError::INVALID_PARAMETER);
    }

    // Pure-sleep form (count == 0) doesn't read a WaitEntry array, so
    // it doesn't need an address space lookup. Only require it when
    // count > 0; this also leaves the door open to kernel threads
    // pure-sleeping (none currently do).
    let addr_space = if count > 0 {
        match CurrentThread::address_space() {
            Some(a) => Some(a),
            None => return Err(SyscallError::INVALID_PARAMETER),
        }
    } else {
        None
    };

    unsafe {
        let tcb_kva = CurrentThread::tcb_kva();

        // Read WaitEntry array from user memory via page table walk (TTBR1).
        // Never touches TTBR0 — immune to context switches.
        //
        // `addrs` is a polymorphic u64 storage: an Endpoint entry holds
        // a `PhysAddr.as_u64()`; a Notification entry (now in KVM) holds
        // a `KernelVa.as_u64()`. The matching `types[i]` says which
        // regime to use when decoding. TCB's `wait_objects` field is
        // already u64-typed for exactly this reason.
        let mut addrs = [0u64; MAX_WAIT_OBJECTS];
        let mut types = [ObjectType::HandleTable; MAX_WAIT_OBJECTS];
        let mut thresholds = [0u64; MAX_WAIT_OBJECTS];

        if count > 0 {
            let addr_space = addr_space.as_ref().unwrap(); // checked above
            let ht = CurrentThread::handle_table();
            for i in 0..count {
                let user_va = entries_va + (i as u64) * core::mem::size_of::<WaitEntry>() as u64;
                let entry: WaitEntry = match addr_space.read(user_va) {
                    Some(e) => e,
                    None => return Err(SyscallError::INVALID_PARAMETER),
                };
                let he = ht.lookup_any(
                    entry.handle as u32,
                    Rights::from_bits(crate::cap::rights::RIGHT_READ),
                )?;
                let (obj_type, addr) = match he.kind {
                    lockjaw_types::object::HandleKind::Endpoint { kva, .. } =>
                        (ObjectType::Endpoint, kva.as_u64()),
                    lockjaw_types::object::HandleKind::Notification { kva } =>
                        (ObjectType::Notification, kva.as_u64()),
                    _ => return Err(SyscallError::INVALID_PARAMETER),
                };
                addrs[i] = addr;
                types[i] = obj_type;
                thresholds[i] = entry.threshold;
            }

            // Fast path: object already ready
            let mask = check_readiness(&addrs, &types, &thresholds, count);
            if mask != 0 {
                return Ok(mask);
            }
        }

        // Deadline-already-past fast path. Applies to both:
        //   - count > 0: the timeout already fired before we got here.
        //   - count == 0: sleep_until called with a past deadline.
        // Returns the timeout-encoding mask (0) without registering
        // waiters or blocking — symmetric with the count > 0 case.
        if !deadline.is_no_deadline() {
            if deadline.has_expired(crate::arch::aarch64::timer::kernel_now()) {
                return Ok(0);
            }
        }

        // Slow path: register as readiness waiter on each object, then block.
        // For count == 0 this loop is a no-op; only the deadline can wake us.
        // Both Endpoint and Notification live in the KVM pool now —
        // decode the polymorphic u64 as KernelVa for both.
        for i in 0..count {
            let kva = lockjaw_types::addr::KernelVa::new(addrs[i]);
            match types[i] {
                ObjectType::Endpoint =>
                    endpoint::set_readiness_waiter(kva, tcb_kva),
                ObjectType::Notification =>
                    notification::set_readiness_waiter(kva, tcb_kva, thresholds[i]),
                _ => {}
            }
        }

        // Store wait state in TCB for post-wake cleanup. For count == 0
        // this records an empty wait set so the post-wake unregister
        // loop is also a no-op.
        {
            let type_bytes: [u8; MAX_WAIT_OBJECTS] = core::array::from_fn(|i| {
                if i < count { types[i] as u8 } else { 0 }
            });
            CurrentThread::store_wait_state(&addrs, &thresholds, &type_bytes, count);
        }
        // Store deadline so the per-tick scan in handle_tick can find us.
        // NO_DEADLINE means the scan will skip this TCB — only IPC
        // unblock can wake us (current behavior for count > 0 callers
        // that don't pass a deadline).
        CurrentThread::set_wait_deadline(deadline);

        scheduler::block_current(scheduler::BlockToken::new());

        // Woke up — could be readiness fired, deadline expired, or both.
        // Clear the deadline so a stale value can't accidentally apply
        // to a future sys_wait_any (the per-tick scan also clears it
        // when it does the unblocking, but be defensive).
        CurrentThread::set_wait_deadline(MonoTicks::NO_DEADLINE);

        let wc = CurrentThread::wait_count();
        for i in 0..wc {
            let (a, type_tag) = CurrentThread::wait_entry(i);
            let kva = lockjaw_types::addr::KernelVa::new(a);
            match obj_type_from_u8(type_tag) {
                ObjectType::Endpoint =>
                    endpoint::clear_readiness_waiter(kva, tcb_kva),
                ObjectType::Notification =>
                    notification::clear_readiness_waiter(kva, tcb_kva),
                _ => {}
            }
        }
        CurrentThread::clear_wait_count();

        // Re-check all objects (others may have become ready while
        // blocked). For count == 0 this returns 0 — pure sleep always
        // returns the timeout-encoding mask, by construction.
        Ok(check_readiness(&addrs, &types, &thresholds, wc))
    }
}

/// Build ObjectReadiness snapshots from live objects and compute the ready bitmask.
/// The readiness logic is in lockjaw_types::wait::compute_ready_mask (tested on host).
///
/// `addrs` is polymorphic u64 storage: both Endpoint and Notification
/// entries decode as `KernelVa` (both kinds live in the KVM pool).
fn check_readiness(
    addrs: &[u64],
    types: &[ObjectType],
    thresholds: &[u64],
    count: usize,
) -> u64 {
    use lockjaw_types::wait::{ObjectReadiness, compute_ready_mask, MAX_WAIT_OBJECTS};
    use lockjaw_types::ipc_state::EpState;

    let mut objects = [ObjectReadiness::Endpoint(EpState::Idle); MAX_WAIT_OBJECTS];
    for i in 0..count {
        let kva = lockjaw_types::addr::KernelVa::new(addrs[i]);
        objects[i] = match types[i] {
            ObjectType::Endpoint => {
                let state = endpoint::read_state(kva);
                ObjectReadiness::Endpoint(state)
            }
            ObjectType::Notification => {
                let value = crate::ipc::notification::read_value(kva);
                ObjectReadiness::Notification { value, threshold: thresholds[i] }
            }
            _ => ObjectReadiness::Endpoint(EpState::Idle), // not waitable, never ready
        };
    }
    compute_ready_mask(&objects[..count])
}

/// sys_export_handle(handle_to_export) — duplicate a handle into the
/// currently-being-handled caller's handle table.
///
/// x0 = handle index in the exporter's table to duplicate.
/// Returns the new handle index (in the caller's table) in x1, or
/// NO_CALLER if the exporting thread has no bound call.
///
/// The caller is identified via the exporter's own TCB's
/// `current_reply_kva` → Reply object → `caller_tcb_kva`.
fn sys_export_handle(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let handle_to_export = ctx.gpr[0] as u32;

    unsafe {
        // Find the bound caller via our TCB's current_reply_kva.
        let reply_kva_u64 = CurrentThread::current_reply_kva();
        if reply_kva_u64 == 0 {
            return Err(SyscallError::NO_CALLER);
        }
        let reply = KernelRef::<crate::ipc::reply::ReplyObject>::from_kva(
            lockjaw_types::addr::KernelVa::new(reply_kva_u64),
        );
        let caller_tcb_kva_u64 = reply.get().caller_tcb_kva;
        if caller_tcb_kva_u64 == 0 {
            return Err(SyscallError::NO_CALLER);
        }

        // Look up the handle to export in the exporter's own table.
        // Type-agnostic: any object type can be exported.
        let exporter_ht = CurrentThread::handle_table();
        let export_entry = exporter_ht.lookup_any(handle_to_export, Rights::none())?;

        // For Endpoint handles: always mint a fresh caller token from
        // the endpoint's monotonic counter, regardless of whether the
        // exporter holds the master (caller_token=None) or a previously-
        // exported sender handle. The identity inheres in the handle,
        // not the lineage chain — every export is a distinct gift event,
        // so each recipient gets a distinct identity the server can
        // distinguish via sys_query_caller_token.
        //
        // See docs/book-of-lockjaw/02-handle-identity-tokens.md for the
        // requirement → implementation mapping.
        let export_kind = match export_entry.kind {
            lockjaw_types::object::HandleKind::Endpoint { kva, .. } => {
                let token = {
                    let mut ep = KernelMut::<crate::ipc::endpoint::EndpointObject>::from_kva(kva);
                    crate::ipc::endpoint::mint_caller_token(ep.get_mut())
                };
                lockjaw_types::object::HandleKind::Endpoint { kva, caller_token: Some(token) }
            }
            other => other, // non-Endpoint: pass through unchanged
        };

        // Insert into the caller's handle table (cross-table operation).
        // The export_kind already carries its typed address (PhysAddr or
        // KernelVa) inside the variant, so insert is the single path.
        let caller_tcb = KernelRef::<Tcb>::from_kva(lockjaw_types::addr::KernelVa::new(caller_tcb_kva_u64));
        let caller_ht_kva = crate::cap::process_obj::process_handle_table(
            lockjaw_types::addr::KernelVa::new(caller_tcb.get().process_kva),
        );
        let caller_ht = handle_table::HandleTableRef::from_kva(caller_ht_kva);
        let idx = caller_ht.insert(export_entry.rights, export_kind)?;
        // Increment refcount for PageSets — a new handle references it.
        if let lockjaw_types::object::HandleKind::PageSet { kva, .. } = export_kind {
            crate::cap::pageset_table::read_header_mut(kva).inc_refcount();
        }
        Ok(idx as u64)
    }
}

/// sys_get_boot_info() — returns boot information about the DTB.
///
/// Inserts a PageSet handle for the DTB pages into the caller's
/// handle table. Returns the handle index in x1 and the in-page
/// offset of the DTB header in x2 (zero on platforms whose firmware
/// page-aligned the DTB; nonzero on Pi 4B etc.). Userspace adds the
/// offset to the mapped VA before reading DTB bytes — see
/// `lockjaw_types::dtb_layout` for the layout model.
///
/// Uses `SyscallReturn::Message` so we can write both return slots;
/// the dispatch only writes x0 = error.
fn sys_get_boot_info(ctx: &mut ExceptionContext) -> SyscallReturn {
    let dtb_id = crate::dtb_pageset_id();
    let (_, header_kva) = match crate::cap::pageset_table::get_pageset(dtb_id) {
        Some(t) => t,
        None => return SyscallReturn::Void(SyscallError::UNKNOWN),
    };
    let ht = CurrentThread::handle_table();
    let h = match ht.insert(
        Rights::from_bits(RIGHT_READ | RIGHT_WRITE),
        lockjaw_types::object::HandleKind::PageSet { kva: header_kva, mapped_va_page: 0 },
    ) {
        Ok(h) => h as u64,
        Err(e) => return SyscallReturn::Void(e),
    };
    // Increment refcount — a new handle references this PageSet.
    unsafe { crate::cap::pageset_table::read_header_mut(header_kva).inc_refcount(); }
    ctx.gpr[1] = h;
    ctx.gpr[2] = crate::dtb_in_page_offset();
    SyscallReturn::Message(SyscallError::OK)
}

/// sys_register_device_page(phys_addr) — wrap a physical address as a tracked PageSet.
/// x0 = physical MMIO address (page-aligned).
/// Returns a PageSet handle. Used by the device manager to create MMIO
/// PageSets for drivers. Drivers then map via sys_map_pages with the handle.
fn sys_register_device_page(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let phys_addr = ctx.gpr[0];
    let id = crate::cap::pageset_table::register_device_page(phys_addr)
        .ok_or(SyscallError::OUT_OF_MEMORY)?;
    let (_, header_kva) = crate::cap::pageset_table::get_pageset(id)
        .ok_or(SyscallError::UNKNOWN)?;
    let ht = CurrentThread::handle_table();
    match ht.insert(
        Rights::from_bits(RIGHT_READ | RIGHT_WRITE),
        lockjaw_types::object::HandleKind::PageSet { kva: header_kva, mapped_va_page: 0 },
    ) {
        Ok(h) => {
            unsafe { crate::cap::pageset_table::read_header_mut(header_kva).inc_refcount(); }
            Ok(h as u64)
        }
        Err(e) => {
            // Handle table full — free the tracking entry + header range.
            // The MMIO page itself is not freed (it's device memory).
            crate::cap::pageset_table::free_header_page(header_kva);
            Err(e)
        }
    }
}

/// sys_query_pageset_phys(handle, page_index) — query a page's physical address.
/// x0 = PageSet handle, x1 = page index within the set.
/// Returns the physical address of that page. Used by drivers that need
/// to program DMA controllers or configure hardware with physical addresses.
fn sys_query_pageset_phys(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let handle = ctx.gpr[0] as u32;
    let page_index = ctx.gpr[1] as usize;

    let ht = CurrentThread::handle_table();
    let entry = ht.lookup(handle, Rights::from_bits(RIGHT_READ), ObjectType::PageSet)?;
    let header_kva = match entry.kind {
        lockjaw_types::object::HandleKind::PageSet { kva, .. } => kva,
        _ => return Err(SyscallError::INVALID_HANDLE),
    };
    // SAFETY: kva came from a PageSet handle — registered header.
    let backed = unsafe { crate::cap::pageset_table::read_header_backed(header_kva) };
    backed.get_page(page_index)
        .ok_or(SyscallError::INVALID_PARAMETER)
}

/// sys_create_thread(entry, stack_top, stack_base, arg) — create a new thread
/// in the calling process. The new thread shares the caller's address space
/// and handle table. Starts at `entry` with SP=stack_top and x0=arg.
///
/// x0 = user entry point VA, x1 = stack top VA (16-byte aligned),
/// x2 = stack base VA (< stack_top), x3 = argument (passed in x0).
///
/// Kernel policy: VA range is validated (must be in user range), but mapping
/// existence is NOT checked. A thread with unmapped entry/stack faults at EL0.
fn sys_create_thread(ctx: &mut ExceptionContext) -> SyscallError {
    let entry_point = ctx.gpr[0];
    let stack_top = ctx.gpr[1];
    let stack_base = ctx.gpr[2];
    let user_arg = ctx.gpr[3];

    // Validate VAs are in user range
    const USER_VA_END: u64 = lockjaw_types::constants::USER_VA_END;
    if entry_point >= USER_VA_END || stack_top >= USER_VA_END || stack_base >= USER_VA_END {
        return SyscallError::INVALID_PARAMETER;
    }
    if stack_base >= stack_top {
        return SyscallError::INVALID_PARAMETER;
    }
    if stack_top & 0xF != 0 {
        return SyscallError::INVALID_PARAMETER; // AArch64 ABI: SP must be 16-byte aligned
    }

    // Get caller's process (returns the KVA of its ProcessObject).
    let process_kva = crate::sched::current::CurrentThread::process_kva();

    // Allocate kernel stack + TCB page — both live in the KVM pool.
    let stack_range = match crate::mm::kvm::alloc_kernel_pages(1) {
        Ok(r) => r,
        Err(_) => return SyscallError::OUT_OF_MEMORY,
    };
    let stack_kva = stack_range.kva;
    let tcb_range = match crate::mm::kvm::alloc_kernel_pages(1) {
        Ok(r) => r,
        Err(_) => {
            // SAFETY: stack_range is the one we just allocated.
            unsafe { crate::mm::kvm::free_kernel_pages(stack_range); }
            return SyscallError::OUT_OF_MEMORY;
        }
    };
    let tcb_kva = tcb_range.kva;

    // Create TCB — reuses process_entry which reads TTBR0 from the
    // shared ProcessObject and drops to EL0.
    unsafe {
        if crate::sched::tcb::create_tcb(
            &crate::sched::tcb::TcbCreateInfo {
                entry: crate::process::process_entry,
                stack: lockjaw_types::thread::KernelStackBase::Pool(stack_kva),
                process_kva,
                user_entry_point: entry_point,
                user_stack_top: stack_top,
                user_stack_base: stack_base,
                user_arg,
                name: *b"thread\0\0\0\0\0\0\0\0\0\0",
            },
            tcb_kva,
        ).is_err() {
            crate::mm::kvm::free_kernel_pages(tcb_range);
            crate::mm::kvm::free_kernel_pages(stack_range);
            return SyscallError::UNKNOWN;
        }
    }

    // Increment process thread count
    crate::cap::process_obj::process_inc_thread_count(process_kva);

    // Register with scheduler
    if !scheduler::add_thread(tcb_kva) {
        // Rollback: dealloc pages, then dec thread count.
        // Invariant: caller is still alive, so dec cannot return LastThread.
        // SAFETY: tcb_range / stack_range are the ones we just allocated;
        // no live refs.
        unsafe {
            crate::mm::kvm::free_kernel_pages(tcb_range);
            crate::mm::kvm::free_kernel_pages(stack_range);
        }
        crate::cap::process_obj::process_dec_thread_count(process_kva);
        // Scheduler slot table exhausted — distinct from physical
        // OOM. Same fix as sys_create_process's SchedulerFull path.
        return SyscallError::QUEUE_FULL;
    }

    SyscallError::OK
}

/// sys_query_mapping(va) — query the mapping state at a user VA.
/// x0 = VA (page-aligned). Returns x0=0, x1=mapped (0/1),
/// x2=run_pages (consecutive pages with same state).
fn sys_query_mapping(ctx: &mut ExceptionContext) -> SyscallReturn {
    let va = ctx.gpr[0];
    if va >= lockjaw_types::constants::USER_VA_END || va & 0xFFF != 0 {
        return SyscallReturn::Value(Err(SyscallError::INVALID_PARAMETER));
    }
    let ttbr0 = CurrentThread::ttbr0();
    let (mapped, run_pages) = unsafe {
        crate::arch::aarch64::vmem::query_mapping_run(ttbr0, va)
    };
    ctx.gpr[1] = if mapped { 1 } else { 0 };
    ctx.gpr[2] = run_pages as u64;
    SyscallReturn::Message(SyscallError::OK)
}

/// sys_close_handle(handle) — remove a handle from the caller's table.
/// x0 = handle index to close.
/// Reclaims the handle slot for reuse. Does NOT free the backing kernel
/// object or its pages — without refcounting, mapping tracking, or
/// revocation, freeing would be use-after-free if other handles or
/// mappings to the same object exist.
/// sys_unmap_pages(handle, va) — unmap a PageSet from the caller's address space.
/// x0 = PageSet handle (must have been mapped via sys_map_pages).
/// x1 = VA (must match the VA used in sys_map_pages).
/// Validates that every L3 PTE maps to the expected physical page from
/// the PageSet before clearing. L3 only — rejects L2 block mappings.
fn sys_unmap_pages(ctx: &mut ExceptionContext) -> SyscallError {
    let handle = ctx.gpr[0] as u32;
    let va = ctx.gpr[1];

    let addr_space = match CurrentThread::address_space() {
        Some(a) => a,
        None => return SyscallError::INVALID_PARAMETER,
    };

    let ht = CurrentThread::handle_table();
    let entry = match ht.lookup(handle, Rights::from_bits(RIGHT_READ), ObjectType::PageSet) {
        Ok(e) => e,
        Err(e) => return e,
    };

    // Verify this handle has an active mapping at the given VA.
    let mapped_va_page = match ht.get_mapped_va(handle) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if mapped_va_page == 0 {
        return SyscallError::INVALID_PARAMETER; // not mapped
    }
    if va != (mapped_va_page as u64) << 12 {
        return SyscallError::INVALID_PARAMETER; // VA doesn't match
    }

    // Read the PageSet header to get expected physical pages.
    // Pass the header's page array directly — no stack copy.
    let header_kva = match entry.kind {
        lockjaw_types::object::HandleKind::PageSet { kva, .. } => kva,
        _ => return SyscallError::INVALID_HANDLE,
    };
    // SAFETY: kva came from a PageSet handle — registered header.
    let backed = unsafe { crate::cap::pageset_table::read_header_backed(header_kva) };
    let expected = backed.pages_slice();

    // Validate PTEs and clear them. TLB flushed inside.
    if unsafe {
        crate::arch::aarch64::vmem::unmap_validated(
            addr_space.ttbr0(), va, expected,
        )
    }.is_err() {
        return SyscallError::INVALID_PARAMETER;
    }

    // Clear the mapping record on this handle.
    let _ = ht.set_mapped_va(handle, 0);

    // Decrement the PageSet's map count. If both map_count and
    // refcount reach zero, no handles or mappings remain — free it.
    let should_free = unsafe {
        crate::cap::pageset_table::read_header_mut(header_kva).dec_map_count()
    };
    if should_free {
        crate::cap::pageset_table::free_by_header_kva(header_kva);
    }

    SyscallError::OK
}

fn sys_close_handle(ctx: &mut ExceptionContext) -> SyscallError {
    use lockjaw_types::object::{CloseHandleResult, decide_close_handle};

    let handle = ctx.gpr[0] as u32;
    let ht = CurrentThread::handle_table();

    // Single decision — determines the entire close protocol.
    let entry = ht.lookup_any(handle, Rights::from_bits(0)).ok();
    match decide_close_handle(entry.as_ref()) {
        CloseHandleResult::InvalidHandle => SyscallError::INVALID_HANDLE,

        CloseHandleResult::RemoveOnly => {
            match ht.remove(handle) {
                Ok(_) => SyscallError::OK,
                Err(e) => e,
            }
        }

        CloseHandleResult::RemoveAndDecRef { header_kva } => {
            match ht.remove(handle) {
                Ok(_) => {
                    crate::cap::pageset_table::dec_refcount_and_maybe_free(header_kva);
                    SyscallError::OK
                }
                Err(e) => e,
            }
        }

        CloseHandleResult::UnmapThenRemove { header_kva, mapped_va_page } => {
            // Unmap PTEs first — fallible. If unmap fails, reject close.
            // SAFETY: header_kva came from a PageSet handle slot —
            // registered header.
            let backed = unsafe {
                crate::cap::pageset_table::read_header_backed(header_kva)
            };
            let pages = backed.pages_slice();
            let va = (mapped_va_page as u64) << 12;
            if let Some(addr_space) = CurrentThread::address_space() {
                if unsafe {
                    crate::arch::aarch64::vmem::unmap_validated(
                        addr_space.ttbr0(), va, pages,
                    )
                }.is_err() {
                    return SyscallError::INVALID_PARAMETER;
                }
            }
            // Unmap succeeded. Remove + dec both counters.
            match ht.remove(handle) {
                Ok(_) => {
                    crate::cap::pageset_table::dec_both_and_maybe_free(header_kva);
                    SyscallError::OK
                }
                Err(e) => e,
            }
        }
    }
}

fn obj_type_from_u8(v: u8) -> ObjectType {
    match v {
        2 => ObjectType::Endpoint,
        3 => ObjectType::Notification,
        _ => ObjectType::HandleTable,
    }
}

/// sys_query_caller_token() — returns the caller token of the most
/// recently dequeued sender/caller on this thread. Set on every
/// successful sys_receive or sys_recv_nb. Returns 0 if this thread
/// has never received.
fn sys_query_caller_token() -> u64 {
    let tcb_kva = scheduler::current_tcb_kva();
    // SAFETY: current_tcb_kva is always valid.
    let tcb = unsafe { KernelRef::<Tcb>::from_kva(tcb_kva) };
    tcb.get().last_caller_token
}
