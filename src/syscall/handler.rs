use crate::arch::aarch64::exceptions::ExceptionContext;
use crate::arch::aarch64::uart::Uart;
use crate::cap::handle_table;
use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::ipc::endpoint;
use crate::mm::addr::PhysAddr;
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
    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *mut Tcb;
        (*tcb).current_syscall = syscall_num;
        (*tcb).current_syscall_args = [ctx.gpr[0], ctx.gpr[1], ctx.gpr[2], ctx.gpr[3]];
    }

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
    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *mut Tcb;
        (*tcb).current_syscall = u64::MAX;
    }
}

/// Get the current thread's handle table physical address.
unsafe fn caller_handle_table() -> PhysAddr {
    let tcb_paddr = scheduler::current_tcb_paddr();
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
    PhysAddr::new((*tcb).handle_table_paddr)
}

/// Look up a handle in the current thread's handle table with type checking.
/// Returns the HandleEntry on success, or a SyscallError on failure.
unsafe fn lookup_handle(handle: u32, required_rights: Rights, expected_type: ObjectType) -> Result<handle_table::HandleEntry, SyscallError> {
    let ht_paddr = caller_handle_table();

    let entry = handle_table::handle_lookup(ht_paddr, handle, required_rights)
        .map_err(|_| SyscallError::INVALID_HANDLE)?;

    if entry.obj_type != expected_type {
        return Err(SyscallError::INVALID_PARAMETER);
    }

    Ok(entry)
}

/// Common logic for sys_create_notification and sys_create_endpoint.
/// Looks up the PageSet, validates it's 1 page, calls the init function,
/// and inserts a handle into the caller's table.
unsafe fn create_kernel_object(
    pageset_id: u64,
    obj_type: ObjectType,
    init_fn: unsafe fn(PhysAddr) -> Result<(), crate::cap::object::CreateError>,
) -> Result<u64, SyscallError> {
    let (count, pages) = match crate::cap::pageset_table::get_pageset(pageset_id) {
        Some(ps) => ps,
        None => return Err(SyscallError::INVALID_HANDLE),
    };
    if count != 1 {
        return Err(SyscallError::INVALID_PARAMETER);
    }
    let paddr = pages[0];
    if init_fn(paddr).is_err() {
        return Err(SyscallError::UNKNOWN);
    }
    // Consume the PageSet so it can't be reused or re-mapped
    crate::cap::pageset_table::consume_pageset(pageset_id);
    let ht_paddr = caller_handle_table();
    match handle_table::handle_insert(
        ht_paddr, paddr, obj_type,
        Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE),
    ) {
        Ok(handle) => Ok(handle as u64),
        Err(_) => Err(SyscallError::OUT_OF_MEMORY),
    }
}

fn sys_debug_putc(char_val: u64) -> SyscallError {
    let uart = Uart::new();
    uart.putc(char_val as u8);
    SyscallError::OK
}

fn sys_yield() -> SyscallError {
    unsafe { scheduler::tick(); }
    SyscallError::OK
}

/// sys_send(handle, msg0, msg1, msg2, msg3) — send a message on an endpoint.
/// x0 = endpoint handle, x1-x4 = message registers.
fn sys_send(ctx: &mut ExceptionContext) -> SyscallError {
    let handle = ctx.gpr[0] as u32;
    let msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(e) => return e,
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match endpoint::ipc_send(ep_paddr, msg, tcb_paddr) {
            Ok(()) => SyscallError::OK,
            Err(_) => SyscallError::ENDPOINT_BUSY,
        }
    }
}

/// sys_receive(handle) — receive a message from an endpoint.
/// x0 = endpoint handle. On success: x0=0, x1-x4 = message words.
fn sys_receive(ctx: &mut ExceptionContext) -> SyscallReturn {
    let handle = ctx.gpr[0] as u32;

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(e) => return SyscallReturn::Message(e),
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match endpoint::ipc_receive(ep_paddr, tcb_paddr) {
            Ok(msg) => {
                ctx.gpr[1] = msg[0];
                ctx.gpr[2] = msg[1];
                ctx.gpr[3] = msg[2];
                ctx.gpr[4] = msg[3];
                SyscallReturn::Message(SyscallError::OK)
            }
            Err(_) => SyscallReturn::Message(SyscallError::UNKNOWN),
        }
    }
}

