use core::arch::asm;
use lockjaw_types::syscall::*;

pub fn putc(c: u8) {
    unsafe {
        asm!("svc #0", in("x0") c as u64, in("x8") SYS_DEBUG_PUTC);
    }
}

pub fn sys_yield() {
    unsafe {
        asm!("svc #0", in("x8") SYS_YIELD);
    }
}

pub fn sys_alloc_pages(count: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") count, in("x8") SYS_ALLOC_PAGES, lateout("x0") result);
    }
    result
}

/// Map pages into the caller's address space.
/// When flags == 0: pageset_id_or_phys is a PageSet ID (from sys_alloc_pages).
/// When flags & MAP_FLAG_DEVICE: pageset_id_or_phys is a raw physical MMIO address.
pub fn sys_map_pages(pageset_id_or_phys: u64, virt_addr: u64, flags: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") pageset_id_or_phys, in("x1") virt_addr, in("x2") flags, in("x8") SYS_MAP_PAGES, lateout("x0") result);
    }
    result
}

pub fn sys_create_process(mappings_ptr: u64, mapping_count: u64, entry_point: u64, stack_pageset_id: u64, scratch_pageset_id: u64, handle_to_copy: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") mappings_ptr,
            in("x1") mapping_count,
            in("x2") entry_point,
            in("x3") stack_pageset_id,
            in("x4") scratch_pageset_id,
            in("x5") handle_to_copy,
            in("x8") SYS_CREATE_PROCESS,
            lateout("x0") result,
        );
    }
    result
}

pub fn sys_create_notification(pageset_id: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") pageset_id, in("x8") SYS_CREATE_NOTIFICATION, lateout("x0") result);
    }
    result
}

pub fn sys_create_endpoint(pageset_id: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") pageset_id, in("x8") SYS_CREATE_ENDPOINT, lateout("x0") result);
    }
    result
}

/// Send a message and block for reply. Returns the first reply word.
pub fn sys_call(handle: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") handle,
            in("x1") msg0,
            in("x2") msg1,
            in("x3") msg2,
            in("x4") msg3,
            in("x8") SYS_CALL,
            lateout("x0") result,
        );
    }
    result
}

/// Send a message and block for reply. Returns all 4 reply words.
pub fn sys_call_ret4(handle: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> [u64; 4] {
    let r0: u64;
    let r1: u64;
    let r2: u64;
    let r3: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") handle,
            in("x1") msg0,
            in("x2") msg1,
            in("x3") msg2,
            in("x4") msg3,
            in("x8") SYS_CALL,
            lateout("x0") r0,
            lateout("x1") r1,
            lateout("x2") r2,
            lateout("x3") r3,
        );
    }
    [r0, r1, r2, r3]
}

pub fn sys_reply(handle: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) {
    unsafe {
        asm!(
            "svc #0",
            in("x0") handle,
            in("x1") msg0,
            in("x2") msg1,
            in("x3") msg2,
            in("x4") msg3,
            in("x8") SYS_REPLY,
        );
    }
}

/// Blocking receive on an endpoint.
/// Returns the first message word. Blocks until a sender is waiting.
pub fn sys_receive(handle: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") handle, in("x8") SYS_RECEIVE, lateout("x0") result);
    }
    result
}

/// Non-blocking receive on an endpoint.
/// Returns the first message word if a sender is waiting,
/// or SYS_ERR_WOULD_BLOCK if no message is pending.
pub fn sys_recv_nb(handle: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x0") handle, in("x8") SYS_RECV_NB, lateout("x0") result);
    }
    result
}

pub fn sys_bind_irq(intid: u64, notif_handle: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") intid,
            in("x1") notif_handle,
            in("x8") SYS_BIND_IRQ,
            lateout("x0") result,
        );
    }
    result
}

/// Export a handle from the caller's table into a blocked caller's table.
/// The endpoint must have a caller blocked via sys_call.
/// Returns the new handle index in the caller's table.
pub fn sys_export_handle(endpoint_handle: u64, handle_to_export: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") endpoint_handle,
            in("x1") handle_to_export,
            in("x8") SYS_EXPORT_HANDLE,
            lateout("x0") result,
        );
    }
    result
}

/// Wait until any of the given objects is ready.
/// entries = array of WaitEntry (handle + threshold pairs).
/// Returns a bitmask: bit N set = entry N is ready.
pub fn sys_wait_any(entries: &[crate::WaitEntry]) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") entries.as_ptr(),
            in("x1") entries.len(),
            in("x8") SYS_WAIT_ANY,
            lateout("x0") result,
        );
    }
    result
}

/// Get boot information from the kernel.
/// Returns the DTB PageSet ID in x0.
pub fn sys_get_boot_info() -> u64 {
    let result: u64;
    unsafe {
        asm!("svc #0", in("x8") SYS_GET_BOOT_INFO, lateout("x0") result);
    }
    result
}
