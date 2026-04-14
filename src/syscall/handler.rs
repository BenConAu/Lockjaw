use crate::arch::aarch64::exceptions::ExceptionContext;
use crate::arch::aarch64::uart::Uart;
use crate::cap::handle_table;
use crate::cap::object::ObjectType;
use crate::cap::rights::Rights;
use crate::ipc::endpoint;
use crate::mm::addr::PhysAddr;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;

// Syscall numbers and error codes from lockjaw-types (shared with userspace).
use lockjaw_types::syscall::*;

/// Dispatch a syscall from userspace.
/// Called from handle_exception_sync_lower when EC = 0x15 (SVC from AArch64).
///
/// Convention: syscall number in x8, arguments in x0-x5, return in x0.
/// Dispatch a syscall from userspace.
///
/// Called from handle_exception_sync_lower when ESR_EL1.EC = 0x15 (SVC from AArch64).
/// Reads the syscall number from x8, dispatches to the handler, and writes the
/// return value (SYS_OK or an error code from lockjaw_types::syscall) to x0.
pub fn handle_syscall(ctx: &mut ExceptionContext) {
    let syscall_num = ctx.gpr[8]; // x8

    ctx.gpr[0] = match syscall_num {
        SYS_DEBUG_PUTC => sys_debug_putc(ctx.gpr[0]),
        SYS_YIELD => sys_yield(),
        SYS_SEND => sys_send(ctx),
        SYS_RECEIVE => sys_receive(ctx),
        SYS_CALL => sys_call(ctx),
        SYS_REPLY => sys_reply(ctx),
        SYS_ALLOC_PAGES => sys_alloc_pages(ctx),
        SYS_MAP_PAGES => sys_map_pages(ctx),
        SYS_CREATE_PROCESS => sys_create_process(ctx),
        SYS_CREATE_NOTIFICATION => sys_create_notification(ctx),
        SYS_SIGNAL_NOTIFICATION => sys_signal_notification(ctx),
        SYS_WAIT_NOTIFICATION => sys_wait_notification(ctx),
        SYS_BIND_IRQ => sys_bind_irq(ctx),
        SYS_CREATE_ENDPOINT => sys_create_endpoint(ctx),
        SYS_RECV_NB => sys_recv_nb(ctx),
        _ => {
            crate::kprintln!("Unknown syscall {}", syscall_num);
            SYS_ERR_INVALID_PARAMETER
        }
    };
}

/// Look up a handle in the current thread's handle table with type checking.
/// Returns the HandleEntry on success, or a syscall error code on failure.
unsafe fn lookup_handle(handle: u32, required_rights: Rights, expected_type: ObjectType) -> Result<handle_table::HandleEntry, u64> {
    let tcb_paddr = scheduler::current_tcb_paddr();
    let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
    let ht_paddr = PhysAddr::new((*tcb).handle_table_paddr);

    let entry = handle_table::handle_lookup(ht_paddr, handle, required_rights)
        .map_err(|_| SYS_ERR_INVALID_HANDLE)?;

    if entry.obj_type != expected_type {
        return Err(SYS_ERR_INVALID_PARAMETER);
    }

    Ok(entry)
}

fn sys_debug_putc(char_val: u64) -> u64 {
    let uart = Uart::new();
    uart.putc(char_val as u8);
    0
}

fn sys_yield() -> u64 {
    unsafe { scheduler::tick(); }
    0
}

/// sys_send(handle, msg0, msg1, msg2, msg3) — send a message on an endpoint.
/// x0 = endpoint handle, x1-x4 = message registers.
fn sys_send(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(code) => return code,
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match endpoint::ipc_send(ep_paddr, msg, tcb_paddr) {
            Ok(()) => SYS_OK,
            Err(_) => SYS_ERR_ENDPOINT_BUSY,
        }
    }
}

/// sys_receive(handle) — receive a message from an endpoint.
/// x0 = endpoint handle. Message returned in x0-x3.
fn sys_receive(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(code) => return code,
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match endpoint::ipc_receive(ep_paddr, tcb_paddr) {
            Ok(msg) => {
                ctx.gpr[0] = msg[0];
                ctx.gpr[1] = msg[1];
                ctx.gpr[2] = msg[2];
                ctx.gpr[3] = msg[3];
                return msg[0];
            }
            Err(_) => return SYS_ERR_UNKNOWN,
        }
    }
}

/// sys_call(handle, msg0, msg1, msg2, msg3) — send message and block for reply.
/// Combines send + receive in one syscall. Returns the reply in x0-x3.
fn sys_call(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(code) => return code,
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match endpoint::ipc_call(ep_paddr, msg, tcb_paddr) {
            Ok(reply) => {
                ctx.gpr[0] = reply[0];
                ctx.gpr[1] = reply[1];
                ctx.gpr[2] = reply[2];
                ctx.gpr[3] = reply[3];
                return reply[0];
            }
            Err(_) => return SYS_ERR_UNKNOWN,
        }
    }
}

