#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use lockjaw_userlib::{puts, sys_debug_puts, sys_exit, sys_call_ret4, sys_alloc_pages, sys_map_pages, sys_create_reply, sys_create_thread, sys_yield, MapMemoryAttribute, VMEM, bootstrap_endpoint};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("Hello from child process!\n");

    // Allocate our Reply object (one per client thread, reused per call).
    let reply = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => {
            puts("hello: create reply FAILED\n");
            sys_exit();
        }
    };

    // Bootstrap: call init on handle 0 to receive our handles.
    // Init exports handles into our table and replies with indices.
    puts("hello: bootstrapping...\n");
    let msg = match sys_call_ret4(bootstrap_endpoint(), reply, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => {
            puts("hello: bootstrap FAILED\n");
            sys_exit();
        }
    };
    puts("hello: got handle ");
    sys_debug_puts(&[b'0' + msg[0] as u8, b'\n']);

    puts("child: alive\n");

    // --- Thread creation smoke test ---
    // Allocate a shared page for the child thread to write a marker.
    let shared_va = VMEM.alloc(1).expect("VA exhausted for shared page");
    let shared_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("[THREAD-TEST] alloc FAILED\n"); sys_exit(); }
    };
    if !sys_map_pages(shared_ps, shared_va, MapMemoryAttribute::Normal).is_ok() {
        puts("[THREAD-TEST] map FAILED\n");
        sys_exit();
    }
    // Zero the marker
    unsafe { core::ptr::write_volatile(shared_va as *mut u64, 0); }

    // Allocate a stack page for the child thread
    let thread_stack_va = VMEM.alloc(1).expect("VA exhausted for thread stack");
    let stack_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("[THREAD-TEST] stack alloc FAILED\n"); sys_exit(); }
    };
    if !sys_map_pages(stack_ps, thread_stack_va, MapMemoryAttribute::Normal).is_ok() {
        puts("[THREAD-TEST] stack map FAILED\n");
        sys_exit();
    }

    // Spawn child thread: entry = thread_test_entry, stack top = VA + 4K,
    // arg = shared_va (where to write the marker)
    let stack_top = thread_stack_va + 4096;
    if sys_create_thread(
        thread_test_entry as *const () as u64, stack_top, thread_stack_va, shared_va,
    ).is_err() {
        puts("[THREAD-TEST] create FAILED\n");
        sys_exit();
    }

    // Wait for child to write the marker (bounded spin)
    let mut found = false;
    for _ in 0..1000 {
        let val = unsafe { core::ptr::read_volatile(shared_va as *const u64) };
        if val == 0xDEAD_CAFE_1234_5678 {
            found = true;
            break;
        }
        sys_yield(); // give child thread a chance to run
    }

    if found {
        puts("[THREAD-TEST] child wrote marker\n");
    } else {
        puts("[THREAD-TEST] FAILED\n");
    }

    sys_exit();
}

/// Entry point for the child thread in the thread smoke test.
/// Writes a marker to the shared page (address passed in x0) and exits.
extern "C" fn thread_test_entry(shared_va: u64) -> ! {
    unsafe { core::ptr::write_volatile(shared_va as *mut u64, 0xDEAD_CAFE_1234_5678); }
    sys_exit();
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
