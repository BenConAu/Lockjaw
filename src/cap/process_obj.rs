use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::{KernelMut, KernelRef};
use lockjaw_types::addr::KernelVa;
use lockjaw_types::object::{ObjectHeader, ObjectType};
use lockjaw_types::process::{self, ProcessLifecycle, TransferError, MAX_CONSUMED_HEADERS};

// ---------------------------------------------------------------------------
// ProcessObject — kernel-side live struct, stored in a KVM-pool page
// ---------------------------------------------------------------------------

/// A process owns an address space (TTBR0) and a handle table (capability
/// namespace). Threads belong to a process via TCB.process_kva.
/// thread_count tracks live threads; when it hits zero, process resources
/// are freed (unless immortal).
///
/// ProcessObjects live in the KVM pool — kernel callers reach them via
/// `KernelMut::<ProcessObject>::from_kva`. See kernel-vmem-roadmap.md.
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
    /// Number of distinct PageSet headers consumed during creation.
    /// Populated by `process_record_consumed_header` while building
    /// the new process; read by `consume_pageset_validate` /
    /// `consume_pageset_apply` loops in `create_process`. Ephemeral
    /// to creation — no code outside `create_process` reads it.
    pub consumed_header_count: u32,
    /// Physical addresses of distinct PageSet headers consumed
    /// during creation (deduplicated). Stored here rather than on
    /// the kernel sync-exception stack: the kernel's per-thread
    /// stack is 4 KB, and this 256-byte array would not safely fit
    /// alongside the AddressSpaceBuilder + scratch state during
    /// provisioning. The proc page is the natural owner — it has
    /// the right lifetime (alive while creation is in flight, freed
    /// with the page if creation aborts).
    pub consumed_headers: [u64; MAX_CONSUMED_HEADERS],
}

// ProcessObject must fit in a single donated 4KB page.
const _: () = assert!(core::mem::size_of::<ProcessObject>() <= 4096);

/// Initialize a ProcessObject in a KVM-mapped page.
/// thread_count starts at 0. The caller must immediately call
/// process_inc_thread_count() for the first thread.
pub fn create_process_object(
    process_kva: KernelVa,
    ttbr0_paddr: u64,
    handle_table_paddr: u64,
    immortal: bool,
    name: &[u8; 16],
) {
    // Zero the page first — owned_pages (1024 bytes) starts as zeros
    // without constructing a large struct literal on the stack.
    // SAFETY: process_kva came from kvm::alloc_kernel_pages; we own it.
    unsafe {
        let mut p = KernelMut::<u8>::from_kva(process_kva);
        core::ptr::write_bytes(p.as_mut_ptr(), 0, crate::mm::addr::PAGE_SIZE as usize);
    }
    init_process_header(process_kva, ttbr0_paddr, handle_table_paddr, immortal, name);
}

