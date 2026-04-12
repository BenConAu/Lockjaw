#![no_std]
#![no_main]

use core::arch::asm;

/// Syscall: print a single character.
fn putc(c: u8) {
    unsafe {
        asm!(
            "svc #0",                        // Trap to kernel
            in("x0") c as u64,              // Character to print
            in("x8") 0u64,                  // Syscall number: debug_putc
        );
    }
}

/// Syscall: yield to scheduler.
fn sys_yield() {
    unsafe {
        asm!(
            "svc #0",                        // Trap to kernel
            in("x8") 1u64,                  // Syscall number: yield
        );
    }
}

/// Print a string by calling putc for each byte.
fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

/// Syscall: allocate physical pages.
fn sys_alloc_pages(count: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") count,
            in("x8") 6u64,
            lateout("x0") result,
        );
    }
    result
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from userspace init!\n");

    // Test sys_alloc_pages
    let pageset_id = sys_alloc_pages(1);
    if pageset_id != u64::MAX {
        puts("init: alloc_pages(1) OK, id=");
        // Print the ID as a digit (it'll be 0 or a small number)
        putc(b'0' + pageset_id as u8);
        putc(b'\n');
    } else {
        puts("init: alloc_pages FAILED\n");
    }

    loop {
        puts("init: alive\n");
        sys_yield();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
