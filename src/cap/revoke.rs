//! Cross-process handle revocation.
//!
//! Two-phase walker that finds every handle in every live process's
//! handle table referencing a given PageSet header, validates that
//! revocation can succeed, then (in a paired apply call) clears the
//! PTEs and slots.
//!
//! See `docs/handle-revocation-plan.md` for the design rationale.
//! Caller must hold the GKL across the validate→apply pair; under
//! GKL, no other thread can mutate handle tables or page tables, so
//! the apply walk's action stream matches the validate walk's
//! exactly and apply cannot fail.
//!
//! Used by `consume_pageset_validate` / `consume_pageset_apply` to
//! make ownership transfer (kernel-object creation, sys_create_process)
//! transactional: the parent's old PageSet handles are revoked from
//! every process — including any cross-process exports — before the
//! header is unlinked and freed.

use crate::cap::handle_table::HandleTableRef;
use crate::cap::process_obj;
use crate::mm::addr::PhysAddr;
use crate::mm::kernel_ptr::KernelRef;
use crate::sched::scheduler;
use crate::sched::tcb::Tcb;
use lockjaw_types::object::HandleKind;

/// Why revocation could not be validated.
///
/// Both variants are recoverable: returning Err from
/// `revoke_validate` leaves every handle table and page table
/// in exactly the state they were in at entry. The caller (typically
/// `consume_pageset_validate`) propagates the failure to userspace
/// as a syscall error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeError {
    /// Sum of cross-process handles or active mappings did not
    /// reconcile with the header's running counters. Indicates a
    /// missed inc_refcount/inc_map_count somewhere or table
    /// corruption.
    AccountingMismatch {
        snapshot_refcount: u32,
        snapshot_map_count: u32,
        walked_refcount: usize,
        walked_map_count: usize,
    },
    /// A cross-process L3 PTE for a mapped slot did not match the
    /// PageSet's expected physical pages. Either the user's page
    /// table diverged from the kernel's PageSet record, or someone
    /// installed a block mapping over the L3 entries.
    UnmapFailed {
        process_paddr: u64,
        va: u64,
    },
}

/// Bounded dedup buffer for the per-process walk. The scheduler's
/// run queue holds at most `MAX_THREADS = 16` TCBs; multiple TCBs
/// may share a process, so the unique-process count is at most 16.
const MAX_VISITED_PROCESSES: usize = 16;

/// Phase 1: read-only walk. For every live process's handle table,
/// count handles to `object_paddr` and verify any active PageSet
/// mapping's PTEs match the header's expected pages.
///
/// Returns Ok if accounting reconciles with the header's snapshot
/// refcount / map_count and every PTE walk succeeded — apply is
/// then guaranteed safe under the GKL precondition. Returns Err
/// **with no state mutated** otherwise.
///
/// Caller must hold the GKL.
pub fn revoke_validate(object_paddr: u64) -> Result<(), RevokeError> {
    // Snapshot the header's counters before walking. revoke_validate
    // never writes the header; the snapshot is what we reconcile against.
    let (snapshot_refcount, snapshot_map_count) = unsafe {
        let h = crate::cap::pageset_table::read_header(object_paddr);
        (h.refcount, h.map_count)
    };

    let mut walked_refcount: usize = 0;
    let mut walked_map_count: usize = 0;
    let mut first_error: Option<RevokeError> = None;

    for_each_unique_process(|process_paddr| {
        if first_error.is_some() {
            return;
        }
        let ttbr0 = process_obj::process_ttbr0(PhysAddr::new(process_paddr));
        let ht = unsafe {
            HandleTableRef::from_paddr(process_obj::process_handle_table(
                PhysAddr::new(process_paddr),
            ))
        };
        ht.revoke_validate(object_paddr, |action| {
            walked_refcount += 1;
            if action.had_mapping {
                let va = (action.mapped_va_page as u64) << 12;
                // Read the header inside each iteration: the borrow is
                // short-lived and clear_validated_pte (apply phase) may
                // dec_map_count between iterations, so caching across
                // iterations would be misleading.
                // SAFETY: object_paddr is a registered PageSet that
                // reached revoke from an active handle slot; the
                // wrapper makes pages_slice safe.
                let backed = unsafe {
                    crate::cap::pageset_table::read_header_backed(object_paddr)
                };
                let expected = backed.pages_slice();
                let ok = unsafe {
                    crate::arch::aarch64::vmem::validate_pte_match(
                        PhysAddr::new(ttbr0),
                        va,
                        expected,
                    )
                };
                if ok.is_err() {
                    first_error = Some(RevokeError::UnmapFailed {
                        process_paddr,
                        va,
                    });
                    return;
                }
                walked_map_count += 1;
            }
        });
    });

    if let Some(e) = first_error {
        return Err(e);
    }

    if walked_refcount != snapshot_refcount as usize
        || walked_map_count != snapshot_map_count as usize
    {
        return Err(RevokeError::AccountingMismatch {
            snapshot_refcount,
            snapshot_map_count,
            walked_refcount,
            walked_map_count,
        });
    }

    Ok(())
}

