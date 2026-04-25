#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

mod arch;
mod cap;
pub mod crash;
mod elf;
mod ipc;
mod mm;
pub mod percpu;
mod print;
mod process;
mod sched;
mod syscall;

use arch::aarch64::uart::Uart;

extern "C" {
    static __bss_start: u8;
    static __bss_end: u8;
    static __kernel_end: u8;
    static __guard_page_0: u8;
    static __guard_page_1: u8;
    static __guard_page_2: u8;
    static __guard_page_3: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
    static __per_cpu_stacks: u8;
    static __per_cpu_stacks_end: u8;
}

/// A value set exactly once during boot, read-only after. Replaces
/// `static mut` for boot-time globals with a safer API that catches
/// double-init via debug_assert.
struct BootOnce(core::cell::UnsafeCell<u64>);
unsafe impl Sync for BootOnce {}

impl BootOnce {
    const fn new() -> Self {
        BootOnce(core::cell::UnsafeCell::new(0))
    }

    /// Set the value. Panics in debug builds if already set.
    fn set(&self, val: u64) {
        // SAFETY: single-core, called during boot before scheduler starts.
        unsafe {
            debug_assert_eq!(*self.0.get(), 0, "BootOnce already set");
            *self.0.get() = val;
        }
    }

    /// Read the value. Returns 0 if never set.
    fn get(&self) -> u64 {
        // SAFETY: single-core; written once at boot, read-only after.
        unsafe { *self.0.get() }
    }
}

/// DTB PageSet ID, set once at boot. Returned by sys_get_boot_info.
static DTB_PAGESET_ID: BootOnce = BootOnce::new();

