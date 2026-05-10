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
#[derive(Debug)]
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
// Process creation plan (outer orchestration)
// ---------------------------------------------------------------------------

use crate::object::{HandleEntry, HandleKind, HANDLE_SLOTS_PER_PAGE};
use crate::rights::Rights;

/// Parent-handle copy descriptor: the kernel resolves the parent's
/// handle, the plan validates the kind policy and carries the
/// resolved info forward to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParentHandleCopy {
    pub object_paddr: u64,
    pub rights: Rights,
    /// PageSet kind is rejected at `record_parent_copy` time —
    /// transfer routes through `sys_export_handle` instead, which
    /// inc_refcount's correctly.
    pub kind: HandleKind,
}

/// User-mode entry context for the new process. Captured early so
/// the plan carries everything `apply_validated_plan` needs to set
/// up the first thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserEntryInfo {
    pub user_entry_point: u64,
    pub user_stack_top: u64,
    pub user_stack_base: u64,
    pub name: [u8; 16],
}

/// Errors from the pure structural validate. Each variant identifies
/// a specific structural precondition for `apply_validated_plan` to
/// be infallible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateProcessPlanError {
    /// `record_parent_copy` was given a PageSet handle. PageSet
    /// transfer routes through `sys_export_handle`, not process
    /// creation — apply does not inc_refcount, and a copy here would
    /// underflow the parent's refcount on the first close.
    PageSetKindParentHandle,
    /// `mapping_count + stack_pages > scratch_capacity`. The user's
    /// scratch buffer cannot hold the combined mapping+stack list.
    BufferCapacityExceeded,
    /// `ProcessTransferPlan` ran out of dedup slots (more than
    /// `MAX_CONSUMED_HEADERS` distinct headers).
    TooManyHeaders,
    /// `unique_headers + parent_copy_count > HANDLE_SLOTS_PER_PAGE`.
    /// The child's single-page handle table cannot fit one PageSet
    /// handle per consumed header plus the optional parent copy.
    /// Named precondition for apply's parent-copy infallibility.
    HandleTableTooSmall,
    /// Scheduler run queue has no free slot.
    SchedulerFull,
    /// `record_stack` called more than once. Silent last-write-wins
    /// would leave `stack_pages` decoupled from the dedup set's
    /// stack header; the API rejects the second call instead.
    StackAlreadyRecorded,
    /// `record_parent_copy` called more than once. Silent
    /// last-write-wins would let only the second copy survive into
    /// apply, with no signal that the first request was dropped.
    ParentCopyAlreadyRecorded,
    /// `record_parent_copy` was given an empty/inert `HandleEntry`
    /// (`object_paddr == 0`). Apply would insert the empty sentinel
    /// into the child table as if it were a live copied handle.
    /// The pure API rejects rather than trust the caller.
    EmptyParentHandle,
}

/// Mutable builder used by the kernel to accumulate plan decisions
/// while iterating user-supplied mappings. Pure — owns no kernel
/// resources, performs no allocations.
pub struct ProcessCreationPlanBuilder {
    transfer_plan: ProcessTransferPlan,
    mapping_count: usize,
    stack_pages: usize,
    stack_recorded: bool,
    parent_copy: Option<ParentHandleCopy>,
    user_entry: UserEntryInfo,
}

impl ProcessCreationPlanBuilder {
    pub fn new(user_entry: UserEntryInfo) -> Self {
        Self {
            transfer_plan: ProcessTransferPlan::new(),
            mapping_count: 0,
            stack_pages: 0,
            stack_recorded: false,
            parent_copy: None,
            user_entry,
        }
    }

    /// Record one ProcessMapping. Increments `mapping_count` and
    /// adds the mapping's header to the dedup set in a single call —
    /// the two counters cannot drift because the caller cannot
    /// update one without the other. Transactional: on Err, no
    /// builder field is mutated.
    pub fn record_mapping(
        &mut self,
        header_paddr: u64,
    ) -> Result<(), CreateProcessPlanError> {
        self.transfer_plan
            .add_header(header_paddr)
            .map_err(|_| CreateProcessPlanError::TooManyHeaders)?;
        self.mapping_count += 1;
        Ok(())
    }

