use crate::cap::object::{ObjectType, ObjectHeader, CreateError};
use crate::mm::addr::{PhysAddr, PAGE_SIZE};
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::context::SavedContext;
use core::ptr;

// Struct definitions and layout assertions live in lockjaw-types
// (host-testable). Re-export so all existing kernel import sites
// (`use crate::sched::tcb::Tcb`) work unchanged.
pub use lockjaw_types::thread::{Tcb, TcbCreateInfo, ThreadBootstrap};

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
    // can "return" into this thread. ThreadBootstrap computes the
    // SavedContext and saved_sp together (they must stay in sync).
    extern "C" {
        fn thread_entry();
    }
    let boot = ThreadBootstrap::new(
        info.entry as u64,
        // SAFETY: thread_entry is a kernel code address (global_asm symbol)
        thread_entry as *const () as u64,
        stack_top,
    );
    // SAFETY: writing into the allocated stack page, above the canary.
    ptr::write(boot.saved_sp as *mut SavedContext, boot.saved_context);

    // Write the TCB
    ptr::write(tcb_km.as_mut_ptr(), Tcb {
        header: ObjectHeader {
            obj_type: ObjectType::ThreadControlBlock,
            page_count: 1,
            refcount: 0, // TCBs are not handle-tracked
        },
        saved_sp: boot.saved_sp,
        entry: info.entry,
        stack_base: stack_va,
        process_paddr: info.process_paddr.as_u64(),
        ipc_blocked_on: 0,
        ipc_msg: [0; 4],
        ipc_queue_next: 0,
        ipc_wait_kind: 0,
        current_reply_paddr: 0,
        ipc_call_reply_paddr: 0,
        user_entry_point: info.user_entry_point,
        user_stack_top: info.user_stack_top,
        user_stack_base: info.user_stack_base,
        user_arg: info.user_arg,
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