/// Get the DTB PageSet ID (called by sys_get_boot_info handler).
pub fn dtb_pageset_id() -> u64 {
    DTB_PAGESET_ID.get()
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
        // SAFETY: linker symbol
        let bss_start = &__bss_start as *const u8 as usize;
        // SAFETY: linker symbol
        let bss_end = &__bss_end as *const u8 as usize;
        // SAFETY: linker symbol
        let kernel_end = &__kernel_end as *const u8 as usize;
        // SAFETY: linker symbol
        let stack_bottom = &__stack_bottom as *const u8 as usize;
        // SAFETY: linker symbol
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

    // Initialize page allocator — reserve firmware + kernel + per-CPU stacks.
    // The 2 MB alignment of __per_cpu_stacks creates a gap between
    // __kernel_end and the stacks. We must free that gap explicitly so
    // those pages aren't silently wasted.
    unsafe {
        let kernel_start = mm::addr::PhysAddr::new(arch::aarch64::platform::KERNEL_LOAD_ADDR);
        // SAFETY: linker symbols
        let kernel_end = mm::addr::PhysAddr::new(&__kernel_end as *const u8 as u64);
        // SAFETY: linker symbol — 2 MB-aligned start of per-CPU stacks
        let stacks_start = mm::addr::PhysAddr::new(&__per_cpu_stacks as *const u8 as u64);
        // SAFETY: linker symbol — end of all per-CPU stacks
        let stacks_end = mm::addr::PhysAddr::new(&__per_cpu_stacks_end as *const u8 as u64);
        mm::page_alloc::init_with_gap(kernel_start, kernel_end, stacks_start, stacks_end);
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
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let dtb_va = (dtb_paddr + mm::addr::KERNEL_VA_OFFSET) as *const u8;
        let magic = u32::from_be_bytes([
            *dtb_va, *dtb_va.add(1), *dtb_va.add(2), *dtb_va.add(3),
        ]);
        kprintln!("DTB: {:#x}, magic={:#010x} ({})", dtb_paddr, magic,
            if magic == 0xd00dfeed { "valid" } else { "INVALID" });
    }

    // Register DTB pages as a PageSet so userspace can map them normally.
    // This avoids the MAIR_DEVICE aliasing problem (DTB is normal RAM, not MMIO).
    // Compute page count from the DTB header's totalsize field instead of
    // hardcoding — the DTB size varies with -smp and -device flags.
    {
        let dtb_content_end = unsafe {
            // SAFETY: kernel VA, DTB header validated above
            let h = (dtb_paddr + mm::addr::KERNEL_VA_OFFSET) as *const u8;
            let header = core::slice::from_raw_parts(h, 40);
            lockjaw_types::fdt::dtb_content_size(header)
                .expect("DTB header invalid") as u64
        };
        let dtb_page_count = ((dtb_content_end + mm::addr::PAGE_SIZE - 1) / mm::addr::PAGE_SIZE) as usize;
        assert!(dtb_page_count <= 16, "DTB content too large: {} pages", dtb_page_count);
        let mut dtb_pages = [mm::addr::PhysAddr::new(0); 16];
        for i in 0..dtb_page_count {
            dtb_pages[i] = mm::addr::PhysAddr::new(dtb_paddr + (i as u64) * mm::addr::PAGE_SIZE);
        }
        let dtb_ps_id = cap::pageset_table::register_existing(dtb_page_count, &dtb_pages[..dtb_page_count])
            .expect("DTB PageSet registration failed");
        DTB_PAGESET_ID.set(dtb_ps_id);
        kprintln!("DTB PageSet registered: id={}, {} pages ({} bytes content)", dtb_ps_id, dtb_page_count, dtb_content_end);
    }

    // Set up guard pages (unmapped) for all per-CPU stacks and init canary
    kprintln!();
    unsafe {
        let guard_pages = [
            // SAFETY: linker symbol — per-CPU guard page physical address
            mm::addr::PhysAddr::new(&__guard_page_0 as *const u8 as u64),
            // SAFETY: linker symbol — per-CPU guard page physical address
            mm::addr::PhysAddr::new(&__guard_page_1 as *const u8 as u64),
            // SAFETY: linker symbol — per-CPU guard page physical address
            mm::addr::PhysAddr::new(&__guard_page_2 as *const u8 as u64),
            // SAFETY: linker symbol — per-CPU guard page physical address
            mm::addr::PhysAddr::new(&__guard_page_3 as *const u8 as u64),
        ];
        kprintln!("Setting up {} guard pages...", guard_pages.len());
        arch::aarch64::mmu::setup_guard_pages(&guard_pages);
        kprintln!("Guard pages active (unmapped).");

        mm::stack::init_canary();
    }
    mm::stack::check_canary();
    kprintln!("Stack canary intact.");

    // Initialize per-CPU data for the boot CPU (CPU 0)
    percpu::init_percpu(0);
    kprintln!("CPU {} initialized (TPIDR_EL1)", percpu::cpu_id());

    // Boot secondary CPUs via PSCI CPU_ON
    {
        extern "C" { fn _secondary_start(); }
        // SAFETY: _secondary_start is the assembly entry point for secondaries.
        // It is a physical address (identity-mapped) that sets up the per-CPU
        // stack and calls secondary_main(cpu_id).
        // SAFETY: _secondary_start is the assembly entry point symbol
        let entry = _secondary_start as *const () as u64;
        for cpu in 1..arch::aarch64::platform::MAX_CPUS {
            let ret = unsafe { arch::aarch64::psci::cpu_on(cpu as u64, entry, cpu as u64) };
            if ret == 0 {
                kprintln!("[SMP] CPU {} started (PSCI OK)", cpu);
            } else {
                kprintln!("[SMP] CPU {} PSCI failed: {}", cpu, ret);
            }
        }
        // Brief delay for secondaries to print their online messages
        // before boot continues. Not correctness-critical — just keeps
        // serial output readable.
        for _ in 0..100_000 { core::hint::spin_loop(); }
    }

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

    let ht_info = HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE };
    let ht_size = query_handle_table_size(&ht_info);
    kprintln!("  HandleTable({} slots) needs {} page(s)", ht_info.slot_count, ht_size.pages);

    // Allocate a pageset and donate it for the handle table
    let ps = pageset::alloc_pages(ht_size.pages).expect("alloc_pages failed");
    kprintln!("  PageSet allocated: {} page(s) at {:#x}", ps.count, ps.pages[0].as_u64());

    let ht_paddr = pageset::donate(&ps, ht_size.pages).expect("donate failed");
    unsafe { create_handle_table(&ht_info, ht_paddr).expect("create failed"); }

    // Read back the header to verify
    let header_va = ht_paddr.as_u64() + mm::addr::KERNEL_VA_OFFSET;
    // SAFETY: kernel object at known VA
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

    // --- Process lifecycle test ---
    // Exercises the core new semantic: thread_count > 1, exit one
    // (process stays alive), exit the other (process freed).
    {
        use lockjaw_types::process::ProcessLifecycle;

        let test_ht = mm::page_alloc::alloc_page().expect("test ht").start_addr();
        unsafe {
            cap::object::create_handle_table(
                &cap::object::HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
                test_ht,
            ).expect("test ht create");
        }
        let test_proc = mm::page_alloc::alloc_page().expect("test proc").start_addr();
        cap::process_obj::create_process_object(
            test_proc, 0, test_ht.as_u64(), false, b"test-process\0\0\0\0",
        );
        // Simulate 2 threads
        cap::process_obj::process_inc_thread_count(test_proc); // 0 → 1
        cap::process_obj::process_inc_thread_count(test_proc); // 1 → 2

        // First thread exits — process stays alive
        let r1 = cap::process_obj::process_dec_thread_count(test_proc);
        match r1 {
            ProcessLifecycle::ThreadsRemaining(1) => {}
            other => panic!("expected ThreadsRemaining(1), got {:?}", other),
        }

        // Second thread exits — process should be freed
        let r2 = cap::process_obj::process_dec_thread_count(test_proc);
        match r2 {
            ProcessLifecycle::LastThread => {}
            other => panic!("expected LastThread, got {:?}", other),
        }

        // Clean up test pages (process would normally be freed by finish_exit)
        mm::page_alloc::dealloc_page(mm::addr::PhysPage::containing(test_ht));
        mm::page_alloc::dealloc_page(mm::addr::PhysPage::containing(test_proc));
        kprintln!("Process lifecycle test passed.");
    }

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
        ipc::endpoint::create_endpoint(mm::addr::ObjectInitPage::new(ep_page)).expect("create endpoint");
        kprintln!("  Endpoint created at phys {:#x}", ep_page.as_u64());

        // Reply object for the ipc_sender benchmark thread. One page,
        // pre-allocated and stashed in a static so ipc_sender can pass it
        // on every call without needing a handle table lookup.
        let bench_reply_page = mm::page_alloc::alloc_page().expect("bench reply alloc").start_addr();
        ipc::reply::create_reply(mm::addr::ObjectInitPage::new(bench_reply_page)).expect("create bench reply");
        IPC_BENCH_REPLY_PADDR.set(bench_reply_page.as_u64());

        // Create kernel process — immortal, ttbr0=0, owns all kernel threads.
        let kernel_ht_page = mm::page_alloc::alloc_page().expect("kernel ht alloc").start_addr();
        create_handle_table(
            &HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
            kernel_ht_page,
        ).expect("kernel ht create");

        let kernel_proc_page = mm::page_alloc::alloc_page().expect("kernel proc alloc").start_addr();
        cap::process_obj::create_process_object(
            kernel_proc_page,
            0, // ttbr0 = 0 (kernel process)
            kernel_ht_page.as_u64(),
            true, // immortal
            b"kernel\0\0\0\0\0\0\0\0\0\0",
        );

        // Insert endpoint + reply handles into kernel handle table
        handle_table::handle_insert(kernel_ht_page, ep_page, ObjectType::Endpoint,
            cap::rights::Rights::from_bits(cap::rights::RIGHT_READ | cap::rights::RIGHT_WRITE)).expect("insert ep handle");
        handle_table::handle_insert(kernel_ht_page, bench_reply_page, ObjectType::Reply,
            cap::rights::Rights::from_bits(cap::rights::RIGHT_READ | cap::rights::RIGHT_WRITE)).expect("insert reply handle");

        // Thread A (sender) — kernel thread in the kernel process
        cap::process_obj::process_inc_thread_count(kernel_proc_page);
        let stack_a = mm::page_alloc::alloc_page().expect("stack alloc").start_addr();
        let tcb_a_page = mm::page_alloc::alloc_page().expect("tcb alloc").start_addr();
        create_tcb(&TcbCreateInfo { entry: ipc_sender, stack_paddr: stack_a, process_paddr: kernel_proc_page, user_entry_point: 0, user_stack_top: 0, user_stack_base: 0, user_arg: 0, name: *b"ipc-sender\0\0\0\0\0\0" }, tcb_a_page)
            .expect("create tcb a");

        // Thread B (receiver) — kernel thread in the kernel process
        cap::process_obj::process_inc_thread_count(kernel_proc_page);
        let stack_b = mm::page_alloc::alloc_page().expect("stack alloc").start_addr();
        let tcb_b_page = mm::page_alloc::alloc_page().expect("tcb alloc").start_addr();
        create_tcb(&TcbCreateInfo { entry: ipc_receiver, stack_paddr: stack_b, process_paddr: kernel_proc_page, user_entry_point: 0, user_stack_top: 0, user_stack_base: 0, user_arg: 0, name: *b"ipc-receiver\0\0\0\0" }, tcb_b_page)
            .expect("create tcb b");

        // Register idle/init thread (index 0 = this boot thread).
        // This thread drops to EL0 and becomes the init process, so it
        // gets its own user process (created later in the ELF loading path).
        // For now it belongs to the kernel process.
        // SAFETY: linker symbol
        let idle_stack_base = &__stack_bottom as *const u8 as u64 + mm::addr::KERNEL_VA_OFFSET;
        cap::process_obj::process_inc_thread_count(kernel_proc_page);

        let idle_tcb_page = mm::page_alloc::alloc_page().expect("idle tcb alloc").start_addr();
        let mut idle_tcb = mm::kernel_ptr::KernelMut::<sched::tcb::Tcb>::from_paddr(idle_tcb_page);
        core::ptr::write(idle_tcb.as_mut_ptr(), sched::tcb::Tcb {
            header: ObjectHeader { obj_type: ObjectType::ThreadControlBlock, page_count: 1 },
            saved_sp: 0,
            entry: idle_thread,
            stack_base: idle_stack_base,
            process_paddr: kernel_proc_page.as_u64(),
            ipc_blocked_on: 0,
            ipc_msg: [0; 4],
            ipc_queue_next: 0,
            ipc_wait_kind: 0,
            current_reply_paddr: 0,
            ipc_call_reply_paddr: 0,
            user_entry_point: 0,
            user_stack_top: 0,
            user_stack_base: 0,
            user_arg: 0,
            wait_objects: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
            wait_thresholds: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
            wait_types: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
            wait_count: 0,
            current_syscall: u64::MAX,
            current_syscall_args: [0; 4],
            name: *b"init\0\0\0\0\0\0\0\0\0\0\0\0",
        });

        sched::scheduler::add_thread(idle_tcb_page);  // index 0: idle/boot (CPU 0)
        sched::scheduler::add_thread(tcb_a_page);      // index 1: thread A
        sched::scheduler::add_thread(tcb_b_page);      // index 2: thread B

        // Per-CPU idle threads for secondary CPUs. Constructed manually
        // (not via create_tcb) because secondary_main IS the idle thread:
        // the TCB uses the per-CPU boot stack from the linker script, and
        // saved_sp=0 (same as the boot thread — never been switched out).
        // When the scheduler first context-switches away from a secondary,
        // it saves the real SP (which is on the per-CPU boot stack) into
        // saved_sp. When switched back, it resumes in secondary_main's
        // wfi loop.
        {
            let stack_bottoms = [
                // SAFETY: linker symbol — per-CPU stack bottom for CPU 1
                &__guard_page_1 as *const u8 as u64 + 4096 + mm::addr::KERNEL_VA_OFFSET,
                // SAFETY: linker symbol — per-CPU stack bottom for CPU 2
                &__guard_page_2 as *const u8 as u64 + 4096 + mm::addr::KERNEL_VA_OFFSET,
                // SAFETY: linker symbol — per-CPU stack bottom for CPU 3
                &__guard_page_3 as *const u8 as u64 + 4096 + mm::addr::KERNEL_VA_OFFSET,
            ];
            for (i, &stack_base) in stack_bottoms.iter().enumerate() {
                let cpu = i + 1;
                cap::process_obj::process_inc_thread_count(kernel_proc_page);
                let tcb_page = mm::page_alloc::alloc_page().expect("secondary idle tcb").start_addr();
                let mut tcb_km = mm::kernel_ptr::KernelMut::<sched::tcb::Tcb>::from_paddr(tcb_page);
                let mut name = *b"idle-cpu0\0\0\0\0\0\0\0";
                name[8] = b'0' + cpu as u8;
                core::ptr::write(tcb_km.as_mut_ptr(), sched::tcb::Tcb {
                    header: ObjectHeader { obj_type: ObjectType::ThreadControlBlock, page_count: 1 },
                    saved_sp: 0,
                    entry: idle_thread,
                    stack_base,
                    process_paddr: kernel_proc_page.as_u64(),
                    ipc_blocked_on: 0,
                    ipc_msg: [0; 4],
                    ipc_queue_next: 0,
                    ipc_wait_kind: 0,
                    current_reply_paddr: 0,
                    ipc_call_reply_paddr: 0,
                    user_entry_point: 0,
                    user_stack_top: 0,
                    user_stack_base: 0,
                    user_arg: 0,
                    wait_objects: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
                    wait_thresholds: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
                    wait_types: [0; lockjaw_types::wait::MAX_WAIT_OBJECTS],
                    wait_count: 0,
                    current_syscall: u64::MAX,
                    current_syscall_args: [0; 4],
                    name,
                });
                sched::scheduler::add_thread_for_cpu(tcb_page, cpu);
            }
        }

        // Do NOT call scheduler::start() here. CPU 0 still has kernel
        // setup work to do (ELF loading, process creation) outside the
        // GKL. Secondaries have timers armed — if start() flips active
        // now, their timer ticks would begin scheduling while CPU 0 is
        // unsynchronized. start() is called right before drop_to_el0.
    }

    kprintln!();

    // --- Phase 8: Load init process from embedded ELF ---
    kprintln!();
    kprintln!("Loading init process...");

    // The init ELF binary, built separately and embedded at compile time
    static INIT_ELF: &[u8] = include_bytes!("../user/init/target/aarch64-unknown-none/release/lockjaw-init");

    // Verify the init binary was built from the same source as the kernel
    kprintln!("Build hash: {:#018x}", LOCKJAW_SOURCE_HASH);
    match lockjaw_types::elf::find_section_u64(INIT_ELF, ".lockjaw_hash") {
        Some(init_hash) if init_hash == LOCKJAW_SOURCE_HASH => {
            kprintln!("Init hash:  {:#018x} (match)", init_hash);
        }
        Some(init_hash) => {
            kprintln!("FATAL: init binary build hash mismatch!");
            kprintln!("  kernel: {:#018x}", LOCKJAW_SOURCE_HASH);
            kprintln!("  init:   {:#018x}", init_hash);
            kprintln!("  Run 'make build' to rebuild all binaries.");
            panic!("stale init binary");
        }
        None => {
            kprintln!("WARNING: init binary has no .lockjaw_hash section");
            kprintln!("  Cannot verify build coherence. Run 'make build'.");
        }
    }

    unsafe {
        use arch::aarch64::vmem::{Mapping, create_address_space, MAPPINGS_PER_PAGE};

        // Parse the ELF
        let elf_info = elf::parse_elf(INIT_ELF).expect("failed to parse init ELF");
        kprintln!("  Entry point: {:#x}", elf_info.entry_point);
        kprintln!("  {} loadable segment(s)", elf_info.segment_count);

        // Allocate a page for the mapping buffer (avoids large array on the kernel stack)
        let map_buf = mm::page_alloc::alloc_page().expect("mapping buffer page");
        mm::page_alloc::zero_page(map_buf.start_addr());
        let mut map_buf_km = mm::kernel_ptr::KernelMut::<Mapping>::from_paddr(map_buf.start_addr());
        let mappings = core::slice::from_raw_parts_mut(map_buf_km.as_mut_ptr(), MAPPINGS_PER_PAGE);
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
                    let mut page_km = mm::kernel_ptr::KernelMut::<u8>::from_paddr(page.start_addr());
                    core::ptr::copy_nonoverlapping(src.as_ptr(), page_km.as_mut_ptr(), file_remaining as usize);
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

        // Allocate user stack (8 pages = 32KB for init, which embeds and spawns
        // multiple processes including the ramfb display driver)
        let user_stack_pages = 8;
        let user_stack_va: u64 = lockjaw_types::constants::USER_STACK_BASE;
        let user_stack_top: u64 = user_stack_va + (user_stack_pages as u64) * mm::addr::PAGE_SIZE;
        for s in 0..user_stack_pages {
            let stack_page = mm::page_alloc::alloc_page().expect("user stack page");
            mappings[mapping_count] = Mapping {
                virt_addr: user_stack_va + (s as u64) * mm::addr::PAGE_SIZE,
                phys_addr: stack_page.start_addr(),
                user_accessible: true,
                executable: false,
            };
            mapping_count += 1;
        }

        // Create the address space (allocate page tables, map everything)
        let ttbr0 = create_address_space(&mappings[..mapping_count])
            .expect("failed to create address space");
        kprintln!("  Address space created: TTBR0 = {:#x}", ttbr0.as_u64());

        // Create init user process with its own handle table and address
        // space. Init's handle table starts empty — init creates its own
        // handles via syscalls from userspace (sys_create_endpoint, etc.).
        let init_ht_page = mm::page_alloc::alloc_page().expect("init ht alloc").start_addr();
        cap::object::create_handle_table(
            &cap::object::HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
            init_ht_page,
        ).expect("init ht create");

        let init_proc_page = mm::page_alloc::alloc_page().expect("init proc alloc").start_addr();
        cap::process_obj::create_process_object(
            init_proc_page,
            ttbr0.as_u64(),
            init_ht_page.as_u64(),
            false, // not immortal
            b"init\0\0\0\0\0\0\0\0\0\0\0\0",
        );
        cap::process_obj::process_inc_thread_count(init_proc_page);

        // Decrement kernel process thread count (this thread is leaving)
        {
            let current_tcb_paddr = sched::scheduler::current_tcb_paddr();
            let old_process = mm::addr::PhysAddr::new(
                mm::kernel_ptr::KernelRef::<sched::tcb::Tcb>::from_paddr(current_tcb_paddr)
                    .get().process_paddr
            );
            cap::process_obj::process_dec_thread_count(old_process);
        }

        // Re-point TCB to the init process
        let current_tcb_paddr = sched::scheduler::current_tcb_paddr();
        let mut current_tcb = mm::kernel_ptr::KernelMut::<sched::tcb::Tcb>::from_paddr(current_tcb_paddr);
        current_tcb.get_mut().process_paddr = init_proc_page.as_u64();

        // Flush I-cache (we copied code into pages)
        core::arch::asm!(
            "ic iallu",                           // Invalidate entire I-cache
            "dsb ish",
            "isb",
        );

        // Activate the scheduler. All kernel setup is complete. After
        // this, secondary timer ticks will begin scheduling. CPU 0 is
        // about to drop to EL0 — the GKL discipline takes over.
        sched::scheduler::start();
        kprintln!("Scheduler started.");

        // Scheduler/MMU integration check. Right before EL0 drop, all
        // threads are kernel threads (ttbr0=0). No TTBR0 writes should
        // have occurred. This is the last kernel-only observation point.
        let (ctx_switches, ttbr0_writes) = sched::scheduler::scheduler_stats();
        kprintln!("[SCHED-KERNEL-PHASE] {} context switches, TTBR0 writes: {}", ctx_switches, ttbr0_writes);

        kprintln!("Dropping to EL0...");
        arch::aarch64::mmu::drop_to_el0_with_ttbr0(
            ttbr0,
            elf_info.entry_point,
            user_stack_top,
            0, // user_arg: 0 for init process first thread
        );
    }
}