/// Write ProcessObject header fields into an already-zeroed KVM page.
/// Used by create_process when owned_pages was populated first.
pub fn init_process_header(
    process_kva: KernelVa,
    ttbr0_paddr: u64,
    handle_table_paddr: u64,
    immortal: bool,
    name: &[u8; 16],
) {
    // SAFETY: process_kva is a zeroed, kernel-owned page in the KVM pool.
    let mut slot = unsafe { KernelMut::<ProcessObject>::from_kva(process_kva) };
    // Write only the header fields. owned_page_count and owned_pages
    // are preserved (may already be populated by process_push_owned_page).
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
pub fn process_ttbr0(process_kva: KernelVa) -> u64 {
    // SAFETY: process_kva is a valid ProcessObject (set by create_process).
    let p = unsafe { KernelRef::<ProcessObject>::from_kva(process_kva) };
    p.get().ttbr0_paddr
}

/// Read the handle table physical address for this process.
pub fn process_handle_table(process_kva: KernelVa) -> PhysAddr {
    // SAFETY: process_kva is a valid ProcessObject.
    let p = unsafe { KernelRef::<ProcessObject>::from_kva(process_kva) };
    PhysAddr::new(p.get().handle_table_paddr)
}

/// Increment the thread count (a new thread was created in this process).
/// Calls the pure model in lockjaw-types and writes back the new count.
pub fn process_inc_thread_count(process_kva: KernelVa) {
    // SAFETY: process_kva is a valid ProcessObject.
    let mut p = unsafe { KernelMut::<ProcessObject>::from_kva(process_kva) };
    let old_count = p.get().thread_count;
    let new_count = process::on_thread_create(old_count);
    p.get_mut().thread_count = new_count;
}

/// Append one page to this process's owned_pages array, deduplicating.
/// The page will be freed when the last thread exits.
/// Returns false if the array is full (MAX_OWNED_PAGES reached).
pub fn process_push_owned_page(process_kva: KernelVa, page_paddr: u64) -> bool {
    // SAFETY: process_kva is a valid ProcessObject (page already zeroed).
    let mut p = unsafe { KernelMut::<ProcessObject>::from_kva(process_kva) };
    let proc = p.get_mut();
    let count = proc.owned_page_count as usize;
    // Dedup: skip if already present
    for i in 0..count {
        if proc.owned_pages[i] == page_paddr {
            return true;
        }
    }
    if count >= process::MAX_OWNED_PAGES {
        return false;
    }
    proc.owned_pages[count] = page_paddr;
    proc.owned_page_count = (count + 1) as u32;
    true
}

/// Number of process-owned pages.
pub fn process_owned_page_count(process_kva: KernelVa) -> u32 {
    // SAFETY: process_kva is a valid ProcessObject.
    let p = unsafe { KernelRef::<ProcessObject>::from_kva(process_kva) };
    p.get().owned_page_count
}

/// Read one owned page by index. Returns None if out of range.
pub fn process_owned_page(process_kva: KernelVa, index: usize) -> Option<u64> {
    // SAFETY: process_kva is a valid ProcessObject.
    let p = unsafe { KernelRef::<ProcessObject>::from_kva(process_kva) };
    let proc = p.get();
    if index < proc.owned_page_count as usize {
        Some(proc.owned_pages[index])
    } else {
        None
    }
}

/// Append a PageSet header KVA to this process's `consumed_headers`
/// list, deduplicating. Used by `create_process` while building
/// the new process — keeps the deduplicated list off the kernel
/// stack. Returns `Ok(true)` if the header was new (added),
/// `Ok(false)` if it was already present, `Err(TooManyHeaders)`
/// if the array is full.
///
/// The proc-page storage is a u64 array (the dedup helper is generic
/// over u64); we cast to/from `KernelVa` at the boundary so callers
/// see typed values.
pub fn process_record_consumed_header(
    process_kva: KernelVa,
    header_kva: KernelVa,
) -> Result<bool, TransferError> {
    // SAFETY: process_kva is a valid ProcessObject (page already zeroed
    // by create_process). The dedup helper takes the array slice + count
    // by mutable reference; we pass the proc page's storage directly.
    let mut p = unsafe { KernelMut::<ProcessObject>::from_kva(process_kva) };
    let proc = p.get_mut();
    let mut count = proc.consumed_header_count as usize;
    let result = process::dedup_add_header(header_kva.as_u64(), &mut proc.consumed_headers, &mut count);
    proc.consumed_header_count = count as u32;
    result
}

/// Number of distinct PageSet headers consumed during creation.
/// Read by `create_process` apply paths; not currently called from
/// elsewhere but mirrors `process_owned_page_count` for symmetry.
#[allow(dead_code)]
pub fn process_consumed_header_count(process_kva: KernelVa) -> u32 {
    // SAFETY: process_kva is a valid ProcessObject.
    let p = unsafe { KernelRef::<ProcessObject>::from_kva(process_kva) };
    p.get().consumed_header_count
}

/// Read one consumed PageSet header KVA by index. Returns None if
/// out of range.
pub fn process_consumed_header(process_kva: KernelVa, index: usize) -> Option<KernelVa> {
    // SAFETY: process_kva is a valid ProcessObject.
    let p = unsafe { KernelRef::<ProcessObject>::from_kva(process_kva) };
    let proc = p.get();
    if index < proc.consumed_header_count as usize {
        Some(KernelVa::new(proc.consumed_headers[index]))
    } else {
        None
    }
}

/// Decrement the thread count (a thread exited from this process).
/// Calls the pure model in lockjaw-types, writes back the new count,
/// and returns the lifecycle outcome. Callers match on the result:
/// - ThreadsRemaining(n): process stays alive, count updated
/// - LastThread: caller must free process resources
/// - Immortal(n): count updated, no teardown (kernel process)
pub fn process_dec_thread_count(process_kva: KernelVa) -> ProcessLifecycle {
    // SAFETY: process_kva is a valid ProcessObject.
    let mut p = unsafe { KernelMut::<ProcessObject>::from_kva(process_kva) };
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
