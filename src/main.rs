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
use print::{Hex, Addr, Hex32, HexByte};

extern "C" {
    static __kernel_start: u8;
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

/// Firmware DTB pointer saved by boot.rs assembly. Written after BSS
/// zeroing so the zero doesn't clobber it. Read by kmain to find the DTB.
#[no_mangle]
static mut BOOT_DTB_PADDR: u64 = 0;

/// DTB PageSet ID, set once at boot. Returned by sys_get_boot_info.
static DTB_PAGESET_ID: BootOnce = BootOnce::new();

/// In-page offset of the DTB header within the first physical page
/// of `DTB_PAGESET_ID`'s page set. Nonzero on platforms whose
/// firmware places the DTB at an unaligned address (notably Pi 4B,
/// which typically uses 0xe00). Userspace adds this offset to the
/// mapped VA before reading DTB bytes. See
/// `lockjaw_types::dtb_layout` for the requirement-to-implementation
/// mapping; the host tests there pin down the recovery flow without
/// requiring a real boot.
static DTB_IN_PAGE_OFFSET: BootOnce = BootOnce::new();

/// Get the DTB PageSet ID (called by sys_get_boot_info handler).
pub fn dtb_pageset_id() -> u64 {
    DTB_PAGESET_ID.get()
}

/// Get the DTB in-page offset (called by sys_get_boot_info handler).
pub fn dtb_in_page_offset() -> u64 {
    DTB_IN_PAGE_OFFSET.get()
}

#[no_mangle]
pub extern "C" fn kmain() -> ! {
    // Discover platform hardware from DTB BEFORE any prints.
    // On Pi 4B, the QEMU default UART address (0x09000000) is plain RAM —
    // putc would spin forever on a fake TXFF flag. We must find the
    // real UART address from the DTB first.
    //
    // This runs BEFORE enable_mmu(), so we read raw physical addresses
    // directly — no KERNEL_VA_OFFSET translation.
    let fw_dtb = unsafe { BOOT_DTB_PADDR };
    // QEMU `-kernel` bare-metal boot places the DTB at the start of RAM.
    // If firmware didn't pass a DTB pointer in x0, search there.
    let dtb_paddr = if fw_dtb != 0 { fw_dtb } else {
        arch::aarch64::platform::QEMU_DTB_SEARCH_ADDR
    };

    // discover() owns all DTB validation: magic check, size, parsing,
    // and required-field validation. On failure, halt — we have no
    // UART and cannot print diagnostics.
    if arch::aarch64::platform::discover(dtb_paddr).is_err() {
        loop { unsafe { core::arch::asm!("wfi"); } }
    }
    let plat = arch::aarch64::platform::info();

    // UART is now safe to use — set_base + init_baud, then first print.
    unsafe { Uart::set_base(plat.uart0_base); }
    unsafe { Uart::new().init_baud(); }

    // First print happens here — banner + platform info.
    kprintln!("=== Lockjaw Microkernel v", env!("CARGO_PKG_VERSION"), " ===");
    kprintln!("Target: AArch64 (ARMv8-A)");
    kprintln!("Platform: UART=", Hex(plat.uart0_base), " GICD=", Hex(plat.gicd_base),
        " GICv", if plat.gic_v2 { "2" } else { "3" }, " RAM=", Hex(plat.ram_base), "+", Hex(plat.ram_size));
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
        // SAFETY: linker symbol
        kprintln!("  Kernel load:  ", Hex32(&__kernel_start as *const u8 as u64));
        kprintln!("  BSS:          ", Hex32(bss_start as u64), " - ", Hex32(bss_end as u64), " (", bss_end - bss_start, " bytes)");
        kprintln!("  Kernel end:   ", Hex32(kernel_end as u64));
        kprintln!("  Stack:        ", Hex32(stack_bottom as u64), " - ", Hex32(stack_top as u64), " (", stack_top - stack_bottom, " bytes)");
    }

    kprintln!();
    kprintln!("Physical memory: ", Hex(mm::addr::ram_start().as_u64()), " - ", Hex(mm::addr::ram_end().as_u64()), " (", mm::addr::total_pages(), " pages)");

    // Initialize page allocator — reserve firmware + kernel + per-CPU stacks.
    // The 2 MB alignment of __per_cpu_stacks creates a gap between
    // __kernel_end and the stacks. We must free that gap explicitly so
    // those pages aren't silently wasted.
    unsafe {
        // SAFETY: linker symbol
        let kernel_start = mm::addr::PhysAddr::new(&__kernel_start as *const u8 as u64);
        // SAFETY: linker symbol
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
    kprintln!("Higher-half active — UART at ", Hex(plat.uart0_base + mm::addr::KERNEL_VA_OFFSET));

    // Bring up the kernel VA (KVM) allocator. Carves a 512 GiB
    // higher-half pool for kernel objects that need virtual
    // contiguity but not physical contiguity (initially: PageSet
    // headers — Phase 3 work). Must run after enable_higher_half
    // (TTBR1 must be installed) and after page_alloc::init_with_gap
    // (we need to allocate the KVM L1 page).
    kprintln!();
    kprintln!("Bringing up kernel VA allocator...");
    unsafe {
        mm::kvm::kvm_init();
        mm::kvm::boot_self_test();
    }

    // Verify DTB is readable at its higher-half VA (DTB discovered by platform::discover)
    unsafe {
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let dtb_va = (dtb_paddr + mm::addr::KERNEL_VA_OFFSET) as *const u8;
        let magic = u32::from_be_bytes([
            *dtb_va, *dtb_va.add(1), *dtb_va.add(2), *dtb_va.add(3),
        ]);
        kprintln!("DTB: ", Hex(dtb_paddr), ", magic=", Hex32(magic as u64), " (",
            if magic == 0xd00dfeed { "valid" } else { "INVALID" }, ")");
    }

    // Register DTB pages as a PageSet so userspace can map them
    // normally (avoids the MAIR_DEVICE aliasing problem — DTB is
    // normal RAM, not MMIO). Compute page count from the DTB
    // header's totalsize field rather than hardcoding — the DTB
    // size varies with `-smp` and `-device` flags on QEMU and
    // differs across boards.
    //
    // The firmware-supplied `dtb_paddr` is *not* guaranteed to be
    // page-aligned: Pi 4B's VC firmware typically uses 0xe00 in the
    // low 12 bits. `lockjaw_types::dtb_layout::compute_layout`
    // returns the page-aligned first page, the in-page offset, and
    // the (offset-aware) page count. The host tests in that module
    // exercise the recovery flow against the QEMU virt DTB blob at
    // multiple offsets — pin down the layout math without needing
    // a real Pi.
    {
        let dtb_content_size = unsafe {
            // SAFETY: kernel VA, DTB header validated above
            let h = (dtb_paddr + mm::addr::KERNEL_VA_OFFSET) as *const u8;
            let header = core::slice::from_raw_parts(h, 40);
            lockjaw_types::fdt::dtb_content_size(header)
                .unwrap_or_else(|_| panic!("DTB header invalid"))
        };
        let layout = lockjaw_types::dtb_layout::compute_layout(
            mm::addr::PhysAddr::new(dtb_paddr),
            dtb_content_size,
        );
        if layout.page_count > 16 {
            kprintln!("DTB content too large: ", layout.page_count, " pages");
            panic!("DTB content too large");
        }
        let mut dtb_pages = [layout.first_page; 16];
        for i in 0..layout.page_count {
            dtb_pages[i] = layout.first_page.add_pages(i);
        }
        let dtb_ps_id = cap::pageset_table::register_existing(
            layout.page_count, &dtb_pages[..layout.page_count])
            .unwrap_or_else(|| panic!("DTB PageSet registration failed"));
        DTB_PAGESET_ID.set(dtb_ps_id);
        DTB_IN_PAGE_OFFSET.set(layout.in_page_offset as u64);
        kprintln!(
            "DTB PageSet registered: id=", dtb_ps_id,
            ", ", layout.page_count, " pages (",
            dtb_content_size, " bytes content, in-page offset ",
            crate::print::Hex(layout.in_page_offset as u64), ")",
        );
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
        kprintln!("Setting up ", guard_pages.len(), " guard pages...");
        arch::aarch64::mmu::setup_guard_pages(&guard_pages);
        kprintln!("Guard pages active (unmapped).");

        // M6: exclude the DMA pool's 2 MiB L2 block from the kernel
        // TTBR1 direct map. Closes the speculative-cache-fill side
        // of the mixed-attribute alias bug for NormalNonCacheable
        // mappings (Tier 3 #13 / docs/m6-subcommit-2-plan.md).
        // No-op if pool init was skipped on tight-RAM platforms.
        arch::aarch64::mmu::exclude_dma_pool_from_direct_map(
            cap::dma_pool::base_phys(),
        );

        mm::stack::init_canary();
    }
    mm::stack::check_canary();
    kprintln!("Stack canary intact.");

    // CPU 0's per-CPU init runs TWICE: once pre-pivot and once
    // post-pivot. The PERCPU_DATA static is referenced via `&raw
    // const`, which is PC-relative — the value stored in TPIDR_EL1
    // is PA pre-pivot, L0[1] kernel-image VA post-pivot.
    //
    // Pre-pivot init: ensures TPIDR_EL1 is a valid PA so that any
    // panic / exception during the SMP-boot or pivot window can
    // dereference the per-CPU pointer through the BOOT_L0 identity
    // map and produce useful crash diagnostics.
    //
    // Post-pivot re-init: refreshes TPIDR_EL1 to the L0[1] VA so
    // that once the boot CPU's TTBR0 is later replaced with a user
    // page table (which has no kernel identity), the per-CPU
    // dereference still resolves through TTBR1.
    percpu::init_percpu(0);

    // Boot secondary CPUs using method detected from DTB
    {
        use lockjaw_types::fdt::SmpMethod;

        extern "C" { fn _secondary_start(); }
        // SAFETY: _secondary_start is the assembly entry point for secondaries.
        // It is a physical address (identity-mapped) that sets up the per-CPU
        // stack and calls secondary_main(cpu_id).
        // SAFETY: _secondary_start is the assembly entry point symbol
        let entry = _secondary_start as *const () as u64;

        // Read boot CPU's MPIDR to skip it in the loop.
        // Mask to Aff0 — sufficient for single-cluster linear topology.
        // Multi-cluster would need Aff1:Aff0, documented as a known limitation.
        let boot_mpidr: u64;
        unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) boot_mpidr) };
        let boot_mpidr = boot_mpidr & 0xFF;

        let plat = arch::aarch64::platform::info();
        match plat.smp_method {
            SmpMethod::Psci { hvc } => {
                for i in 0..plat.cpu_count as usize {
                    let cpu = &plat.cpus[i];
                    if cpu.mpidr == boot_mpidr { continue; }
                    let ret = unsafe {
                        arch::aarch64::psci::cpu_on(cpu.mpidr, entry, cpu.mpidr, hvc)
                    };
                    if ret == 0 {
                        kprintln!("[SMP] CPU ", cpu.mpidr, " started (PSCI)");
                    } else {
                        kprintln!("[SMP] CPU ", cpu.mpidr, " PSCI failed: ", ret);
                    }
                }
            }
            SmpMethod::SpinTable => {
                for i in 0..plat.cpu_count as usize {
                    let cpu = &plat.cpus[i];
                    if cpu.mpidr == boot_mpidr { continue; }
                    unsafe {
                        arch::aarch64::spin_table::write_release_addr(
                            cpu.release_addr, entry,
                        );
                    }
                    kprintln!("[SMP] CPU ", cpu.mpidr, " released (spin-table)");
                }
                // Single SEV after all writes to wake all secondaries at once
                unsafe { core::arch::asm!("sev"); }
            }
            SmpMethod::None => {
                if plat.cpu_count > 1 {
                    kprintln!("[SMP] DTB has ", plat.cpu_count,
                              " CPUs but no boot method — single-core only");
                }
            }
        }

        // Brief delay for secondaries to print their online messages
        // before boot continues. Not correctness-critical — just keeps
        // serial output readable.
        for _ in 0..100_000 { core::hint::spin_loop(); }
    }

    // Pivot PC, SP, and FP to higher-half (TTBR1) addresses.
    // After this call, all PC-relative references resolve to the L0[1]
    // kernel-image VA range — VBAR gets an L0[1] address, exception
    // handlers run via TTBR1's L0[1] mapping, and the kernel no longer
    // depends on TTBR0 identity. The shift is `LINKER_BASE - load_PA`
    // (computed by init_kernel_image_map at MMU setup time); pre-relink
    // this was the constant `KERNEL_VA_OFFSET` because linker_VA == PA +
    // KERNEL_VA_OFFSET held by accident. Must happen AFTER secondary CPU
    // boot (PSCI needs physical entry address) and BEFORE exceptions::init
    // (VBAR must be the post-pivot higher-half address).
    unsafe {
        extern "C" { fn _pivot_to_higher_half(offset: u64); }
        _pivot_to_higher_half(arch::aarch64::mmu::kernel_image_pivot_shift());
    }
    kprintln!("Pivoted to higher-half (TTBR1).");

    // Refresh CPU 0's TPIDR_EL1 to the L0[1] VA pointer. The pre-
    // pivot init (above the SMP-boot block) wrote the PA pointer
    // for crash robustness; now that we're post-pivot, re-init so
    // the value survives the eventual user-TTBR0 install (which
    // has no kernel identity, so PA dereferences would fail).
    percpu::init_percpu(0);
    kprintln!("CPU ", percpu::cpu_id(), " initialized (TPIDR_EL1)");

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
    kprintln!("  ", arch::aarch64::timer::tick_count(), " ticks received!");

    // Verification: alloc 10 pages, dealloc, realloc — should get same addresses
    kprintln!();
    kprintln!("Page allocator test:");
    let mut pages = [None; 10];
    for i in 0..10 {
        pages[i] = mm::page_alloc::alloc_page();
        kprintln!("  alloc  ", i, ": ", Hex(pages[i].unwrap().start_addr().as_u64()));
    }
    for i in 0..10 {
        mm::page_alloc::dealloc_page(pages[i].unwrap());
    }
    kprintln!("  (deallocated all 10)");
    for i in 0usize..10 {
        let f = mm::page_alloc::alloc_page().unwrap();
        kprintln!("  realloc ", i, ": ", Hex(f.start_addr().as_u64()));
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
    kprintln!("  raw:  ", Addr(entry.raw()));
    kprintln!("  valid=", entry.is_valid(), " table=", entry.is_table(), " block=", entry.is_block(), " attr=", entry.attr_index());

    let table_entry = PageTableEntry::new_table(mm::addr::PhysAddr::new(0x4009_0000));
    kprintln!("  table: ", Addr(table_entry.raw()), " valid=", table_entry.is_valid(), " is_table=", table_entry.is_table());

    let block_entry = PageTableEntry::new_block(
        mm::addr::PhysAddr::new(0x0000_0000),
        MAIR_DEVICE,
        AP_RW_EL1,
        SH_NON,
    );
    kprintln!("  block: ", Addr(block_entry.raw()), " is_block=", block_entry.is_block(), " attr=", block_entry.attr_index());

    // Object model: PageSet → donate → create handle table
    kprintln!();
    kprintln!("Object model test:");
    use cap::object::*;
    use cap::pageset;
    use cap::handle_table::*;
    use cap::rights::*;

    let ht_info = HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE };
    let ht_size = query_handle_table_size(&ht_info);
    kprintln!("  HandleTable(", ht_info.slot_count, " slots) needs ", ht_size.pages, " page(s)");

    // Allocate a pageset and donate it for the handle table
    let ps = pageset::alloc_pages(ht_size.pages).unwrap_or_else(|_| panic!("alloc_pages failed"));
    kprintln!("  PageSet allocated: ", ps.count, " page(s) at ", Hex(ps.pages[0].as_u64()));

    // Donate the PageSet's data page as the HandleTable backing,
    // then expose it via KVA so create_handle_table can write through
    // the new addressing regime. The PageSet itself is leaked on this
    // boot-test path (one-shot smoke test, no teardown).
    let ht_paddr = pageset::donate(&ps, ht_size.pages).unwrap_or_else(|_| panic!("donate failed"));
    let ht_kva = mm::kvm::map_existing(mm::addr::PhysPage::containing(ht_paddr))
        .unwrap_or_else(|_| panic!("bench ht kvm map")).kva;
    unsafe { create_handle_table(&ht_info, ht_kva).unwrap_or_else(|_| panic!("create failed")); }

    // Read back the header to verify
    // SAFETY: kernel object at known KVA (mapped by KVM allocator).
    let header = unsafe { &*(ht_kva.as_u64() as *const HandleTableHeader) };
    kprintln!("  Created: type=", header.header.obj_type.name(), ", pages=", header.header.page_count, ", slots=", header.slot_count);

    // Insert a handle pointing to the table itself (for testing)
    let h0 = unsafe {
        handle_insert(
            ht_kva,
            Rights::from_bits(RIGHT_READ | RIGHT_WRITE),
            HandleKind::HandleTable { kva: ht_kva },
        )
    }.unwrap_or_else(|_| panic!("insert failed"));
    kprintln!("  Inserted handle ", h0, " (RW)");

    // Look up with matching rights — should succeed
    let entry = unsafe { handle_lookup(ht_kva, h0, Rights::from_bits(RIGHT_READ)) }.unwrap_or_else(|_| panic!("lookup failed"));
    kprintln!("  Lookup h", h0, ": kind=", entry.kind.name(), ", rights=", HexByte(entry.rights.bits() as u64));

    // Look up with Grant right — should fail (we only gave RW)
    let bad = unsafe { handle_lookup(ht_kva, h0, Rights::from_bits(RIGHT_GRANT)) };
    kprintln!("  Lookup h", h0, " with Grant: ", bad.err().unwrap().name());

    // Remove the handle
    let removed = unsafe { handle_remove(ht_kva, h0) }.unwrap_or_else(|_| panic!("remove failed"));
    kprintln!("  Removed h", h0, ": kind=", removed.kind.name());

    // Verify slot is now empty
    let empty = unsafe { handle_lookup(ht_kva, h0, Rights::none()) };
    kprintln!("  Lookup h", h0, " after remove: ", empty.err().unwrap().name());

    // --- Process lifecycle test ---
    // Exercises the core new semantic: thread_count > 1, exit one
    // (process stays alive), exit the other (process freed).
    {
        use lockjaw_types::process::ProcessLifecycle;

        let test_ht_range = mm::kvm::alloc_kernel_pages(1).unwrap_or_else(|_| panic!("test ht kvm alloc"));
        let test_ht = test_ht_range.kva;
        unsafe {
            cap::object::create_handle_table(
                &cap::object::HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
                test_ht,
            ).unwrap_or_else(|_| panic!("test ht create"));
        }
        let test_proc_range = mm::kvm::alloc_kernel_pages(1)
            .unwrap_or_else(|_| panic!("test proc kvm alloc"));
        let test_proc = test_proc_range.kva;
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
            other => {
                kprintln!("expected ThreadsRemaining(1), got ", other.name());
                panic!("process lifecycle test failed");
            }
        }

        // Second thread exits — process should be freed
        let r2 = cap::process_obj::process_dec_thread_count(test_proc);
        match r2 {
            ProcessLifecycle::LastThread => {}
            other => {
                kprintln!("expected LastThread, got ", other.name());
                panic!("process lifecycle test failed");
            }
        }

        // Clean up test pages (process would normally be freed by finish_exit)
        // SAFETY: ranges came from kvm::alloc_kernel_pages above; no live refs.
        unsafe {
            mm::kvm::free_kernel_pages(test_ht_range);
            mm::kvm::free_kernel_pages(test_proc_range);
        }
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

        // Create endpoint object. Init through the linear map (paddr),
        // then expose at a KVA via kvm::map_existing so the bench
        // handle slot can carry `HandleKind::Endpoint { kva }`. The
        // bench Endpoint lives for the kernel's lifetime — no destroy
        // path needed.
        let ep_page = mm::page_alloc::alloc_page().unwrap_or_else(|| panic!("endpoint alloc"));
        let ep_paddr = ep_page.start_addr();
        ipc::endpoint::create_endpoint(mm::addr::ObjectInitPage::new(ep_paddr)).unwrap_or_else(|_| panic!("create endpoint"));
        let ep_kva = mm::kvm::map_existing(ep_page)
            .unwrap_or_else(|_| panic!("endpoint kvm map")).kva;
        kprintln!("  Endpoint created at phys ", Hex(ep_paddr.as_u64()), ", kva ", Hex(ep_kva.as_u64()));

        // Reply object for the ipc_sender benchmark thread. One page,
        // pre-allocated and exposed via KVA so the handle slot can carry
        // a `HandleKind::Reply { kva }`. Init runs through the linear
        // map (paddr); map_existing then makes the same frame reachable
        // through a KVM-pool VA. The bench Reply lives for the kernel's
        // lifetime — no destroy path needed.
        let bench_reply_page = mm::page_alloc::alloc_page().unwrap_or_else(|| panic!("bench reply alloc"));
        ipc::reply::create_reply(mm::addr::ObjectInitPage::new(bench_reply_page.start_addr())).unwrap_or_else(|_| panic!("create bench reply"));
        let bench_reply_kva = mm::kvm::map_existing(bench_reply_page)
            .unwrap_or_else(|_| panic!("bench reply kvm map")).kva;

        // Create kernel process — immortal, ttbr0=0, owns all kernel threads.
        let kernel_ht_kva = mm::kvm::alloc_kernel_pages(1)
            .unwrap_or_else(|_| panic!("kernel ht kvm alloc")).kva;
        create_handle_table(
            &HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
            kernel_ht_kva,
        ).unwrap_or_else(|_| panic!("kernel ht create"));

        let kernel_proc_kva = mm::kvm::alloc_kernel_pages(1)
            .unwrap_or_else(|_| panic!("kernel proc kvm alloc")).kva;
        cap::process_obj::create_process_object(
            kernel_proc_kva,
            0, // ttbr0 = 0 (kernel process)
            kernel_ht_kva.as_u64(),
            true, // immortal
            b"kernel\0\0\0\0\0\0\0\0\0\0",
        );

        // Insert endpoint + reply handles into kernel handle table.
        // Mint a sender token via the same path sys_export_handle uses
        // (the master/receive-only handle would have caller_token=None
        // and reject sends; the kernel sender thread needs a sender
        // handle just like any userspace client would).
        let ep_token = {
            let mut ep_km = mm::kernel_ptr::KernelMut::<ipc::endpoint::EndpointObject>::from_kva(ep_kva);
            ipc::endpoint::mint_caller_token(ep_km.get_mut())
        };
        handle_table::handle_insert(
            kernel_ht_kva,
            cap::rights::Rights::from_bits(cap::rights::RIGHT_READ | cap::rights::RIGHT_WRITE),
            lockjaw_types::object::HandleKind::Endpoint { kva: ep_kva, caller_token: Some(ep_token) },
        ).unwrap_or_else(|_| panic!("insert ep handle"));
        handle_table::handle_insert(
            kernel_ht_kva,
            cap::rights::Rights::from_bits(cap::rights::RIGHT_READ | cap::rights::RIGHT_WRITE),
            lockjaw_types::object::HandleKind::Reply { kva: bench_reply_kva },
        ).unwrap_or_else(|_| panic!("insert reply handle"));

        // Thread A (sender) — kernel thread in the kernel process.
        // TCB pages and per-thread kernel stacks both live in the KVM
        // pool. Bench threads are immortal; the KVA range and backing
        // frame leak for the kernel's lifetime (no destroy path).
        cap::process_obj::process_inc_thread_count(kernel_proc_kva);
        let stack_a_kva = mm::kvm::alloc_kernel_pages(1).unwrap_or_else(|_| panic!("stack a kvm alloc")).kva;
        let tcb_a_kva = mm::kvm::alloc_kernel_pages(1).unwrap_or_else(|_| panic!("tcb a kvm alloc")).kva;
        create_tcb(&TcbCreateInfo { entry: ipc_sender, stack: lockjaw_types::thread::KernelStackBase::Pool(stack_a_kva), process_kva: kernel_proc_kva, user_entry_point: 0, user_stack_top: 0, user_stack_base: 0, user_arg: 0, name: *b"ipc-sender\0\0\0\0\0\0" }, tcb_a_kva)
            .unwrap_or_else(|_| panic!("create tcb a"));

        // Thread B (receiver) — kernel thread in the kernel process
        cap::process_obj::process_inc_thread_count(kernel_proc_kva);
        let stack_b_kva = mm::kvm::alloc_kernel_pages(1).unwrap_or_else(|_| panic!("stack b kvm alloc")).kva;
        let tcb_b_kva = mm::kvm::alloc_kernel_pages(1).unwrap_or_else(|_| panic!("tcb b kvm alloc")).kva;
        create_tcb(&TcbCreateInfo { entry: ipc_receiver, stack: lockjaw_types::thread::KernelStackBase::Pool(stack_b_kva), process_kva: kernel_proc_kva, user_entry_point: 0, user_stack_top: 0, user_stack_base: 0, user_arg: 0, name: *b"ipc-receiver\0\0\0\0" }, tcb_b_kva)
            .unwrap_or_else(|_| panic!("create tcb b"));

        // Register idle/init thread (index 0 = this boot thread).
        // This thread drops to EL0 and becomes the init process, so it
        // gets its own user process (created later in the ELF loading path).
        // For now it belongs to the kernel process.
        // SAFETY: linker symbol — post-pivot, &__symbol gives higher-half VA directly
        let idle_stack_base = lockjaw_types::addr::KernelImageVa::new(
            &__stack_bottom as *const u8 as u64,
        );
        cap::process_obj::process_inc_thread_count(kernel_proc_kva);

        let idle_tcb_page = create_idle_tcb(
            idle_stack_base, kernel_proc_kva, *b"init\0\0\0\0\0\0\0\0\0\0\0\0",
        );

        sched::scheduler::add_thread(idle_tcb_page);  // index 0: idle/boot (CPU 0)
        sched::scheduler::add_thread(tcb_a_kva);      // index 1: thread A
        sched::scheduler::add_thread(tcb_b_kva);      // index 2: thread B

        // Secondary CPUs no longer hold idle TCBs — the scheduler
        // refactor (plan in /Users/Ben/.claude/plans/, see
        // `sched::scheduler::idle_wait` / `schedule_from_idle`)
        // replaced per-CPU idle threads with a kernel-owned wfi loop
        // on each secondary's boot stack. Secondaries enter
        // `secondary_main` which calls `idle_wait`; the first tick on
        // a secondary routes through `schedule_from_idle` to pick up
        // Ready work.
        //
        // Removing the idle TCBs eliminates the round-robin selection
        // bug they caused on CPU 0 (M6 emmc2 ADMA2 perf measurements
        // saw 5-30ms tick-boundary slack because synthetic idle Ready
        // entries kept stealing CPU from real busy-poll workloads).

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

    // The init ELF binary, built separately and embedded at compile time.
    // The actual bytes go in `.user_elf_blob` so the check-vtables tool
    // skips them — u64-aligned positions inside the binary may
    // coincidentally fall in the kernel's text range (init's own data
    // tables and sub-binaries it embeds) and would otherwise be
    // misreported as kernel code pointers.
    //
    // `link_section` on a `&[u8]` would only relocate the slice
    // descriptor; the bytes need to live in a named array so the
    // attribute applies to them.
    const INIT_ELF_SIZE: usize =
        include_bytes!("../user/init/target/aarch64-unknown-none/release/lockjaw-init").len();
    #[link_section = ".user_elf_blob"]
    static INIT_ELF_BYTES: [u8; INIT_ELF_SIZE] =
        *include_bytes!("../user/init/target/aarch64-unknown-none/release/lockjaw-init");
    static INIT_ELF: &[u8] = &INIT_ELF_BYTES;

    // Verify the init binary was built from the same source as the kernel
    kprintln!("Build hash: ", Addr(LOCKJAW_SOURCE_HASH));
    match lockjaw_types::elf::find_section_u64(INIT_ELF, ".lockjaw_hash") {
        Some(init_hash) if init_hash == LOCKJAW_SOURCE_HASH => {
            kprintln!("Init hash:  ", Addr(init_hash), " (match)");
        }
        Some(init_hash) => {
            kprintln!("FATAL: init binary build hash mismatch!");
            kprintln!("  kernel: ", Addr(LOCKJAW_SOURCE_HASH));
            kprintln!("  init:   ", Addr(init_hash));
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
        let elf_info = elf::parse_elf(INIT_ELF).unwrap_or_else(|_| panic!("failed to parse init ELF"));
        kprintln!("  Entry point: ", Hex(elf_info.entry_point));
        kprintln!("  ", elf_info.segment_count, " loadable segment(s)");

        // Allocate 16 contiguous pages for the mapping buffer — enough for
        // ~2720 mappings (~10.6 MB of init binaries via include_bytes!).
        const MAP_BUF_PAGES: usize = 16;
        let map_buf_capacity = MAP_BUF_PAGES * MAPPINGS_PER_PAGE;
        let map_buf = mm::page_alloc::alloc_pages_contiguous(MAP_BUF_PAGES)
            .unwrap_or_else(|| panic!("mapping buffer pages"));
        // SAFETY: contiguous pages → contiguous kernel VA; zero all of them.
        for i in 0..MAP_BUF_PAGES {
            let page_addr = mm::addr::PhysAddr::new(map_buf.start_addr().as_u64() + (i as u64) * mm::addr::PAGE_SIZE);
            mm::page_alloc::zero_page(page_addr);
        }
        let mut map_buf_km = mm::kernel_ptr::KernelMut::<Mapping>::from_paddr(map_buf.start_addr());
        let mappings = core::slice::from_raw_parts_mut(map_buf_km.as_mut_ptr(), map_buf_capacity);
        let mut mapping_count = 0;

        for i in 0..elf_info.segment_count {
            let seg = &elf_info.segments[i];
            let num_pages = ((seg.mem_size + mm::addr::PAGE_SIZE - 1) / mm::addr::PAGE_SIZE) as usize;
            kprintln!("  Segment ", i, ": VA ", Hex(seg.vaddr), ", ", num_pages, " page(s), ",
                if seg.executable { "X" } else { "-" },
                if seg.writable { "W" } else { "R" });

            for p in 0..num_pages {
                assert!(mapping_count < map_buf_capacity, "init ELF has too many pages for mapping buffer");
                let page = mm::page_alloc::alloc_page().unwrap_or_else(|| panic!("segment page"));

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
            let stack_page = mm::page_alloc::alloc_page().unwrap_or_else(|| panic!("user stack page"));
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
            .unwrap_or_else(|_| panic!("failed to create address space"));
        kprintln!("  Address space created: TTBR0 = ", Hex(ttbr0.as_u64()));

        // Create init user process with its own handle table and address
        // space. Init's handle table starts empty — init creates its own
        // handles via syscalls from userspace (sys_create_endpoint, etc.).
        let init_ht_kva = mm::kvm::alloc_kernel_pages(1)
            .unwrap_or_else(|_| panic!("init ht kvm alloc")).kva;
        cap::object::create_handle_table(
            &cap::object::HandleTableCreateInfo { slot_count: lockjaw_types::object::HANDLE_SLOTS_PER_PAGE },
            init_ht_kva,
        ).unwrap_or_else(|_| panic!("init ht create"));

        let init_proc_kva = mm::kvm::alloc_kernel_pages(1)
            .unwrap_or_else(|_| panic!("init proc kvm alloc")).kva;
        cap::process_obj::create_process_object(
            init_proc_kva,
            ttbr0.as_u64(),
            init_ht_kva.as_u64(),
            false, // not immortal
            b"init\0\0\0\0\0\0\0\0\0\0\0\0",
        );
        cap::process_obj::process_inc_thread_count(init_proc_kva);

        // Decrement kernel process thread count (this thread is leaving)
        {
            let current_tcb_kva = sched::scheduler::current_tcb_kva();
            let old_process = lockjaw_types::addr::KernelVa::new(
                mm::kernel_ptr::KernelRef::<sched::tcb::Tcb>::from_kva(current_tcb_kva)
                    .get().process_kva
            );
            cap::process_obj::process_dec_thread_count(old_process);
        }

        // Re-point TCB to the init process
        let current_tcb_kva = sched::scheduler::current_tcb_kva();
        let mut current_tcb = mm::kernel_ptr::KernelMut::<sched::tcb::Tcb>::from_kva(current_tcb_kva);
        current_tcb.get_mut().process_kva = init_proc_kva.as_u64();

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
        kprintln!("[SCHED-KERNEL-PHASE] ", ctx_switches, " context switches, TTBR0 writes: ", ttbr0_writes);

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

/// Client thread: calls the server with a request, gets a reply.
/// Uses ipc_call (send + block for reply in one operation).
/// Endpoint at handle 0, Reply at handle 1.
fn ipc_sender() -> ! {
    const BENCHMARK_ROUNDS: u64 = 500;
    let mut counter: u64 = 1;

    // Warm up
    for _ in 0..10 {
        cap::object_ops::call(0, 1, [counter, 0, 0, 0])
            .unwrap_or_else(|_| panic!("lookup")).unwrap_or_else(|_| panic!("call"));
        counter += 1;
    }

    // Benchmark using call/reply pattern
    let start_tick = arch::aarch64::timer::tick_count();
    for _ in 0..BENCHMARK_ROUNDS {
        let reply_msg = cap::object_ops::call(0, 1, [counter, 0, 0, 0])
            .unwrap_or_else(|_| panic!("lookup")).unwrap_or_else(|_| panic!("call"));
        // Print first few to verify the server doubled our value
        if counter <= 13 {
            kprintln!("[IPC] call(", counter, ") -> reply(", reply_msg[0], ")");
        }
        counter += 1;
    }
    let end_tick = arch::aarch64::timer::tick_count();
    let elapsed_ticks = end_tick - start_tick;

    kprintln!();
    kprintln!("[IPC BENCHMARK] ", BENCHMARK_ROUNDS, " call/reply round-trips in ", elapsed_ticks, " ticks");
    if elapsed_ticks > 0 {
        kprintln!("[IPC BENCHMARK] ", BENCHMARK_ROUNDS / elapsed_ticks, " round-trips per tick");
    }

    loop {
        cap::object_ops::call(0, 1, [counter, 0, 0, 0])
            .unwrap_or_else(|_| panic!("lookup")).unwrap_or_else(|_| panic!("call"));
        counter += 1;
    }
}

/// Server thread: receives a request, doubles the first value, replies.
fn ipc_receiver() -> ! {
    loop {
        let msg = cap::object_ops::receive(0)
            .unwrap_or_else(|_| panic!("lookup")).unwrap_or_else(|_| panic!("receive"));
        let reply = [msg[0] * 2, msg[1], msg[2], msg[3]];
        ipc::reply::ipc_reply(reply).unwrap_or_else(|_| panic!("reply"));
    }
}

/// Create a TCB for an idle/boot thread using a linker-provided boot stack.
/// Unlike create_tcb(), this does NOT set up a SavedContext or canary —
/// idle threads are the initial thread on each CPU and start running
/// directly, not via context_switch.
///
/// Takes the stack base as `KernelImageVa` because boot stacks are
/// reserved in the kernel image (linker symbols `__stack_bottom_*`),
/// not allocated from the KVM pool. The regime distinction is
/// preserved through Tcb.stack_base so finish_exit refuses to free
/// an idle thread's stack.
unsafe fn create_idle_tcb(
    stack_image_va: lockjaw_types::addr::KernelImageVa,
    process_kva: lockjaw_types::addr::KernelVa,
    name: [u8; 16],
) -> lockjaw_types::addr::KernelVa {
    let tcb_kva = mm::kvm::alloc_kernel_pages(1).unwrap_or_else(|_| panic!("idle tcb kvm alloc")).kva;
    // Zero the TCB page (KVM allocator hands back uninitialized backing).
    {
        let mut p = mm::kernel_ptr::KernelMut::<u8>::from_kva(tcb_kva);
        core::ptr::write_bytes(p.as_mut_ptr(), 0, mm::addr::PAGE_SIZE as usize);
    }
    let mut tcb_km = mm::kernel_ptr::KernelMut::<sched::tcb::Tcb>::from_kva(tcb_kva);
    // Idle TCBs don't use create_tcb() because they start directly
    // (no synthetic SavedContext on a separate stack page). Initialize
    // in place — no by-value Tcb on the kernel stack.
    let p = tcb_km.as_mut_ptr();
    sched::tcb::Tcb::init_in_place(p, idle_thread);
    (*p).stack_base = lockjaw_types::thread::KernelStackBase::Image(stack_image_va);
    (*p).process_kva = process_kva.as_u64();
    (*p).name = name;
    tcb_kva
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

    // Pre-pivot per-CPU init: writes a PA pointer to TPIDR_EL1 so
    // any panic in the pre-pivot window dereferences through
    // BOOT_L0's identity. Same pattern as CPU 0 (see kmain).
    percpu::init_percpu(cpu_id as u32);

    // Pivot to higher-half — same as CPU 0's pivot in kmain. The shift
    // is the boot-discovered LINKER_BASE - load_PA stored in
    // KERNEL_PHYS_OFFSET; CPU 0 set it before secondaries booted.
    unsafe {
        extern "C" { fn _pivot_to_higher_half(offset: u64); }
        _pivot_to_higher_half(arch::aarch64::mmu::kernel_image_pivot_shift());
    }

    // Post-pivot re-init: refresh TPIDR_EL1 to the L0[1] VA so it
    // survives this CPU's eventual user-TTBR0 install.
    percpu::init_percpu(cpu_id as u32);

    // Install exception vectors (per-CPU VBAR_EL1)
    unsafe { arch::aarch64::exceptions::init(); }

    // Initialize stack canary for this CPU
    unsafe { mm::stack::init_canary_for_cpu(cpu_id as u32); }

    // Initialize this CPU's GIC redistributor + CPU interface (silent —
    // no kprintln, UART not serialized during secondary bring-up).
    unsafe { arch::aarch64::gic::init_cpu(cpu_id as u32); }

    // Arm this CPU's virtual timer (silent variant)
    unsafe { arch::aarch64::timer::init_secondary(); }

    // This CPU has no thread of its own. It parks in the kernel-
    // owned idle_wait until a timer tick selects work via
    // schedule_from_idle (which transitions current_per_cpu[cpu]
    // from None -> Some(picked_idx) and context-switches in). When
    // that thread later blocks or exits, the path returns the CPU
    // to idle_wait via the normal block/exit machinery — though see
    // the plan's Known scope limit: block_current and ExitAndHalt
    // still WFI in thread context, so this CPU only re-enters
    // idle_wait between full thread lifecycles, not on every block.
    //
    // No GKL to release (we never held it — booted fresh from PSCI).
    // idle_wait unmasks IRQs internally.
    sched::scheduler::idle_wait(cpu_id as usize)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let uart = Uart::new();

    uart.puts("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n");
    uart.puts("[PANIC:KERN]  KERNEL PANIC\n");
    mm::stack::check_canary_report("[PANIC:KERN]");
    crash::print_thread_context("[PANIC:KERN]");
    if let Some(location) = info.location() {
        uart.puts("[PANIC:KERN]  ");
        uart.puts(location.file());
        uart.puts(":");
        print::KPrint::kprint(&location.line());
        uart.puts("\n");
    }
    uart.puts("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\n");

    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
