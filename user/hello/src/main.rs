#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use lockjaw_userlib::{puts, putc, sys_yield, sys_call_ret4, sys_alloc_pages, sys_create_reply};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from child process!\n");

    // Allocate our Reply object (one per client thread, reused per call).
    let reply = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => {
            puts("hello: create reply FAILED\n");
            loop { sys_yield(); }
        }
    };

    // Bootstrap: call init on handle 0 to receive our handles.
    // Init exports handles into our table and replies with indices.
    puts("hello: bootstrapping...\n");
    let msg = match sys_call_ret4(0, reply, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => {
            puts("hello: bootstrap FAILED\n");
            loop { sys_yield(); }
        }
    };
    puts("hello: got handle ");
    putc(b'0' + msg[0] as u8);
    putc(b'\n');

    puts("child: alive\n");
    loop {
        sys_yield();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
