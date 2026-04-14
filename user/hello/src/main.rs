#![no_std]
#![no_main]

use core::arch::asm;
use lockjaw_userlib::{puts, sys_yield};

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
