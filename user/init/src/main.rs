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

/// Syscall: map a PageSet into our address space.
fn sys_map_pages(pageset_id: u64, virt_addr: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "svc #0",
            in("x0") pageset_id,
            in("x1") virt_addr,
            in("x8") 7u64,
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
        putc(b'0' + pageset_id as u8);
        putc(b'\n');
    } else {
        puts("init: alloc_pages FAILED\n");
    }

    // Test sys_map_pages: map the allocated page at VA 0x0060_0000 (6MB)
    let map_va: u64 = 0x0060_0000;
    let map_result = sys_map_pages(pageset_id, map_va);
    if map_result == 0 {
        puts("init: map_pages OK\n");

        // Write a value to the mapped page and read it back
        unsafe {
            let ptr = map_va as *mut u64;
            core::ptr::write_volatile(ptr, 0xDEAD_CAFE);
            let readback = core::ptr::read_volatile(ptr);
            if readback == 0xDEAD_CAFE {
                puts("init: mapped memory read/write OK\n");
            } else {
                puts("init: mapped memory MISMATCH\n");
            }
        }
    } else {
        puts("init: map_pages FAILED\n");
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
