#![no_std]
#![no_main]

mod arch;
mod cap;
mod mm;
mod print;

use arch::aarch64::uart::Uart;

extern "C" {
    static __bss_start: u8;
    static __bss_end: u8;
    static __kernel_end: u8;
    static __guard_page: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
}

#[no_mangle]
pub extern "C" fn kmain() -> ! {
    kprintln!("=== Lockjaw Microkernel v{} ===", env!("CARGO_PKG_VERSION"));
    kprintln!("Target: AArch64 (ARMv8-A), QEMU virt");
    kprintln!();

    unsafe {
        let bss_start = &__bss_start as *const u8 as usize;
        let bss_end = &__bss_end as *const u8 as usize;
        let kernel_end = &__kernel_end as *const u8 as usize;
        let stack_bottom = &__stack_bottom as *const u8 as usize;
        let stack_top = &__stack_top as *const u8 as usize;

        kprintln!("Memory layout:");
        kprintln!("  Kernel load:  0x{:08x}", 0x4008_0000u64);
        kprintln!("  BSS:          0x{:08x} - 0x{:08x} ({} bytes)", bss_start, bss_end, bss_end - bss_start);
        kprintln!("  Kernel end:   0x{:08x}", kernel_end);
        kprintln!("  Stack:        0x{:08x} - 0x{:08x} ({} bytes)", stack_bottom, stack_top, stack_top - stack_bottom);
    }

    kprintln!();
    kprintln!("Physical memory: {:#x} - {:#x} ({} frames)",
        mm::addr::RAM_START.as_u64(),
        mm::addr::RAM_END.as_u64(),
        mm::addr::TOTAL_FRAMES);

    // Initialize frame allocator — reserve firmware + kernel + stack frames
    unsafe {
        let kernel_start = mm::addr::PhysAddr::new(0x4008_0000);
        let stack_top = mm::addr::PhysAddr::new(&__stack_top as *const u8 as u64);
        mm::frame::init(kernel_start, stack_top);
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

    // Verification: alloc 10 frames, dealloc, realloc — should get same addresses
    kprintln!();
    kprintln!("Frame allocator test:");
    let mut frames = [None; 10];
    for i in 0..10 {
        frames[i] = mm::frame::alloc_frame();
        kprintln!("  alloc  {}: {:#x}", i, frames[i].unwrap().start_addr().as_u64());
    }
    for i in 0..10 {
        mm::frame::dealloc_frame(frames[i].unwrap());
    }
    kprintln!("  (deallocated all 10)");
    for i in 0..10 {
        let f = mm::frame::alloc_frame().unwrap();
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

    kprintln!();
    kprintln!("Boot complete.");

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
