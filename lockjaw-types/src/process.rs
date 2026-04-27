/// Process lifetime model.
///
/// Pure decision logic for process thread-count transitions.
/// The kernel calls these functions to determine lifecycle outcomes;
/// this pushes the invariants into testable code.

/// Maximum number of process-owned pages (transferred from parent
/// via sys_create_process). 128 pages = 512KB of process image.
/// sys_create_process fails cleanly if this limit is exceeded.
pub const MAX_OWNED_PAGES: usize = 128;

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
    thread_count.checked_add(1).expect("thread count overflow")
}

// ---------------------------------------------------------------------------
// Process ownership transfer plan
// ---------------------------------------------------------------------------

/// Maximum number of distinct PageSet headers that can be consumed
/// during a single sys_create_process. 32 is generous — a process
/// typically has 3–5 PageSets (code, rodata, data, bss, stack).
pub const MAX_CONSUMED_HEADERS: usize = 32;

/// Opaque index into the consumed-headers array, returned by
/// `ProcessTransferPlan::add_header` and consumed by `record_unmap`.
/// Prevents callers from guessing indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderIndex(usize);

impl HeaderIndex {
    /// Placeholder value for array initialization.
    pub const ZERO: Self = Self(0);
}

/// Errors from building or validating a process transfer plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferError {
    /// consumed_headers array is full (MAX_CONSUMED_HEADERS reached).
    TooManyHeaders,
    /// Some parent handle unmaps failed — cannot safely transfer.
    UnmapFailed {
        header_idx: usize,
        total: usize,
        failed: usize,
    },
}

/// Pure planning/validation for process ownership transfer.
///
/// The kernel builds this incrementally as it resolves mappings,
/// then queries it for all the tricky decisions:
/// - Did we dedup owned pages correctly?
/// - Did we dedup consumed headers correctly?
/// - If some parent unmaps failed, must we abort?
/// - Are we trying to transfer too many pages/headers?
/// - What exactly should be consumed on success?
///
/// Side effects (page alloc, PTE ops, handle table mutation) stay
/// in the kernel. This struct only holds the decision state.
/// Tracks PageSet headers for consumption and validates that parent
/// unmaps fully succeeded before committing. Owned pages are written
/// directly to the ProcessObject page by the kernel (not tracked here)
/// to avoid a 1 KB stack array.
pub struct ProcessTransferPlan {
    headers: [u64; MAX_CONSUMED_HEADERS],
    header_count: usize,
    /// Per-header: (total_mapped_handles, successfully_unmapped).
    /// Populated by the kernel after calling unmap_for_object.
    unmap_results: [(usize, usize); MAX_CONSUMED_HEADERS],
}

impl ProcessTransferPlan {
    pub fn new() -> Self {
        Self {
            headers: [0; MAX_CONSUMED_HEADERS],
            header_count: 0,
            unmap_results: [(0, 0); MAX_CONSUMED_HEADERS],
        }
    }

    /// Record a PageSet header for consumption. Deduplicates —
    /// multiple mappings from the same PageSet produce one entry.
    /// Returns the HeaderIndex for use with `record_unmap`.
    pub fn add_header(&mut self, header_paddr: u64) -> Result<HeaderIndex, TransferError> {
        // Dedup: return existing index if already tracked
        for i in 0..self.header_count {
            if self.headers[i] == header_paddr {
                return Ok(HeaderIndex(i));
            }
        }
        if self.header_count >= MAX_CONSUMED_HEADERS {
            return Err(TransferError::TooManyHeaders);
        }
        let idx = self.header_count;
        self.headers[idx] = header_paddr;
        self.header_count += 1;
        Ok(HeaderIndex(idx))
    }

    /// Record the result of unmapping parent handles for a header.
    /// `header_idx` must be a value returned by `add_header`.
    pub fn record_unmap(&mut self, header_idx: HeaderIndex, total: usize, unmapped: usize) {
        self.unmap_results[header_idx.0] = (total, unmapped);
    }

    /// Can we commit? All unmaps must have fully succeeded.
    pub fn validate(&self) -> Result<(), TransferError> {
        for i in 0..self.header_count {
            let (total, unmapped) = self.unmap_results[i];
            if unmapped != total {
                return Err(TransferError::UnmapFailed {
                    header_idx: i,
                    total,
                    failed: total - unmapped,
                });
            }
        }
        Ok(())
    }

