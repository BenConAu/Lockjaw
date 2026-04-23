use crate::arch::aarch64::exceptions::ExceptionContext;
use crate::arch::aarch64::uart::Uart;
use crate::cap::handle_table;
use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::ipc::endpoint;
use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::KernelRef;
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
        SYS_DEBUG_PUTC => SyscallReturn::Void(sys_debug_putc(ctx.gpr[0])),
        SYS_YIELD => SyscallReturn::Void(sys_yield()),
        SYS_SEND => SyscallReturn::Void(sys_send(ctx)),
        SYS_RECEIVE => sys_receive(ctx),
        SYS_CALL => sys_call(ctx),
        SYS_REPLY => SyscallReturn::Void(sys_reply(ctx)),
        SYS_ALLOC_PAGES => SyscallReturn::Value(sys_alloc_pages(ctx)),
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
        SYS_GET_BOOT_INFO => SyscallReturn::Value(Ok(sys_get_boot_info())),
        SYS_REGISTER_DEVICE_PAGE => SyscallReturn::Value(sys_register_device_page(ctx)),
        SYS_QUERY_PAGESET_PHYS => SyscallReturn::Value(sys_query_pageset_phys(ctx)),
        SYS_CREATE_REPLY => SyscallReturn::Value(sys_create_reply(ctx)),
        SYS_CREATE_THREAD => SyscallReturn::Void(sys_create_thread(ctx)),
        SYS_QUERY_MAPPING => sys_query_mapping(ctx),
        SYS_EXIT => {
            scheduler::exit_current(); // never returns
        }
        _ => {
            crate::kprintln!("Unknown syscall {}", syscall_num);
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

/// Common logic for sys_create_notification, sys_create_endpoint, etc.
/// Validates the PageSet is 1 page, calls the safe init function, then
/// consumes the PageSet and inserts a handle into the caller's table.
fn create_kernel_object(
    pageset_id: u64,
    obj_type: ObjectType,
    init_fn: fn(crate::mm::addr::ObjectInitPage) -> Result<(), crate::cap::object::CreateError>,
) -> Result<u64, SyscallError> {
    let page = crate::cap::pageset_table::donate_single_page(pageset_id)?;
    let paddr = page.paddr();
    if init_fn(page).is_err() {
        return Err(SyscallError::UNKNOWN);
    }
    // Consume only after successful initialization — preserves rollback
    // semantics if init_fn ever fails.
    crate::cap::pageset_table::consume_pageset(pageset_id);
    let ht = CurrentThread::handle_table();
    ht.insert(paddr, obj_type, Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE))
        .map(|h| h as u64)
}

fn sys_debug_putc(char_val: u64) -> SyscallError {
    let uart = Uart::new();
    uart.putc(char_val as u8);
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
/// Returns a PageSet ID in x1 on success.
fn sys_alloc_pages(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let count = ctx.gpr[0] as usize;
    let flags = ctx.gpr[1];

    if flags & !lockjaw_types::syscall::ALLOC_FLAG_CONTIGUOUS != 0 {
        return Err(SyscallError::INVALID_PARAMETER);
    }

    let result = if flags & lockjaw_types::syscall::ALLOC_FLAG_CONTIGUOUS != 0 {
        crate::cap::pageset_table::alloc_pages_contiguous(count)
    } else {
        crate::cap::pageset_table::alloc_pages(count)
    };
    match result {
        Some(id) => Ok(id),
        None => Err(SyscallError::OUT_OF_MEMORY),
    }
}

/// sys_map_pages(pageset_id, virt_addr, flags) — map pages into the caller's address space.
/// x0 = PageSet ID (from sys_alloc_pages or sys_register_device_page).
/// x1 = virtual address to map at (must be page-aligned, in user range).
/// x2 = flags (MAP_FLAG_DEVICE for MMIO memory attributes).
fn sys_map_pages(ctx: &mut ExceptionContext) -> SyscallError {
    let pageset_id = ctx.gpr[0];
    let virt_addr = ctx.gpr[1];
    let flags = ctx.gpr[2];

    let addr_space = match CurrentThread::address_space() {
        Some(a) => a,
        None => return SyscallError::INVALID_PARAMETER,
    };

    // All mappings go through PageSets — no raw physical addresses accepted.
    let pageset = match crate::cap::pageset_table::PageSetRef::from_id(pageset_id) {
        Some(ps) => ps,
        None => return SyscallError::INVALID_HANDLE,
    };
    unsafe {
        match crate::arch::aarch64::vmem::map_pages_in_existing(addr_space.ttbr0(), virt_addr, pageset.header(), flags) {
            Ok(()) => SyscallError::OK,
            Err(_) => SyscallError::UNKNOWN,
        }
    }
}

/// sys_create_process(mappings_ptr, mapping_count, entry_point, stack_pageset_id, scratch_pageset_id)
/// x0 = pointer to ProcessMapping array in caller's memory
/// x1 = number of mappings
/// x2 = entry point VA for the new process
/// x3 = PageSet ID for the stack page
/// x4 = PageSet ID for a scratch page (kernel uses as Mapping buffer, caller keeps)
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
        Err(_) => SyscallError::UNKNOWN,
    }
}