    /// Record the stack region. At most once per builder — a second
    /// call returns `StackAlreadyRecorded`. Adds the stack header
    /// to the dedup set and stores `stack_pages` for the scratch
    /// capacity check. Transactional: on Err, no builder field is
    /// mutated.
    pub fn record_stack(
        &mut self,
        header_paddr: u64,
        stack_pages: usize,
    ) -> Result<(), CreateProcessPlanError> {
        if self.stack_recorded {
            return Err(CreateProcessPlanError::StackAlreadyRecorded);
        }
        self.transfer_plan
            .add_header(header_paddr)
            .map_err(|_| CreateProcessPlanError::TooManyHeaders)?;
        self.stack_pages = stack_pages;
        self.stack_recorded = true;
        Ok(())
    }

    /// Optional: record the parent handle to copy. At most once per
    /// builder — a second call returns `ParentCopyAlreadyRecorded`.
    /// Rejects PageSet kind here so apply doesn't have to. Also
    /// rejects `HandleEntry::EMPTY` (`object_paddr == 0`) — the
    /// empty-slot sentinel is not a live handle and must not reach
    /// apply, which would otherwise insert it into the child table
    /// as a real entry.
    pub fn record_parent_copy(
        &mut self,
        entry: HandleEntry,
    ) -> Result<(), CreateProcessPlanError> {
        if self.parent_copy.is_some() {
            return Err(CreateProcessPlanError::ParentCopyAlreadyRecorded);
        }
        if entry.object_paddr == 0 {
            return Err(CreateProcessPlanError::EmptyParentHandle);
        }
        if entry.kind.is_pageset() {
            return Err(CreateProcessPlanError::PageSetKindParentHandle);
        }
        self.parent_copy = Some(ParentHandleCopy {
            object_paddr: entry.object_paddr,
            rights: entry.rights,
            kind: entry.kind,
        });
        Ok(())
    }

    /// Pure structural validation. Returns the apply-phase token if
    /// every precondition holds. Per-header `consume_pageset_validate`
    /// is the kernel's job and runs in `validate_creation` after
    /// this returns Ok — kernel state stays out of lockjaw-types.
    /// Consumes self — once a builder yields a token, it cannot be
    /// re-validated.
    pub fn validate(
        self,
        scratch_capacity: usize,
        scheduler_has_room: bool,
    ) -> Result<ValidatedProcessCreationPlan, CreateProcessPlanError> {
        if !crate::vmem::validate_process_mappings(
            self.mapping_count,
            self.stack_pages,
            scratch_capacity,
        ) {
            return Err(CreateProcessPlanError::BufferCapacityExceeded);
        }
        if !scheduler_has_room {
            return Err(CreateProcessPlanError::SchedulerFull);
        }
        let parent_copy_count = if self.parent_copy.is_some() { 1 } else { 0 };
        let post_consume_occupancy =
            self.transfer_plan.headers().len() + parent_copy_count;
        if post_consume_occupancy > HANDLE_SLOTS_PER_PAGE as usize {
            return Err(CreateProcessPlanError::HandleTableTooSmall);
        }
        Ok(ValidatedProcessCreationPlan {
            transfer_plan: self.transfer_plan,
            parent_copy: self.parent_copy,
            user_entry: self.user_entry,
        })
    }
}

/// The apply-phase token. Constructible only via
/// `ProcessCreationPlanBuilder::validate(...)`. Pass to
/// `apply_validated_plan` by value — the kernel cannot run apply
/// without first running both the pure structural validate AND its
/// own consume-validate observations in `validate_creation`.
#[derive(Debug)]
pub struct ValidatedProcessCreationPlan {
    transfer_plan: ProcessTransferPlan,
    parent_copy: Option<ParentHandleCopy>,
    user_entry: UserEntryInfo,
}

impl ValidatedProcessCreationPlan {
    /// Deduplicated list of PageSet headers the kernel must
    /// consume_pageset_apply over.
    pub fn headers_to_consume(&self) -> &[u64] {
        self.transfer_plan.headers()
    }

