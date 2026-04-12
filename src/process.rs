use crate::arch::aarch64::vmem::{Mapping, create_address_space, VmemError, MAX_MAPPINGS};
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
    mappings_ptr: *const ProcessMapping,
    mapping_count: usize,
    entry_point: u64,
    stack_pageset_id: u64,
) -> Result<(), &'static str> {
    if mapping_count == 0 || mapping_count > MAX_MAPPINGS - 1 {
        return Err("invalid mapping count");
    }

    // Build the kernel-side Mapping array by reading from userspace one at a time.
    // This array is small (MAX_MAPPINGS = 32, each 32 bytes = 1024 bytes total)
    // and fits on the stack safely.
    let mut mappings = [Mapping {
        virt_addr: 0, phys_addr: PhysAddr::new(0),
        user_accessible: false, executable: false,
    }; MAX_MAPPINGS];
    let mut count = 0;

    for i in 0..mapping_count {
        let user_mapping = core::ptr::read(mappings_ptr.add(i));

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

    // Add stack page
    let (stack_count, stack_pages) = pageset_table::get_pageset(stack_pageset_id)
        .ok_or("invalid stack pageset")?;
    if stack_count == 0 {
        return Err("empty stack pageset");
    }

    // Use a fixed stack VA for new processes
    let stack_va: u64 = 0x0080_0000;
    mappings[count] = Mapping {
        virt_addr: stack_va,
        phys_addr: stack_pages[0],
        user_accessible: true,
        executable: false,
    };
    count += 1;

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

    // Create TCB
    let tcb_stack = page_alloc::alloc_page().ok_or("out of pages for TCB stack")?;
    let tcb_page = page_alloc::alloc_page().ok_or("out of pages for TCB")?;

    create_tcb(
        &TcbCreateInfo {
            entry: process_entry,
            stack_paddr: tcb_stack.start_addr(),
            handle_table_paddr: ht_page.start_addr(),
            ttbr0_paddr: ttbr0,
        },
        tcb_page.start_addr(),
    ).map_err(|_| "TCB create failed")?;

    // Store entry point and stack top in TCB's ipc_msg scratch
    let tcb_va = (tcb_page.start_addr().as_u64() + KERNEL_VA_OFFSET) as *mut Tcb;
    (*tcb_va).ipc_msg[0] = entry_point;
    (*tcb_va).ipc_msg[1] = stack_va + PAGE_SIZE;

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
        let tcb = (tcb_paddr.as_u64() + KERNEL_VA_OFFSET) as *const Tcb;
        let entry = (*tcb).ipc_msg[0];
        let stack_top = (*tcb).ipc_msg[1];
        let ttbr0 = PhysAddr::new((*tcb).ttbr0_paddr);

        crate::arch::aarch64::mmu::drop_to_el0_with_ttbr0(ttbr0, entry, stack_top);
    }
}