// ---------------------------------------------------------------------------
// IPC test threads (Phase 7)
// ---------------------------------------------------------------------------

/// Reply object used by the ipc_sender benchmark kernel thread. Allocated
/// and initialized in kmain before the scheduler starts. Stored as a raw
/// paddr so both threads can read it without needing a handle table.
static IPC_BENCH_REPLY_PADDR: BootOnce = BootOnce::new();

/// Client thread: calls the server with a request, gets a reply.
/// Uses ipc_call (send + block for reply in one operation).
/// Endpoint at handle 0, Reply at handle 1.
fn ipc_sender() -> ! {
    const BENCHMARK_ROUNDS: u64 = 500;
    let mut counter: u64 = 1;

    // Warm up
    for _ in 0..10 {
        cap::object_ops::call(0, 1, [counter, 0, 0, 0])
            .expect("lookup").expect("call");
        counter += 1;
    }

    // Benchmark using call/reply pattern
    let start_tick = arch::aarch64::timer::tick_count();
    for _ in 0..BENCHMARK_ROUNDS {
        let reply_msg = cap::object_ops::call(0, 1, [counter, 0, 0, 0])
            .expect("lookup").expect("call");
        // Print first few to verify the server doubled our value
        if counter <= 13 {
            kprintln!("[IPC] call({}) -> reply({})", counter, reply_msg[0]);
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
        cap::object_ops::call(0, 1, [counter, 0, 0, 0])
            .expect("lookup").expect("call");
        counter += 1;
    }
}

/// Server thread: receives a request, doubles the first value, replies.
fn ipc_receiver() -> ! {
    loop {
        let msg = cap::object_ops::receive(0)
            .expect("lookup").expect("receive");
        let reply = [msg[0] * 2, msg[1], msg[2], msg[3]];
        ipc::reply::ipc_reply(reply).expect("reply");
    }
}

fn idle_thread() -> ! {
    // Release GKL inherited from thread_entry. Idle thread touches no
    // shared state — just wfi. Timer ticks acquire GKL in the handler.
    sched::gkl::gkl_unlock();
    unsafe { core::arch::asm!("msr DAIFClr, #2"); } // Unmask IRQs
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

// ---------------------------------------------------------------------------
// Secondary CPU boot
// ---------------------------------------------------------------------------

/// Rust entry point for secondary CPUs, called from _secondary_start assembly.
/// Sets up MMU, per-CPU state, exception vectors, stack canary, GIC, and
/// timer. Then enters the idle loop with IRQs enabled — timer ticks will
/// call schedule() via the GKL.
#[no_mangle]
pub extern "C" fn secondary_main(cpu_id: u64) -> ! {
    // Enable MMU with the same page tables CPU 0 built
    unsafe { arch::aarch64::mmu::enable_mmu_secondary(); }

    // Initialize per-CPU data (TPIDR_EL1)
    percpu::init_percpu(cpu_id as u32);

    // Install exception vectors (per-CPU VBAR_EL1)
    unsafe { arch::aarch64::exceptions::init(); }

    // Initialize stack canary for this CPU
    unsafe { mm::stack::init_canary_for_cpu(cpu_id as u32); }

    // Initialize this CPU's GIC redistributor + CPU interface (silent —
    // no kprintln, UART not serialized during secondary bring-up).
    unsafe { arch::aarch64::gic::init_redistributor(cpu_id as u32); }

    // Arm this CPU's virtual timer (silent variant)
    unsafe { arch::aarch64::timer::init_secondary(); }

    // This CPU IS the idle thread. CPU 0 registered a TCB for us
    // with saved_sp=0 and stack_base pointing at our per-CPU boot
    // stack. When the scheduler context-switches away from us, it
    // saves our real SP into that TCB. When switched back, we
    // resume here in the wfi loop.
    //
    // No GKL to release (we never held it — booted fresh from PSCI).
    // Unmask IRQs so timer ticks can preempt us into the scheduler.
    unsafe { core::arch::asm!("msr DAIFClr, #2"); }
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let mut uart = Uart::new();

    let _ = writeln!(uart, "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
    let _ = writeln!(uart, "[PANIC:KERN]  KERNEL PANIC");
    mm::stack::check_canary_report("[PANIC:KERN]");
    crash::print_thread_context("[PANIC:KERN]");
    if let Some(location) = info.location() {
        let _ = writeln!(uart, "[PANIC:KERN]  {}:{}", location.file(), location.line());
    }
    let _ = writeln!(uart, "[PANIC:KERN]  {}", info.message());
    let _ = writeln!(uart, "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");

    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
