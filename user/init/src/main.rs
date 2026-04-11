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

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from userspace init!\n");

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
