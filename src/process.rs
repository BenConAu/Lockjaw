use crate::arch::aarch64::vmem::{AddressSpaceBuilder, Mapping, MAPPINGS_PER_PAGE};
use crate::cap::handle_table::HandleTableRef;
use crate::cap::object::{HandleTableCreateInfo, create_handle_table};
use crate::cap::pageset_table::PageSetRef;
use crate::mm::addr::{PhysAddr, PAGE_SIZE};
use crate::mm::kernel_ptr::{KernelMut, KernelRef};
use crate::mm::page_alloc;
use crate::mm::user_access::UserAddressSpace;
use crate::sched::current::CurrentThread;
use crate::sched::tcb::{Tcb, TcbCreateInfo, create_tcb};
use crate::sched::scheduler;
use lockjaw_types::addr::KernelVa;
use lockjaw_types::syscall::SyscallError;

// Re-export from lockjaw-types — single source of truth.
use lockjaw_types::process::{
    CreateProcessPlanError, ProcessCreationPlanBuilder, ProcessMapping,
    PROCESS_MAP_FLAG_EXECUTABLE,
};
use lockjaw_types::vmem::{ScratchAction, ScratchCursor};

/// Drop guard for a TTBR0 page table tree — walks and frees all table
/// pages (L0/L1/L2/L3) on drop unless defused.
struct Ttbr0Guard(Option<PhysAddr>);
impl Ttbr0Guard {
    fn new(paddr: PhysAddr) -> Self { Self(Some(paddr)) }
    fn addr(&self) -> PhysAddr { self.0.unwrap() }
    fn defuse(&mut self) { self.0 = None; }
}
impl Drop for Ttbr0Guard {
    fn drop(&mut self) {
        if let Some(paddr) = self.0.take() {
            unsafe { crate::arch::aarch64::vmem::free_address_space(paddr); }
        }
    }
}

