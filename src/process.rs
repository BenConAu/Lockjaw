use crate::arch::aarch64::vmem::{Mapping, create_address_space, MAPPINGS_PER_PAGE};
use crate::cap::object::{HandleTableCreateInfo, create_handle_table};
use crate::cap::pageset_table;
use crate::mm::addr::{PhysAddr, KERNEL_VA_OFFSET, PAGE_SIZE};
use crate::mm::page_alloc;
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
///
/// # Safety
/// `mappings_ptr` must point to `mapping_count` valid ProcessMapping entries
/// in the caller's address space. TTBR0 must be the caller's page table.
pub unsafe fn create_process(
    mappings_va: u64,
    mapping_count: usize,
    entry_point: u64,
    stack_pageset_id: u64,
    scratch_pageset_id: u64,
    parent_handle_to_copy: u64,
    caller_ttbr0: PhysAddr,
) -> Result<(), &'static str> {
    // Look up stack PageSet early so we can validate total mapping count
    let (stack_count, stack_pages) = pageset_table::get_pageset(stack_pageset_id)
        .ok_or("invalid stack pageset")?;

    // Validate that user mappings + stack pages fit in the scratch buffer
    if !lockjaw_types::vmem::validate_process_mappings(mapping_count, stack_count, MAPPINGS_PER_PAGE) {
        return Err("mapping count exceeds buffer capacity");
    }

    // Use the caller-provided scratch page as the Mapping buffer.
    let (scratch_count, scratch_pages) = pageset_table::get_pageset(scratch_pageset_id)
        .ok_or("invalid scratch pageset")?;
    if scratch_count != 1 {
        return Err("scratch must be 1 page");
    }
    page_alloc::zero_page(scratch_pages[0]);
    // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
    let buf_va = (scratch_pages[0].as_u64() + KERNEL_VA_OFFSET) as *mut Mapping;
    let mappings = core::slice::from_raw_parts_mut(buf_va, MAPPINGS_PER_PAGE);
    let mut count = 0;

    for i in 0..mapping_count {
        // Read ProcessMapping from user memory via page table walk (TTBR1).
        let entry_va = mappings_va + (i as u64) * core::mem::size_of::<ProcessMapping>() as u64;
        let user_mapping: ProcessMapping = crate::mm::user_access::copy_from_user(caller_ttbr0, entry_va)
            .ok_or("unmapped user mapping pointer")?;

        // Resolve the PageSet ID to a physical address
        let (ps_count, ps_pages) = pageset_table::get_pageset(user_mapping.pageset_id)
            .ok_or("invalid pageset ID")?;

        let page_idx = user_mapping.page_index as usize;
        if page_idx >= ps_count {
            return Err("page index out of range");
        }

        mappings[count] = Mapping {
            virt_addr: user_mapping.virt_addr,
            phys_addr: ps_pages[page_idx],
            user_accessible: true,
            executable: (user_mapping.flags & FLAG_EXECUTABLE) != 0,
        };
        count += 1;
    }

    // Add stack pages contiguously at USER_STACK_BASE
    let stack_va: u64 = lockjaw_types::constants::USER_STACK_BASE;
    for s in 0..stack_count {
        mappings[count] = Mapping {
            virt_addr: stack_va + (s as u64) * PAGE_SIZE,
            phys_addr: stack_pages[s],
            user_accessible: true,
            executable: false,
        };
        count += 1;
    }

    // Create address space
    let ttbr0 = create_address_space(&mappings[..count])
        .map_err(|_| "address space creation failed")?;

    // Flush I-cache
    core::arch::asm!("ic iallu", "dsb ish", "isb");

    // Create handle table
    let ht_page = page_alloc::alloc_page().ok_or("out of pages for handle table")?;
    create_handle_table(
        &HandleTableCreateInfo { slot_count: 8 },
        ht_page.start_addr(),
    ).map_err(|_| "handle table create failed")?;

    // Copy a handle from the parent's table into the child's table.
    // This is the simplest form of capability transfer at process creation.
    if parent_handle_to_copy != u64::MAX {
        let parent_tcb_paddr = scheduler::current_tcb_paddr();
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let parent_tcb = (parent_tcb_paddr.as_u64() + KERNEL_VA_OFFSET) as *const crate::sched::tcb::Tcb;
        let parent_ht = PhysAddr::new((*parent_tcb).handle_table_paddr);

        let entry = crate::cap::handle_table::handle_lookup(
            parent_ht, parent_handle_to_copy as u32,
            crate::cap::rights::Rights::none(), // no rights check — just copy the entry
        ).map_err(|_| "parent handle lookup failed")?;

        crate::cap::handle_table::handle_insert(
            ht_page.start_addr(),
            PhysAddr::new(entry.object_paddr),
            entry.obj_type,
            entry.rights,
        ).map_err(|_| "child handle insert failed")?;
    }

    // Create TCB
    let tcb_stack = page_alloc::alloc_page().ok_or("out of pages for TCB stack")?;
    let tcb_page = page_alloc::alloc_page().ok_or("out of pages for TCB")?;

    create_tcb(
        &TcbCreateInfo {
            entry: process_entry,
            stack_paddr: tcb_stack.start_addr(),
            handle_table_paddr: ht_page.start_addr(),
            ttbr0_paddr: ttbr0,
            user_entry_point: entry_point,
            user_stack_top: stack_va + (stack_count as u64) * PAGE_SIZE,
            user_stack_base: stack_va,
        },
        tcb_page.start_addr(),
    ).map_err(|_| "TCB create failed")?;

    if !scheduler::add_thread(tcb_page.start_addr()) {
        return Err("scheduler run queue full");
    }

    Ok(())
}

/// Kernel-mode entry for new user processes. Reads entry point and stack
/// from TCB, then drops to EL0.
fn process_entry() -> ! {
    unsafe {
        let tcb_paddr = scheduler::current_tcb_paddr();
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let tcb = (tcb_paddr.as_u64() + KERNEL_VA_OFFSET) as *const Tcb;
        let entry = (*tcb).user_entry_point;
        let stack_top = (*tcb).user_stack_top;
        let ttbr0 = PhysAddr::new((*tcb).ttbr0_paddr);

        crate::arch::aarch64::mmu::drop_to_el0_with_ttbr0(ttbr0, entry, stack_top);
    }
}