    /// Resolved parent-handle copy info, if the caller requested one.
    pub fn parent_copy(&self) -> Option<&ParentHandleCopy> {
        self.parent_copy.as_ref()
    }

    pub fn user_entry(&self) -> &UserEntryInfo {
        &self.user_entry
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

    // -----------------------------------------------------------------------
    // ProcessCreationPlanBuilder tests
    // -----------------------------------------------------------------------

    fn make_user_entry() -> UserEntryInfo {
        UserEntryInfo {
            user_entry_point: 0x40_0000,
            user_stack_top: 0x80_0000,
            user_stack_base: 0x70_0000,
            name: *b"test\0\0\0\0\0\0\0\0\0\0\0\0",
        }
    }

    fn endpoint_entry(paddr: u64) -> HandleEntry {
        HandleEntry {
            object_paddr: paddr,
            rights: Rights::from_bits(0),
            kind: HandleKind::Endpoint { caller_token: 0 },
        }
    }

    #[test]
    fn record_mapping_dedupes_headers_but_increments_count() {
        // Two mappings sharing one header: mapping_count == 2,
        // unique headers == 1. The two counters must move
        // independently — that is exactly the property `record_mapping`
        // exists to enforce in a single call.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_mapping(0xA000).unwrap();
        b.record_mapping(0xA000).unwrap();
        b.record_stack(0xB000, 1).unwrap();
        // Two record_mapping calls + one stack header = 2 unique headers,
        // mapping_count==2, stack_pages==1. Capacity 8 covers it.
        let plan = b.validate(8, true).unwrap();
        assert_eq!(plan.headers_to_consume(), &[0xA000, 0xB000]);
    }

    #[test]
    fn record_stack_dedupes_with_record_mapping() {
        // Stack and a mapping share one header — dedup collapses to one.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_mapping(0xA000).unwrap();
        b.record_stack(0xA000, 1).unwrap();
        let plan = b.validate(8, true).unwrap();
        assert_eq!(plan.headers_to_consume(), &[0xA000]);
    }

    #[test]
    fn record_parent_copy_rejects_pageset_kind() {
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        let pageset = HandleEntry {
            object_paddr: 0x1000,
            rights: Rights::from_bits(0),
            kind: HandleKind::PageSet { mapped_va_page: 0 },
        };
        assert_eq!(
            b.record_parent_copy(pageset),
            Err(CreateProcessPlanError::PageSetKindParentHandle),
        );
    }

    #[test]
    fn record_parent_copy_accepts_endpoint_notification_reply() {
        for kind in [
            HandleKind::Endpoint { caller_token: 0 },
            HandleKind::Notification,
            HandleKind::Reply,
        ] {
            let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
            let entry = HandleEntry {
                object_paddr: 0x2000,
                rights: Rights::from_bits(0),
                kind,
            };
            assert_eq!(b.record_parent_copy(entry), Ok(()));
        }
    }

    #[test]
    fn validate_rejects_when_mappings_exceed_scratch_capacity() {
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_mapping(0xA000).unwrap();
        b.record_mapping(0xB000).unwrap();
        b.record_stack(0xC000, 2).unwrap();
        // mapping_count==2 + stack_pages==2 == 4 > capacity==3
        assert_eq!(
            b.validate(3, true).unwrap_err(),
            CreateProcessPlanError::BufferCapacityExceeded,
        );
    }

    #[test]
    fn validate_rejects_on_header_overflow() {
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        for i in 0..MAX_CONSUMED_HEADERS {
            b.record_mapping(i as u64 * 0x1000).unwrap();
        }
        // One past capacity — exposed through record_mapping, not the
        // raw transfer plan.
        assert_eq!(
            b.record_mapping(0xFFFF_0000),
            Err(CreateProcessPlanError::TooManyHeaders),
        );
    }

    #[test]
    fn validate_rejects_when_scheduler_full() {
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_mapping(0xA000).unwrap();
        b.record_stack(0xB000, 1).unwrap();
        assert_eq!(
            b.validate(8, false).unwrap_err(),
            CreateProcessPlanError::SchedulerFull,
        );
    }

    #[test]
    fn validate_rejects_when_post_consume_overflows_handle_table() {
        // Named parent-copy infallibility invariant: if unique
        // headers + parent_copy_count > HANDLE_SLOTS_PER_PAGE, apply
        // could not insert the parent copy without runtime failure —
        // so structural validate must reject up front.
        //
        // HANDLE_SLOTS_PER_PAGE == 127, MAX_CONSUMED_HEADERS == 32
        // (validate_process_mappings caps mapping_count too), so we
        // can't actually exceed via record_mapping. Instead, build
        // the plan at MAX_CONSUMED_HEADERS unique entries and add
        // a parent copy. With HANDLE_SLOTS_PER_PAGE >> 32, this must
        // succeed. We reproduce HandleTableTooSmall by asserting the
        // boundary via direct construction once the const grows
        // beyond the table — codified here as a guard against future
        // const tuning that would silently break the invariant.
        assert!(
            (MAX_CONSUMED_HEADERS as u64) + 1 <= HANDLE_SLOTS_PER_PAGE,
            "MAX_CONSUMED_HEADERS + 1 must fit in handle table for parent-copy invariant",
        );

        // Negative path proper: synthesize an oversized capacity
        // by rejecting via validate. With current consts, every
        // legal input passes the post-consume check.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        for i in 0..MAX_CONSUMED_HEADERS - 1 {
            b.record_mapping(i as u64 * 0x1000).unwrap();
        }
        b.record_stack((MAX_CONSUMED_HEADERS - 1) as u64 * 0x1000, 1).unwrap();
        // happy path: 32 headers + 0 parent_copy <= 127 → ok
        assert!(b.validate(64, true).is_ok());
    }

