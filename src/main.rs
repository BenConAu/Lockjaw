#![no_std]
#![no_main]

mod arch;
mod cap;
mod elf;
mod ipc;
mod mm;
mod print;
mod process;
mod sched;
mod syscall;

use arch::aarch64::uart::Uart;

extern "C" {
    static __bss_start: u8;
    static __bss_end: u8;
    static __kernel_end: u8;
    static __guard_page: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
}

/// DTB PageSet ID, set at boot. Returned by sys_get_boot_info.
static mut DTB_PAGESET_ID: u64 = 0;

/// Get the DTB PageSet ID (called by sys_get_boot_info handler).
pub fn dtb_pageset_id() -> u64 {
    unsafe { DTB_PAGESET_ID }
}

#[no_mangle]
pub extern "C" fn kmain() -> ! {
    kprintln!("=== Lockjaw Microkernel v{} ===", env!("CARGO_PKG_VERSION"));
    kprintln!("Target: AArch64 (ARMv8-A), QEMU virt");

    // QEMU bare-metal boot places the DTB at the start of RAM.
    // Ref: https://wiki.osdev.org/QEMU_AArch64_Virt_Bare_Bones
    let dtb_paddr = arch::aarch64::platform::RAM_BASE;
    kprintln!("DTB: paddr {:#x}", dtb_paddr);
    kprintln!();

    unsafe {
        let bss_start = &__bss_start as *const u8 as usize;
        let bss_end = &__bss_end as *const u8 as usize;
        let kernel_end = &__kernel_end as *const u8 as usize;
        let stack_bottom = &__stack_bottom as *const u8 as usize;
        let stack_top = &__stack_top as *const u8 as usize;

        kprintln!("Memory layout:");
        kprintln!("  Kernel load:  0x{:08x}", arch::aarch64::platform::KERNEL_LOAD_ADDR);
        kprintln!("  BSS:          0x{:08x} - 0x{:08x} ({} bytes)", bss_start, bss_end, bss_end - bss_start);
        kprintln!("  Kernel end:   0x{:08x}", kernel_end);
        kprintln!("  Stack:        0x{:08x} - 0x{:08x} ({} bytes)", stack_bottom, stack_top, stack_top - stack_bottom);
    }

    kprintln!();
    kprintln!("Physical memory: {:#x} - {:#x} ({} pages)",
        mm::addr::RAM_START.as_u64(),
        mm::addr::RAM_END.as_u64(),
        mm::addr::TOTAL_PAGES);

    // Initialize page allocator — reserve firmware + kernel + stack pages
    unsafe {
        let kernel_start = mm::addr::PhysAddr::new(arch::aarch64::platform::KERNEL_LOAD_ADDR);
        let stack_top = mm::addr::PhysAddr::new(&__stack_top as *const u8 as u64);
        mm::page_alloc::init(kernel_start, stack_top);
    }

    // Enable MMU with identity mapping
    kprintln!();
    kprintln!("Enabling MMU (identity map)...");
    unsafe {
        arch::aarch64::mmu::init_boot_page_tables();
        arch::aarch64::mmu::enable_mmu();
    }
    kprintln!("MMU enabled — UART still working!");

    // Enable higher-half kernel mapping
    kprintln!();
    kprintln!("Enabling higher-half kernel mapping...");
    unsafe {
        arch::aarch64::mmu::enable_higher_half();
        Uart::use_high_addresses();
    }
    kprintln!("Higher-half active — UART at {:#x}", 0xFFFF_0000_0900_0000u64);

    // Verify DTB at RAM_BASE (placed there by QEMU bare-metal boot)
    unsafe {
        let dtb_va = (dtb_paddr + mm::addr::KERNEL_VA_OFFSET) as *const u8;
        let magic = u32::from_be_bytes([
            *dtb_va, *dtb_va.add(1), *dtb_va.add(2), *dtb_va.add(3),
        ]);
        kprintln!("DTB: {:#x}, magic={:#010x} ({})", dtb_paddr, magic,
            if magic == 0xd00dfeed { "valid" } else { "INVALID" });
    }

    // Register DTB pages as a PageSet so userspace can map them normally.
    // This avoids the MAIR_DEVICE aliasing problem (DTB is normal RAM, not MMIO).
    unsafe {
        let mut dtb_pages = [mm::addr::PhysAddr::new(0); lockjaw_types::pageset_table::MAX_PAGES_PER_SET];
        dtb_pages[0] = mm::addr::PhysAddr::new(dtb_paddr);
        dtb_pages[1] = mm::addr::PhysAddr::new(dtb_paddr + mm::addr::PAGE_SIZE);
        let dtb_ps_id = cap::pageset_table::register_existing(2, dtb_pages)
            .expect("DTB PageSet registration failed");
        DTB_PAGESET_ID = dtb_ps_id;
        kprintln!("DTB PageSet registered: id={}", dtb_ps_id);
    }

    // Set up guard page (unmapped) and stack canary
    kprintln!();
    unsafe {
        let guard_phys = mm::addr::PhysAddr::new(&__guard_page as *const u8 as u64);
        kprintln!("Setting up guard page at phys {:#x}...", guard_phys.as_u64());
        arch::aarch64::mmu::setup_guard_page(guard_phys);
        kprintln!("Guard page active (unmapped).");

        mm::stack::init_canary();
    }
    mm::stack::check_canary();
    kprintln!("Stack canary intact.");

    // Install exception vector table
    kprintln!();
    unsafe { arch::aarch64::exceptions::init(); }
    kprintln!("Exception vectors installed.");

    // Initialize GICv3 interrupt controller
    unsafe { arch::aarch64::gic::init(); }

    // Initialize timer and unmask IRQs
    unsafe {
        arch::aarch64::timer::init();
        // Unmask IRQ exceptions (clear the I bit in DAIF)
        core::arch::asm!("msr DAIFClr, #2");    // Unmask IRQ (bit 1 of DAIF)
    }
    kprintln!("IRQs unmasked.");

    // Wait for a few ticks to verify timer is working
    kprintln!();
    kprintln!("Waiting for timer ticks...");
    while arch::aarch64::timer::tick_count() < 5 {
        core::hint::spin_loop();
    }
    kprintln!("  {} ticks received!", arch::aarch64::timer::tick_count());

    // Verification: alloc 10 pages, dealloc, realloc — should get same addresses
    kprintln!();
    kprintln!("Page allocator test:");
    let mut pages = [None; 10];
    for i in 0..10 {
        pages[i] = mm::page_alloc::alloc_page();
        kprintln!("  alloc  {}: {:#x}", i, pages[i].unwrap().start_addr().as_u64());
    }
    for i in 0..10 {
        mm::page_alloc::dealloc_page(pages[i].unwrap());
    }
    kprintln!("  (deallocated all 10)");
    for i in 0..10 {
        let f = mm::page_alloc::alloc_page().unwrap();
        kprintln!("  realloc {}: {:#x}", i, f.start_addr().as_u64());
    }

    // Page table entry verification
    kprintln!();
    kprintln!("Page table entry test:");
    use mm::page_table::*;
    let entry = PageTableEntry::new_page(
        mm::addr::PhysAddr::new(0x4008_0000),
        MAIR_NORMAL,
        AP_RW_EL1,
        SH_INNER,
    );
    kprintln!("  raw:  {:#018x}", entry.raw());
    kprintln!("  {:?}", entry);

    let table_entry = PageTableEntry::new_table(mm::addr::PhysAddr::new(0x4009_0000));
    kprintln!("  table: {:#018x} valid={} is_table={}", table_entry.raw(), table_entry.is_valid(), table_entry.is_table());

    let block_entry = PageTableEntry::new_block(
        mm::addr::PhysAddr::new(0x0000_0000),
        MAIR_DEVICE,
        AP_RW_EL1,
        SH_NON,
    );
    kprintln!("  block: {:#018x} is_block={} attr={}", block_entry.raw(), block_entry.is_block(), block_entry.attr_index());

    // Object model: PageSet → donate → create handle table
    kprintln!();
    kprintln!("Object model test:");
    use cap::object::*;
    use cap::pageset;
    use cap::handle_table::*;
    use cap::rights::*;

    let ht_info = HandleTableCreateInfo { slot_count: 8 };
    let ht_size = query_handle_table_size(&ht_info);
    kprintln!("  HandleTable(8 slots) needs {} page(s)", ht_size.pages);

    // Allocate a pageset and donate it for the handle table
    let ps = pageset::alloc_pages(ht_size.pages).expect("alloc_pages failed");
    kprintln!("  PageSet allocated: {} page(s) at {:#x}", ps.count, ps.pages[0].as_u64());

    let ht_paddr = pageset::donate(&ps, ht_size.pages).expect("donate failed");
    unsafe { create_handle_table(&ht_info, ht_paddr).expect("create failed"); }

    // Read back the header to verify
    let header_va = ht_paddr.as_u64() + mm::addr::KERNEL_VA_OFFSET;
    let header = unsafe { &*(header_va as *const HandleTableHeader) };
    kprintln!("  Created: type={:?}, pages={}, slots={}",
        header.header.obj_type, header.header.page_count, header.slot_count);

    // Insert a handle pointing to the table itself (for testing)
    let h0 = unsafe {
        handle_insert(ht_paddr, ht_paddr, ObjectType::HandleTable, Rights::from_bits(RIGHT_READ | RIGHT_WRITE))
    }.expect("insert failed");
    kprintln!("  Inserted handle {} (RW)", h0);

    // Look up with matching rights — should succeed
    let entry = unsafe { handle_lookup(ht_paddr, h0, Rights::from_bits(RIGHT_READ)) }.expect("lookup failed");
    kprintln!("  Lookup h{}: type={:?}, rights={:#04x}", h0, entry.obj_type, entry.rights.bits());

    // Look up with Grant right — should fail (we only gave RW)
    let bad = unsafe { handle_lookup(ht_paddr, h0, Rights::from_bits(RIGHT_GRANT)) };
    kprintln!("  Lookup h{} with Grant: {:?}", h0, bad.err().unwrap());

    // Remove the handle
    let removed = unsafe { handle_remove(ht_paddr, h0) }.expect("remove failed");
    kprintln!("  Removed h{}: type={:?}", h0, removed.obj_type);

    // Verify slot is now empty
    let empty = unsafe { handle_lookup(ht_paddr, h0, Rights::none()) };
    kprintln!("  Lookup h{} after remove: {:?}", h0, empty.err().unwrap());

    // --- Phase 5: Threads & Scheduling ---
    kprintln!();
    kprintln!("Starting threads...");

    // --- Phase 7: IPC Setup ---
    // Create an endpoint and handle tables for the sender/receiver threads
    unsafe {
        use sched::tcb::{TcbCreateInfo, create_tcb};
        use cap::handle_table;

        // Create endpoint object
        let ep_page = mm::page_alloc::alloc_page().expect("endpoint alloc").start_addr();
        ipc::endpoint::create_endpoint(ep_page).expect("create endpoint");
        kprintln!("  Endpoint created at phys {:#x}", ep_page.as_u64());

        // Create handle table for sender (Thread A)
        let ht_a_page = mm::page_alloc::alloc_page().expect("ht alloc").start_addr();
        let ht_info = cap::object::HandleTableCreateInfo { slot_count: 8 };
        cap::object::create_handle_table(&ht_info, ht_a_page).expect("create ht a");
        handle_table::handle_insert(ht_a_page, ep_page, ObjectType::Endpoint,
            cap::rights::Rights::from_bits(cap::rights::RIGHT_READ | cap::rights::RIGHT_WRITE)).expect("insert ep handle");

        // Create handle table for receiver (Thread B) — also needs RW for ping-pong
        let ht_b_page = mm::page_alloc::alloc_page().expect("ht alloc").start_addr();
        cap::object::create_handle_table(&ht_info, ht_b_page).expect("create ht b");
        handle_table::handle_insert(ht_b_page, ep_page, ObjectType::Endpoint,
            cap::rights::Rights::from_bits(cap::rights::RIGHT_READ | cap::rights::RIGHT_WRITE)).expect("insert ep handle");

        // Thread A (sender) — sends messages on the endpoint
        let stack_a = mm::page_alloc::alloc_page().expect("stack alloc").start_addr();
        let tcb_a_page = mm::page_alloc::alloc_page().expect("tcb alloc").start_addr();
        create_tcb(&TcbCreateInfo { entry: ipc_sender, stack_paddr: stack_a, handle_table_paddr: ht_a_page, ttbr0_paddr: mm::addr::PhysAddr::new(0), user_entry_point: 0, user_stack_top: 0 }, tcb_a_page)
            .expect("create tcb a");

        // Thread B (receiver) — receives and prints messages
        let stack_b = mm::page_alloc::alloc_page().expect("stack alloc").start_addr();
        let tcb_b_page = mm::page_alloc::alloc_page().expect("tcb alloc").start_addr();
        create_tcb(&TcbCreateInfo { entry: ipc_receiver, stack_paddr: stack_b, handle_table_paddr: ht_b_page, ttbr0_paddr: mm::addr::PhysAddr::new(0), user_entry_point: 0, user_stack_top: 0 }, tcb_b_page)
            .expect("create tcb b");

        // Register idle thread (index 0 = this boot thread, uses the boot stack).
        // This thread drops to EL0 and becomes the init process, so it needs
        // a handle table for init's syscalls (create_endpoint, etc).
        let idle_stack_base = &__stack_bottom as *const u8 as u64 + mm::addr::KERNEL_VA_OFFSET;
        let idle_ht_page = mm::page_alloc::alloc_page().expect("idle ht alloc").start_addr();
        create_handle_table(
            &HandleTableCreateInfo { slot_count: 8 },
            idle_ht_page,
        ).expect("idle ht create");

        let idle_tcb_page = mm::page_alloc::alloc_page().expect("idle tcb alloc").start_addr();
        let idle_tcb_va = (idle_tcb_page.as_u64() + mm::addr::KERNEL_VA_OFFSET) as *mut sched::tcb::Tcb;
        core::ptr::write(idle_tcb_va, sched::tcb::Tcb {
            header: ObjectHeader { obj_type: ObjectType::ThreadControlBlock, page_count: 1 },
            saved_sp: 0,
            state: sched::tcb::ThreadState::Running,
            entry: idle_thread,
            stack_base: idle_stack_base,
            handle_table_paddr: idle_ht_page.as_u64(),
            ttbr0_paddr: 0,
            ipc_blocked_on: 0,
            ipc_msg: [0; 4],
            user_entry_point: 0,
            user_stack_top: 0,
            wait_objects: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
            wait_thresholds: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
            wait_types: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
            wait_count: 0,
        });

        sched::scheduler::add_thread(idle_tcb_page);  // index 0: idle/boot
        sched::scheduler::add_thread(tcb_a_page);      // index 1: thread A
        sched::scheduler::add_thread(tcb_b_page);      // index 2: thread B
        sched::scheduler::start();
    }

    kprintln!("Scheduler started.");
    kprintln!();

    // --- Phase 8: Load init process from embedded ELF ---
    kprintln!();
    kprintln!("Loading init process...");

    // The init ELF binary, built separately and embedded at compile time
    static INIT_ELF: &[u8] = include_bytes!("../user/init/target/aarch64-unknown-none/release/lockjaw-init");

    unsafe {
        use arch::aarch64::vmem::{Mapping, create_address_space, MAPPINGS_PER_PAGE};

        // Parse the ELF
        let elf_info = elf::parse_elf(INIT_ELF).expect("failed to parse init ELF");
        kprintln!("  Entry point: {:#x}", elf_info.entry_point);
        kprintln!("  {} loadable segment(s)", elf_info.segment_count);

        // Allocate a page for the mapping buffer (avoids large array on the kernel stack)
        let map_buf = mm::page_alloc::alloc_page().expect("mapping buffer page");
        mm::page_alloc::zero_page(map_buf.start_addr());
        let map_buf_va = (map_buf.start_addr().as_u64() + mm::addr::KERNEL_VA_OFFSET) as *mut Mapping;
        let mappings = core::slice::from_raw_parts_mut(map_buf_va, MAPPINGS_PER_PAGE);
        let mut mapping_count = 0;

        for i in 0..elf_info.segment_count {
            let seg = &elf_info.segments[i];
            let num_pages = ((seg.mem_size + mm::addr::PAGE_SIZE - 1) / mm::addr::PAGE_SIZE) as usize;
            kprintln!("  Segment {}: VA {:#x}, {} page(s), {}{}",
                i, seg.vaddr, num_pages,
                if seg.executable { "X" } else { "-" },
                if seg.writable { "W" } else { "R" });

            for p in 0..num_pages {
                assert!(mapping_count < MAPPINGS_PER_PAGE, "init ELF has too many pages for mapping buffer");
                let page = mm::page_alloc::alloc_page().expect("segment page");
                let page_va = page.start_addr().as_u64() + mm::addr::KERNEL_VA_OFFSET;

                // Copy file data into this page (if any)
                let seg_page_offset = (p as u64) * mm::addr::PAGE_SIZE;
                let file_start = seg.file_offset + seg_page_offset;
                let file_remaining = if seg.file_size > seg_page_offset {
                    core::cmp::min(seg.file_size - seg_page_offset, mm::addr::PAGE_SIZE)
                } else {
                    0
                };

                // Zero the page first (for BSS-style segments where mem_size > file_size)
                mm::page_alloc::zero_page(page.start_addr());

                if file_remaining > 0 {
                    let src = &INIT_ELF[file_start as usize..(file_start + file_remaining) as usize];
                    core::ptr::copy_nonoverlapping(src.as_ptr(), page_va as *mut u8, file_remaining as usize);
                }

                mappings[mapping_count] = Mapping {
                    virt_addr: seg.vaddr + seg_page_offset,
                    phys_addr: page.start_addr(),
                    user_accessible: true,
                    executable: seg.executable,
                };
                mapping_count += 1;
            }
        }

        // Allocate user stack page
        let stack_page = mm::page_alloc::alloc_page().expect("user stack page");
        let user_stack_va: u64 = lockjaw_types::constants::USER_STACK_BASE;
        let user_stack_top: u64 = user_stack_va + mm::addr::PAGE_SIZE;
        mappings[mapping_count] = Mapping {
            virt_addr: user_stack_va,
            phys_addr: stack_page.start_addr(),
            user_accessible: true,
            executable: false,
        };
        mapping_count += 1;

        // Create the address space (allocate page tables, map everything)
        let ttbr0 = create_address_space(&mappings[..mapping_count])
            .expect("failed to create address space");
        kprintln!("  Address space created: TTBR0 = {:#x}", ttbr0.as_u64());

        // Store the TTBR0 in the boot/idle thread's TCB so that syscalls
        // from the init process can find the caller's address space.
        let current_tcb_paddr = sched::scheduler::current_tcb_paddr();
        let current_tcb = (current_tcb_paddr.as_u64() + mm::addr::KERNEL_VA_OFFSET) as *mut sched::tcb::Tcb;
        (*current_tcb).ttbr0_paddr = ttbr0.as_u64();

        // Flush I-cache (we copied code into pages)
        core::arch::asm!(
            "ic iallu",                           // Invalidate entire I-cache
            "dsb ish",
            "isb",
        );

        kprintln!("Dropping to EL0...");
        arch::aarch64::mmu::drop_to_el0_with_ttbr0(
            ttbr0,
            elf_info.entry_point,
            user_stack_top,
        );
    }
}

