use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET, PAGE_SIZE};
use core::ptr;

// ---------------------------------------------------------------------------
// Thread state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Ready,
    Running,
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
}

// ---------------------------------------------------------------------------
// Vulkan-style create-info
// ---------------------------------------------------------------------------

/// Describes a thread to create.
pub struct TcbCreateInfo {
    pub entry: fn() -> !,
    pub stack_paddr: PhysAddr,
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
    ptr::write_volatile(canary_ptr, 0xDEAD_BEEF_DEAD_BEEF);

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

    // Write the TCB header
    ptr::write(tcb_va, Tcb {
        header: ObjectHeader {
            obj_type: ObjectType::ThreadControlBlock,
            page_count: 1,
        },
        saved_sp: saved_ctx_sp,
        state: ThreadState::Ready,
        entry: info.entry,
        stack_base: stack_va,
    });

    Ok(())
}
