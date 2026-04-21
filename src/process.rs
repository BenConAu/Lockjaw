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
    /// PageSet ID (from sys_alloc_pages) containing the physical page.
    pub pageset_id: u64,
    /// Index of the page within the PageSet (0 for single-page sets).
    pub page_index: u64,
    /// Flags: bit 0 = executable, bit 1 = writable.
    pub flags: u64,
}

// SAFETY: ProcessMapping is repr(C) with only u64 fields — every bit pattern valid.
unsafe impl lockjaw_types::user_pod::UserPod for ProcessMapping {}

const FLAG_EXECUTABLE: u64 = 1 << 0;

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
    // Look up stack PageSet early so we can validate total mapping count
    let stack_ps = PageSetRef::from_id(stack_pageset_id)
        .ok_or("invalid stack pageset")?;

    // Validate that user mappings + stack pages fit in the scratch buffer
    if !lockjaw_types::vmem::validate_process_mappings(mapping_count, stack_ps.count(), MAPPINGS_PER_PAGE) {
        return Err("mapping count exceeds buffer capacity");
    }

    // Use the caller-provided scratch page as the Mapping buffer.
    let scratch_ps = PageSetRef::from_id(scratch_pageset_id)
        .ok_or("invalid scratch pageset")?;
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

    for i in 0..mapping_count {
        // Read ProcessMapping from user memory via page table walk (TTBR1).
        let entry_va = mappings_va + (i as u64) * core::mem::size_of::<ProcessMapping>() as u64;
        let user_mapping: ProcessMapping = addr_space.read(entry_va)
            .ok_or("unmapped user mapping pointer")?;

        // Resolve the PageSet ID to a physical address
        let ps = PageSetRef::from_id(user_mapping.pageset_id)
            .ok_or("invalid pageset ID")?;
        let page_idx = user_mapping.page_index as usize;
        let phys = ps.page(page_idx).ok_or("page index out of range")?;

        mappings[count] = Mapping {
            virt_addr: user_mapping.virt_addr,
            phys_addr: phys,
            user_accessible: true,
            executable: (user_mapping.flags & FLAG_EXECUTABLE) != 0,
        };
        count += 1;
    }

    // Add stack pages contiguously at USER_STACK_BASE
    let stack_va: u64 = lockjaw_types::constants::USER_STACK_BASE;
    for s in 0..stack_ps.count() {
        mappings[count] = Mapping {
            virt_addr: stack_va + (s as u64) * PAGE_SIZE,
            phys_addr: stack_ps.page(s).ok_or("stack page missing")?,
            user_accessible: true,
            executable: false,
        };
        count += 1;
    }

    // Create address space
    // SAFETY: all physical addresses in mappings are from validated PageSets.
    let ttbr0 = unsafe { create_address_space(&mappings[..count]) }
        .map_err(|_| "address space creation failed")?;

    // Flush I-cache
    unsafe { core::arch::asm!("ic iallu", "dsb ish", "isb") };

    // Create handle table
    let ht_page = page_alloc::alloc_page().ok_or("out of pages for handle table")?;
    // SAFETY: ht_page is a freshly allocated kernel page.
    unsafe {
        create_handle_table(
            &HandleTableCreateInfo { slot_count: 16 },
            ht_page.start_addr(),
        ).map_err(|_| "handle table create failed")?;
    }

    // Create ProcessObject — owns address space + handle table
    let proc_page = page_alloc::alloc_page().ok_or("out of pages for process")?;
    crate::cap::process_obj::create_process_object(
        proc_page.start_addr(),
        ttbr0.as_u64(),
        ht_page.start_addr().as_u64(),
        false, // not immortal
        &name,
    );
    // First thread — increment via narrow op (count 0 → 1)
    crate::cap::process_obj::process_inc_thread_count(proc_page.start_addr());

    // Copy a handle from the parent's table into the child's table.
    if parent_handle_to_copy != u64::MAX {
        let parent_ht = CurrentThread::handle_table();
        let entry = parent_ht.lookup_any(
            parent_handle_to_copy as u32,
            crate::cap::rights::Rights::none(),
        ).map_err(|_| "parent handle lookup failed")?;

        // SAFETY: ht_page was just initialized as a valid handle table above.
        let child_ht = unsafe { HandleTableRef::from_paddr(ht_page.start_addr()) };
        child_ht.insert(
            PhysAddr::new(entry.object_paddr),
            entry.obj_type,
            entry.rights,
        ).map_err(|_| "child handle insert failed")?;
    }

    // Create TCB — first thread in this process
    let tcb_stack = page_alloc::alloc_page().ok_or("out of pages for TCB stack")?;
    let tcb_page = page_alloc::alloc_page().ok_or("out of pages for TCB")?;

    // SAFETY: stack and TCB are freshly allocated kernel pages.
    unsafe {
        create_tcb(
            &TcbCreateInfo {
                entry: process_entry,
                stack_paddr: tcb_stack.start_addr(),
                process_paddr: proc_page.start_addr(),
                user_entry_point: entry_point,
                user_stack_top: stack_va + (stack_ps.count() as u64) * PAGE_SIZE,
                user_stack_base: stack_va,
                name,
            },
            tcb_page.start_addr(),
        ).map_err(|_| "TCB create failed")?;
    }

    if !scheduler::add_thread(tcb_page.start_addr()) {
        return Err("scheduler run queue full");
    }

    Ok(())
}

/// Kernel-mode entry for new user processes. Reads entry point and stack
/// from TCB, then drops to EL0.
fn process_entry() -> ! {
    unsafe {
        // GKL held + IRQs masked (inherited from thread_entry).
        // Read TCB fields under the lock, then release before eret.
        let tcb_paddr = scheduler::current_tcb_paddr();
        let tcb = KernelRef::<Tcb>::from_paddr(tcb_paddr);
        let t = tcb.get();
        let entry = t.user_entry_point;
        let stack_top = t.user_stack_top;
        let ttbr0 = PhysAddr::new(
            crate::cap::process_obj::process_ttbr0(PhysAddr::new(t.process_paddr))
        );

        // Release GKL before dropping to EL0. eret restores user
        // SPSR with IRQs enabled — no explicit unmask needed.
        crate::sched::gkl::gkl_unlock();

        crate::arch::aarch64::mmu::drop_to_el0_with_ttbr0(ttbr0, entry, stack_top);
    }
}
