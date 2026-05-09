/// Process lifetime model.
///
/// Pure decision logic for process thread-count transitions.
/// The kernel calls these functions to determine lifecycle outcomes;
/// this pushes the invariants into testable code.

/// Maximum number of process-owned pages (transferred from parent
/// via sys_create_process). 128 pages = 512KB of process image.
/// sys_create_process fails cleanly if this limit is exceeded.
pub const MAX_OWNED_PAGES: usize = 128;

/// A mapping entry provided by userspace in the sys_create_process call.
/// Lives in the caller's mapped memory — the kernel reads it one at a time.
/// Shared between kernel and userspace (single source of truth).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessMapping {
    /// Virtual address in the new process's address space.
    pub virt_addr: u64,
    /// PageSet handle (from sys_alloc_pages) containing the physical page.
    pub pageset_id: u64,
    /// Index of the page within the PageSet (0 for single-page sets).
    pub page_index: u64,
    /// Flags: bit 0 = executable.
    pub flags: u64,
}

/// ProcessMapping flag: page contains executable code.
pub const PROCESS_MAP_FLAG_EXECUTABLE: u64 = 1 << 0;

/// Number of `ProcessMapping` entries that fit in a single 4 KB page.
/// Userspace process loaders allocate the mapping array in N×4 KB pages
/// and use this constant as the per-page capacity. Pairing the loader's
/// plan-buffer cap with the same constant keeps the two in lock-step:
/// a plan can never produce more entries than the array can hold.
pub const PROCESS_MAPPINGS_PER_PAGE: usize =
    crate::addr::PAGE_SIZE as usize / core::mem::size_of::<ProcessMapping>();

// SAFETY: ProcessMapping is repr(C) with only u64 fields — every bit pattern valid.
unsafe impl crate::user_pod::UserPod for ProcessMapping {}

/// Outcome of a thread exiting from a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessLifecycle {
    /// Process has remaining threads. Contains the new count.
    ThreadsRemaining(u32),
    /// Last thread exited. Caller must free process resources.
    LastThread,
    /// Process is immortal (kernel process). Count is decremented
    /// but process resources are never freed. Contains the new count.
    Immortal(u32),
}

impl ProcessLifecycle {
    /// Return the variant name as a static string.
    pub fn name(&self) -> &'static str {
        match self {
            ProcessLifecycle::ThreadsRemaining(_) => "ThreadsRemaining",
            ProcessLifecycle::LastThread => "LastThread",
            ProcessLifecycle::Immortal(_) => "Immortal",
        }
    }
}

/// Pure decision: what happens when a thread in this process exits?
///
/// Always decrements the count. `immortal` means "do not free process
/// resources," not "do not update count." Panics if `thread_count == 0`
/// (precondition: at least one thread must exist to exit).
pub fn on_thread_exit(thread_count: u32, immortal: bool) -> ProcessLifecycle {
    assert!(thread_count > 0, "on_thread_exit: no threads to exit");
    let new_count = thread_count - 1;
    if immortal {
        return ProcessLifecycle::Immortal(new_count);
    }
    if new_count == 0 {
        ProcessLifecycle::LastThread
    } else {
        ProcessLifecycle::ThreadsRemaining(new_count)
    }
}

/// Pure increment for thread creation. Returns the new count.
/// Panics on overflow.
pub fn on_thread_create(thread_count: u32) -> u32 {
    thread_count.checked_add(1).unwrap_or_else(|| panic!("thread count overflow"))
}

// ---------------------------------------------------------------------------
// Process ownership transfer plan
// ---------------------------------------------------------------------------

/// Maximum number of distinct PageSet headers that can be consumed
/// during a single sys_create_process. 32 is generous — a process
/// typically has 3–5 PageSets (code, rodata, data, bss, stack).
pub const MAX_CONSUMED_HEADERS: usize = 32;

