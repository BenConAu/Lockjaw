use crate::arch::aarch64::vmem::{Mapping, create_address_space, MAPPINGS_PER_PAGE};
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

/// A mapping entry provided by userspace in the sys_create_process call.
/// Lives in the caller's mapped memory — the kernel reads it one at a time.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessMapping {
    /// Virtual address in the new process's address space.
    pub virt_addr: u64,
    /// PageSet handle (from sys_alloc_pages) containing the physical page.
    pub pageset_id: u64,
    /// Index of the page within the PageSet (0 for single-page sets).
    pub page_index: u64,
    /// Flags: bit 0 = executable, bit 1 = writable.
    pub flags: u64,
}

// SAFETY: ProcessMapping is repr(C) with only u64 fields — every bit pattern valid.
unsafe impl lockjaw_types::user_pod::UserPod for ProcessMapping {}

const FLAG_EXECUTABLE: u64 = 1 << 0;

/// Drop guard for a kernel page — freed on drop unless defused.
struct PageGuard(Option<crate::mm::addr::PhysPage>);
impl PageGuard {
    fn new(page: crate::mm::addr::PhysPage) -> Self { Self(Some(page)) }
    fn addr(&self) -> PhysAddr { self.0.unwrap().start_addr() }
    fn defuse(&mut self) { self.0 = None; }
}
impl Drop for PageGuard {
    fn drop(&mut self) {
        if let Some(page) = self.0.take() {
            page_alloc::dealloc_page(page);
        }
    }
}

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
/// PageSet consumption (the destructive transfer of ownership from parent
/// to child) happens LAST, after all fallible steps have succeeded. This
/// ensures the parent retains its handles if any step fails.
pub fn create_process(
    addr_space: &UserAddressSpace,
    mappings_va: u64,
    mapping_count: usize,
    entry_point: u64,
    stack_pageset_id: u64,
    scratch_pageset_id: u64,
    parent_handle_to_copy: u64,
    name: [u8; 16],
) -> Result<(), &'static str> {
    // Look up stack and scratch PageSets from handle table.
    // These are handle indices, not raw pageset IDs.
    let ht = CurrentThread::handle_table();
    let stack_entry = ht.lookup(stack_pageset_id as u32,
        crate::cap::rights::Rights::from_bits(crate::cap::rights::RIGHT_READ),
        crate::cap::object::ObjectType::PageSet)
        .map_err(|_| "invalid stack handle")?;
    // SAFETY: object_paddr from a PageSet handle — valid header page.
    let stack_ps = unsafe { PageSetRef::from_header_paddr(stack_entry.object_paddr) };

    // Validate that user mappings + stack pages fit in the scratch buffer
    if !lockjaw_types::vmem::validate_process_mappings(mapping_count, stack_ps.count(), MAPPINGS_PER_PAGE) {
        return Err("mapping count exceeds buffer capacity");
    }

    // Use the caller-provided scratch page as the Mapping buffer.
    let scratch_entry = ht.lookup(scratch_pageset_id as u32,
        crate::cap::rights::Rights::from_bits(crate::cap::rights::RIGHT_READ),
        crate::cap::object::ObjectType::PageSet)
        .map_err(|_| "invalid scratch handle")?;
    let scratch_ps = unsafe { PageSetRef::from_header_paddr(scratch_entry.object_paddr) };
    if scratch_ps.count() != 1 {
        return Err("scratch must be 1 page");
    }
    let scratch_paddr = scratch_ps.page(0).ok_or("scratch page missing")?;
    page_alloc::zero_page(scratch_paddr);
    // SAFETY: scratch_paddr is a kernel-allocated page; we use it as a
    // temporary Mapping buffer for create_address_space.
    let mut buf = unsafe { KernelMut::<Mapping>::from_paddr(scratch_paddr) };
    let mappings = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), MAPPINGS_PER_PAGE) };
    let mut count = 0;

    // Allocate process page early so we can write owned_pages directly
    // into it during mapping resolution (avoids large stack arrays).
    let mut proc_guard = PageGuard::new(
        page_alloc::alloc_page().ok_or("out of pages for process")?
    );
    page_alloc::zero_page(proc_guard.addr());

    // Track unique PageSet headers for consumption (small — one per
    // distinct PageSet, not per page). 32 is generous for any process.
    const MAX_CONSUMED_HEADERS: usize = 32;
    let mut consumed_headers = [0u64; MAX_CONSUMED_HEADERS];
    let mut header_count: usize = 0;

    for i in 0..mapping_count {
        // Read ProcessMapping from user memory via page table walk (TTBR1).
        let entry_va = mappings_va + (i as u64) * core::mem::size_of::<ProcessMapping>() as u64;
        let user_mapping: ProcessMapping = addr_space.read(entry_va)
            .ok_or("unmapped user mapping pointer")?;

        // Resolve the PageSet handle to a physical address
        let ps_entry = ht.lookup(user_mapping.pageset_id as u32,
            crate::cap::rights::Rights::from_bits(crate::cap::rights::RIGHT_READ),
            crate::cap::object::ObjectType::PageSet)
            .map_err(|_| "invalid pageset handle")?;
        // SAFETY: object_paddr from a PageSet handle — valid header page.
        let ps = unsafe { PageSetRef::from_header_paddr(ps_entry.object_paddr) };
        let page_idx = user_mapping.page_index as usize;
        let phys = ps.page(page_idx).ok_or("page index out of range")?;

        mappings[count] = Mapping {
            virt_addr: user_mapping.virt_addr,
            phys_addr: phys,
            user_accessible: true,
            executable: (user_mapping.flags & FLAG_EXECUTABLE) != 0,
        };
        count += 1;

        // Record data page directly in ProcessObject (no stack array)
        if !crate::cap::process_obj::process_push_owned_page(
            proc_guard.addr(), phys.as_u64()
        ) {
            return Err("too many owned pages");
        }

        // Track unique PageSet headers for consumption
        let hdr = ps_entry.object_paddr;
        if !consumed_headers[..header_count].contains(&hdr) {
            if header_count >= MAX_CONSUMED_HEADERS {
                return Err("too many PageSets for ownership transfer");
            }
            consumed_headers[header_count] = hdr;
            header_count += 1;
        }
    }

    // Add stack pages contiguously at USER_STACK_BASE
    let stack_va: u64 = lockjaw_types::constants::USER_STACK_BASE;
    for s in 0..stack_ps.count() {
        let phys = stack_ps.page(s).ok_or("stack page missing")?;
        mappings[count] = Mapping {
            virt_addr: stack_va + (s as u64) * PAGE_SIZE,
            phys_addr: phys,
            user_accessible: true,
            executable: false,
        };
        count += 1;

        // Record stack pages directly in ProcessObject
        if !crate::cap::process_obj::process_push_owned_page(
            proc_guard.addr(), phys.as_u64()
        ) {
            return Err("too many owned pages");
        }
    }

    // Track stack PageSet header for consumption
    let stack_hdr = stack_entry.object_paddr;
    if !consumed_headers[..header_count].contains(&stack_hdr) {
        if header_count >= MAX_CONSUMED_HEADERS {
            return Err("too many PageSets for ownership transfer");
        }
        consumed_headers[header_count] = stack_hdr;
        header_count += 1;
    }

    // Create address space
    // SAFETY: all physical addresses in mappings are from validated PageSets.
    let ttbr0 = unsafe { create_address_space(&mappings[..count]) }
        .map_err(|_| "address space creation failed")?;
    let mut ttbr0_guard = Ttbr0Guard::new(ttbr0);

    // Flush I-cache
    unsafe { core::arch::asm!("ic iallu", "dsb ish", "isb") };

    // Create handle table
    let mut ht_guard = PageGuard::new(
        page_alloc::alloc_page().ok_or("out of pages for handle table")?
    );
    // SAFETY: ht_page is a freshly allocated kernel page.
    unsafe {
        create_handle_table(
            &HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
            ht_guard.addr(),
        ).map_err(|_| "handle table create failed")?;
    }

    // Write ProcessObject header into the pre-allocated page.
    // owned_pages was already populated during mapping resolution above.
    crate::cap::process_obj::init_process_header(
        proc_guard.addr(),
        ttbr0_guard.addr().as_u64(),
        ht_guard.addr().as_u64(),
        false, // not immortal
        &name,
    );

    // First thread — increment via narrow op (count 0 → 1)
    crate::cap::process_obj::process_inc_thread_count(proc_guard.addr());

    // Copy a handle from the parent's table into the child's table.
    // This is the simplest form of capability transfer at process creation.
    if parent_handle_to_copy != u64::MAX {
        let parent_ht = CurrentThread::handle_table();
        let entry = parent_ht.lookup_any(
            parent_handle_to_copy as u32,
            crate::cap::rights::Rights::none(),
        ).map_err(|_| "parent handle lookup failed")?;

        // SAFETY: ht_guard was just initialized as a valid handle table above.
        let child_ht = unsafe { HandleTableRef::from_paddr(ht_guard.addr()) };
        child_ht.insert(
            PhysAddr::new(entry.object_paddr),
            entry.obj_type,
            entry.rights,
        ).map_err(|_| "child handle insert failed")?;
    }

    // Create TCB — first thread in this process
    let mut tcb_stack_guard = PageGuard::new(
        page_alloc::alloc_page().ok_or("out of pages for TCB stack")?
    );
    let mut tcb_guard = PageGuard::new(
        page_alloc::alloc_page().ok_or("out of pages for TCB")?
    );

    // SAFETY: stack and TCB are freshly allocated kernel pages.
    unsafe {
        create_tcb(
            &TcbCreateInfo {
                entry: process_entry,
                stack_paddr: tcb_stack_guard.addr(),
                process_paddr: proc_guard.addr(),
                user_entry_point: entry_point,
                user_stack_top: stack_va + (stack_ps.count() as u64) * PAGE_SIZE,
                user_stack_base: stack_va,
                user_arg: 0,
                name,
            },
            tcb_guard.addr(),
        ).map_err(|_| "TCB create failed")?;
    }

    // --- Phase 1: Tear down parent's VA mappings (fallible) ---
    // Must happen BEFORE the point of no return. If any unmap fails,
    // the entire create_process fails and guards clean up everything.
    let parent_ttbr0 = addr_space.ttbr0();
    for i in 0..header_count {
        let hdr = consumed_headers[i];
        let header = unsafe { crate::cap::pageset_table::read_header(hdr) };
        let page_count = header.data_page_count();
        let pages = &header.pages[..page_count];

        let (total_mapped, unmapped) = ht.unmap_for_object(hdr, |va| {
            unsafe {
                crate::arch::aarch64::vmem::unmap_validated(parent_ttbr0, va, pages).is_ok()
            }
        });

        if unmapped != total_mapped {
            return Err("parent unmap failed during ownership transfer");
        }

        // Decrement map_count for successfully unmapped handles
        if unmapped > 0 {
            unsafe {
                let hdr_mut = crate::cap::pageset_table::read_header_mut(hdr);
                for _ in 0..unmapped {
                    hdr_mut.dec_map_count();
                }
            }
        }
    }

    // Enqueue the thread — last fallible step.
    if !scheduler::add_thread(tcb_guard.addr()) {
        return Err("scheduler run queue full");
    }

    // === Point of no return ===
    // All fallible steps succeeded (including parent unmaps).
    // Defuse guards and consume PageSets.
    proc_guard.defuse();
    ht_guard.defuse();
    ttbr0_guard.defuse();
    tcb_stack_guard.defuse();
    tcb_guard.defuse();

    // --- Phase 2: Consume transferred PageSets (infallible) ---
    // Parent unmaps already succeeded above, so this is safe.
    for i in 0..header_count {
        crate::cap::pageset_table::consume_pageset(consumed_headers[i], &ht);
    }

    Ok(())
}

/// Kernel-mode entry for new user processes. Reads entry point and stack
/// from TCB, then drops to EL0.
pub fn process_entry() -> ! {
    unsafe {
        // GKL held + IRQs masked (inherited from thread_entry).
        // Read TCB fields under the lock, then release before eret.
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = KernelRef::<Tcb>::from_paddr(tcb_paddr);
        let t = tcb.get();
        let entry = t.user_entry_point;
        let stack_top = t.user_stack_top;
        let user_arg = t.user_arg;
        let ttbr0 = PhysAddr::new(
            crate::cap::process_obj::process_ttbr0(PhysAddr::new(t.process_paddr))
        );

        // Release GKL before dropping to EL0. eret restores user
        // SPSR with IRQs enabled — no explicit unmask needed.
        crate::sched::gkl::gkl_unlock();

        crate::arch::aarch64::mmu::drop_to_el0_with_ttbr0(ttbr0, entry, stack_top, user_arg);
    }
}
