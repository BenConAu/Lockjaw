use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET, PAGE_SIZE};
use core::ptr;

// ---------------------------------------------------------------------------
// Thread state
// ---------------------------------------------------------------------------

/// Scheduling state of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Ready,
    Running,
    Blocked,
}

// ---------------------------------------------------------------------------
// TCB — stored in donated pages
// ---------------------------------------------------------------------------

/// Thread Control Block. Stored at the start of a donated page.
#[repr(C)]
pub struct Tcb {
    pub header: ObjectHeader,
    pub saved_sp: u64,
    pub state: ThreadState,
    pub entry: fn() -> !,
    pub stack_base: u64,
    pub handle_table_paddr: u64,
    pub ttbr0_paddr: u64,
    pub ipc_blocked_on: u64,
    /// Kernel-internal IPC mailbox. The IPC state machine writes received
    /// messages here; the syscall handler copies them to the exception
    /// context (x0-x3) for userspace. Do not use for other purposes.
    pub ipc_msg: [u64; 4],
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
}

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
    let tcb_va = (base_paddr.as_u64() + KERNEL_VA_OFFSET) as *mut Tcb;
    let stack_va = info.stack_paddr.as_u64() + KERNEL_VA_OFFSET;
    let stack_top = stack_va + PAGE_SIZE;

    // Write canary at stack bottom
    let canary_ptr = stack_va as *mut u64;
    ptr::write_volatile(canary_ptr, lockjaw_types::constants::STACK_CANARY);

    // Set up synthetic SavedContext at top of stack so context_switch
    // can "return" into this thread. SavedContext is 12 x u64 = 96 bytes.
    let saved_ctx_sp = stack_top - 96;
    let ctx = saved_ctx_sp as *mut u64;

    // Zero all callee-saved regs
    for i in 0..12 {
        ptr::write(ctx.add(i), 0);
    }

    // x19 (offset 0) = entry function pointer — thread_entry trampoline reads this
    ptr::write(ctx.add(0), info.entry as u64);

    // LR (offset 11) = thread_entry trampoline address
    extern "C" {
        fn thread_entry();
    }
    ptr::write(ctx.add(11), thread_entry as *const () as u64);

    // Write the TCB
    ptr::write(tcb_va, Tcb {
        header: ObjectHeader {
            obj_type: ObjectType::ThreadControlBlock,
            page_count: 1,
        },
        saved_sp: saved_ctx_sp,
        state: ThreadState::Ready,
        entry: info.entry,
        stack_base: stack_va,
        handle_table_paddr: info.handle_table_paddr.as_u64(),
        ttbr0_paddr: info.ttbr0_paddr.as_u64(),
        ipc_blocked_on: 0,
        ipc_msg: [0; 4],
        user_entry_point: info.user_entry_point,
        user_stack_top: info.user_stack_top,
        user_stack_base: info.user_stack_base,
        wait_objects: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
        wait_thresholds: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
        wait_types: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
        wait_count: 0,
    });

    Ok(())
}