/// Phase 2: write walk. For every live process's handle table,
/// clear PTEs for active PageSet mappings, decrement the header's
/// map_count and refcount per cleared slot, then zero the slot.
///
/// Returns `(slots_cleared, mappings_cleared, processes_visited)`
/// for the caller to surface as a diagnostic. `slots_cleared` matches
/// the snapshot refcount validate observed; `mappings_cleared` matches
/// the snapshot map_count.
///
/// MUST be called only after a matching successful `revoke_validate`
/// for the same `object_paddr` within the same critical section
/// (no GKL release between). Cannot fail under that precondition.
/// After return:
/// - `header.refcount == 0 && header.map_count == 0`
/// - No handle in any process's table references `object_paddr`
/// - No PTE in any process's address space references the data pages
///
/// The caller may now free the header pages.
pub fn revoke_apply(object_paddr: u64) -> RevokeStats {
    let mut stats = RevokeStats::default();
    for_each_unique_process(|process_paddr| {
        stats.processes += 1;
        let ttbr0 = process_obj::process_ttbr0(PhysAddr::new(process_paddr));
        let ht = unsafe {
            HandleTableRef::from_paddr(process_obj::process_handle_table(
                PhysAddr::new(process_paddr),
            ))
        };
        ht.revoke_apply(object_paddr, |action| {
            stats.slots += 1;
            if action.had_mapping {
                stats.mappings += 1;
                let va = (action.mapped_va_page as u64) << 12;
                // Use the trusted count for the PTE clear range — a
                // corrupted on-disk header.count could otherwise
                // truncate the unmap and leave stale PTEs behind.
                let count = unsafe {
                    crate::cap::pageset_table::read_header_backed(object_paddr)
                        .data_page_count()
                };
                // SAFETY: validate_pte_match succeeded for this exact
                // (ttbr0, va, count) in the validate phase; the GKL
                // has been held continuously, so the page table is
                // unchanged. clear_validated_pte panics if the
                // precondition is violated — see lockjaw-types/page_table.rs.
                unsafe {
                    crate::arch::aarch64::vmem::clear_validated_pte(
                        PhysAddr::new(ttbr0),
                        va,
                        count,
                    );
                }
                unsafe {
                    crate::cap::pageset_table::read_header_mut(object_paddr)
                        .dec_map_count();
                }
            }
            if matches!(action.kind, HandleKind::PageSet { .. }) {
                unsafe {
                    crate::cap::pageset_table::read_header_mut(object_paddr)
                        .dec_refcount();
                }
            }
        });
    });
    stats
}

/// Per-revoke counts surfaced for diagnostic logging. The plan calls
/// for a "revoke OK" message at the end of each consume so a single
/// `make run` boot proves the walker actually ran (and how often).
#[derive(Clone, Copy, Debug, Default)]
pub struct RevokeStats {
    /// Unique processes walked during the apply pass.
    pub processes: usize,
    /// Total handle-table slots cleared across all processes.
    /// Equals the header's snapshot refcount (validated in phase 1).
    pub slots: usize,
    /// Subset of cleared slots that had an active mapping.
    /// Equals the header's snapshot map_count.
    pub mappings: usize,
}

/// Walk the run queue, deduplicate by process_paddr, and invoke
/// `f` once per unique non-kernel process. Kernel threads
/// (process_paddr == 0) are skipped — they do not own a handle
/// table that could hold user-visible PageSet handles.
fn for_each_unique_process(mut f: impl FnMut(u64)) {
    let mut visited = [0u64; MAX_VISITED_PROCESSES];
    let mut visited_count: usize = 0;

    scheduler::for_each_tcb(|tcb_paddr| {
        // SAFETY: tcb_paddr came from the scheduler's threads array,
        // which holds only registered TCBs in kernel-owned pages.
        let process_paddr = unsafe {
            let tcb = KernelRef::<Tcb>::from_paddr(tcb_paddr);
            tcb.get().process_paddr
        };
        if process_paddr == 0 {
            return;
        }
        for i in 0..visited_count {
            if visited[i] == process_paddr {
                return;
            }
        }
        if visited_count < MAX_VISITED_PROCESSES {
            visited[visited_count] = process_paddr;
            visited_count += 1;
        }
        f(process_paddr);
    });
}
