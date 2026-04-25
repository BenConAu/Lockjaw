#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use lockjaw_userlib::*;
use lockjaw_userlib::display::{DisplayClient, PIXEL_FORMAT_XRGB8888};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("[DISPLAY-TEST] starting\n");

    // Allocate Reply object for IPC calls.
    let reply = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("[DISPLAY-TEST] create reply FAILED\n"); sys_exit(); }
    };

    // Bootstrap: get display endpoint from init.
    let bootstrap = match sys_call_ret4(bootstrap_endpoint(), reply, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("[DISPLAY-TEST] bootstrap FAILED\n"); sys_exit(); }
    };
    let display_ep = EndpointHandle(bootstrap[0]);
    puts("[DISPLAY-TEST] bootstrapped\n");

    // Create a typed display client.
    let display = DisplayClient::new(display_ep, reply);

    // Query available modes (first = preferred).
    let mode_count = match display.list_modes() {
        Ok(c) => c,
        Err(_) => { puts("[DISPLAY-TEST] list_modes FAILED\n"); sys_exit(); }
    };
    puts("[DISPLAY-TEST] modes: ");
    putc(b'0' + mode_count as u8);
    putc(b'\n');

    let mode = match display.get_mode(0) {
        Ok(m) => m,
        Err(_) => { puts("[DISPLAY-TEST] get_mode FAILED\n"); sys_exit(); }
    };
    puts("[DISPLAY-TEST] preferred: ");
    print_u32(mode.width);
    putc(b'x');
    print_u32(mode.height);
    putc(b'\n');

    // Create a display session (prevents races with other clients).
    let session = match display.create_session() {
        Ok(s) => s,
        Err(_) => { puts("[DISPLAY-TEST] create_session FAILED\n"); sys_exit(); }
    };
    puts("[DISPLAY-TEST] session created\n");

    // Allocate a scanout-compatible buffer from the driver.
    let buf = match display.alloc_buffer(session, mode.width, mode.height, mode.format) {
        Ok(b) => b,
        Err(_) => { puts("[DISPLAY-TEST] alloc_buffer FAILED\n"); sys_exit(); }
    };
    puts("[DISPLAY-TEST] buffer allocated\n");

    // Map the buffer into our address space.
    let pages = ((buf.size as usize) + 4095) / 4096;
    let fb_va = VMEM.alloc(pages).expect("VA exhausted for framebuffer");
    if !sys_map_pages(buf.handle, fb_va, 0).is_ok() {
        puts("[DISPLAY-TEST] map FAILED\n");
        sys_exit();
    }

    // Draw a vertical color gradient (same pattern as the self-test,
    // proving the DDI pipeline delivers the same result).
    let stride = buf.stride;
    let bpp = 4u32; // XRGB8888
    unsafe {
        let fb = fb_va as *mut u8;
        for y in 0..mode.height {
            for x in 0..mode.width {
                let offset = (y * stride + x * bpp) as usize;
                let r = (x * 255 / mode.width) as u8;
                let g = (y * 255 / mode.height) as u8;
                let b = 128u8;
                // XRGB8888: byte order is B, G, R, X (little-endian pixel)
                *fb.add(offset) = b;
                *fb.add(offset + 1) = g;
                *fb.add(offset + 2) = r;
                *fb.add(offset + 3) = 0xFF;
            }
        }
    }

    // Set mode and start scanout — this overwrites the driver's self-test gradient.
    match display.set_mode(session, 0, buf.handle) {
        Ok(()) => puts("[DISPLAY-TEST] mode set OK\n"),
        Err(_) => { puts("[DISPLAY-TEST] set_mode FAILED\n"); sys_exit(); }
    };

    puts("[DISPLAY-TEST] done\n");
    loop { sys_yield(); }
}

/// Print a u32 as decimal digits.
fn print_u32(mut val: u32) {
    if val == 0 {
        putc(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while val > 0 {
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        putc(buf[i]);
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("[DISPLAY-TEST] PANIC\n");
    loop {
        unsafe { asm!("wfi") };
    }
}
