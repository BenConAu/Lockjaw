#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use core::ptr;
use lockjaw_userlib::*;
use lockjaw_types::fdt::parse_fdt;
use lockjaw_types::device::{CMD_PROBE_DEVICE, CMD_CLAIM_BY_ADDR, CLAIM_OK, CLAIM_ERR};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum DTB pages (must match kernel cap in main.rs).
const DTB_MAX_PAGES: usize = 16;



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
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("devmgr: bootstrap FAILED\n"); halt(); }
    };
    let server_ep = EndpointHandle(reply[0]);
    puts("devmgr: bootstrapped, server_ep=");
    put_decimal(reply[0]);
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
    // Reserve the first PL011 for the kernel — scan_platform() takes the
    // first one it finds, so the device manager must match that policy.
    let pl011_hash = PL011_HASH;
    let mut first_pl011 = true;
    for i in 0..devices.count {
        let dev = &devices.devices[i];
        if dev.has_compat(pl011_hash) {
            puts("devmgr: PL011 at ");
            put_hex(dev.mmio_addr);
            puts(" intid=");
            put_decimal(dev.intid as u64);
            if first_pl011 {
                puts(" (kernel, reserved)");
                devices.devices[i].claimed = true;
                first_pl011 = false;
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
                if dev.has_compat(requested_hash) && !dev.claimed {
                    // Register the MMIO page as a tracked PageSet
                    let mmio_ps = match sys_register_device_page(dev.mmio_addr) {
                        Ok(id) => id,
                        Err(_) => {
                            puts("devmgr: register MMIO page FAILED\n");
                            sys_reply(CLAIM_ERR, 0, 0, 0);
                            found = true;
                            break;
                        }
                    };
                    // Export the MMIO PageSet handle into the claiming
                    // driver's handle table (the caller blocked on our
                    // endpoint). Reply with [status, handle, intid, 0].
                    // Mark claimed AFTER export succeeds — if export fails,
                    // the device stays available for a future claim attempt.
                    let exported = match sys_export_handle(mmio_ps) {
                        Ok(idx) => idx,
                        Err(_) => {
                            sys_close_handle(mmio_ps);
                            puts("devmgr: export MMIO handle FAILED\n");
                            sys_reply(CLAIM_ERR, 0, 0, 0);
                            found = true;
                            break;
                        }
                    };
                    devices.devices[i].claimed = true;
                    puts("devmgr: claimed device at ");
                    put_hex(dev.mmio_addr);
                    putc(b'\n');
                    sys_reply(CLAIM_OK, exported, dev.intid as u64, 0);
                    found = true;
                    break;
                }
            }
            if !found {
                puts("devmgr: no matching device\n");
                sys_reply(CLAIM_ERR, 0, 0, 0);
            }
        } else if cmd == CMD_PROBE_DEVICE {
            handle_probe_device(&mut devices, &msg);
        } else if cmd == CMD_CLAIM_BY_ADDR {
            handle_claim_by_addr(&mut devices, msg[1]);
        } else {
            sys_reply(0, 0, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// CMD_PROBE_DEVICE handler
// ---------------------------------------------------------------------------

/// Probe the Nth device (absolute index) matching a compatible hash.
///
/// Index is over ALL matching devices in the DTB-derived list,
/// including claimed ones. This makes enumeration stable regardless
/// of concurrent claims by other drivers.
///
/// Response: [status, mmio_addr, intid, device_id].
/// Magic validation is done internally; bad magic → PROBE_ERR.
fn handle_probe_device(devices: &mut lockjaw_types::fdt::FdtDevices, msg: &[u64; 4]) {
    use lockjaw_types::device::*;

    let requested_hash = msg[1];
    let index = msg[2] as usize;

    // Find the Nth device matching the hash (regardless of claimed).
    let mut matched = 0;
    let mut target_idx = None;
    for i in 0..devices.count {
        if devices.devices[i].has_compat(requested_hash) {
            if matched == index {
                target_idx = Some(i);
                break;
            }
            matched += 1;
        }
    }

    let idx = match target_idx {
        Some(i) => i,
        None => {
            sys_reply(PROBE_END, 0, 0, 0);
            return;
        }
    };

    let dev = devices.devices[idx];

    if dev.claimed {
        sys_reply(PROBE_CLAIMED, dev.mmio_addr, dev.intid as u64, 0);
        return;
    }

    // Unclaimed: register + map MMIO page temporarily to read device_id.
    let mmio_ps = match sys_register_device_page(dev.mmio_addr) {
        Ok(id) => id,
        Err(_) => {
            puts("devmgr: probe register FAILED\n");
            sys_reply(PROBE_ERR, dev.mmio_addr, dev.intid as u64, 0);
            return;
        }
    };

    let probe_va = VMEM.alloc(1).expect("VA exhausted for probe");
    if !sys_map_pages(mmio_ps, probe_va, MAP_FLAG_DEVICE).is_ok() {
        puts("devmgr: probe map FAILED\n");
        VMEM.free(probe_va, 1);
        sys_close_handle(mmio_ps);
        sys_reply(PROBE_ERR, dev.mmio_addr, dev.intid as u64, 0);
        return;
    }

    // Read magic (offset 0) and device_id (offset 8) within the device's
    // sub-page region. Multiple virtio-mmio devices share a single 4K page
    // (each device is 512 bytes), so we must add the intra-page offset.
    let intra_page = dev.mmio_addr & 0xFFF;
    let dev_base = probe_va + intra_page;
    let magic = unsafe { ptr::read_volatile(dev_base as *const u32) };
    let device_id = unsafe { ptr::read_volatile((dev_base + 8) as *const u32) };

    // Teardown: unmap first, then close handle, then free VA.
    // Ordering matters: VA must not be freed while still mapped.
    let unmap_ok = sys_unmap_pages(mmio_ps, probe_va).is_ok();
    sys_close_handle(mmio_ps);
    if unmap_ok {
        VMEM.free(probe_va, 1);
    }

    // Validate magic internally — bad magic means the device is not
    // a valid virtio-mmio transport.
    if magic != 0x74726976 {
        sys_reply(PROBE_ERR, dev.mmio_addr, dev.intid as u64, 0);
        return;
    }

    sys_reply(PROBE_OK, dev.mmio_addr, dev.intid as u64, device_id as u64);
}

// ---------------------------------------------------------------------------
// CMD_CLAIM_BY_ADDR handler
// ---------------------------------------------------------------------------

/// Claim a device by its exact MMIO physical address (TOCTOU-safe).
/// The driver discovers mmio_addr via CMD_PROBE_DEVICE, then claims
/// by stable identity — no skip_count, no race.
fn handle_claim_by_addr(devices: &mut lockjaw_types::fdt::FdtDevices, mmio_addr: u64) {
    // Find the device by exact MMIO address.
    let mut target_idx = None;
    for i in 0..devices.count {
        if devices.devices[i].mmio_addr == mmio_addr && !devices.devices[i].claimed {
            target_idx = Some(i);
            break;
        }
    }

    let idx = match target_idx {
        Some(i) => i,
        None => {
            sys_reply(CLAIM_ERR, 0, 0, 0);
            return;
        }
    };

    let dev = devices.devices[idx];

    // Register the MMIO page as a tracked PageSet.
    let mmio_ps = match sys_register_device_page(dev.mmio_addr) {
        Ok(id) => id,
        Err(_) => {
            puts("devmgr: claim-by-addr register FAILED\n");
            sys_reply(CLAIM_ERR, 0, 0, 0);
            return;
        }
    };

    // Export the handle into the caller's handle table.
    let exported = match sys_export_handle(mmio_ps) {
        Ok(idx) => idx,
        Err(_) => {
            sys_close_handle(mmio_ps);
            puts("devmgr: claim-by-addr export FAILED\n");
            sys_reply(CLAIM_ERR, 0, 0, 0);
            return;
        }
    };

    devices.devices[idx].claimed = true;
    puts("devmgr: claimed device at ");
    put_hex(dev.mmio_addr);
    putc(b'\n');
    sys_reply(CLAIM_OK, exported, dev.intid as u64, 0);
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
