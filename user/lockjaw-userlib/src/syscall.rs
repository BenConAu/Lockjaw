use core::arch::asm;
use lockjaw_types::syscall::*;
use lockjaw_types::syscall::SyscallError;

pub fn putc(c: u8) {
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") c as u64 => _,
            in("x8") SYS_DEBUG_PUTC,
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

/// Allocate physical pages. Returns a PageSet ID on success.
pub fn sys_alloc_pages(count: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") count => err,
            lateout("x1") val,
            in("x8") SYS_ALLOC_PAGES,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Map pages into the caller's address space.
/// When flags == 0: pageset_id_or_phys is a PageSet ID (from sys_alloc_pages).
/// When flags & MAP_FLAG_DEVICE: pageset_id_or_phys is a raw physical MMIO address.
pub fn sys_map_pages(pageset_id_or_phys: u64, virt_addr: u64, flags: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") pageset_id_or_phys => err,
            in("x1") virt_addr,
            in("x2") flags,
            in("x8") SYS_MAP_PAGES,
        );
    }
    SyscallError(err)
}

pub fn sys_create_process(mappings_ptr: u64, mapping_count: u64, entry_point: u64, stack_pageset_id: u64, scratch_pageset_id: u64, handle_to_copy: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") mappings_ptr => err,
            in("x1") mapping_count,
            in("x2") entry_point,
            in("x3") stack_pageset_id,
            in("x4") scratch_pageset_id,
            in("x5") handle_to_copy,
            in("x8") SYS_CREATE_PROCESS,
        );
    }
    SyscallError(err)
}

/// Create a Notification from a donated page. Returns the new handle index.
pub fn sys_create_notification(pageset_id: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") pageset_id => err,
            lateout("x1") val,
            in("x8") SYS_CREATE_NOTIFICATION,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Create an Endpoint from a donated page. Returns the new handle index.
pub fn sys_create_endpoint(pageset_id: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") pageset_id => err,
            lateout("x1") val,
            in("x8") SYS_CREATE_ENDPOINT,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Send a message and block for reply. Returns the first reply word.
pub fn sys_call(handle: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") handle => err,
            inlateout("x1") msg0 => val,
            inlateout("x2") msg1 => _,
            inlateout("x3") msg2 => _,
            inlateout("x4") msg3 => _,
            in("x8") SYS_CALL,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Send a message and block for reply. Returns all 4 reply words.
pub fn sys_call_ret4(handle: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> Result<[u64; 4], SyscallError> {
    let err: u64;
    let r1: u64;
    let r2: u64;
    let r3: u64;
    let r4: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") handle => err,
            inlateout("x1") msg0 => r1,
            inlateout("x2") msg1 => r2,
            inlateout("x3") msg2 => r3,
            inlateout("x4") msg3 => r4,
            in("x8") SYS_CALL,
        );
    }
    if err == 0 { Ok([r1, r2, r3, r4]) } else { Err(SyscallError(err)) }
}

pub fn sys_reply(handle: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") handle => err,
            in("x1") msg0,
            in("x2") msg1,
            in("x3") msg2,
            in("x4") msg3,
            in("x8") SYS_REPLY,
        );
    }
    SyscallError(err)
}

/// Blocking receive on an endpoint.
/// Returns the first message word on success. Blocks until a sender is waiting.
pub fn sys_receive(handle: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") handle => err,
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
pub fn sys_receive_ret4(handle: u64) -> Result<[u64; 4], SyscallError> {
    let err: u64;
    let r1: u64;
    let r2: u64;
    let r3: u64;
    let r4: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") handle => err,
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
pub fn sys_recv_nb(handle: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") handle => err,
            lateout("x1") val,
            lateout("x2") _,
            lateout("x3") _,
            lateout("x4") _,
            in("x8") SYS_RECV_NB,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

pub fn sys_bind_irq(intid: u64, notif_handle: u64) -> SyscallError {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") intid => err,
            in("x1") notif_handle,
            in("x8") SYS_BIND_IRQ,
        );
    }
    SyscallError(err)
}

/// Export a handle from the caller's table into a blocked caller's table.
/// The endpoint must have a caller blocked via sys_call.
/// Returns the new handle index in the caller's table.
pub fn sys_export_handle(endpoint_handle: u64, handle_to_export: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") endpoint_handle => err,
            inlateout("x1") handle_to_export => val,
            in("x8") SYS_EXPORT_HANDLE,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Wait until any of the given objects is ready.
/// entries = array of WaitEntry (handle + threshold pairs).
/// Returns a bitmask: bit N set = entry N is ready.
pub fn sys_wait_any(entries: &[crate::WaitEntry]) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") entries.as_ptr() => err,
            inlateout("x1") entries.len() => val,
            in("x8") SYS_WAIT_ANY,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Get boot information from the kernel.
/// Returns the DTB PageSet ID.
pub fn sys_get_boot_info() -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x8") SYS_GET_BOOT_INFO,
            lateout("x0") err,
            lateout("x1") val,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Register a physical MMIO address as a tracked PageSet.
/// Returns the PageSet ID for use with sys_map_pages.
pub fn sys_register_device_page(phys_addr: u64) -> Result<u64, SyscallError> {
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
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}
