#![no_std]
#![no_main]

use core::arch::asm;

fn putc(c: u8) {
    unsafe {
        asm!(
            "svc #0",
            in("x0") c as u64,
            in("x8") 0u64,
        );
    }
}

fn sys_yield() {
    unsafe {
        asm!(
            "svc #0",
            in("x8") 1u64,
        );
    }
}

fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from child process!\n");

    loop {
        puts("child: alive\n");
        sys_yield();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
