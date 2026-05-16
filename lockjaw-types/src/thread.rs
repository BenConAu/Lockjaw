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

use crate::addr::{KernelImageVa, KernelVa};
use crate::object::ObjectHeader;
use crate::time::MonoTicks;
use crate::wait::MAX_WAIT_OBJECTS;

// ---------------------------------------------------------------------------
// KernelStackBase — typed regime for the kernel stack page
// ---------------------------------------------------------------------------

/// The kernel-side base address of a thread's kernel stack page,
/// tagged with which VA regime owns the backing.
///
/// The two regimes have different lifecycles:
/// - `Image`: the boot stack reserved by the linker (`__stack_bottom`).
///   Lives for the kernel's lifetime. Only the CPU 0 boot TCB (becomes
///   init) uses this — the page is part of the kernel image and is
///   never freed.
/// - `Pool`: a 4 KB page allocated from the KVM pool by the kernel
///   stack allocator. Freed via `kvm::free_kernel_pages` when the
///   thread exits.
///
/// `finish_exit` matches on the variant so the wrong free path
/// is unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum KernelStackBase {
    Image(KernelImageVa),
    Pool(KernelVa),
}

impl KernelStackBase {
    /// The raw stack-base address as a u64. Used by the canary check
    /// and crash diagnostics, both of which only need to dereference
    /// the address — they do not care about the regime.
    pub fn as_u64(self) -> u64 {
        match self {
            KernelStackBase::Image(va) => va.as_u64(),
            KernelStackBase::Pool(va) => va.as_u64(),
        }
    }
}

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
    /// Kernel stack base, regime-tagged. `Image` = boot stack from
    /// the linker (CPU 0 boot TCB only); `Pool` = KVM-allocated dynamic
    /// stack (everything else). `finish_exit` matches on this so
    /// the wrong free path is unrepresentable.
    pub stack_base: KernelStackBase,
    /// KVA of the owning ProcessObject. Every thread belongs to a
    /// process. The process owns the address space (TTBR0) and handle
    /// table. Access via process_ops narrow accessors.
    /// ProcessObject lives in the KVM pool — see kernel-vmem-roadmap.md.
    pub process_kva: u64,
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
    /// Caller token from the endpoint handle used for this send/call.
    /// Set on sys_send/sys_call, read by sys_receive on dequeue.
    pub ipc_caller_token: u64,
    /// Server-side: KVA of the Reply object bound to the call currently
    /// being handled by this thread. Set by sys_receive when dequeuing a
    /// Call; cleared by sys_reply. 0 = no outstanding call.
    /// Reply objects live in the KVM pool — see kernel-vmem-roadmap.md.
    pub current_reply_kva: u64,
    /// Token of the most recently dequeued sender/caller on this thread.
    /// Written on every successful sys_receive/sys_recv_nb dequeue.
    /// Read by sys_query_caller_token. Overwritten on next dequeue.
    pub last_caller_token: u64,
    /// Caller-side: KVA of this thread's own Reply object while queued
    /// as a Call waiter, so the server can pick it up on sys_receive.
    /// 0 when not queued as a Call.
    pub ipc_call_reply_kva: u64,
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
    /// Number of valid entries in wait_objects (0 = not in a sys_wait_any
    /// — but a future S3 will allow `count == 0` as a pure-sleep variant
    /// driven by `wait_deadline` alone).
    pub wait_count: u8,
    /// Absolute monotonic deadline (CNTVCT_EL0 ticks) for the current
    /// `sys_wait_any`. `MonoTicks::NO_DEADLINE` (= u64::MAX) means
    /// "no timeout — wait indefinitely on object readiness". Set on
    /// syscall entry, scanned by the per-tick deadline walker, cleared
    /// on syscall return. Stored as raw u64 because `Tcb` is `repr(C)`
    /// and crossed by raw-pointer accessors; the `MonoTicks` newtype
    /// is reconstructed at the use sites.
    pub wait_deadline: u64,
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

impl Tcb {
    /// Initialize a TCB in place through a raw pointer. The page must
    /// already be zeroed (most fields are zero-default). This avoids
    /// materializing a full Tcb on the kernel stack.
    ///
    /// Writes the header, entry function, and current_syscall sentinel
    /// (u64::MAX, since 0 is a valid syscall number). All other fields
    /// remain zero from the page zeroing.
    ///
    /// # Safety
    /// `ptr` must point to a zeroed, page-aligned, kernel-owned page.
    pub unsafe fn init_in_place(ptr: *mut Self, entry: fn() -> !) {
        (*ptr).header = ObjectHeader {
            obj_type: crate::object::ObjectType::ThreadControlBlock,
            page_count: 1,
            refcount: 0,
        };
        (*ptr).entry = entry;
        (*ptr).current_syscall = u64::MAX;
        (*ptr).wait_deadline = MonoTicks::NO_DEADLINE.0; // No outstanding sleep on a fresh TCB.
    }
}

// ---------------------------------------------------------------------------
// TcbCreateInfo — Vulkan-style create-info
// ---------------------------------------------------------------------------

/// Describes a thread to create. Pure data container — the kernel's
/// create_tcb() writes these values into a donated page.
pub struct TcbCreateInfo {
    pub entry: fn() -> !,
    /// Kernel stack base, regime-tagged. `create_tcb` writes this
    /// straight into `Tcb.stack_base`; `finish_exit` later matches
    /// on the variant to choose the correct free path.
    pub stack: KernelStackBase,
    /// KVA of the owning ProcessObject — kernel objects live in the
    /// KVM pool. See kernel-vmem-roadmap.md.
    pub process_kva: crate::addr::KernelVa,
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

    // --- Tcb layout ---

    #[test]
    fn tcb_fits_in_page() {
        assert!(core::mem::size_of::<Tcb>() <= 4096);
    }

    #[test]
    fn tcb_init_in_place_sets_wait_deadline_to_no_deadline() {
        // WHY: a freshly-created thread has no outstanding sleep. The
        // per-tick deadline scan must see NO_DEADLINE and skip it,
        // otherwise a thread could appear "expired" at t=0 because
        // the page-zeroed default (0) is in the past for any nonzero
        // CNTVCT_EL0 reading.
        let mut storage = [0u8; core::mem::size_of::<Tcb>()];
        let ptr = storage.as_mut_ptr() as *mut Tcb;
        fn dummy() -> ! { loop {} }
        unsafe { Tcb::init_in_place(ptr, dummy); }
        let tcb = unsafe { &*ptr };
        assert_eq!(tcb.wait_deadline, MonoTicks::NO_DEADLINE.0);
        assert_eq!(tcb.wait_deadline, u64::MAX);
    }
}