/// Typed errors from `create_process`. The syscall handler maps each
/// variant to the matching `SyscallError` so userspace gets a
/// meaningful errno instead of a collapsed UNKNOWN.
#[derive(Debug)]
pub enum CreateProcessError {
    OutOfMemory,
    BadHandle,
    InvalidUserMemory,
    TooManyOwnedPages,
    AddressSpaceMappingFailed,
    PlanError(CreateProcessPlanError),
    ConsumeValidateFailed { #[allow(dead_code)] idx: usize },
}

impl CreateProcessError {
    pub fn to_syscall_error(&self) -> SyscallError {
        match self {
            // Genuine kernel-internal physical/virtual page exhaustion
            // (KVM pool, page allocator, page-table tree growth).
            CreateProcessError::OutOfMemory
            | CreateProcessError::AddressSpaceMappingFailed =>
                SyscallError::OUT_OF_MEMORY,
            // Scheduler slot table is full — distinct failure class
            // from memory OOM. Previously collapsed into OUT_OF_MEMORY,
            // which conflated "ran out of pages" with "ran out of
            // thread slots" and made the userspace failure class
            // unguessable without a kernel boot log. The QUEUE_FULL
            // variant already exists for exactly this case.
            CreateProcessError::PlanError(CreateProcessPlanError::SchedulerFull) =>
                SyscallError::QUEUE_FULL,
            CreateProcessError::PlanError(CreateProcessPlanError::HandleTableTooSmall) =>
                SyscallError::HANDLE_TABLE_FULL,
            CreateProcessError::PlanError(CreateProcessPlanError::PageSetKindParentHandle)
            | CreateProcessError::BadHandle
            | CreateProcessError::PlanError(CreateProcessPlanError::EmptyParentHandle)
            | CreateProcessError::ConsumeValidateFailed { .. } =>
                SyscallError::INVALID_HANDLE,
            CreateProcessError::PlanError(CreateProcessPlanError::BufferCapacityExceeded)
            | CreateProcessError::PlanError(CreateProcessPlanError::TooManyHeaders)
            | CreateProcessError::TooManyOwnedPages
            | CreateProcessError::InvalidUserMemory =>
                SyscallError::INVALID_PARAMETER,
            CreateProcessError::PlanError(CreateProcessPlanError::StackAlreadyRecorded)
            | CreateProcessError::PlanError(CreateProcessPlanError::ParentCopyAlreadyRecorded) =>
                SyscallError::UNKNOWN,
        }
    }
}

/// Diagnostic — name the failing allocation step plus the
/// page-allocator free count, so a single boot log identifies
/// which sub-pool actually exhausted. Process creation fans out
/// across ~7 distinct allocation sites that all collapse to
/// `CreateProcessError::OutOfMemory`; without a label the syscall
/// only tells the caller "OOM" and leaves the kernel-internal
/// resource a guess.
///
/// Cheap by design: only fires on a failing path, doesn't change
/// hot-path control flow, and removes cleanly when the OOM trigger
/// is fixed.
fn log_create_process_oom(step: &'static str) {
    crate::kprintln!(
        "[create_process OOM] step=", step,
        " page_alloc_free=", crate::mm::page_alloc::free_count(),
    );
}

/// Bundle returned by `provision_resources`: every kernel page the
/// new process needs, under guards. The guards are dropped on
/// failure (freeing the pages); on success the orchestrator defuses
/// each one before handing the addresses off to apply.
struct ProvisionedResources {
    /// ProcessObject lives in the KVM pool; the guard frees the KVA
    /// range (and its backing frame) on drop unless taken.
    proc: crate::mm::kvm::OwnedKvmRangeGuard,
    ttbr0: Ttbr0Guard,
    /// HandleTable also lives in the KVM pool.
    handle_table: crate::mm::kvm::OwnedKvmRangeGuard,
    /// Per-thread kernel stack also lives in the KVM pool.
    tcb_stack: crate::mm::kvm::OwnedKvmRangeGuard,
    /// TCB also lives in the KVM pool.
    tcb: crate::mm::kvm::OwnedKvmRangeGuard,
    /// Scratch capacity discovered while doing the lookups —
    /// returned because `validate(...)` needs it and the caller
    /// has no other way to learn it without redoing the lookup.
    scratch_capacity: usize,
}

/// Create a new process from a userspace-provided mapping list.
///
/// The caller (init) has already:
///   1. Parsed the ELF in userspace
///   2. Allocated pages via sys_alloc_pages
///   3. Mapped them into its own space via sys_map_pages
///   4. Copied ELF segment data into the pages
///   5. Built the mapping list in its own memory
///
/// The kernel reads the mapping list from userspace memory (one entry at
/// a time, constant stack usage), resolves PageSet IDs to physical addresses,
/// creates the address space, TCB, and schedules the new thread.
///
/// Two-phase: validate (all fallible work, no destructive mutation) then
/// apply (all infallible commits). If any validate step fails the parent
/// is unchanged; if validate succeeds apply runs to completion. The
/// child's handle table is allocated empty in validate and the
/// parent_handle_to_copy slot is inserted in apply, AFTER all consumes,
/// so the not-yet-scheduled child does not participate in revoke walks.
///
/// Stack split: the heavy provisioning (AddressSpaceBuilder, scratch
/// buffer state, mapping iteration, TCB/handle-table init) lives in
/// `provision_resources`. This orchestrator holds only the guards and
/// the plan builder. That keeps the deepest sync-exception frame
/// from carrying the address-space builder + scratch state + plan
/// data + apply state at the same time.
pub fn create_process(
    addr_space: &UserAddressSpace,
    mappings_va: u64,
    mapping_count: usize,
    entry_point: u64,
    stack_pageset_id: u64,
    scratch_pageset_id: u64,
    parent_handle_to_copy: u64,
    name: [u8; 16],
) -> Result<(), CreateProcessError> {
    let mut plan_builder = ProcessCreationPlanBuilder::new();
    let mut resources = provision_resources(
        addr_space,
        mappings_va,
        mapping_count,
        entry_point,
        stack_pageset_id,
        scratch_pageset_id,
        parent_handle_to_copy,
        name,
        &mut plan_builder,
    )?;

    // Validate the plan (pure structural checks: capacity, scheduler
    // room, post-consume handle-table capacity invariant). The token
    // is the only way to get headers_to_consume() and parent_copy()
    // for the apply phase.
    // unique_header_count is sourced from the proc-page storage —
    // the single source of truth for the deduplicated headers list.
    // Passing it explicitly into validate prevents any possibility
    // of drift between the kernel-owned dedup count and a builder-
    // side mirror.
    let proc_kva = resources.proc.kva();
    let unique_header_count =
        crate::cap::process_obj::process_consumed_header_count(proc_kva) as usize;
    let plan = plan_builder
        .validate(
            resources.scratch_capacity,
            scheduler::has_room(),
            unique_header_count,
        )
        .map_err(CreateProcessError::PlanError)?;

    // Validate that revoking every consumed PageSet header would
    // succeed. consume_pageset_validate is read-only — Err here
    // leaves every parent's handle table and page table untouched.
    // Headers live in the proc page (off the kernel stack); read
    // them by index. The proc-page storage is a polymorphic u64
    // array — interpret the entries as PageSet header KVAs.
    for idx in 0..plan.unique_header_count() {
        let hdr = crate::cap::process_obj::process_consumed_header(proc_kva, idx)
            .expect("header index < unique_header_count by construction");
        crate::cap::pageset_table::consume_pageset_validate(hdr)
            .map_err(|_| CreateProcessError::ConsumeValidateFailed { idx })?;
    }

    // ============ Apply phase: all infallible commits ============

    // Apply consume for each unique header. revoke_apply walks every
    // process's handle table (including the parent's), clears PTEs
    // for active mappings, decrements refcount/map_count per cleared
    // slot, then unlinks and frees the header. Cannot fail under the
    // validate→apply contract (GKL held throughout).
    for idx in 0..plan.unique_header_count() {
        let hdr = crate::cap::process_obj::process_consumed_header(proc_kva, idx)
            .expect("header index < unique_header_count by construction");
        crate::cap::pageset_table::consume_pageset_apply(hdr);
    }

    // Insert parent_handle_to_copy into the child's now-empty table.
    // Runs after consume_apply so the not-yet-scheduled child does
    // not appear in the revoke walks above. Cannot fail: the table
    // was freshly allocated empty above and has zero entries.
    if let Some(parent) = plan.parent_copy() {
        // For Endpoint handles: always mint a fresh caller token from
        // the endpoint's monotonic counter. Same rule as
        // sys_export_handle: every gift event creates a distinct
        // identity, regardless of whether the parent's handle was the
        // master or a previously-minted sender. The child gets its own
        // identity that the server can distinguish from the parent's.
        //
        // See docs/architecture/02-handle-identity-tokens.md.
        let child_kind = match parent.kind {
            lockjaw_types::object::HandleKind::Endpoint { kva, .. } => {
                let token = {
                    let mut ep = unsafe {
                        KernelMut::<crate::ipc::endpoint::EndpointObject>::from_kva(kva)
                    };
                    crate::ipc::endpoint::mint_caller_token(ep.get_mut())
                };
                lockjaw_types::object::HandleKind::Endpoint { kva, caller_token: Some(token) }
            }
            other => other,
        };

        // SAFETY: handle_table KVA was initialized as a valid handle
        // table in provision_resources.
        let child_ht = unsafe { HandleTableRef::from_kva(resources.handle_table.kva()) };
        // Cannot fail: the child table was freshly allocated empty
        // with HANDLE_SLOTS_PER_PAGE empty slots, and the post-consume
        // capacity invariant was checked in the pure validate. Avoid
        // Result::expect so the panic path doesn't pull in Debug
        // formatting for SyscallError (would inflate exception stack).
        if child_ht.insert(parent.rights, child_kind).is_err() {
            panic!("child handle insert into fresh empty table failed (kernel-invariant violation)");
        }
    }

    // Capture the TCB KVA before taking — take() drops the guard's
    // ownership, after which kva() would panic.
    let tcb_kva = resources.tcb.kva();

    // Defuse drop guards — child now owns all its resources.
    // OwnedKvmRangeGuard transfers ownership via take() (drops the
    // guard's claim without freeing). Ttbr0Guard uses defuse().
    let _ = resources.proc.take();
    let _ = resources.handle_table.take();
    resources.ttbr0.defuse();
    let _ = resources.tcb_stack.take();
    let _ = resources.tcb.take();

    // Enqueue the thread. Cannot fail because has_room() above
    // returned true and GKL has been held throughout.
    if !scheduler::add_thread(tcb_kva) {
        panic!("scheduler::add_thread failed after has_room() returned true (kernel-invariant violation)");
    }

    Ok(())
}

/// Phase-1 helper: every fallible kernel-side allocation and write
/// for the new process. Lives in its own frame so the heavy locals
/// (AddressSpaceBuilder, scratch-buffer state, mapping iteration
/// state, per-iteration HandleEntry copies) don't share a stack
/// frame with the apply-phase state in `create_process`. The plan
/// builder is owned by the caller and passed in by `&mut` — its
/// data lives in the caller's frame and is observed by the
/// validated-token borrow after this function returns.
///
/// `#[inline(never)]` is load-bearing: without it, the compiler
/// inlines this back into create_process, the frames merge, and
/// the deepest sync-exception path balloons past the per-thread
/// kernel stack.
#[inline(never)]
fn provision_resources(
    addr_space: &UserAddressSpace,
    mappings_va: u64,
    mapping_count: usize,
    entry_point: u64,
    stack_pageset_id: u64,
    scratch_pageset_id: u64,
    parent_handle_to_copy: u64,
    name: [u8; 16],
    plan_builder: &mut ProcessCreationPlanBuilder,
) -> Result<ProvisionedResources, CreateProcessError> {
    // Look up stack and scratch PageSets from handle table.
    // These are handle indices, not raw pageset IDs.
    let ht = CurrentThread::handle_table();
    let stack_entry = ht.lookup(stack_pageset_id as u32,
        crate::cap::rights::Rights::from_bits(crate::cap::rights::RIGHT_READ),
        crate::cap::object::ObjectType::PageSet)
        .map_err(|_| CreateProcessError::BadHandle)?;
    let stack_kva = match stack_entry.kind {
        lockjaw_types::object::HandleKind::PageSet { kva, .. } => kva,
        _ => return Err(CreateProcessError::BadHandle),
    };
    // SAFETY: kva from a PageSet handle — registered header KVA.
    let stack_ps = unsafe { PageSetRef::from_header_kva(stack_kva) };
    // Reject DmaPool-origin PageSets as process stacks. Post C1 of
    // the cacheable-DMA migration, DmaPool pages ARE mappable as
    // Normal Cacheable, so the original M6 mixed-attribute alias
    // argument no longer applies — but the rejection stands for
    // two distinct reasons:
    //   (a) DmaPool is a tight 2 MiB reservation intended for
    //       device-DMA buffers. Charging process stacks against
    //       that budget would starve the actual DMA path.
    //   (b) DmaPool consumers operate under an explicit sync
    //       discipline (sys_dma_sync_for_cpu / for_device at
    //       device handoff points). Process stacks have no
    //       device handoffs and no sync calls; granting them
    //       DmaPool origin would let a stack slip through the
    //       discipline by accident.
    // Uninit origin (None) is also rejected as a typed surface
    // for the explicit-init invariant.
    match stack_ps.origin() {
        Some(lockjaw_types::pageset_table::PageSetOrigin::Buddy) => {}
        _ => return Err(CreateProcessError::BadHandle),
    }

    // Resolve parent_handle_to_copy in validate phase. The actual
    // child-table insertion runs in apply (step 6) so the
    // not-yet-scheduled child stays out of revoke accounting.
    //
    // PageSet kind is rejected here: the existing copy logic inserts
    // without inc_refcount, which would underflow refcount the first
    // time anyone closes the parent handle and would let the data
    // pages outlive their accounting. Clean fix is sys_export_handle
    // (which DOES inc_refcount); the parent can call it from
    // userspace if PageSet transfer is needed.
    if parent_handle_to_copy != u64::MAX {
        let entry = CurrentThread::handle_table().lookup_any(
            parent_handle_to_copy as u32,
            crate::cap::rights::Rights::none(),
        ).map_err(|_| CreateProcessError::BadHandle)?;
        plan_builder.record_parent_copy(entry)
            .map_err(CreateProcessError::PlanError)?;
    }

    // Use the caller-provided scratch pages as the Mapping buffer.
    let scratch_entry = ht.lookup(scratch_pageset_id as u32,
        crate::cap::rights::Rights::from_bits(crate::cap::rights::RIGHT_READ),
        crate::cap::object::ObjectType::PageSet)
        .map_err(|_| CreateProcessError::BadHandle)?;
    let scratch_kva = match scratch_entry.kind {
        lockjaw_types::object::HandleKind::PageSet { kva, .. } => kva,
        _ => return Err(CreateProcessError::BadHandle),
    };
    let scratch_ps = unsafe { PageSetRef::from_header_kva(scratch_kva) };
    // Reject DmaPool-origin PageSets as scratch pages. Post C1 of
    // the cacheable-DMA migration the kernel's cacheable access of
    // the page during mapping-batch flush no longer creates an
    // alias (DmaPool is also Cacheable post-C1), so the original
    // alias argument no longer applies. The rejection stands for
    // the same two reasons as the stack rejection above:
    //   (a) DmaPool is a tight 2 MiB device-DMA reservation;
    //       charging scratch against that budget starves real DMA.
    //   (b) DmaPool consumers operate under explicit sync
    //       discipline at device handoff points. Scratch pages
    //       have no device handoffs and would slip the discipline.
    // Uninit origin is rejected as a bug signal.
    match scratch_ps.origin() {
        Some(lockjaw_types::pageset_table::PageSetOrigin::Buddy) => {}
        _ => return Err(CreateProcessError::BadHandle),
    }
    let scratch_count = scratch_ps.count();
    if scratch_count == 0 {
        return Err(CreateProcessError::InvalidUserMemory);
    }

    // Validate that user mappings + stack pages fit in total scratch capacity
    let scratch_capacity = scratch_count * MAPPINGS_PER_PAGE;
    if !lockjaw_types::vmem::validate_process_mappings(mapping_count, stack_ps.count(), scratch_capacity) {
        return Err(CreateProcessError::PlanError(CreateProcessPlanError::BufferCapacityExceeded));
    }

    // Set up first scratch page
    let scratch_paddr = scratch_ps.page(0).ok_or(CreateProcessError::InvalidUserMemory)?;
    page_alloc::zero_page(scratch_paddr);
    // SAFETY: scratch_paddr is a kernel-allocated page; we use it as a
    // temporary Mapping buffer for AddressSpaceBuilder.
    let mut buf = unsafe { KernelMut::<Mapping>::from_paddr(scratch_paddr) };
    let mut mappings = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), MAPPINGS_PER_PAGE) };
    let mut cursor = ScratchCursor::new(scratch_count);

    // Allocate process page early (in the KVM pool) so we can write
    // owned_pages directly into it during mapping resolution. The KVM
    // alloc returns an OwnedKvmRange whose backing frame is fresh and
    // zero-initialized via the explicit write_bytes below.
    let proc_range = crate::mm::kvm::alloc_kernel_pages(1)
        .map_err(|_| { log_create_process_oom("proc_range"); CreateProcessError::OutOfMemory })?;
    let proc_guard = crate::mm::kvm::OwnedKvmRangeGuard::new(proc_range);
    let proc_kva = proc_guard.kva();
    // SAFETY: proc_kva is a freshly-allocated KVM range; we own it.
    unsafe {
        let mut p = crate::mm::kernel_ptr::KernelMut::<u8>::from_kva(proc_kva);
        core::ptr::write_bytes(p.as_mut_ptr(), 0, PAGE_SIZE as usize);
    }

    // Create incremental address space builder — Drop handles cleanup on failure.
    let mut as_builder = unsafe { AddressSpaceBuilder::new() }
        .map_err(|_| { log_create_process_oom("AddressSpaceBuilder::new"); CreateProcessError::OutOfMemory })?;

    for i in 0..mapping_count {
        // Read ProcessMapping from user memory via page table walk (TTBR1).
        let entry_va = mappings_va + (i as u64) * core::mem::size_of::<ProcessMapping>() as u64;
        let user_mapping: ProcessMapping = addr_space.read(entry_va)
            .ok_or(CreateProcessError::InvalidUserMemory)?;

        // Resolve the PageSet handle to a physical address
        let ps_entry = ht.lookup(user_mapping.pageset_id as u32,
            crate::cap::rights::Rights::from_bits(crate::cap::rights::RIGHT_READ),
            crate::cap::object::ObjectType::PageSet)
            .map_err(|_| CreateProcessError::BadHandle)?;
        let ps_kva = match ps_entry.kind {
            lockjaw_types::object::HandleKind::PageSet { kva, .. } => kva,
            _ => return Err(CreateProcessError::BadHandle),
        };
        // SAFETY: kva from a PageSet handle — registered header KVA.
        let ps = unsafe { PageSetRef::from_header_kva(ps_kva) };
        // M6: process mappings get installed as cacheable Normal
        // PTEs by AddressSpaceBuilder::map_batch (no per-mapping
        // attribute selector — this is the create_process path, not
        // sys_map_pages). DmaPool PageSets must not enter this path;
        // uninit origin is rejected as a typed bug signal.
        match ps.origin() {
            Some(lockjaw_types::pageset_table::PageSetOrigin::Buddy) => {}
            _ => return Err(CreateProcessError::BadHandle),
        }
        let page_idx = user_mapping.page_index as usize;
        let phys = ps.page(page_idx).ok_or(CreateProcessError::InvalidUserMemory)?;

        mappings[cursor.offset()] = Mapping {
            virt_addr: user_mapping.virt_addr,
            phys_addr: phys,
            user_accessible: true,
            executable: (user_mapping.flags & PROCESS_MAP_FLAG_EXECUTABLE) != 0,
        };

        match cursor.advance() {
            ScratchAction::Continue => {}
            ScratchAction::FlushAndAdvance { next_page_idx } => {
                unsafe { as_builder.map_batch(&mappings[..MAPPINGS_PER_PAGE]) }
                    .map_err(|_| { log_create_process_oom("map_batch flush"); CreateProcessError::AddressSpaceMappingFailed })?;
                let next = scratch_ps.page(next_page_idx).ok_or(CreateProcessError::InvalidUserMemory)?;
                page_alloc::zero_page(next);
                buf = unsafe { KernelMut::<Mapping>::from_paddr(next) };
                mappings = unsafe {
                    core::slice::from_raw_parts_mut(buf.as_mut_ptr(), MAPPINGS_PER_PAGE)
                };
                cursor.did_advance();
            }
        }

        // Record data page directly in ProcessObject (no stack array)
        if !crate::cap::process_obj::process_push_owned_page(
            proc_kva, phys.as_u64()
        ) {
            return Err(CreateProcessError::TooManyOwnedPages);
        }
        record_mapping_into_plan(
            plan_builder, proc_kva, ps_kva,
        )?;
    }

    // Add stack pages contiguously at USER_STACK_BASE
    let stack_va: u64 = lockjaw_types::constants::USER_STACK_BASE;
    for s in 0..stack_ps.count() {
        let phys = stack_ps.page(s).ok_or(CreateProcessError::InvalidUserMemory)?;
        mappings[cursor.offset()] = Mapping {
            virt_addr: stack_va + (s as u64) * PAGE_SIZE,
            phys_addr: phys,
            user_accessible: true,
            executable: false,
        };

        match cursor.advance() {
            ScratchAction::Continue => {}
            ScratchAction::FlushAndAdvance { next_page_idx } => {
                unsafe { as_builder.map_batch(&mappings[..MAPPINGS_PER_PAGE]) }
                    .map_err(|_| { log_create_process_oom("map_batch flush"); CreateProcessError::AddressSpaceMappingFailed })?;
                let next = scratch_ps.page(next_page_idx).ok_or(CreateProcessError::InvalidUserMemory)?;
                page_alloc::zero_page(next);
                buf = unsafe { KernelMut::<Mapping>::from_paddr(next) };
                mappings = unsafe {
                    core::slice::from_raw_parts_mut(buf.as_mut_ptr(), MAPPINGS_PER_PAGE)
                };
                cursor.did_advance();
            }
        }

        // Record stack pages directly in ProcessObject
        if !crate::cap::process_obj::process_push_owned_page(
            proc_kva, phys.as_u64()
        ) {
            return Err(CreateProcessError::TooManyOwnedPages);
        }
    }
    record_stack_into_plan(
        plan_builder, proc_kva, stack_kva, stack_ps.count(),
    )?;

    // Final flush of pending mappings and finalize address space
    if cursor.has_pending() {
        unsafe { as_builder.map_batch(&mappings[..cursor.pending_count()]) }
            .map_err(|_| { log_create_process_oom("map_batch final"); CreateProcessError::AddressSpaceMappingFailed })?;
    }
    let ttbr0 = as_builder.finish();
    let ttbr0_guard = Ttbr0Guard::new(ttbr0);

    // Flush I-cache
    unsafe { core::arch::asm!("ic iallu", "dsb ish", "isb") };

    // Create handle table in the KVM pool.
    let ht_range = crate::mm::kvm::alloc_kernel_pages(1)
        .map_err(|_| { log_create_process_oom("ht_range"); CreateProcessError::OutOfMemory })?;
    let ht_guard = crate::mm::kvm::OwnedKvmRangeGuard::new(ht_range);
    let ht_kva = ht_guard.kva();
    // SAFETY: ht_kva is a freshly allocated KVM range.
    unsafe {
        create_handle_table(
            &HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
            ht_kva,
        ).map_err(|_| { log_create_process_oom("create_handle_table"); CreateProcessError::OutOfMemory })?;
    }

    // Write ProcessObject header (owned_pages already populated above).
    crate::cap::process_obj::init_process_header(
        proc_kva,
        ttbr0_guard.addr().as_u64(),
        ht_kva.as_u64(),
        false, // not immortal
        &name,
    );

    // First thread — increment via narrow op (count 0 → 1)
    crate::cap::process_obj::process_inc_thread_count(proc_kva);

    // Create TCB — first thread in this process. Both the TCB page
    // and the per-thread kernel stack live in the KVM pool.
    let tcb_stack_range = crate::mm::kvm::alloc_kernel_pages(1)
        .map_err(|_| { log_create_process_oom("tcb_stack_range"); CreateProcessError::OutOfMemory })?;
    let tcb_stack_guard = crate::mm::kvm::OwnedKvmRangeGuard::new(tcb_stack_range);
    let tcb_range = crate::mm::kvm::alloc_kernel_pages(1)
        .map_err(|_| { log_create_process_oom("tcb_range"); CreateProcessError::OutOfMemory })?;
    let tcb_guard = crate::mm::kvm::OwnedKvmRangeGuard::new(tcb_range);

    // SAFETY: stack and TCB are freshly allocated kernel pages.
    unsafe {
        create_tcb(
            &TcbCreateInfo {
                entry: process_entry,
                stack: lockjaw_types::thread::KernelStackBase::Pool(tcb_stack_guard.kva()),
                process_kva: proc_kva,
                user_entry_point: entry_point,
                user_stack_top: stack_va + (stack_ps.count() as u64) * PAGE_SIZE,
                user_stack_base: stack_va,
                user_arg: 0,
                name,
            },
            tcb_guard.kva(),
        ).map_err(|_| { log_create_process_oom("create_tcb"); CreateProcessError::OutOfMemory })?;
    }

    Ok(ProvisionedResources {
        proc: proc_guard,
        ttbr0: ttbr0_guard,
        handle_table: ht_guard,
        tcb_stack: tcb_stack_guard,
        tcb: tcb_guard,
        scratch_capacity,
    })
}

