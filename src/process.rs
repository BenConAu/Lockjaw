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
    /// The donated page-table workspace was too small for the
    /// requested mapping set (NK4+NK5). Distinct from
    /// `AddressSpaceMappingFailed` so the SyscallError carries
    /// `OUT_OF_PAGE_TABLES`, not the collapsed `OUT_OF_MEMORY`.
    OutOfPageTables,
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
            CreateProcessError::OutOfPageTables =>
                SyscallError::OUT_OF_PAGE_TABLES,
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

/// Bundle returned by `provision_resources` (NK4+NK5): the donated
/// PageSet that backs every kernel page the new process needs, plus
/// resolved KVAs and the workspace-claimed count for finish.
///
/// The donation typestate is the single guard for all four
/// kernel-object pages AND the page-table workspace. On failure
/// before `into_consumed`, the donation stays caller-owned and
/// userspace recovers via `sys_close_handle`. On success the
/// orchestrator calls `donation.into_consumed(workspace_claimed)`
/// which runs the single `consume_pageset_apply` for the
/// process-resources PageSet and frees unclaimed workspace pages
/// to buddy.
struct ProvisionedResources {
    donation: crate::syscall::handler::ProcessPagesDonation,
    /// TTBR0 L0 physical address from `AddressSpaceBuilder::finish`.
    /// Held bare (not under `Ttbr0Guard`) because pre-`into_consumed`
    /// failure leaves the page-table workspace caller-owned (donation
    /// guard), and post-`into_consumed` the orchestrator arms its
    /// own `Ttbr0Guard` directly. Holding it under a guard inside
    /// `provision_resources` would risk a stale guard firing through
    /// `free_address_space` on workspace pages still owned by the
    /// caller's PageSet.
    ttbr0: PhysAddr,
    /// Resolved direct-map KVAs needed by Phase 5: ProcessObject
    /// (for the consume-validate loop and HT insertion), HandleTable
    /// (for parent_copy_apply), and TCB (for scheduler::add_thread).
    /// kstack KVA is consumed inside Phase 4 of `provision_resources`
    /// when building the TCB; it's not needed in the orchestrator.
    proc_kva: KernelVa,
    ht_kva: KernelVa,
    tcb_kva: KernelVa,
    /// Number of workspace pages claimed by `AddressSpaceBuilder`;
    /// passed to `donation.into_consumed` at finish.
    workspace_claimed: usize,
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
    process_resources_ps: u32,
    name: [u8; 16],
) -> Result<(), CreateProcessError> {
    let mut plan_builder = ProcessCreationPlanBuilder::new();
    // provision_resources runs Phases 1-4 of the NK4+NK5 design:
    // donate → init ProcessObject + mapping iteration + late
    // validate → build TTBR0 from workspace → init HT/kstack/TCB.
    // On any failure the donation stays caller-owned (no consume
    // has run); userspace recovers via sys_close_handle on the
    // donated PageSet handle.
    let resources = provision_resources(
        addr_space,
        mappings_va,
        mapping_count,
        entry_point,
        stack_pageset_id,
        scratch_pageset_id,
        parent_handle_to_copy,
        process_resources_ps,
        name,
        &mut plan_builder,
    )?;

    // ============ Phase 5: Apply (infallible) ============

    // Validate the plan from Phase-2 state (proc-page consumed-header
    // storage is the single source of truth for unique header count;
    // see the comment in provision_resources Phase 2).
    let unique_header_count =
        crate::cap::process_obj::process_consumed_header_count(resources.proc_kva) as usize;
    let plan = match plan_builder.validate(
        resources.scratch_capacity,
        scheduler::has_room(),
        unique_header_count,
    ) {
        Ok(p) => p,
        Err(e) => {
            // Pre-`into_consumed`: donation must stay caller-owned
            // to keep the typestate resolved. Without this branch
            // `resources` drops and `ProcessPagesDonation::drop`
            // panics on unresolved-drop.
            resources.donation.into_caller_owned();
            return Err(CreateProcessError::PlanError(e));
        }
    };

    // consume_pageset_validate for every unique mapping header — read-only
    // pairing for the apply loop below. Err here leaves every parent's
    // handle table and page table untouched AND the donation
    // caller-owned (since into_consumed hasn't run).
    for idx in 0..plan.unique_header_count() {
        let hdr = crate::cap::process_obj::process_consumed_header(resources.proc_kva, idx)
            .expect("header index < unique_header_count by construction");
        if crate::cap::pageset_table::consume_pageset_validate(hdr).is_err() {
            resources.donation.into_caller_owned();
            return Err(CreateProcessError::ConsumeValidateFailed { idx });
        }
    }

    // Phase-5 apply ordering:
    //   (a) into_consumed: single consume_pageset_apply on the
    //       process-resources PageSet, free unclaimed workspace.
    //   (b) Ttbr0Guard arms the workspace pages as kernel-owned.
    //   (c) I-cache flush before any thread can start running.
    //   (d) init_process_header + per-mapping consume_pageset_apply
    //       loop + parent_copy_apply (all infallible by contract).
    //   (e) Defuse Ttbr0Guard + add_thread.
    let proc_kva = resources.proc_kva;
    let ht_kva = resources.ht_kva;
    let tcb_kva = resources.tcb_kva;
    let ttbr0 = resources.ttbr0;
    resources.donation.into_consumed(resources.workspace_claimed);

    let mut ttbr0_guard = Ttbr0Guard::new(ttbr0);

    // I-cache flush before the new process can fetch its first
    // instruction. Load-bearing: the kernel just wrote ELF text into
    // pages via the linear-map alias; the i-cache can hold stale
    // lines if the same paddrs were ever instruction-mapped before
    // for the kernel image. (ProcessObject header + thread count
    // were written in Phase 4 of provision_resources so the
    // create_tcb precondition was satisfied.)
    // SAFETY: standard cache-maintenance ops; broadcast via dsb ish.
    unsafe { core::arch::asm!("ic iallu", "dsb ish", "isb") };

    // Per-mapping consume_pageset_apply loop. Infallibility
    // precondition: the consume_pageset_validate loop above ran
    // within the same GKL-held critical section. revoke_apply walks
    // every process's handle table (including the parent's), clears
    // PTEs for active mappings, decrements refcount/map_count per
    // cleared slot, then unlinks and frees the header.
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

        // SAFETY: ht_kva was initialized as a valid handle table in
        // provision_resources Phase 4.
        let child_ht = unsafe { HandleTableRef::from_kva(ht_kva) };
        // Cannot fail: the child table was freshly allocated empty
        // with HANDLE_SLOTS_PER_PAGE empty slots, and the post-consume
        // capacity invariant was checked in the pure validate. Avoid
        // Result::expect so the panic path doesn't pull in Debug
        // formatting for SyscallError (would inflate exception stack).
        if child_ht.insert(parent.rights, child_kind).is_err() {
            panic!("child handle insert into fresh empty table failed (kernel-invariant violation)");
        }
    }

    // Defuse TTBR0 guard — child now owns its address space.
    ttbr0_guard.defuse();

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
    process_resources_ps: u32,
    name: [u8; 16],
    plan_builder: &mut ProcessCreationPlanBuilder,
) -> Result<ProvisionedResources, CreateProcessError> {
    // ============ Phase 1: Pure prerequisites + donation ============

    // has_room() before any donate so a full scheduler doesn't
    // burn through the donate-and-claim cycle.
    if !scheduler::has_room() {
        return Err(CreateProcessError::PlanError(
            CreateProcessPlanError::SchedulerFull));
    }

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

    // Donate the multi-page process-resources PageSet (NK4+NK5).
    // Validates Buddy origin, page count in [PROCESS_MIN, PROCESS_MAX].
    // Does NOT consume — the donation guard holds the typestate until
    // Phase 5's into_consumed call.
    let donation = crate::syscall::handler::donate_process_pages(process_resources_ps)
        .map_err(|e| match e {
            SyscallError::INVALID_HANDLE | SyscallError::INSUFFICIENT_RIGHTS =>
                CreateProcessError::BadHandle,
            _ => CreateProcessError::InvalidUserMemory,
        })?;

    // Resolve kernel-object KVAs from the donation. idx 0=ProcessObject,
    // 1=HandleTable, 2=kstack, 3=TCB; idx 4..N is the page-table
    // workspace. All four live in the TTBR1 direct map
    // (paddr + KERNEL_VA_OFFSET).
    //
    // `obj_kva` returns `Option` for indices outside the donation, but
    // `donate_process_pages` validated `data_page_count >=
    // PROCESS_MIN_PAGES = 15`, so indices 0..=3 are infallibly present.
    // `expect` keeps the unresolved-drop enforcement zone tight: between
    // `donate_process_pages` and the `phase_2_4` closure below we must
    // hold zero fallible `?` sites — otherwise `ProcessPagesDonation`'s
    // Drop would panic on an unresolved drop, which is louder than the
    // kernel-invariant assertion these `expect`s carry.
    let proc_kva = donation.obj_kva(0).expect("donation idx 0 within PROCESS_MIN_PAGES");
    let ht_kva = donation.obj_kva(1).expect("donation idx 1 within PROCESS_MIN_PAGES");
    let kstack_kva = donation.obj_kva(2).expect("donation idx 2 within PROCESS_MIN_PAGES");
    let tcb_kva = donation.obj_kva(3).expect("donation idx 3 within PROCESS_MIN_PAGES");

    // ============ Phase 2: ProcessObject init + mapping iteration + validate ============

    // Zero-init proc_kva (kernel-object page 0).
    // SAFETY: proc_kva derives from a Buddy-origin donation page;
    // sole writer under GKL; page is in the direct map.
    unsafe {
        let mut p = crate::mm::kernel_ptr::KernelMut::<u8>::from_kva(proc_kva);
        core::ptr::write_bytes(p.as_mut_ptr(), 0, PAGE_SIZE as usize);
    }

    // Set up first scratch page (for the temporary Mapping buffer the
    // AddressSpaceBuilder consumes — scratch lives in the caller's
    // scratch PageSet, NOT the donated process-resources PageSet).
    //
    // `scratch_ps.page(0)` is infallible here: the `scratch_count == 0`
    // rejection at line 442 (see above) ran before `donate_process_pages`,
    // so by this point `scratch_count >= 1` and idx 0 is valid. The
    // `expect` keeps the donate→closure window free of `?` that would
    // drop `donation` unresolved.
    let scratch_paddr = scratch_ps.page(0).expect("scratch idx 0 valid after scratch_count > 0 check");
    page_alloc::zero_page(scratch_paddr);
    // SAFETY: scratch_paddr is a kernel-allocated page; we use it as a
    // temporary Mapping buffer for AddressSpaceBuilder.
    let mut buf = unsafe { KernelMut::<Mapping>::from_paddr(scratch_paddr) };
    let mut mappings = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), MAPPINGS_PER_PAGE) };
    let mut cursor = ScratchCursor::new(scratch_count);

    // ============ Phase 2-4 body (fallible) =====================
    //
    // All fallible work between donate and Phase 5 runs inside this
    // inner block. The donation borrow stays alive for the block; on
    // Err the outer scope below calls `donation.into_caller_owned()`
    // so the PageSet stays caller-owned. On Ok we return the values
    // needed for ProvisionedResources and Phase 5 in the orchestrator.
    //
    // map_vmem_err narrows VmemError::OutOfPageTables to its own
    // CreateProcessError variant (so it surfaces as
    // SyscallError::OUT_OF_PAGE_TABLES, not the collapsed
    // OUT_OF_MEMORY). All four VmemError-producing sites
    // (AddressSpaceBuilder::new + three map_batch calls) thread
    // through this mapper.
    let map_vmem_err = |e: crate::arch::aarch64::vmem::VmemError| match e {
        // Both "donation workspace exhausted" and "L3RegionTracker
        // hit MAX_L3_TABLES" surface as OUT_OF_PAGE_TABLES — they
        // are the two ways a process can run out of page-table
        // backing for its mappings. Userspace distinguishes them
        // via the donation page count it provides; both signal
        // "ran out of page-table space" rather than the broader
        // OUT_OF_MEMORY (kernel-internal exhaustion).
        crate::arch::aarch64::vmem::VmemError::OutOfPageTables
        | crate::arch::aarch64::vmem::VmemError::TooManyL3Regions =>
            CreateProcessError::OutOfPageTables,
        _ => {
            log_create_process_oom("vmem builder");
            CreateProcessError::AddressSpaceMappingFailed
        }
    };

    let phase_2_4 = (|| -> Result<(PhysAddr, usize), CreateProcessError> {
        // ============ Phase 3: AddressSpaceBuilder from donation workspace ============

        let workspace_capacity =
            donation.data_page_count() - lockjaw_types::vmem::PROCESS_KERNEL_OBJ_PAGES;
        let workspace = lockjaw_types::vmem::PageTableWorkspace::new(workspace_capacity);
        let mut as_builder = AddressSpaceBuilder::new(&donation, workspace)
            .map_err(map_vmem_err)?;

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
            // Reject self-aliasing: if userspace passes the process-
            // resources donation handle as a mapping pageset_id, the
            // Phase 5 per-mapping consume loop would call
            // consume_pageset_apply on the donation header AFTER
            // into_consumed already did, tripping
            // pageset_header_pool::release's double-release assertion.
            // The donation handle has WRITE rights (for donate) and
            // implicit READ (so the mapping lookup succeeds); the
            // header-KVA comparison closes the alias gap by
            // construction.
            if ps_kva == donation.header_kva() {
                return Err(CreateProcessError::BadHandle);
            }
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
                    as_builder.map_batch(&mappings[..MAPPINGS_PER_PAGE])
                        .map_err(map_vmem_err)?;
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
                    as_builder.map_batch(&mappings[..MAPPINGS_PER_PAGE])
                        .map_err(map_vmem_err)?;
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
            as_builder.map_batch(&mappings[..cursor.pending_count()])
                .map_err(map_vmem_err)?;
        }
        let (ttbr0, workspace_claimed) = as_builder.finish();

        // ============ Phase 4: Init HandleTable + ProcessObject + TCB ============
        // No i-cache flush here — it moves to Phase 5 in
        // create_process, after into_consumed transfers ownership.

        // SAFETY: ht_kva is a Buddy-origin direct-map page (donation
        // index 1); GKL-held, sole writer.
        unsafe {
            create_handle_table(
                &HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
                ht_kva,
            ).map_err(|_| { log_create_process_oom("create_handle_table"); CreateProcessError::OutOfMemory })?;
        }

        // Write the ProcessObject header now so `create_tcb`'s
        // `process_ttbr0(proc_kva) != 0` invariant check sees a
        // populated TTBR0 field. owned_pages was populated by Phase 2
        // and is preserved by init_process_header (it only writes the
        // header fields).
        crate::cap::process_obj::init_process_header(
            proc_kva,
            ttbr0.as_u64(),
            ht_kva.as_u64(),
            false, // not immortal
            &name,
        );
        // First thread — increment via narrow op (count 0 → 1).
        // Bringing this into Phase 4 mirrors the create_tcb
        // precondition: the TCB belongs to a process whose thread
        // count is already accounted for.
        crate::cap::process_obj::process_inc_thread_count(proc_kva);

        // SAFETY: kstack_kva and tcb_kva are Buddy-origin direct-map
        // pages (donation indices 2 and 3); GKL-held, sole writer.
        // KernelStackBase::DirectMap carries the kstack KVA; the
        // finish_exit free path matches the regime via
        // stack_base.regime() (TD4 future-tracks splitting the
        // regime per-resource).
        unsafe {
            create_tcb(
                &TcbCreateInfo {
                    entry: process_entry,
                    stack: lockjaw_types::thread::KernelStackBase::DirectMap(kstack_kva),
                    process_kva: proc_kva,
                    user_entry_point: entry_point,
                    user_stack_top: stack_va + (stack_ps.count() as u64) * PAGE_SIZE,
                    user_stack_base: stack_va,
                    user_arg: 0,
                    name,
                },
                tcb_kva,
            ).map_err(|_| { log_create_process_oom("create_tcb"); CreateProcessError::OutOfMemory })?;
        }

        Ok((ttbr0, workspace_claimed))
    })();

    let (ttbr0, workspace_claimed) = match phase_2_4 {
        Ok(v) => v,
        Err(e) => {
            // Any failure between donate and Phase 5's into_consumed
            // leaves the donation caller-owned: userspace can
            // sys_close_handle to recover every donated page.
            donation.into_caller_owned();
            return Err(e);
        }
    };

    Ok(ProvisionedResources {
        donation,
        ttbr0,
        proc_kva,
        ht_kva,
        tcb_kva,
        workspace_claimed,
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
