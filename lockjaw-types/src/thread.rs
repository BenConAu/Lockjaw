/// Thread layout types — the contract between Rust and assembly.
///
/// SavedContext defines the callee-saved register frame that
/// context_switch pushes/pops. Tcb defines the per-thread state
/// stored in a donated page. Both are repr(C) with compile-time
/// offset assertions because their layout is load-bearing:
///
/// - Assembly (context_switch, thread_entry) accesses SavedContext
///   fields at hard-coded offsets.
/// - Crash diagnostics (print_thread_context) reads Tcb fields via
///   raw pointer + byte offset to avoid debug-mode alignment panics.
///
/// These structs live in lockjaw-types so layout invariants are
/// host-testable. The kernel accesses them via KernelRef/KernelMut.

use crate::object::ObjectHeader;
use crate::wait::MAX_WAIT_OBJECTS;

// ---------------------------------------------------------------------------
// SavedContext — callee-saved register frame
// ---------------------------------------------------------------------------

/// Callee-saved register frame pushed/popped by context_switch.
/// Layout must match the stp/ldp pairs in the assembly.
#[repr(C)]
pub struct SavedContext {
    pub x19: u64, pub x20: u64,
    pub x21: u64, pub x22: u64,
    pub x23: u64, pub x24: u64,
    pub x25: u64, pub x26: u64,
    pub x27: u64, pub x28: u64,
    pub x29: u64, pub lr: u64,
}

// Compile-time assertions tying struct layout to the assembly offsets.
// If the struct gains a field or changes order, these fail immediately
// instead of silently corrupting the context switch.
const _: () = {
    assert!(core::mem::offset_of!(SavedContext, x19) == 0 * 8);
    assert!(core::mem::offset_of!(SavedContext, x20) == 1 * 8);
    assert!(core::mem::offset_of!(SavedContext, x29) == 10 * 8);
    assert!(core::mem::offset_of!(SavedContext, lr) == 11 * 8);
    assert!(core::mem::size_of::<SavedContext>() == 12 * 8);
};

// ---------------------------------------------------------------------------
// ThreadBootstrap — new thread bootstrap frame
// ---------------------------------------------------------------------------

/// The bootstrap frame for a new thread: a SavedContext placed at the
/// top of the kernel stack, and the saved_sp that context_switch will
/// load to resume this thread.
///
/// These two values must always be computed together — saved_sp is
/// stack_top minus the SavedContext size. Splitting them apart is a
/// bug waiting to happen.
pub struct ThreadBootstrap {
    pub saved_context: SavedContext,
    pub saved_sp: u64,
}

impl ThreadBootstrap {
    /// Compute the bootstrap frame for a new thread.
    ///
    /// - `entry_fn`: the kernel-side entry function (stored in x19,
    ///   called via `blr x19` by the thread_entry trampoline)
    /// - `thread_entry_addr`: address of the thread_entry trampoline
    ///   (stored in lr so context_switch "returns" to it)
    /// - `stack_top`: VA of the top of the kernel stack page
    pub fn new(entry_fn: u64, thread_entry_addr: u64, stack_top: u64) -> Self {
        let saved_sp = stack_top - core::mem::size_of::<SavedContext>() as u64;
        Self {
            saved_context: SavedContext {
                x19: entry_fn,
                lr: thread_entry_addr,
                x20: 0, x21: 0, x22: 0, x23: 0, x24: 0,
                x25: 0, x26: 0, x27: 0, x28: 0, x29: 0,
            },
            saved_sp,
        }
    }
}

// ---------------------------------------------------------------------------
// TCB — Thread Control Block
// ---------------------------------------------------------------------------
// Thread scheduling state (Ready/Running/Blocked) is NOT stored here.
// The scheduler's pure state machine in lockjaw_types::scheduler owns
// it. The TCB holds hardware-facing context (saved SP, entry point,
// etc.) plus IPC bookkeeping and object metadata.
//
// This struct moves to lockjaw-types because its layout is a shared
// contract and host-testable invariant — not because its mutation
// logic becomes pure. Many fields are scheduler/IPC runtime
// bookkeeping that only the kernel mutates via KernelRef/KernelMut.