/// sys_create_notification(pageset_id) — create a Notification from a donated page.
/// x0 = PageSet ID (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_notification(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    create_kernel_object(ctx.gpr[0], ObjectType::Notification, crate::ipc::notification::create_notification)
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

/// sys_bind_irq(intid, notification_handle) — bind a hardware IRQ to a notification.
/// x0 = hardware INTID, x1 = notification handle.
/// When the IRQ fires, the kernel signals the notification (increments timeline by 1).
fn sys_bind_irq(ctx: &mut ExceptionContext) -> SyscallError {
    let intid = ctx.gpr[0] as u32;
    let notif_handle = ctx.gpr[1] as u32;

    // bind_irq needs the notification paddr for the arch/ binding layer,
    // not a mutable reference — use the existing typed lookup.
    let ht = CurrentThread::handle_table();
    let entry = match ht.lookup(notif_handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), crate::cap::object::ObjectType::Notification) {
        Ok(e) => e,
        Err(e) => return e,
    };
    let notif_paddr = PhysAddr::new(entry.object_paddr);
    if crate::arch::aarch64::irq_bind::bind(intid, notif_paddr) {
        // Enable SPI in the GIC distributor (PPIs are already enabled in gic::init)
        if intid >= 32 {
            // SAFETY: intid validated by irq_bind::bind; enable_spi is a GIC
            // MMIO write that is safe to execute for any valid SPI.
            unsafe { crate::arch::aarch64::gic::enable_spi(intid) };
        }
        SyscallError::OK
    } else {
        SyscallError::INVALID_PARAMETER
    }
}

/// sys_create_endpoint(pageset_id) — create an Endpoint from a donated page.
/// x0 = PageSet ID (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_endpoint(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    create_kernel_object(ctx.gpr[0], ObjectType::Endpoint, endpoint::create_endpoint)
}

/// sys_create_reply(pageset_id) — create a Reply object from a donated page.
/// x0 = PageSet ID (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_reply(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    create_kernel_object(ctx.gpr[0], ObjectType::Reply, crate::ipc::reply::create_reply)
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