/// sys_reply(msg0, msg1, msg2, msg3) — reply to the last caller on handle 0.
/// x0 = handle, x1-x4 = reply message.
fn sys_reply(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let reply_msg = [ctx.gpr[1], ctx.gpr[2], ctx.gpr[3], ctx.gpr[4]];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(code) => return code,
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        match endpoint::ipc_reply(ep_paddr, reply_msg) {
            Ok(()) => 0,
            Err(_) => SYS_ERR_UNKNOWN,
        }
    }
}

/// sys_alloc_pages(count) — allocate physical pages.
/// x0 = number of pages to allocate.
/// Returns a PageSet ID on success, SYS_ERR_OUT_OF_MEMORY on failure.
fn sys_alloc_pages(ctx: &mut ExceptionContext) -> u64 {
    let count = ctx.gpr[0] as usize;

    match crate::cap::pageset_table::alloc_pages(count) {
        Some(id) => id,
        None => SYS_ERR_OUT_OF_MEMORY,
    }
}

/// sys_map_pages(x0, virt_addr, flags) — map pages into the caller's address space.
/// When flags == 0 (normal memory): x0 = PageSet ID (from sys_alloc_pages).
/// When flags & MAP_FLAG_DEVICE: x0 = raw physical MMIO address (page-aligned).
/// x1 = virtual address to map at (must be page-aligned, in user range).
/// x2 = flags.
/// Returns SYS_OK on success.
fn sys_map_pages(ctx: &mut ExceptionContext) -> u64 {
    let x0 = ctx.gpr[0];
    let virt_addr = ctx.gpr[1];
    let flags = ctx.gpr[2];

    unsafe {
        // Get the caller's TTBR0 from their TCB
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ttbr0 = PhysAddr::new((*tcb).ttbr0_paddr);

        if ttbr0.as_u64() == 0 {
            return SYS_ERR_INVALID_PARAMETER;
        }

        if flags & crate::arch::aarch64::vmem::MAP_FLAG_DEVICE != 0 {
            // x0 = raw physical address for device MMIO (single page)
            let pages = [PhysAddr::new(x0)];
            match crate::arch::aarch64::vmem::map_pages_in_existing(ttbr0, virt_addr, &pages, flags) {
                Ok(()) => SYS_OK,
                Err(_) => SYS_ERR_INVALID_PARAMETER,
            }
        } else {
            // x0 = PageSet ID (normal memory)
            let (count, pages) = match crate::cap::pageset_table::get_pageset(x0) {
                Some(ps) => ps,
                None => return SYS_ERR_INVALID_HANDLE,
            };
            match crate::arch::aarch64::vmem::map_pages_in_existing(ttbr0, virt_addr, &pages[..count], flags) {
                Ok(()) => SYS_OK,
                Err(_) => SYS_ERR_UNKNOWN,
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
/// Returns 0 on success, SYS_ERR_UNKNOWN on failure.
fn sys_create_process(ctx: &mut ExceptionContext) -> u64 {
    let mappings_ptr = ctx.gpr[0] as *const crate::process::ProcessMapping;
    let mapping_count = ctx.gpr[1] as usize;
    let entry_point = ctx.gpr[2];
    let stack_pageset_id = ctx.gpr[3];
    let scratch_pageset_id = ctx.gpr[4];
    let parent_handle_to_copy = ctx.gpr[5];

    unsafe {
        match crate::process::create_process(mappings_ptr, mapping_count, entry_point, stack_pageset_id, scratch_pageset_id, parent_handle_to_copy) {
            Ok(()) => SYS_OK,
            Err(_) => SYS_ERR_UNKNOWN,
        }
    }
}

/// sys_create_notification(pageset_id) — create a Notification from a donated page.
/// x0 = PageSet ID (must be a 1-page PageSet).
/// Returns the new handle index on success.
fn sys_create_notification(ctx: &mut ExceptionContext) -> u64 {
    let pageset_id = ctx.gpr[0];

    unsafe {
        // Look up the PageSet to get the physical page
        let (count, pages) = match crate::cap::pageset_table::get_pageset(pageset_id) {
            Some(ps) => ps,
            None => return SYS_ERR_INVALID_HANDLE,
        };
        if count != 1 {
            return SYS_ERR_INVALID_PARAMETER;
        }

        let paddr = pages[0];

        // Initialize the page as a NotificationObject
        if crate::ipc::notification::create_notification(paddr).is_err() {
            return SYS_ERR_UNKNOWN;
        }

        // Insert a handle into the caller's handle table
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ht_paddr = PhysAddr::new((*tcb).handle_table_paddr);

        match handle_table::handle_insert(
            ht_paddr, paddr, ObjectType::Notification,
            Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE),
        ) {
            Ok(handle) => handle as u64,
            Err(_) => SYS_ERR_OUT_OF_MEMORY,
        }
    }
}

/// sys_signal_notification(handle, value) — signal a notification.
/// x0 = notification handle, x1 = new timeline value (must be > current).
/// Returns SYS_OK on success. Wakes any thread waiting with threshold <= value.
fn sys_signal_notification(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let new_value = ctx.gpr[1];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Notification) {
            Ok(e) => e,
            Err(code) => return code,
        };

        let notif_paddr = PhysAddr::new(entry.object_paddr);
        match crate::ipc::notification::notification_signal(notif_paddr, new_value) {
            Ok(()) => SYS_OK,
            Err(lockjaw_types::notification_state::NotificationError::ValueNotMonotonic) => SYS_ERR_NOT_MONOTONIC,
            Err(_) => SYS_ERR_UNKNOWN,
        }
    }
}

/// sys_wait_notification(handle, threshold) — wait on a notification.
/// x0 = notification handle, x1 = threshold value to wait for.
/// Returns the current counter value when counter >= threshold.
/// Blocks if counter < threshold.
fn sys_wait_notification(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;
    let threshold = ctx.gpr[1];

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ), ObjectType::Notification) {
            Ok(e) => e,
            Err(code) => return code,
        };

        let notif_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();
        match crate::ipc::notification::notification_wait(notif_paddr, threshold, tcb_paddr) {
            Ok(value) => value,
            Err(lockjaw_types::notification_state::NotificationError::AlreadyHasWaiter) => SYS_ERR_ALREADY_WAITING,
            Err(_) => SYS_ERR_UNKNOWN,
        }
    }
}

