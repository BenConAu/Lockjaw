use crate::cap::object::CreateError;
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

    // Initialize TCB in place — zero the page first, then write
    // header + non-default fields through the pointer. No by-value
    // Tcb on the kernel stack.
    crate::mm::page_alloc::zero_page(base_paddr);
    let p = tcb_km.as_mut_ptr();
    Tcb::init_in_place(p, info.entry);
    (*p).saved_sp = boot.saved_sp;
    (*p).stack_base = stack_va;
    (*p).process_paddr = info.process_paddr.as_u64();
    (*p).user_entry_point = info.user_entry_point;
    (*p).user_stack_top = info.user_stack_top;
    (*p).user_stack_base = info.user_stack_base;
    (*p).user_arg = info.user_arg;
    (*p).name = info.name;

    Ok(())
}
