#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_types::fdt::parse_fdt;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum DTB pages (must match kernel cap in main.rs).
const DTB_MAX_PAGES: usize = 16;


/// UART0 physical address — reserved for kernel debug output.
const KERNEL_UART0_PHYS: u64 = 0x0900_0000;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("devmgr: starting\n");

    // Allocate our Reply object for outbound sys_call (just the bootstrap
    // call to init; after that we only reply, we don't call).
    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("devmgr: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: call init on handle 0 to receive our server endpoint.
    puts("devmgr: bootstrapping...\n");
    let reply = match sys_call_ret4(0, reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("devmgr: bootstrap FAILED\n"); halt(); }
    };
    let server_ep = reply[0];
    puts("devmgr: bootstrapped, server_ep=");
    put_decimal(server_ep);
    putc(b'\n');

    // Step 1: Get the DTB PageSet from the kernel and map it.
    // sys_get_boot_info returns the PageSet ID for the DTB physical pages.
    // sys_map_pages without MAP_FLAG_DEVICE maps with normal memory attributes,
    // avoiding the MAIR aliasing problem with the kernel identity map.
    let dtb_ps = match sys_get_boot_info() {
        Ok(id) => id,
        Err(_) => { puts("devmgr: get_boot_info FAILED\n"); halt(); }
    };
    let dtb_va = VMEM.alloc(DTB_MAX_PAGES).expect("VA exhausted for DTB");
    if !sys_map_pages(dtb_ps, dtb_va, 0).is_ok() {
        puts("devmgr: DTB map FAILED\n");
        halt();
    }
    puts("devmgr: DTB mapped\n");

    // Step 2: Parse the DTB to discover devices.
    // Read only the 40-byte FDT header first to compute the actual content
    // size (off_dt_strings + size_dt_strings). The kernel mapped exactly
    // this many pages worth of content via dtb_content_size().
    let dtb_content_end = {
        let header = unsafe { core::slice::from_raw_parts(dtb_va as *const u8, 40) };
        match lockjaw_types::fdt::dtb_content_size(header) {
            Ok(size) => size,
            Err(_) => { puts("devmgr: DTB header invalid\n"); halt(); }
        }
    };
    let dtb_slice = unsafe {
        core::slice::from_raw_parts(dtb_va as *const u8, dtb_content_end)
    };
    let mut devices = match parse_fdt(dtb_slice) {
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
    // Reserve UART0 for the kernel (it uses 0x0900_0000 for debug output).
    let pl011_hash = PL011_HASH;
    for i in 0..devices.count {
        let dev = &devices.devices[i];
        if dev.compatible_hash == pl011_hash {
            puts("devmgr: PL011 at ");
            put_hex(dev.mmio_addr);
            puts(" intid=");
            put_decimal(dev.intid as u64);
            if dev.mmio_addr == KERNEL_UART0_PHYS {
                puts(" (kernel, reserved)");
                devices.devices[i].claimed = true;
            }
            putc(b'\n');
        }
    }

    // Step 3: IPC server loop — serve device claim requests.
    puts("devmgr: serving\n");
    loop {
        let msg = match sys_receive_ret4(server_ep) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let cmd = msg[0];

        if cmd == CMD_CLAIM_DEVICE {
            let requested_hash = msg[1];
            let mut found = false;
            for i in 0..devices.count {
                let dev = devices.devices[i];
                if dev.compatible_hash == requested_hash && !dev.claimed {
                    // Register the MMIO page as a tracked PageSet
                    let mmio_ps = match sys_register_device_page(dev.mmio_addr) {
                        Ok(id) => id,
                        Err(_) => {
                            puts("devmgr: register MMIO page FAILED\n");
                            sys_reply(0, 0, 0, 0);
                            found = true;
                            break;
                        }
                    };
                    // Export the MMIO PageSet handle into the claiming
                    // driver's handle table (the caller blocked on our
                    // endpoint). Reply with the exported index + INTID.
                    // Mark claimed AFTER export succeeds — if export fails,
                    // the device stays available for a future claim attempt.
                    let exported = match sys_export_handle(mmio_ps) {
                        Ok(idx) => idx,
                        Err(_) => {
                            // Reclaim the handle slot. Backing PageSet pages
                            // still leak (no refcount-aware free yet).
                            sys_close_handle(mmio_ps);
                            puts("devmgr: export MMIO handle FAILED\n");
                            sys_reply(0, 0, 0, 0);
                            found = true;
                            break;
                        }
                    };
                    devices.devices[i].claimed = true;
                    puts("devmgr: claimed device at ");
                    put_hex(dev.mmio_addr);
                    putc(b'\n');
                    sys_reply(exported, dev.intid as u64, 0, 0);
                    found = true;
                    break;
                }
            }
            if !found {
                puts("devmgr: no matching device\n");
                sys_reply(0, 0, 0, 0);
            }
        } else {
            sys_reply(0, 0, 0, 0);
        }
    }
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