/// sys_bind_irq(intid, notification_handle) — bind a hardware IRQ to a notification.
/// x0 = hardware INTID, x1 = notification handle.
/// When the IRQ fires, the kernel signals the notification (increments timeline by 1).
/// Returns SYS_OK on success.
fn sys_bind_irq(ctx: &mut ExceptionContext) -> u64 {
    let intid = ctx.gpr[0] as u32;
    let notif_handle = ctx.gpr[1] as u32;

    unsafe {
        let entry = match lookup_handle(notif_handle, Rights::from_bits(crate::cap::rights::RIGHT_WRITE), ObjectType::Notification) {
            Ok(e) => e,
            Err(code) => return code,
        };

        let notif_paddr = PhysAddr::new(entry.object_paddr);
        if crate::arch::aarch64::irq_bind::bind(intid, notif_paddr) {
            // Enable SPI in the GIC distributor (PPIs are already enabled in gic::init)
            if intid >= 32 {
                crate::arch::aarch64::gic::enable_spi(intid);
            }
            SYS_OK
        } else {
            SYS_ERR_INVALID_PARAMETER
        }
    }
}

/// sys_create_endpoint(pageset_id) — create an Endpoint from a donated page.
/// x0 = PageSet ID (must be a 1-page PageSet).
/// Returns the new handle index on success.
fn sys_create_endpoint(ctx: &mut ExceptionContext) -> u64 {
    let pageset_id = ctx.gpr[0];

    unsafe {
        let (count, pages) = match crate::cap::pageset_table::get_pageset(pageset_id) {
            Some(ps) => ps,
            None => return SYS_ERR_INVALID_HANDLE,
        };
        if count != 1 {
            return SYS_ERR_INVALID_PARAMETER;
        }

        let paddr = pages[0];

        if endpoint::create_endpoint(paddr).is_err() {
            return SYS_ERR_UNKNOWN;
        }

        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = (tcb_paddr.as_u64() + crate::mm::addr::KERNEL_VA_OFFSET) as *const Tcb;
        let ht_paddr = PhysAddr::new((*tcb).handle_table_paddr);

        match handle_table::handle_insert(
            ht_paddr, paddr, ObjectType::Endpoint,
            Rights::from_bits(crate::cap::rights::RIGHT_READ | crate::cap::rights::RIGHT_WRITE),
        ) {
            Ok(handle) => handle as u64,
            Err(_) => SYS_ERR_OUT_OF_MEMORY,
        }
    }
}

/// sys_recv_nb(handle) — non-blocking receive on an endpoint.
/// x0 = endpoint handle. If a sender is waiting, returns the message in x0-x3.
/// Otherwise returns SYS_ERR_WOULD_BLOCK immediately.
fn sys_recv_nb(ctx: &mut ExceptionContext) -> u64 {
    let handle = ctx.gpr[0] as u32;

    unsafe {
        let entry = match lookup_handle(handle, Rights::from_bits(crate::cap::rights::RIGHT_READ), ObjectType::Endpoint) {
            Ok(e) => e,
            Err(code) => {
                crate::kprintln!("[recv_nb] handle lookup failed: h={} code={}", handle, code);
                return code;
            }
        };

        let ep_paddr = PhysAddr::new(entry.object_paddr);
        let tcb_paddr = scheduler::current_tcb_paddr();

        match endpoint::ipc_receive_nb(ep_paddr, tcb_paddr) {
            Ok(msg) => {
                ctx.gpr[0] = msg[0];
                ctx.gpr[1] = msg[1];
                ctx.gpr[2] = msg[2];
                ctx.gpr[3] = msg[3];
                return msg[0];
            }
            Err(endpoint::IpcError::WouldBlock) => return SYS_ERR_WOULD_BLOCK,
            Err(_) => return SYS_ERR_UNKNOWN,
        }
    }
}