/// Thread Control Block. Stored at the start of a donated page.
#[repr(C)]
pub struct Tcb {
    pub header: ObjectHeader,
    pub saved_sp: u64,
    pub entry: fn() -> !,
    pub stack_base: u64,
    /// Physical address of the owning ProcessObject. Every thread belongs
    /// to a process. The process owns the address space (TTBR0) and handle
    /// table. Access via process_ops narrow accessors.
    pub process_paddr: u64,
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
    /// Argument passed to the new thread's entry point in x0 (0 for process first thread).
    pub user_arg: u64,
    /// Objects this thread is waiting on via sys_wait_any (paddrs, 0 = unused).
    pub wait_objects: [u64; MAX_WAIT_OBJECTS],
    /// Per-object thresholds for the wait (notification target values).
    pub wait_thresholds: [u64; MAX_WAIT_OBJECTS],
    /// Object types for each wait entry (ObjectType as u8).
    pub wait_types: [u8; MAX_WAIT_OBJECTS],
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
const _: () = assert!(core::mem::size_of::<Tcb>() <= crate::addr::PAGE_SIZE as usize);

// ---------------------------------------------------------------------------
// TcbCreateInfo — Vulkan-style create-info
// ---------------------------------------------------------------------------

/// Describes a thread to create. Pure data container — the kernel's
/// create_tcb() writes these values into a donated page.
pub struct TcbCreateInfo {
    pub entry: fn() -> !,
    pub stack_paddr: crate::addr::PhysAddr,
    pub process_paddr: crate::addr::PhysAddr,
    pub user_entry_point: u64,
    pub user_stack_base: u64,
    pub user_stack_top: u64,
    pub user_arg: u64,
    pub name: [u8; 16],
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- SavedContext layout (assembly ABI contract) ---

    #[test]
    fn saved_context_size_is_96_bytes() {
        assert_eq!(core::mem::size_of::<SavedContext>(), 96);
    }

    #[test]
    fn saved_context_x19_at_offset_0() {
        // WHY: context_switch assembly stores x19 at [sp, #(0 * 8)]
        assert_eq!(core::mem::offset_of!(SavedContext, x19), 0);
    }

    #[test]
    fn saved_context_lr_at_offset_88() {
        // WHY: context_switch assembly stores x30 (lr) at [sp, #(10 * 8)]
        // and restores it before ret — lr determines where the thread resumes
        assert_eq!(core::mem::offset_of!(SavedContext, lr), 88);
    }

    // --- ThreadBootstrap (new thread frame) ---

    #[test]
    fn bootstrap_sets_x19_and_lr() {
        let boot = ThreadBootstrap::new(0xAAAA, 0xBBBB, 0x1_0000);
        assert_eq!(boot.saved_context.x19, 0xAAAA);
        assert_eq!(boot.saved_context.lr, 0xBBBB);
    }

    #[test]
    fn bootstrap_zeroes_other_regs() {
        let boot = ThreadBootstrap::new(0xAAAA, 0xBBBB, 0x1_0000);
        assert_eq!(boot.saved_context.x20, 0);
        assert_eq!(boot.saved_context.x21, 0);
        assert_eq!(boot.saved_context.x22, 0);
        assert_eq!(boot.saved_context.x23, 0);
        assert_eq!(boot.saved_context.x24, 0);
        assert_eq!(boot.saved_context.x25, 0);
        assert_eq!(boot.saved_context.x26, 0);
        assert_eq!(boot.saved_context.x27, 0);
        assert_eq!(boot.saved_context.x28, 0);
        assert_eq!(boot.saved_context.x29, 0);
    }

    #[test]
    fn bootstrap_saved_sp_is_stack_top_minus_96() {
        let stack_top = 0x1_0000u64;
        let boot = ThreadBootstrap::new(0, 0, stack_top);
        assert_eq!(boot.saved_sp, stack_top - 96);
    }

    // --- Tcb layout (crash diagnostic contract) ---

    #[test]
    fn tcb_fits_in_page() {
        assert!(core::mem::size_of::<Tcb>() <= 4096);
    }

    #[test]
    fn tcb_name_at_expected_offset() {
        // WHY: crash.rs:75 reads the thread name at this byte offset
        // via raw pointer arithmetic (no &Tcb reference, avoids
        // debug-mode alignment panics). If this offset changes,
        // crash diagnostics silently read garbage.
        let offset = core::mem::offset_of!(Tcb, name);
        assert_eq!(offset, 256, "Tcb::name offset changed — update crash.rs:75");
    }

    #[test]
    fn tcb_current_syscall_at_expected_offset() {
        // WHY: crash.rs:89 reads the current syscall number at this
        // byte offset via raw pointer arithmetic. A wrong offset
        // means crash output shows garbage syscall names.
        let offset = core::mem::offset_of!(Tcb, current_syscall);
        assert_eq!(offset, 216, "Tcb::current_syscall offset changed — update crash.rs:89");
    }

    #[test]
    fn tcb_current_syscall_args_at_expected_offset() {
        // WHY: crash.rs:97 reads the 4 syscall arguments at this
        // byte offset. Wrong offset = wrong args in crash dump.
        let offset = core::mem::offset_of!(Tcb, current_syscall_args);
        assert_eq!(offset, 224, "Tcb::current_syscall_args offset changed — update crash.rs:97");
    }
}
