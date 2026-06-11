use crate::cap::object::CreateError;
use crate::mm::addr::PAGE_SIZE;
use crate::mm::kernel_ptr::KernelMut;
use crate::sched::context::SavedContext;
use core::ptr;
use lockjaw_types::addr::KernelVa;

// Struct definitions and layout assertions live in lockjaw-types
// (host-testable). Re-export so all existing kernel import sites
// (`use crate::sched::tcb::Tcb`) work unchanged.
pub use lockjaw_types::thread::{KernelStackBase, Tcb, TcbCreateInfo, ThreadBootstrap};

/// Initialize a TCB in a KVM-mapped page and set up its stack with a
/// synthetic frame so it can be context-switched into.
///
/// Rejects kernel-process TCBs (`info.process_kva` with TTBR0 = 0).
/// The reason is a substrate-level invariant: kernel-mode threads
/// run with `DAIF.I` masked (see `src/sched/context.rs`'s
/// `thread_entry` doc — "kernel threads run under GKL with IRQs
/// masked"). A tight loop in such a thread starves IRQ delivery on
/// its CPU, breaking timer ticks, deadline wakes, and IPC unblock —
/// see the deleted ipc_sender/ipc_receiver bench threads, which
/// concretely deadlocked the system in single-CPU QEMU once init
/// stopped busy-spinning. The boot TCB is the only legitimate
/// kernel-process TCB and uses `create_boot_tcb` (a separate
/// constructor); every TCB created through `create_tcb` must run
/// in a user process so it returns to EL0 between syscalls and
/// allows IRQ delivery.
///
/// # Safety
/// `base_kva` must be a kernel-writable VA in one of two regimes:
/// - KVM-allocated page (`info.stack == KernelStackBase::Pool(_)`,
///   NK3-era `sys_create_thread`); or
/// - TTBR1 direct-map page from a donated PageSet
///   (`info.stack == KernelStackBase::DirectMap(_)`, NK4+
///   `sys_create_process`).
/// The CPU 0 boot TCB uses `create_boot_tcb` with
/// `KernelStackBase::Image` and never reaches this path.
pub unsafe fn create_tcb(
    info: &TcbCreateInfo,
    base_kva: KernelVa,
) -> Result<(), CreateError> {
    // Reject kernel-process TCBs — see the doc on this fn for why.
    // Kernel processes are identified by TTBR0 = 0 (no user address
    // space). Only the boot TCB legitimately runs in the kernel
    // process and it uses `create_boot_tcb`, never this path.
    if crate::cap::process_obj::process_ttbr0(info.process_kva) == 0 {
        return Err(CreateError::InvalidParameter);
    }
    let stack_kva = match info.stack {
        // Both regimes carry a usable kstack KVA at variant payload;
        // create_tcb writes the saved-context frame the same way for
        // both. finish_exit later dispatches the free path per
        // stack_base.regime() (see lockjaw-types/src/thread.rs).
        KernelStackBase::Pool(kva) | KernelStackBase::DirectMap(kva) => kva,
        KernelStackBase::Image(_) => {
            // Image stacks belong to the CPU 0 boot TCB which uses
            // create_boot_tcb; reaching this branch is an invariant
            // violation.
            return Err(CreateError::InvalidParameter);
        }
    };

    let mut tcb_km = KernelMut::<Tcb>::from_kva(base_kva);
    let mut stack_km = KernelMut::<u64>::from_kva(stack_kva);
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
    // SAFETY: base_kva is a freshly-allocated KVM range; we own it.
    {
        let mut p = KernelMut::<u8>::from_kva(base_kva);
        ptr::write_bytes(p.as_mut_ptr(), 0, PAGE_SIZE as usize);
    }
    let p = tcb_km.as_mut_ptr();
    Tcb::init_in_place(p, info.entry);
    (*p).saved_sp = boot.saved_sp;
    (*p).stack_base = info.stack;
    (*p).process_kva = info.process_kva.as_u64();
    (*p).user_entry_point = info.user_entry_point;
    (*p).user_stack_top = info.user_stack_top;
    (*p).user_stack_base = info.user_stack_base;
    (*p).user_arg = info.user_arg;
    (*p).name = info.name;

    Ok(())
}