    /// Number of successful unmaps for a header (derived from
    /// record_unmap inputs — not a second source of truth).
    pub fn successful_unmaps(&self, header_idx: HeaderIndex) -> usize {
        self.unmap_results[header_idx.0].1
    }

    /// The deduplicated list of PageSet headers to consume.
    pub fn headers(&self) -> &[u64] {
        &self.headers[..self.header_count]
    }

    /// Get header and its opaque index by position. For use in loops
    /// where record_unmap needs to be called on the same plan.
    pub fn header_at(&self, i: usize) -> Option<(HeaderIndex, u64)> {
        if i < self.header_count {
            Some((HeaderIndex(i), self.headers[i]))
        } else {
            None
        }
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
        let idx1 = plan.add_header(0xA000).unwrap();
        let idx2 = plan.add_header(0xB000).unwrap();
        let idx3 = plan.add_header(0xA000).unwrap(); // duplicate
        assert_eq!(plan.headers(), &[0xA000, 0xB000]);
        assert_eq!(idx1, idx3); // same index returned for duplicate
        assert_ne!(idx1, idx2);
    }

    #[test]
    fn headers_full_returns_error() {
        let mut plan = ProcessTransferPlan::new();
        for i in 0..MAX_CONSUMED_HEADERS {
            plan.add_header(i as u64 * 0x1000).unwrap();
        }
        assert_eq!(
            plan.add_header(0xFFFF_0000),
            Err(TransferError::TooManyHeaders)
        );
    }

    #[test]
    fn partial_unmap_blocks_commit() {
        let mut plan = ProcessTransferPlan::new();
        let idx = plan.add_header(0xA000).unwrap();
        plan.record_unmap(idx, 3, 2); // 1 failed
        assert_eq!(
            plan.validate(),
            Err(TransferError::UnmapFailed {
                header_idx: 0,
                total: 3,
                failed: 1,
            })
        );
    }

    #[test]
    fn all_unmaps_succeed_allows_commit() {
        let mut plan = ProcessTransferPlan::new();
        let idx1 = plan.add_header(0xA000).unwrap();
        let idx2 = plan.add_header(0xB000).unwrap();
        plan.record_unmap(idx1, 2, 2);
        plan.record_unmap(idx2, 1, 1);
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn zero_mapped_handles_allows_commit() {
        let mut plan = ProcessTransferPlan::new();
        let idx = plan.add_header(0xA000).unwrap();
        plan.record_unmap(idx, 0, 0); // no handles were mapped
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn successful_unmaps_derived_from_record() {
        let mut plan = ProcessTransferPlan::new();
        let idx = plan.add_header(0xA000).unwrap();
        plan.record_unmap(idx, 5, 3);
        assert_eq!(plan.successful_unmaps(idx), 3);
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
        let mut plan = ProcessTransferPlan::new();
        // Multiple mappings from the same PageSet
        plan.add_header(0xA000).unwrap();
        let idx2 = plan.add_header(0xA000).unwrap();
        assert_eq!(plan.headers().len(), 1); // deduped
        // Returned index matches the original
        assert_eq!(plan.successful_unmaps(idx2), 0);
    }

    #[test]
    fn empty_plan_validates() {
        let plan = ProcessTransferPlan::new();
        assert_eq!(plan.validate(), Ok(()));
        assert_eq!(plan.headers().len(), 0);
    }

    #[test]
    fn header_at_and_record_unmap_roundtrip() {
        let mut plan = ProcessTransferPlan::new();
        plan.add_header(0xA000).unwrap();
        plan.add_header(0xB000).unwrap();
        plan.add_header(0xC000).unwrap();
        assert_eq!(plan.headers().len(), 3);

        // Iterate by index, record unmaps
        for i in 0..plan.headers().len() {
            let (idx, _hdr) = plan.header_at(i).unwrap();
            plan.record_unmap(idx, 1, 1);
        }
        assert_eq!(plan.validate(), Ok(()));

        // Out of bounds returns None
        assert!(plan.header_at(3).is_none());
    }

    #[test]
    fn second_header_unmap_failure_detected() {
        let mut plan = ProcessTransferPlan::new();
        let idx1 = plan.add_header(0xA000).unwrap();
        let idx2 = plan.add_header(0xB000).unwrap();
        plan.record_unmap(idx1, 1, 1); // OK
        plan.record_unmap(idx2, 2, 1); // 1 failed
        assert_eq!(
            plan.validate(),
            Err(TransferError::UnmapFailed {
                header_idx: 1,
                total: 2,
                failed: 1,
            })
        );
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