/// sys_call(handle, msg0, msg1, msg2, msg3) — send message and block for reply.
/// Combines send + receive in one syscall. On success: x0=0, x1-x4 = reply words.
fn sys_call(ctx: &mut ExceptionContext) -> SyscallReturn {
    let handle = ctx.gpr[0] as u32;
    let msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(e) => return SyscallReturn::Message(e),
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match endpoint::ipc_call(ep_paddr, msg, tcb_paddr) {
            Ok(reply) => {
                ctx.gpr[1] = reply[0];
                ctx.gpr[2] = reply[1];
                ctx.gpr[3] = reply[2];
                ctx.gpr[4] = reply[3];
                SyscallReturn::Message(SyscallError::OK)
            }
            Err(_) => SyscallReturn::Message(SyscallError::UNKNOWN),
        }
    }
}

/// sys_reply(msg0, msg1, msg2, msg3) — reply to the last caller on an endpoint.
/// x0 = handle, x1-x4 = reply message.
fn sys_reply(ctx: &mut ExceptionContext) -> SyscallError {
    let handle = ctx.gpr[0] as u32;
    let reply_msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(e) => return e,
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        match endpoint::ipc_reply(ep_paddr, reply_msg) {
            Ok(()) => SyscallError::OK,
            Err(_) => SyscallError::UNKNOWN,
        }
    }
}

/// sys_alloc_pages(count) — allocate physical pages.
/// x0 = number of pages to allocate.
/// Returns a PageSet ID in x1 on success.
fn sys_alloc_pages(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let count = ctx.gpr[0] as usize;

    match crate::cap::pageset_table::alloc_pages(count) {
        Some(id) => Ok(id),
        None => Err(SyscallError::OUT_OF_MEMORY),
    }
}

/// sys_map_pages(x0, virt_addr, flags) — map pages into the caller's address space.
/// When flags == 0 (normal memory): x0 = PageSet ID (from sys_alloc_pages).
/// When flags & MAP_FLAG_DEVICE: x0 = raw physical MMIO address (page-aligned).
/// x1 = virtual address to map at (must be page-aligned, in user range).
/// x2 = flags.
fn sys_map_pages(ctx: &mut ExceptionContext) -> SyscallError {
    let x0 = ctx.gpr[0];
    let virt_addr = ctx.gpr[1];
    let flags = ctx.gpr[2];

    unsafe {
        // Get the caller's TTBR0 from their TCB
        let tcb_paddr = scheduler::current_tcb_paddr();
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ttbr0 = PhysAddr::new((*tcb).ttbr0_paddr);

        if ttbr0.as_u64() == 0 {
            return SyscallError::INVALID_PARAMETER;
        }

        if flags & crate::arch::aarch64::vmem::MAP_FLAG_DEVICE != 0 {
            // x0 = raw physical address for device MMIO (single page)
            let pages = [PhysAddr::new(x0)];
            match crate::arch::aarch64::vmem::map_pages_in_existing(ttbr0, virt_addr, &pages, flags) {
                Ok(()) => SyscallError::OK,
                Err(_) => SyscallError::INVALID_PARAMETER,
            }
        } else {
            // x0 = PageSet ID (normal memory)
            let (count, pages) = match crate::cap::pageset_table::get_pageset(x0) {
                Some(ps) => ps,
                None => return SyscallError::INVALID_HANDLE,
            };
            match crate::arch::aarch64::vmem::map_pages_in_existing(ttbr0, virt_addr, &pages[..count], flags) {
                Ok(()) => SyscallError::OK,
                Err(_) => SyscallError::UNKNOWN,
            }
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

    unsafe {
        // Get caller's TTBR0 for safe user memory access
        let tcb_paddr = scheduler::current_tcb_paddr();
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let caller_ttbr0 = PhysAddr::new((*tcb).ttbr0_paddr);

        match crate::process::create_process(mappings_va, mapping_count, entry_point, stack_pageset_id, scratch_pageset_id, parent_handle_to_copy, caller_ttbr0) {
            Ok(()) => SyscallError::OK,
            Err(_) => SyscallError::UNKNOWN,
        }
    }
}

/// sys_create_notification(pageset_id) — create a Notification from a donated page.
/// x0 = PageSet ID (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_notification(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    unsafe { create_kernel_object(ctx.gpr[0], ObjectType::Notification, crate::ipc::notification::create_notification) }
}

/// sys_signal_notification(handle, value) — signal a notification.
/// x0 = notification handle, x1 = new timeline value (must be > current).
/// Wakes any thread waiting with threshold <= value.
fn sys_signal_notification(ctx: &mut ExceptionContext) -> SyscallError {
    let handle = ctx.gpr[0] as u32;
    let new_value = ctx.gpr[1];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Notification) {
            Ok(e) => e,
            Err(e) => return e,
        };

        let notif_paddr = PhysAddr::new(entry.object_paddr);
        match crate::ipc::notification::notification_signal(notif_paddr, new_value) {
            Ok(()) => SyscallError::OK,
            Err(lockjaw_types::notification_state::NotificationError::ValueNotMonotonic) => SyscallError::NOT_MONOTONIC,
            Err(_) => SyscallError::UNKNOWN,
        }
    }
}

