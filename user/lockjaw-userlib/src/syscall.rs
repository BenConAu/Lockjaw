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
            inlateout("x1") 0u64 => val,
            in("x8") SYS_ALLOC_PAGES,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Allocate physically contiguous pages. Returns a PageSet ID on success.
/// The pages are guaranteed to be sequential in physical memory,
/// suitable for DMA buffers. The block size is rounded up to the next
/// power of two internally.
pub fn sys_alloc_pages_contiguous(count: u64) -> Result<u64, SyscallError> {
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

pub fn sys_create_process(mappings_ptr: u64, mapping_count: u64, entry_point: u64, stack_pageset_id: u64, scratch_pageset_id: u64, handle_to_copy: u64, name_ptr: u64) -> SyscallError {
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
            in("x6") name_ptr,
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

/// Create a Reply object from a 1-page PageSet. Each client thread
/// allocates one of these at startup and reuses it across sys_call
/// invocations. The Reply is the per-call "who to wake" marker that
/// replaces the single caller_tcb slot on endpoints.
pub fn sys_create_reply(pageset_id: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",                        // trap into kernel
            inlateout("x0") pageset_id => err, // in: pageset id, out: error code
            lateout("x1") val,               // out: new handle index
            in("x8") SYS_CREATE_REPLY,       // syscall number
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Send a message on `ep` and block for a reply bound to `reply`.
/// Returns the first reply word on success.
///
/// The Reply object must be Fresh (either never used, or released by a
/// prior completed call). Each client thread typically owns one Reply and
/// reuses it. A second concurrent call with the same Reply returns
/// `REPLY_BOUND`.
pub fn sys_call(ep: u64, reply: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",                       // trap into kernel
            inlateout("x0") ep => err,      // in: endpoint handle, out: error
            inlateout("x1") reply => val,   // in: reply handle, out: reply[0]
            inlateout("x2") msg0 => _,      // in: msg word 0
            inlateout("x3") msg1 => _,      // in: msg word 1
            inlateout("x4") msg2 => _,      // in: msg word 2
            inlateout("x5") msg3 => _,      // in: msg word 3
            in("x8") SYS_CALL,              // syscall number
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Send a message and block for reply. Returns all 4 reply words.
pub fn sys_call_ret4(ep: u64, reply: u64, msg0: u64, msg1: u64, msg2: u64, msg3: u64) -> Result<[u64; 4], SyscallError> {
    let err: u64;
    let r1: u64;
    let r2: u64;
    let r3: u64;
    let r4: u64;
    unsafe {
        asm!(
            "svc #0",                       // trap into kernel
            inlateout("x0") ep => err,      // in: endpoint handle, out: error
            inlateout("x1") reply => r1,    // in: reply handle, out: reply[0]
            inlateout("x2") msg0 => r2,     // in: msg[0], out: reply[1]
            inlateout("x3") msg1 => r3,     // in: msg[1], out: reply[2]
            inlateout("x4") msg2 => r4,     // in: msg[2], out: reply[3]
            inlateout("x5") msg3 => _,      // in: msg[3]
            in("x8") SYS_CALL,              // syscall number
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
            "svc #0",                       // trap into kernel
            inlateout("x0") msg0 => err,    // in: msg[0], out: error
            in("x1") msg1,                  // in: msg[1]
            in("x2") msg2,                  // in: msg[2]
            in("x3") msg3,                  // in: msg[3]
            in("x8") SYS_REPLY,             // syscall number
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
/// Duplicate a handle into the caller currently bound on this thread's
/// TCB (set by the preceding sys_receive). No endpoint handle is needed.
///
/// Returns `NO_CALLER` if there is no bound call.
pub fn sys_export_handle(handle_to_export: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",                                 // trap into kernel
            inlateout("x0") handle_to_export => err,  // in: handle, out: error
            lateout("x1") val,                        // out: new handle index
            in("x8") SYS_EXPORT_HANDLE,               // syscall number
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

/// Query the physical address of a page within a PageSet.
/// Used by drivers that need to program DMA base registers.
pub fn sys_query_pageset_phys(pageset_id: u64, page_index: u64) -> Result<u64, SyscallError> {
    let err: u64;
    let val: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") pageset_id => err,
            inlateout("x1") page_index => val,
            in("x8") SYS_QUERY_PAGESET_PHYS,
        );
    }
    if err == 0 { Ok(val) } else { Err(SyscallError(err)) }
}

/// Create a new thread in the calling process. The thread shares the
/// caller's address space and handle table. Starts at `entry` with
/// SP=stack_top and x0=arg.
pub fn sys_create_thread(entry: u64, stack_top: u64, stack_base: u64, arg: u64) -> Result<(), SyscallError> {
    let err: u64;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") entry => err,
            in("x1") stack_top,
            in("x2") stack_base,
            in("x3") arg,
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