/// sys_wait_any(entries_ptr, count) — wait until any of N objects is ready.
/// x0 = pointer to WaitEntry array in caller memory.
/// x1 = count (1..MAX_WAIT_OBJECTS).
/// Returns bitmask of ready objects in x1 (bit N = entry N is ready).
fn sys_wait_any(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    use lockjaw_types::wait::{WaitEntry, MAX_WAIT_OBJECTS, validate_wait_count};
    use crate::ipc::notification;

    let entries_va = ctx.gpr[0];
    let count = ctx.gpr[1] as usize;

    if !validate_wait_count(count) {
        return Err(SyscallError::INVALID_PARAMETER);
    }

    let addr_space = match CurrentThread::address_space() {
        Some(a) => a,
        None => return Err(SyscallError::INVALID_PARAMETER),
    };

    unsafe {
        let tcb_paddr = CurrentThread::tcb_paddr();
        let ht = CurrentThread::handle_table();

        // Read WaitEntry array from user memory via page table walk (TTBR1).
        // Never touches TTBR0 — immune to context switches.
        let mut paddrs = [PhysAddr::new(0); MAX_WAIT_OBJECTS];
        let mut types = [ObjectType::HandleTable; MAX_WAIT_OBJECTS];
        let mut thresholds = [0u64; MAX_WAIT_OBJECTS];

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
            if he.obj_type != ObjectType::Endpoint && he.obj_type != ObjectType::Notification {
                return Err(SyscallError::INVALID_PARAMETER);
            }
            paddrs[i] = PhysAddr::new(he.object_paddr);
            types[i] = he.obj_type;
            thresholds[i] = entry.threshold;
        }

        // Fast path: check if any object is already ready
        let mask = check_readiness(&paddrs, &types, &thresholds, count);
        if mask != 0 {
            return Ok(mask);
        }

        // Slow path: register as readiness waiter on each object, then block
        for i in 0..count {
            match types[i] {
                ObjectType::Endpoint => endpoint::set_readiness_waiter(paddrs[i], tcb_paddr),
                ObjectType::Notification => notification::set_readiness_waiter(paddrs[i], tcb_paddr, thresholds[i]),
                _ => {}
            }
        }

        // Store wait state in TCB for post-wake cleanup
        {
            let type_bytes: [u8; MAX_WAIT_OBJECTS] = core::array::from_fn(|i| {
                if i < count { types[i] as u8 } else { 0 }
            });
            CurrentThread::store_wait_state(&paddrs, &thresholds, &type_bytes, count);
        }

        scheduler::block_current(scheduler::BlockToken::new());

        // Woke up — unregister from all objects (only clear our own registration)
        let wc = CurrentThread::wait_count();
        for i in 0..wc {
            let (p, type_tag) = CurrentThread::wait_entry(i);
            match obj_type_from_u8(type_tag) {
                ObjectType::Endpoint => endpoint::clear_readiness_waiter(p, tcb_paddr),
                ObjectType::Notification => notification::clear_readiness_waiter(p, tcb_paddr),
                _ => {}
            }
        }
        CurrentThread::clear_wait_count();

        // Re-check all objects (others may have become ready while blocked)
        Ok(check_readiness(&paddrs, &types, &thresholds, wc))
    }
}