/// sys_wait_notification(handle, threshold) — wait on a notification.
/// x0 = notification handle, x1 = threshold value to wait for.
/// Returns the current counter value in x1 when counter >= threshold.
/// Blocks if counter < threshold.
fn sys_wait_notification(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let handle = ctx.gpr[0] as u32;
    let threshold = ctx.gpr[1];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ), ObjectType::Notification) {
            Ok(e) => e,
            Err(e) => return Err(e),
        };

        let notif_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match crate::ipc::notification::notification_wait(notif_paddr, threshold, tcb_paddr) {
            Ok(value) => Ok(value),
            Err(lockjaw_types::notification_state::NotificationError::AlreadyHasWaiter) => Err(SyscallError::ALREADY_WAITING),
            Err(_) => Err(SyscallError::UNKNOWN),
        }
    }
}

/// sys_bind_irq(intid, notification_handle) — bind a hardware IRQ to a notification.
/// x0 = hardware INTID, x1 = notification handle.
/// When the IRQ fires, the kernel signals the notification (increments timeline by 1).
fn sys_bind_irq(ctx: &mut ExceptionContext) -> SyscallError {
    let intid = ctx.gpr[0] as u32;
    let notif_handle = ctx.gpr[1] as u32;

    unsafe {
        let entry = match lookup_handle(notif_handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Notification) {
            Ok(e) => e,
            Err(e) => return e,
        };

        let notif_paddr = PhysAddr::new(entry.object_paddr);
        if crate::arch::aarch64::irq_bind::bind(intid, notif_paddr) {
            // Enable SPI in the GIC distributor (PPIs are already enabled in gic::init)
            if intid >= 32 {
                crate::arch::aarch64::gic::enable_spi(intid);
            }
            SyscallError::OK
        } else {
            SyscallError::INVALID_PARAMETER
        }
    }
}

/// sys_create_endpoint(pageset_id) — create an Endpoint from a donated page.
/// x0 = PageSet ID (must be a 1-page PageSet).
/// Returns the new handle index in x1 on success.
fn sys_create_endpoint(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    unsafe { create_kernel_object(ctx.gpr[0], ObjectType::Endpoint, endpoint::create_endpoint) }
}

/// sys_recv_nb(handle) — non-blocking receive on an endpoint.
/// x0 = endpoint handle. On success: x0=0, x1-x4 = message words.
/// Returns SyscallError::WOULD_BLOCK if no sender is waiting.
fn sys_recv_nb(ctx: &mut ExceptionContext) -> SyscallReturn {
    let handle = ctx.gpr[0] as u32;

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(e) => return SyscallReturn::Message(e),
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();

        match endpoint::ipc_receive_nb(ep_paddr, tcb_paddr) {
            Ok(msg) => {
                ctx.gpr[1] = msg[0];
                ctx.gpr[2] = msg[1];
                ctx.gpr[3] = msg[2];
                ctx.gpr[4] = msg[3];
                SyscallReturn::Message(SyscallError::OK)
            }
            Err(endpoint::IpcError::WouldBlock) => SyscallReturn::Message(SyscallError::WOULD_BLOCK),
            Err(_) => SyscallReturn::Message(SyscallError::UNKNOWN),
        }
    }
}