/// Record one ProcessMapping in the new process being built. Single
/// API boundary that wraps the dedup-into-proc-page step and the
/// builder count update — callers can't drift the two halves out of
/// step. The proc page's `consumed_header_count` is the single
/// source of truth for unique headers; the builder only tracks
/// `mapping_count`.
fn record_mapping_into_plan(
    plan_builder: &mut ProcessCreationPlanBuilder,
    proc_kva: KernelVa,
    header_kva: KernelVa,
) -> Result<(), CreateProcessError> {
    crate::cap::process_obj::process_record_consumed_header(proc_kva, header_kva)
        .map_err(|_| CreateProcessError::PlanError(CreateProcessPlanError::TooManyHeaders))?;
    plan_builder.record_mapping();
    Ok(())
}

/// Record the stack region in the new process being built. Same
/// shape as `record_mapping_into_plan` — one boundary, no caller
/// discipline between the proc-page write and the builder update.
/// The dedup happens first so a "too many headers" rejection
/// leaves the builder un-touched (the second-call rejection path
/// for `record_stack` would then still reject if the caller ever
/// retried).
fn record_stack_into_plan(
    plan_builder: &mut ProcessCreationPlanBuilder,
    proc_kva: KernelVa,
    header_kva: KernelVa,
    stack_pages: usize,
) -> Result<(), CreateProcessError> {
    crate::cap::process_obj::process_record_consumed_header(proc_kva, header_kva)
        .map_err(|_| CreateProcessError::PlanError(CreateProcessPlanError::TooManyHeaders))?;
    plan_builder.record_stack(stack_pages)
        .map_err(CreateProcessError::PlanError)?;
    Ok(())
}

/// Kernel-mode entry for new user processes. Reads entry point and stack
/// from TCB, then drops to EL0.
pub fn process_entry() -> ! {
    unsafe {
        // GKL held + IRQs masked (inherited from thread_entry).
        // Read TCB fields under the lock, then release before eret.
        let tcb_paddr = scheduler::current_tcb_kva();
        let tcb = KernelRef::<Tcb>::from_kva(tcb_paddr);
        let t = tcb.get();
        let entry = t.user_entry_point;
        let stack_top = t.user_stack_top;
        let user_arg = t.user_arg;
        let ttbr0 = PhysAddr::new(
            crate::cap::process_obj::process_ttbr0(KernelVa::new(t.process_kva))
        );

        // Release GKL before dropping to EL0. eret restores user
        // SPSR with IRQs enabled — no explicit unmask needed.
        crate::sched::gkl::gkl_unlock();

        crate::arch::aarch64::mmu::drop_to_el0_with_ttbr0(ttbr0, entry, stack_top, user_arg);
    }
}
