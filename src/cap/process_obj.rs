use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::{KernelMut, KernelRef};
use lockjaw_types::object::{ObjectHeader, ObjectType};
use lockjaw_types::process::{self, ProcessLifecycle};

// ---------------------------------------------------------------------------
// ProcessObject — kernel-side live struct, stored in a donated page
// ---------------------------------------------------------------------------

/// A process owns an address space (TTBR0) and a handle table (capability
/// namespace). Threads belong to a process via TCB.process_paddr.
/// thread_count tracks live threads; when it hits zero, process resources
/// are freed (unless immortal).
#[repr(C)]
pub struct ProcessObject {
    pub header: ObjectHeader,
    pub ttbr0_paddr: u64,
    pub handle_table_paddr: u64,
    pub thread_count: u32,
    pub immortal: bool,
    pub name: [u8; 16],
    /// Number of process-owned pages (transferred from parent via
    /// sys_create_process). Freed when the process's last thread exits.
    pub owned_page_count: u32,
    /// Physical addresses of process-owned pages.
    pub owned_pages: [u64; lockjaw_types::process::MAX_OWNED_PAGES],
}

// ProcessObject must fit in a single donated 4KB page.
const _: () = assert!(core::mem::size_of::<ProcessObject>() <= 4096);

/// Initialize a ProcessObject in a donated page.
/// thread_count starts at 0. The caller must immediately call
/// process_inc_thread_count() for the first thread.
pub fn create_process_object(
    page_paddr: PhysAddr,
    ttbr0_paddr: u64,
    handle_table_paddr: u64,
    immortal: bool,
    name: &[u8; 16],
) {
    // Zero the page first — owned_pages (1024 bytes) starts as zeros
    // without constructing a large struct literal on the stack.
    crate::mm::page_alloc::zero_page(page_paddr);

    // SAFETY: page_paddr is a freshly zeroed, kernel-owned page.
    let mut slot = unsafe { KernelMut::<ProcessObject>::from_paddr(page_paddr) };
    // Write only the header fields. owned_page_count and owned_pages
    // are already zero from the page zeroing above.
    unsafe {
        let p = slot.as_mut_ptr();
        (*p).header = ObjectHeader {
            obj_type: ObjectType::Process,
            page_count: 1,
            refcount: 0, // incremented by first handle_insert
        };
        (*p).ttbr0_paddr = ttbr0_paddr;
        (*p).handle_table_paddr = handle_table_paddr;
        (*p).thread_count = 0;
        (*p).immortal = immortal;
        (*p).name = *name;
    }
}

// ---------------------------------------------------------------------------
// Narrow per-field accessors — no &mut ProcessObject escapes
// ---------------------------------------------------------------------------
// Post-initialization mutation of ProcessObject fields goes only through
// these ops. Same pattern as CurrentThread and PerCpu.

/// Read the TTBR0 physical address for this process (0 for kernel process).
pub fn process_ttbr0(process_paddr: PhysAddr) -> u64 {
    // SAFETY: process_paddr is a valid ProcessObject (set by create_process).
    let p = unsafe { KernelRef::<ProcessObject>::from_paddr(process_paddr) };
    p.get().ttbr0_paddr
}

/// Read the handle table physical address for this process.
pub fn process_handle_table(process_paddr: PhysAddr) -> PhysAddr {
    // SAFETY: process_paddr is a valid ProcessObject.
    let p = unsafe { KernelRef::<ProcessObject>::from_paddr(process_paddr) };
    PhysAddr::new(p.get().handle_table_paddr)
}

/// Increment the thread count (a new thread was created in this process).
/// Calls the pure model in lockjaw-types and writes back the new count.
pub fn process_inc_thread_count(process_paddr: PhysAddr) {
    // SAFETY: process_paddr is a valid ProcessObject.
    let mut p = unsafe { KernelMut::<ProcessObject>::from_paddr(process_paddr) };
    let old_count = p.get().thread_count;
    let new_count = process::on_thread_create(old_count);
    p.get_mut().thread_count = new_count;
}

/// Decrement the thread count (a thread exited from this process).
/// Calls the pure model in lockjaw-types, writes back the new count,
/// and returns the lifecycle outcome. Callers match on the result:
/// - ThreadsRemaining(n): process stays alive, count updated
/// - LastThread: caller must free process resources
/// - Immortal(n): count updated, no teardown (kernel process)
pub fn process_dec_thread_count(process_paddr: PhysAddr) -> ProcessLifecycle {
    // SAFETY: process_paddr is a valid ProcessObject.
    let mut p = unsafe { KernelMut::<ProcessObject>::from_paddr(process_paddr) };
    let old_count = p.get().thread_count;
    let immortal = p.get().immortal;
    let lifecycle = process::on_thread_exit(old_count, immortal);
    // Write back the new count for all outcomes
    let new_count = match lifecycle {
        ProcessLifecycle::ThreadsRemaining(n) => n,
        ProcessLifecycle::LastThread => 0,
        ProcessLifecycle::Immortal(n) => n,
    };
    p.get_mut().thread_count = new_count;
    lifecycle
}