/// Errors from building a process transfer plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferError {
    /// consumed_headers array is full (MAX_CONSUMED_HEADERS reached).
    TooManyHeaders,
}

/// Pure deduplication for the PageSet headers a `sys_create_process`
/// must consume. The kernel walks the user-supplied mapping list and
/// pushes every referenced header through `add_header`; the plan
/// collapses duplicates so the consume loop runs exactly once per
/// unique header.
///
/// Without dedup, an N-page PageSet mapped at N VAs would yield N
/// validate/apply pairs against the same header — the second pair
/// would see refcount == 0 (revoke_apply already drained it) and
/// double-consume the global PageSet table slot.
///
/// Side effects (revoke walk, PTE ops, handle table mutation) live
/// in the kernel. This struct only owns the deduplicated header set.
pub struct ProcessTransferPlan {
    headers: [u64; MAX_CONSUMED_HEADERS],
    header_count: usize,
}

impl ProcessTransferPlan {
    pub fn new() -> Self {
        Self {
            headers: [0; MAX_CONSUMED_HEADERS],
            header_count: 0,
        }
    }

    /// Record a PageSet header for consumption. Deduplicates —
    /// multiple mappings from the same PageSet produce one entry.
    pub fn add_header(&mut self, header_paddr: u64) -> Result<(), TransferError> {
        // Dedup: skip if already tracked.
        for i in 0..self.header_count {
            if self.headers[i] == header_paddr {
                return Ok(());
            }
        }
        if self.header_count >= MAX_CONSUMED_HEADERS {
            return Err(TransferError::TooManyHeaders);
        }
        self.headers[self.header_count] = header_paddr;
        self.header_count += 1;
        Ok(())
    }

    /// The deduplicated list of PageSet headers to consume.
    pub fn headers(&self) -> &[u64] {
        &self.headers[..self.header_count]
    }
}

// ---------------------------------------------------------------------------
// Process teardown plan
// ---------------------------------------------------------------------------

/// A step in the process teardown sequence. Each variant carries the
/// behavioral facts the kernel needs to execute it correctly.
/// Steps are conditional — absent resources are omitted from the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeardownStep {
    /// Free N owned pages (code/data/stack transferred from parent).
    FreeOwnedPages { count: u32 },
    /// Free address space page table tree (L0/L1/L2/L3).
    FreeAddressSpace,
    /// Handle cleanup after FreeAddressSpace has run (user process).
    /// PTEs are gone. The kernel calls decide_close_handle per entry;
    /// UnmapThenRemove entries skip the actual unmap and just dec
    /// both counters.
    CleanupHandleEntriesPtesGone,
    /// Handle cleanup for a process with no address space (kernel
    /// process). The kernel calls decide_teardown_handle per entry,
    /// which can only return DecRef or Skip — no unmap variant exists
    /// in the return type, making the illegal state unrepresentable.
    CleanupHandleEntriesNoAddressSpace,
    /// Free handle table pages.
    FreeHandleTable { page_count: u8 },
    /// Free process object page. Must be last — prior steps read
    /// from the process object.
    FreeProcessPage,
}

/// Plan for tearing down a process after its last thread exits.
/// Built from process state observations by `build_teardown_plan`.
/// The kernel iterates the steps and executes each mechanically.
pub struct ProcessTeardownPlan {
    steps: [Option<TeardownStep>; 5],
    count: usize,
}

impl ProcessTeardownPlan {
    fn new() -> Self {
        Self {
            steps: [None; 5],
            count: 0,
        }
    }

    fn push(&mut self, step: TeardownStep) {
        debug_assert!(self.count < 5, "teardown plan overflow");
        self.steps[self.count] = Some(step);
        self.count += 1;
    }

    /// Number of steps in the plan.
    pub fn step_count(&self) -> usize {
        self.count
    }

