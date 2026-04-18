use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, PAGE_SIZE};
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::context::SavedContext;
use core::ptr;

// ---------------------------------------------------------------------------
// Thread state
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// TCB — stored in donated pages
// ---------------------------------------------------------------------------
// Thread scheduling state (Ready/Running/Blocked) is NOT stored here.
// The scheduler's pure state machine in lockjaw_types::scheduler owns
// it. The TCB holds hardware-facing context (saved SP, TTBR0, etc.)
// plus object metadata.

/// Thread Control Block. Stored at the start of a donated page.
#[repr(C)]
pub struct Tcb {
    pub header: ObjectHeader,
    pub saved_sp: u64,
    pub entry: fn() -> !,
    pub stack_base: u64,
    pub handle_table_paddr: u64,
    pub ttbr0_paddr: u64,
    pub ipc_blocked_on: u64,
    /// Kernel-internal IPC mailbox. The IPC state machine writes received
    /// messages here; the syscall handler copies them to the exception
    /// context (x0-x3) for userspace. Do not use for other purposes.
    pub ipc_msg: [u64; 4],
    /// Intrusive link in an endpoint's waiter queue (paddr of next TCB,
    /// 0 = tail). Written by ep_queue::enqueue, cleared by dequeue.
    pub ipc_queue_next: u64,
    /// Kind of wait currently held on an endpoint: 0 = none, 1 = Send,
    /// 2 = Receive, 3 = Call. Set on enqueue, cleared on dequeue.
    /// The server reads this on sys_receive to decide whether to unblock
    /// the head waiter (Send) or leave it blocked awaiting reply (Call).
    pub ipc_wait_kind: u8,
    /// Server-side: paddr of the Reply object bound to the call currently
    /// being handled by this thread. Set by sys_receive when dequeuing a
    /// Call; cleared by sys_reply. 0 = no outstanding call.
    pub current_reply_paddr: u64,
    /// Caller-side: paddr of this thread's own Reply object while queued
    /// as a Call waiter, so the server can pick it up on sys_receive.
    /// 0 when not queued as a Call.
    pub ipc_call_reply_paddr: u64,
    /// ELF entry point VA for user processes (0 for kernel threads).
    pub user_entry_point: u64,
    /// User stack top VA for user processes (0 for kernel threads).
    pub user_stack_top: u64,
    /// User stack base VA (lowest mapped page) for overflow detection (0 for kernel threads).
    pub user_stack_base: u64,
    /// Objects this thread is waiting on via sys_wait_any (paddrs, 0 = unused).
    pub wait_objects: [u64; lockjaw_types::wait::MAX_WAIT_OBJECTS],
    /// Per-object thresholds for the wait (notification target values).
    pub wait_thresholds: [u64; lockjaw_types::wait::MAX_WAIT_OBJECTS],
    /// Object types for each wait entry (ObjectType as u8).
    pub wait_types: [u8; lockjaw_types::wait::MAX_WAIT_OBJECTS],
    /// Number of valid entries in wait_objects (0 = not in a sys_wait_any).
    pub wait_count: u8,
    /// Currently executing syscall number (u64::MAX = not in a syscall).
    /// Set at syscall entry, cleared at exit. Printed on crash.
    pub current_syscall: u64,
    /// Arguments to the current syscall (x0-x3).
    pub current_syscall_args: [u64; 4],
    /// Process name for diagnostics. NUL-terminated, max 15 chars + NUL.
    pub name: [u8; 16],
}

// A TCB must fit in one 4 KB page — it's allocated from a single donated page.
const _: () = assert!(core::mem::size_of::<Tcb>() <= PAGE_SIZE as usize);

// ---------------------------------------------------------------------------
// Vulkan-style create-info
// ---------------------------------------------------------------------------

/// Describes a thread to create.
pub struct TcbCreateInfo {
    pub entry: fn() -> !,
    pub stack_paddr: PhysAddr,
    pub handle_table_paddr: PhysAddr,
    pub ttbr0_paddr: PhysAddr,
    pub user_entry_point: u64,
    pub user_stack_base: u64,
    pub user_stack_top: u64,
    pub name: [u8; 16],
}

/// Initialize a TCB in donated memory and set up its stack with a
/// synthetic frame so it can be context-switched into.
///
/// # Safety
/// `base_paddr` must be a donated page. `info.stack_paddr` must be a
/// separate page for the thread's stack.
pub unsafe fn create_tcb(
    info: &TcbCreateInfo,
    base_paddr: PhysAddr,
) -> Result<(), CreateError> {
    let mut tcb_km = KernelMut::<Tcb>::from_paddr(base_paddr);
    let mut stack_km = KernelMut::<u64>::from_paddr(info.stack_paddr);
    // SAFETY: KernelMut pointer is the kernel VA of the stack page
    let stack_va = stack_km.as_mut_ptr() as usize as u64;
    let stack_top = stack_va + PAGE_SIZE;

    // Write canary at stack bottom
    ptr::write_volatile(stack_km.as_mut_ptr(), lockjaw_types::constants::STACK_CANARY);

    // Set up synthetic SavedContext at top of stack so context_switch
    // can "return" into this thread. The SavedContext struct layout is
    // tied to the assembly via compile-time offset assertions in context.rs.
    let saved_ctx_sp = stack_top - core::mem::size_of::<SavedContext>() as u64;

    extern "C" {
        fn thread_entry();
    }
    // SAFETY: writing into the allocated stack page, above the canary.
    let ctx_ptr = saved_ctx_sp as *mut SavedContext;
    ptr::write(ctx_ptr, SavedContext {
        // x19 = entry function pointer — thread_entry trampoline reads this
        x19: info.entry as u64,
        // LR = thread_entry trampoline address
        // SAFETY: function pointer to kernel code
        lr: thread_entry as *const () as u64,
        // All other callee-saved regs start at zero
        x20: 0, x21: 0, x22: 0, x23: 0, x24: 0,
        x25: 0, x26: 0, x27: 0, x28: 0, x29: 0,
    });

    // Write the TCB
    ptr::write(tcb_km.as_mut_ptr(), Tcb {
        header: ObjectHeader {
            obj_type: ObjectType::ThreadControlBlock,
            page_count: 1,
        },
        saved_sp: saved_ctx_sp,
        entry: info.entry,
        stack_base: stack_va,
        handle_table_paddr: info.handle_table_paddr.as_u64(),
        ttbr0_paddr: info.ttbr0_paddr.as_u64(),
        ipc_blocked_on: 0,
        ipc_msg: [0; 4],
        ipc_queue_next: 0,
        ipc_wait_kind: 0,
        current_reply_paddr: 0,
        ipc_call_reply_paddr: 0,
        user_entry_point: info.user_entry_point,
        user_stack_top: info.user_stack_top,
        user_stack_base: info.user_stack_base,
        wait_objects: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
        wait_thresholds: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
        wait_types: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
        wait_count: 0,
        current_syscall: u64::MAX,
        current_syscall_args: [0; 4],
        name: info.name,
    });

    Ok(())
}