/// Build ObjectReadiness snapshots from live objects and compute the ready bitmask.
/// The readiness logic is in lockjaw_types::wait::compute_ready_mask (tested on host).
fn check_readiness(
    paddrs: &[PhysAddr],
    types: &[ObjectType],
    thresholds: &[u64],
    count: usize,
) -> u64 {
    use lockjaw_types::wait::{ObjectReadiness, compute_ready_mask, MAX_WAIT_OBJECTS};
    use lockjaw_types::ipc_state::EpState;

    let mut objects = [ObjectReadiness::Endpoint(EpState::Idle); MAX_WAIT_OBJECTS];
    for i in 0..count {
        objects[i] = match types[i] {
            ObjectType::Endpoint => {
                let state = endpoint::read_state(paddrs[i]);
                ObjectReadiness::Endpoint(state)
            }
            ObjectType::Notification => {
                let value = crate::ipc::notification::read_value(paddrs[i]);
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
/// `current_reply_paddr` → Reply object → `caller_tcb_paddr`.
fn sys_export_handle(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let handle_to_export = ctx.gpr[0] as u32;

    unsafe {
        // Find the bound caller via our TCB's current_reply_paddr.
        let reply_paddr_u64 = CurrentThread::current_reply_paddr();
        if reply_paddr_u64 == 0 {
            return Err(SyscallError::NO_CALLER);
        }
        let reply = KernelRef::<crate::ipc::reply::ReplyObject>::from_paddr(
            PhysAddr::new(reply_paddr_u64),
        );
        let caller_tcb_paddr_u64 = reply.get().caller_tcb_paddr;
        if caller_tcb_paddr_u64 == 0 {
            return Err(SyscallError::NO_CALLER);
        }

        // Look up the handle to export in the exporter's own table.
        // Type-agnostic: any object type can be exported.
        let exporter_ht = CurrentThread::handle_table();
        let export_entry = exporter_ht.lookup_any(handle_to_export, Rights::none())?;

        // Insert into the caller's handle table (cross-table operation).
        let caller_tcb = KernelRef::<Tcb>::from_paddr(PhysAddr::new(caller_tcb_paddr_u64));
        let caller_ht_paddr = crate::cap::process_obj::process_handle_table(PhysAddr::new(caller_tcb.get().process_paddr));
        let caller_ht = handle_table::HandleTableRef::from_paddr(caller_ht_paddr);
        caller_ht.insert(
            PhysAddr::new(export_entry.object_paddr),
            export_entry.obj_type,
            export_entry.rights,
        ).map(|idx| idx as u64)
    }
}

/// sys_get_boot_info() — returns boot information.
/// DTB PageSet ID returned in x1.
fn sys_get_boot_info() -> u64 {
    crate::dtb_pageset_id()
}

/// sys_register_device_page(phys_addr) — wrap a physical address as a tracked PageSet.
/// x0 = physical MMIO address (page-aligned).
/// Returns the PageSet ID. Used by the device manager to create MMIO PageSets
/// for drivers. Drivers then map via sys_map_pages with the PageSet ID.
fn sys_register_device_page(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let phys_addr = ctx.gpr[0];
    match crate::cap::pageset_table::register_device_page(phys_addr) {
        Some(id) => Ok(id),
        None => Err(SyscallError::OUT_OF_MEMORY),
    }
}

/// sys_query_pageset_phys(pageset_id, page_index) — query a page's physical address.
/// x0 = PageSet ID, x1 = page index within the set.
/// Returns the physical address of that page. Used by drivers that need
/// to program DMA controllers or configure hardware with physical addresses.
fn sys_query_pageset_phys(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let pageset_id = ctx.gpr[0];
    let page_index = ctx.gpr[1] as usize;

    let pageset = crate::cap::pageset_table::PageSetRef::from_id(pageset_id)
        .ok_or(SyscallError::INVALID_HANDLE)?;
    pageset.page(page_index)
        .map(|p| p.as_u64())
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

    // Get caller's process (returns PhysAddr)
    let process_paddr = crate::sched::current::CurrentThread::process_paddr();

    // Allocate kernel stack + TCB pages
    let kernel_stack = match crate::mm::page_alloc::alloc_page() {
        Some(p) => p,
        None => return SyscallError::OUT_OF_MEMORY,
    };
    let tcb_page = match crate::mm::page_alloc::alloc_page() {
        Some(p) => p,
        None => {
            crate::mm::page_alloc::dealloc_page(kernel_stack);
            return SyscallError::OUT_OF_MEMORY;
        }
    };

    // Create TCB — reuses process_entry which reads TTBR0 from the
    // shared ProcessObject and drops to EL0.
    unsafe {
        if crate::sched::tcb::create_tcb(
            &crate::sched::tcb::TcbCreateInfo {
                entry: crate::process::process_entry,
                stack_paddr: kernel_stack.start_addr(),
                process_paddr,
                user_entry_point: entry_point,
                user_stack_top: stack_top,
                user_stack_base: stack_base,
                user_arg,
                name: *b"thread\0\0\0\0\0\0\0\0\0\0",
            },
            tcb_page.start_addr(),
        ).is_err() {
            crate::mm::page_alloc::dealloc_page(tcb_page);
            crate::mm::page_alloc::dealloc_page(kernel_stack);
            return SyscallError::UNKNOWN;
        }
    }

    // Increment process thread count
    crate::cap::process_obj::process_inc_thread_count(process_paddr);

    // Register with scheduler
    if !scheduler::add_thread(tcb_page.start_addr()) {
        // Rollback: dealloc pages, then dec thread count.
        // Invariant: caller is still alive, so dec cannot return LastThread.
        crate::mm::page_alloc::dealloc_page(tcb_page);
        crate::mm::page_alloc::dealloc_page(kernel_stack);
        crate::cap::process_obj::process_dec_thread_count(process_paddr);
        return SyscallError::OUT_OF_MEMORY;
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

fn obj_type_from_u8(v: u8) -> ObjectType {
    match v {
        2 => ObjectType::Endpoint,
        3 => ObjectType::Notification,
        _ => ObjectType::HandleTable,
    }
}