    /// Iterate the steps in execution order.
    pub fn iter(&self) -> impl Iterator<Item = &TeardownStep> {
        self.steps[..self.count].iter().filter_map(|s| s.as_ref())
    }
}

/// Build a teardown plan from process state observations.
///
/// The builder decides which steps to include and computes
/// behavioral flags (mappings_already_cleared). The kernel
/// provides the observations; types owns the plan.
pub fn build_teardown_plan(
    owned_page_count: u32,
    has_address_space: bool,
    has_handle_table: bool,
    handle_table_page_count: u8,
) -> ProcessTeardownPlan {
    let mut plan = ProcessTeardownPlan::new();

    if owned_page_count > 0 {
        plan.push(TeardownStep::FreeOwnedPages { count: owned_page_count });
    }
    if has_address_space {
        plan.push(TeardownStep::FreeAddressSpace);
    }
    if has_handle_table {
        if has_address_space {
            plan.push(TeardownStep::CleanupHandleEntriesPtesGone);
        } else {
            plan.push(TeardownStep::CleanupHandleEntriesNoAddressSpace);
        }
        plan.push(TeardownStep::FreeHandleTable {
            page_count: handle_table_page_count,
        });
    }
    // Always last — prior steps read from the process object.
    plan.push(TeardownStep::FreeProcessPage);
    plan
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_thread_exits_normal_process() {
        assert_eq!(on_thread_exit(1, false), ProcessLifecycle::LastThread);
    }

    #[test]
    fn threads_remaining_after_exit() {
        assert_eq!(on_thread_exit(2, false), ProcessLifecycle::ThreadsRemaining(1));
        assert_eq!(on_thread_exit(5, false), ProcessLifecycle::ThreadsRemaining(4));
    }

    #[test]
    fn immortal_process_decrements_but_never_freed() {
        assert_eq!(on_thread_exit(1, true), ProcessLifecycle::Immortal(0));
        assert_eq!(on_thread_exit(2, true), ProcessLifecycle::Immortal(1));
    }

    #[test]
    #[should_panic(expected = "no threads to exit")]
    fn exit_with_zero_threads_panics() {
        on_thread_exit(0, false);
    }

    #[test]
    #[should_panic(expected = "no threads to exit")]
    fn exit_with_zero_threads_immortal_panics() {
        on_thread_exit(0, true);
    }

    #[test]
    fn thread_create_increments() {
        assert_eq!(on_thread_create(0), 1);
        assert_eq!(on_thread_create(1), 2);
        assert_eq!(on_thread_create(99), 100);
    }

    #[test]
    #[should_panic(expected = "thread count overflow")]
    fn thread_create_overflow_panics() {
        on_thread_create(u32::MAX);
    }

    // -----------------------------------------------------------------------
    // ProcessTransferPlan tests
    // -----------------------------------------------------------------------

    #[test]
    fn duplicate_headers_deduped() {
        let mut plan = ProcessTransferPlan::new();
        plan.add_header(0xA000).unwrap();
        plan.add_header(0xB000).unwrap();
        plan.add_header(0xA000).unwrap(); // duplicate — collapsed
        assert_eq!(plan.headers(), &[0xA000, 0xB000]);
    }

    #[test]
    fn headers_full_returns_error() {
        let mut plan = ProcessTransferPlan::new();
        for i in 0..MAX_CONSUMED_HEADERS {
            plan.add_header(i as u64 * 0x1000).unwrap();
        }
        assert_eq!(
            plan.add_header(0xFFFF_0000),
            Err(TransferError::TooManyHeaders),
        );
    }

    #[test]
    fn stack_header_tracked_separately() {
        let mut plan = ProcessTransferPlan::new();
        plan.add_header(0xA000).unwrap(); // code PageSet
        plan.add_header(0xB000).unwrap(); // stack PageSet
        assert_eq!(plan.headers().len(), 2);
    }

    #[test]
    fn same_header_from_multiple_mappings_deduped() {
        // Models: an N-page PageSet mapped at N contiguous VAs
        // produces N add_header calls but exactly one consume_pageset
        // pair downstream.
        let mut plan = ProcessTransferPlan::new();
        plan.add_header(0xA000).unwrap();
        plan.add_header(0xA000).unwrap();
        plan.add_header(0xA000).unwrap();
        assert_eq!(plan.headers(), &[0xA000]);
    }

    #[test]
    fn empty_plan_has_empty_headers() {
        let plan = ProcessTransferPlan::new();
        assert!(plan.headers().is_empty());
    }

    #[test]
    fn dedup_after_full_does_not_overflow() {
        // Already-tracked header is accepted even when the array is
        // at capacity, since dedup short-circuits.
        let mut plan = ProcessTransferPlan::new();
        for i in 0..MAX_CONSUMED_HEADERS {
            plan.add_header(i as u64 * 0x1000).unwrap();
        }
        assert!(plan.add_header(0).is_ok()); // duplicate of headers[0]
        assert_eq!(plan.headers().len(), MAX_CONSUMED_HEADERS);
    }

    // -----------------------------------------------------------------------
    // ProcessTeardownPlan tests
    // -----------------------------------------------------------------------

    #[test]
    fn teardown_user_process_uses_ptes_gone_variant() {
        let plan = build_teardown_plan(0, true, true, 1);
        assert!(plan.iter().any(|s|
            matches!(s, TeardownStep::CleanupHandleEntriesPtesGone)));
    }

    #[test]
    fn teardown_kernel_process_uses_no_address_space_variant() {
        let plan = build_teardown_plan(0, false, true, 1);
        assert!(plan.iter().any(|s|
            matches!(s, TeardownStep::CleanupHandleEntriesNoAddressSpace)));
    }

    #[test]
    fn teardown_no_handle_table_skips_cleanup_and_free() {
        let plan = build_teardown_plan(5, true, false, 0);
        assert!(!plan.iter().any(|s|
            matches!(s, TeardownStep::CleanupHandleEntriesPtesGone
                | TeardownStep::CleanupHandleEntriesNoAddressSpace)));
        assert!(!plan.iter().any(|s|
            matches!(s, TeardownStep::FreeHandleTable { .. })));
    }

    #[test]
    fn teardown_no_owned_pages_skips_free_owned() {
        let plan = build_teardown_plan(0, true, true, 1);
        assert!(!plan.iter().any(|s|
            matches!(s, TeardownStep::FreeOwnedPages { .. })));
    }

    #[test]
    fn teardown_process_page_always_last() {
        let plan = build_teardown_plan(5, true, true, 1);
        let last = plan.iter().last().unwrap();
        assert_eq!(*last, TeardownStep::FreeProcessPage);
    }

    #[test]
    fn teardown_empty_process_only_frees_process_page() {
        let plan = build_teardown_plan(0, false, false, 0);
        assert_eq!(plan.step_count(), 1);
        assert_eq!(*plan.iter().next().unwrap(), TeardownStep::FreeProcessPage);
    }

    #[test]
    fn teardown_addr_space_freed_before_handle_cleanup() {
        let plan = build_teardown_plan(0, true, true, 1);
        let steps: [TeardownStep; 5] = core::array::from_fn(|i|
            plan.iter().nth(i).copied().unwrap_or(TeardownStep::FreeProcessPage));
        let addr_idx = steps.iter().position(|s|
            matches!(s, TeardownStep::FreeAddressSpace));
        let cleanup_idx = steps.iter().position(|s|
            matches!(s, TeardownStep::CleanupHandleEntriesPtesGone));
        assert!(addr_idx.unwrap() < cleanup_idx.unwrap());
    }

    #[test]
    fn teardown_full_process_has_all_five_steps() {
        let plan = build_teardown_plan(3, true, true, 1);
        assert_eq!(plan.step_count(), 5);
    }
}
