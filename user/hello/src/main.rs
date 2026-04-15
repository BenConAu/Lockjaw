#![no_std]
#![no_main]

use core::arch::asm;
use lockjaw_userlib::{puts, putc, sys_yield, sys_call_ret4};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from child process!\n");

    // Bootstrap: call init on handle 0 to receive our handles.
    // Init exports handles into our table and replies with indices.
    puts("hello: bootstrapping...\n");
    let reply = sys_call_ret4(0, 0, 0, 0, 0);
    puts("hello: got handle ");
    putc(b'0' + reply[0] as u8);
    putc(b'\n');

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