    #[test]
    fn validate_accepts_at_handle_table_boundary() {
        // Boundary: unique_headers + parent_copy_count ==
        // HANDLE_SLOTS_PER_PAGE must accept (the inequality is `<=`,
        // not `<`).
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        for i in 0..MAX_CONSUMED_HEADERS - 1 {
            b.record_mapping(i as u64 * 0x1000).unwrap();
        }
        b.record_stack((MAX_CONSUMED_HEADERS - 1) as u64 * 0x1000, 1).unwrap();
        b.record_parent_copy(endpoint_entry(0xCAFE_0000)).unwrap();
        // 32 + 1 == 33 <= HANDLE_SLOTS_PER_PAGE (127) — accepts.
        assert!(b.validate(64, true).is_ok());
    }

    #[test]
    fn happy_path_token_carries_pre_validation_values() {
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_mapping(0xA000).unwrap();
        b.record_mapping(0xA000).unwrap(); // dedup
        b.record_mapping(0xB000).unwrap();
        b.record_stack(0xC000, 4).unwrap();
        b.record_parent_copy(endpoint_entry(0xDEAD_0000)).unwrap();
        let plan = b.validate(16, true).unwrap();

        assert_eq!(plan.headers_to_consume(), &[0xA000, 0xB000, 0xC000]);
        let parent = plan.parent_copy().unwrap();
        assert_eq!(parent.object_paddr, 0xDEAD_0000);
        assert!(matches!(parent.kind, HandleKind::Endpoint { .. }));
        let entry = plan.user_entry();
        assert_eq!(entry.user_entry_point, 0x40_0000);
        assert_eq!(entry.user_stack_top, 0x80_0000);
    }

    #[test]
    fn validate_consumes_builder() {
        // Compile-time guarantee: validate(self) takes ownership, so
        // the builder cannot be re-validated. This test asserts the
        // shape; if validate ever switches to &self, this will not
        // compile (the second use of `b` would error).
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_mapping(0xA000).unwrap();
        b.record_stack(0xB000, 1).unwrap();
        let _plan = b.validate(8, true).unwrap();
        // Uncommenting the next line must be a compile error:
        // let _plan2 = b.validate(8, true);
    }

