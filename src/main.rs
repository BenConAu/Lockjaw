#![no_std]
#![no_main]

mod arch;
mod mm;
mod print;

use arch::aarch64::uart::Uart;

extern "C" {
    static __bss_start: u8;
    static __bss_end: u8;
    static __kernel_end: u8;
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