/// sys_wait_any(entries_ptr, count) — wait until any of N objects is ready.
/// x0 = pointer to WaitEntry array in caller memory.
/// x1 = count (1..MAX_WAIT_OBJECTS).
/// Returns bitmask of ready objects in x1 (bit N = entry N is ready).
fn sys_wait_any(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    use lockjaw_types::wait::{WaitEntry, MAX_WAIT_OBJECTS, validate_wait_count};
    use crate::ipc::notification;
    use crate::mm::user_access::copy_from_user;

    let entries_va = ctx.gpr[0];
    let count = ctx.gpr[1] as usize;

    if !validate_wait_count(count) {
        return Err(SyscallError::INVALID_PARAMETER);
    }

    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *mut Tcb;
        let ttbr0 = PhysAddr::new((*tcb).ttbr0_paddr);
        let ht_paddr = caller_handle_table();

        // Read WaitEntry array from user memory via page table walk (TTBR1).
        // Never touches TTBR0 — immune to context switches.
        let mut paddrs = [PhysAddr::new(0); MAX_WAIT_OBJECTS];
        let mut types = [ObjectType::HandleTable; MAX_WAIT_OBJECTS];
        let mut thresholds = [0u64; MAX_WAIT_OBJECTS];

        for i in 0..count {
            let user_va = entries_va + (i as u64) * core::mem::size_of::<WaitEntry>() as u64;
            let entry: WaitEntry = match copy_from_user(ttbr0, user_va) {
                Some(e) => e,
                None => return Err(SyscallError::INVALID_PARAMETER),
            };
            let he = match handle_table::handle_lookup(
                ht_paddr, entry.handle as u32,
                Rights::from_bits(crate::cap::rights::RIGHT_READ),
            ) {
                Ok(e) => e,
                Err(_) => return Err(SyscallError::INVALID_HANDLE),
            };
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
        for i in 0..count {
            (*tcb).wait_objects[i] = paddrs[i].as_u64();
            (*tcb).wait_thresholds[i] = thresholds[i];
            (*tcb).wait_types[i] = types[i] as u8;
        }
        (*tcb).wait_count = count as u8;

        scheduler::block_current();

        // Woke up — unregister from all objects (only clear our own registration)
        let wc = (*tcb).wait_count as usize;
        for i in 0..wc {
            let p = PhysAddr::new((*tcb).wait_objects[i]);
            match obj_type_from_u8((*tcb).wait_types[i]) {
                ObjectType::Endpoint => endpoint::clear_readiness_waiter(p, tcb_paddr),
                ObjectType::Notification => notification::clear_readiness_waiter(p, tcb_paddr),
                _ => {}
            }
        }
        (*tcb).wait_count = 0;

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

/// sys_export_handle(endpoint_handle, handle_to_export) — duplicate a handle
/// into a blocked caller's handle table.
/// x0 = endpoint handle (must have a caller blocked via sys_call).
/// x1 = handle index in the exporter's table to duplicate.
/// Returns the new handle index in x1 in the caller's table, or an error.
fn sys_export_handle(ctx: &mut ExceptionContext) -> Result<u64, SyscallError> {
    let ep_handle = ctx.gpr[0] as u32;
    let handle_to_export = ctx.gpr[1] as u32;

    unsafe {
        // Look up the endpoint — exporter needs WRITE rights
        let ep_entry = match lookup_handle(ep_handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(e) => return Err(e),
        };

        let ep_paddr = PhysAddr::new(ep_entry.object_paddr);

        // Verify a caller is blocked waiting for reply
        let caller_tcb = endpoint::read_caller_tcb(ep_paddr);
        if caller_tcb == 0 {
            return Err(SyscallError::NO_CALLER);
        }

        // Look up the handle to export in the exporter's own table
        let ht_paddr = caller_handle_table();
        let export_entry = match handle_table::handle_lookup(
            ht_paddr, handle_to_export, Rights::none(),
        ) {
            Ok(e) => e,
            Err(_) => return Err(SyscallError::INVALID_HANDLE),
        };

        // Find the blocked caller's handle table
        let caller_tcb_paddr = endpoint::read_caller_tcb(ep_paddr);
        crate::kprintln!("[export] caller_tcb={:#x}", caller_tcb_paddr);
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let caller_tcb = (caller_tcb_paddr + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let caller_ht = PhysAddr::new((*caller_tcb).handle_table_paddr);
        crate::kprintln!("[export] caller_ht={:#x}", caller_ht.as_u64());

        // Insert a copy into the caller's table
        match handle_table::handle_insert(
            caller_ht,
            PhysAddr::new(export_entry.object_paddr),
            export_entry.obj_type,
            export_entry.rights,
        ) {
            Ok(new_index) => Ok(new_index as u64),
            Err(_) => Err(SyscallError::OUT_OF_MEMORY),
        }
    }
}

/// sys_get_boot_info() — returns boot information.
/// DTB PageSet ID returned in x1.
fn sys_get_boot_info() -> u64 {
    crate::dtb_pageset_id()
}

fn obj_type_from_u8(v: u8) -> ObjectType {
    match v {
        2 => ObjectType::Endpoint,
        3 => ObjectType::Notification,
        _ => ObjectType::HandleTable,
    }
}