// ---------------------------------------------------------------------------
// IPC test threads (Phase 7)
// ---------------------------------------------------------------------------

/// Helper: look up endpoint at handle 0 for the current thread.
unsafe fn lookup_endpoint() -> (mm::addr::PhysAddr, mm::addr::PhysAddr) {
    let tcb_paddr = sched::scheduler::current_tcb_paddr();
    let tcb = (tcb_paddr.as_u64() + mm::addr::KERNEL_VA_OFFSET) as *const sched::tcb::Tcb;
    let ht_paddr = mm::addr::PhysAddr::new((*tcb).handle_table_paddr);
    let entry = cap::handle_table::handle_lookup(
        ht_paddr, 0,
        cap::rights::Rights::from_bits(cap::rights::RIGHT_READ | cap::rights::RIGHT_WRITE),
    ).expect("handle lookup");
    (mm::addr::PhysAddr::new(entry.object_paddr), tcb_paddr)
}

/// Client thread: calls the server with a request, gets a reply.
/// Uses ipc_call (send + block for reply in one operation).
fn ipc_sender() -> ! {
    const BENCHMARK_ROUNDS: u64 = 10000;
    let mut counter: u64 = 1;

    // Warm up
    for _ in 0..10 {
        unsafe {
            let (ep, tcb) = lookup_endpoint();
            ipc::endpoint::ipc_call(ep, [counter, 0, 0, 0], tcb).expect("call");
        }
        counter += 1;
    }

    // Benchmark using call/reply pattern
    let start_tick = arch::aarch64::timer::tick_count();
    for _ in 0..BENCHMARK_ROUNDS {
        unsafe {
            let (ep, tcb) = lookup_endpoint();
            let reply = ipc::endpoint::ipc_call(ep, [counter, 0, 0, 0], tcb).expect("call");
            // Print first few to verify the server doubled our value
            if counter <= 13 {
                kprintln!("[IPC] call({}) -> reply({})", counter, reply[0]);
            }
        }
        counter += 1;
    }
    let end_tick = arch::aarch64::timer::tick_count();
    let elapsed_ticks = end_tick - start_tick;

    kprintln!();
    kprintln!("[IPC BENCHMARK] {} call/reply round-trips in {} ticks", BENCHMARK_ROUNDS, elapsed_ticks);
    if elapsed_ticks > 0 {
        kprintln!("[IPC BENCHMARK] {} round-trips per tick", BENCHMARK_ROUNDS / elapsed_ticks);
    }

    loop {
        unsafe {
            let (ep, tcb) = lookup_endpoint();
            ipc::endpoint::ipc_call(ep, [counter, 0, 0, 0], tcb).expect("call");
        }
        counter += 1;
    }
}

/// Server thread: receives a request, doubles the first value, replies.
fn ipc_receiver() -> ! {
    loop {
        unsafe {
            let (ep, tcb) = lookup_endpoint();
            let msg = ipc::endpoint::ipc_receive(ep, tcb).expect("receive");
            let reply = [msg[0] * 2, msg[1], msg[2], msg[3]];
            ipc::endpoint::ipc_reply(ep, reply).expect("reply");
        }
    }
}

fn idle_thread() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let mut uart = Uart::new();

    let _ = writeln!(uart, "\n!!! KERNEL PANIC !!!");
    if let Some(location) = info.location() {
        let _ = writeln!(uart, "  at {}:{}", location.file(), location.line());
    }
    let _ = writeln!(uart, "  {}", info.message());

    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