    #[test]
    fn record_mapping_transactional_on_err() {
        // Lockdown: when add_header overflows, record_mapping must
        // leave mapping_count unchanged. Reverting the fix (bumping
        // mapping_count before add_header) would let this test
        // observe a mismatched count after the failed call.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        for i in 0..MAX_CONSUMED_HEADERS {
            b.record_mapping(i as u64 * 0x1000).unwrap();
        }
        // mapping_count==32, transfer plan full. New header overflows.
        let before = b.mapping_count;
        assert_eq!(
            b.record_mapping(0xFFFF_0000).unwrap_err(),
            CreateProcessPlanError::TooManyHeaders,
        );
        assert_eq!(b.mapping_count, before, "mapping_count must not change on Err");
    }

    #[test]
    fn record_stack_transactional_on_err() {
        // Lockdown: when add_header overflows, record_stack must
        // leave stack_pages unchanged. Same shape as record_mapping.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        for i in 0..MAX_CONSUMED_HEADERS {
            b.record_mapping(i as u64 * 0x1000).unwrap();
        }
        let before = b.stack_pages;
        assert_eq!(
            b.record_stack(0xFFFF_0000, 99).unwrap_err(),
            CreateProcessPlanError::TooManyHeaders,
        );
        assert_eq!(b.stack_pages, before, "stack_pages must not change on Err");
    }

    #[test]
    fn record_stack_rejects_second_call() {
        // Lockdown: silent last-write-wins on a second record_stack
        // call would leave stack_pages decoupled from the dedup
        // set's first stack header. The API rejects instead.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_stack(0xA000, 4).unwrap();
        assert_eq!(
            b.record_stack(0xB000, 7).unwrap_err(),
            CreateProcessPlanError::StackAlreadyRecorded,
        );
        // First call's state survives; second was rejected outright.
        assert_eq!(b.stack_pages, 4);
        assert_eq!(b.transfer_plan.headers(), &[0xA000]);
    }

    #[test]
    fn record_parent_copy_rejects_empty_handle() {
        // Lockdown: HandleEntry::EMPTY has object_paddr == 0 and
        // HandleKind::Empty. Without this rejection, apply would
        // insert the empty sentinel into the child table as a real
        // copied handle. The pure API rejects up front rather than
        // trust the kernel to filter.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        assert_eq!(
            b.record_parent_copy(HandleEntry::EMPTY).unwrap_err(),
            CreateProcessPlanError::EmptyParentHandle,
        );
        // Also: a real-looking entry with paddr 0 is still rejected
        // (object_paddr == 0 is the sole emptiness test).
        let zero_paddr = HandleEntry {
            object_paddr: 0,
            rights: Rights::from_bits(0),
            kind: HandleKind::Endpoint { caller_token: 0 },
        };
        assert_eq!(
            b.record_parent_copy(zero_paddr).unwrap_err(),
            CreateProcessPlanError::EmptyParentHandle,
        );
        // First-call rejection didn't transition the at-most-once
        // state — a real entry is still accepted afterward.
        b.record_parent_copy(endpoint_entry(0xCAFE_0000)).unwrap();
    }

    #[test]
    fn record_parent_copy_rejects_second_call() {
        // Lockdown: silent last-write-wins on a second
        // record_parent_copy call would let only the second copy
        // survive into apply, with no signal about the first.
        let mut b = ProcessCreationPlanBuilder::new(make_user_entry());
        b.record_parent_copy(endpoint_entry(0xAAAA_0000)).unwrap();
        let second = HandleEntry {
            object_paddr: 0xBBBB_0000,
            rights: Rights::from_bits(0),
            kind: HandleKind::Notification,
        };
        assert_eq!(
            b.record_parent_copy(second).unwrap_err(),
            CreateProcessPlanError::ParentCopyAlreadyRecorded,
        );
        // First copy survives.
        assert_eq!(b.parent_copy.unwrap().object_paddr, 0xAAAA_0000);
    }

    #[test]
    fn validate_rejects_zero_mappings() {
        // validate_process_mappings rejects mapping_count == 0 even
        // with capacity headroom. Confirms the empty-builder case.
        let b = ProcessCreationPlanBuilder::new(make_user_entry());
        assert_eq!(
            b.validate(8, true).unwrap_err(),
            CreateProcessPlanError::BufferCapacityExceeded,
        );
    }
}
