#![no_std]
#![no_main]

use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_types::fdt::parse_fdt;
use lockjaw_types::device::*;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// User VA where we map the DTB pages.
const DTB_VA: u64 = 0x0010_0000;

/// Number of DTB pages to map (actual content is ~8KB).
const DTB_PAGES: u64 = 2;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("devmgr: starting\n");

    // Step 1: Get the DTB PageSet from the kernel and map it.
    // sys_get_boot_info returns the PageSet ID for the DTB physical pages.
    // sys_map_pages without MAP_FLAG_DEVICE maps with normal memory attributes,
    // avoiding the MAIR aliasing problem with the kernel identity map.
    let dtb_ps = match sys_get_boot_info() {
        Ok(id) => id,
        Err(_) => { puts("devmgr: get_boot_info FAILED\n"); halt(); }
    };
    if !sys_map_pages(dtb_ps, DTB_VA, 0).is_ok() {
        puts("devmgr: DTB map FAILED\n");
        halt();
    }
    puts("devmgr: DTB mapped\n");

    // Step 2: Parse the DTB to discover devices.
    let dtb_slice = unsafe {
        core::slice::from_raw_parts(DTB_VA as *const u8, (DTB_PAGES * PAGE_SIZE) as usize)
    };
    let devices = match parse_fdt(dtb_slice) {
        Ok(d) => d,
        Err(_) => {
            puts("devmgr: DTB parse FAILED\n");
            halt();
        }
    };
    puts("devmgr: parsed DTB, ");
    put_decimal(devices.count as u64);
    puts(" devices\n");

    // Step 3: Print PL011 device addresses found in the DTB.
    let pl011_hash = PL011_HASH;
    for i in 0..devices.count {
        let dev = &devices.devices[i];
        if dev.compatible_hash == pl011_hash {
            puts("devmgr: PL011 at ");
            put_hex(dev.mmio_addr);
            puts(" intid=");
            put_decimal(dev.intid as u64);
            putc(b'\n');
        }
    }

    puts("devmgr: idle\n");
    loop { sys_yield(); }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

fn put_decimal(mut n: u64) {
    if n == 0 { putc(b'0'); return; }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 { i -= 1; putc(buf[i]); }
}

fn put_hex(mut n: u64) {
    puts("0x");
    if n == 0 { putc(b'0'); return; }
    let mut buf = [0u8; 16];
    let mut i = 0;
    while n > 0 {
        let d = (n & 0xF) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        n >>= 4;
        i += 1;
    }
    while i > 0 { i -= 1; putc(buf[i]); }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("devmgr: PANIC\n");
    halt();
}
