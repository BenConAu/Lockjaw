#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use core::ptr;
use lockjaw_userlib::*;

// ---------------------------------------------------------------------------
// fw_cfg MMIO register offsets (relative to base VA)
// ---------------------------------------------------------------------------

const FWCFG_DATA: u64 = 0x00;     // Data register (read/write, 8-64 bit)
const FWCFG_SEL: u64 = 0x08;      // Selector register (write-only, 16-bit BE)
const FWCFG_DMA: u64 = 0x10;      // DMA address register (write-only, 64-bit BE)

// fw_cfg well-known selectors
const FW_CFG_FILE_DIR: u16 = 0x0019;

// ---------------------------------------------------------------------------
// ramfb configuration
// ---------------------------------------------------------------------------

const FB_WIDTH: u32 = 320;
const FB_HEIGHT: u32 = 240;
const FB_BPP: u32 = 4;            // 32-bit XRGB8888
const FB_STRIDE: u32 = FB_WIDTH * FB_BPP;
const FB_SIZE: u64 = FB_STRIDE as u64 * FB_HEIGHT as u64;
const FB_PAGES: u64 = (FB_SIZE + PAGE_SIZE - 1) / PAGE_SIZE;

// DRM fourcc for XRGB8888 = "XR24" = 0x34325258
const FOURCC_XRGB8888: u32 = 0x34325258;

// ---------------------------------------------------------------------------
// User VA layout
// ---------------------------------------------------------------------------

/// fw_cfg MMIO page (1 page)
const FWCFG_VA: u64 = 0x0020_0000;

/// Framebuffer VA (must be L2-aligned for large mapping)
const FB_VA: u64 = 0x0040_0000;

// ---------------------------------------------------------------------------
// fw_cfg MMIO helpers
// ---------------------------------------------------------------------------

unsafe fn fwcfg_select(selector: u16) {
    // Selector register is 16-bit big-endian
    ptr::write_volatile((FWCFG_VA + FWCFG_SEL) as *mut u16, selector.to_be());
}

unsafe fn fwcfg_read8() -> u8 {
    ptr::read_volatile((FWCFG_VA + FWCFG_DATA) as *const u8)
}

unsafe fn fwcfg_write8(val: u8) {
    ptr::write_volatile((FWCFG_VA + FWCFG_DATA) as *mut u8, val);
}

/// Read N bytes from the currently selected fw_cfg item.
unsafe fn fwcfg_read_bytes(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = fwcfg_read8();
    }
}

/// Write N bytes to the currently selected fw_cfg item.
unsafe fn fwcfg_write_bytes(buf: &[u8]) {
    for &b in buf.iter() {
        fwcfg_write8(b);
    }
}

/// Read a big-endian u32 from the current fw_cfg item.
unsafe fn fwcfg_read_be32() -> u32 {
    let mut buf = [0u8; 4];
    fwcfg_read_bytes(&mut buf);
    u32::from_be_bytes(buf)
}

/// Read a big-endian u16 from the current fw_cfg item.
unsafe fn fwcfg_read_be16() -> u16 {
    let mut buf = [0u8; 2];
    fwcfg_read_bytes(&mut buf);
    u16::from_be_bytes(buf)
}

// ---------------------------------------------------------------------------
// fw_cfg directory search
// ---------------------------------------------------------------------------

/// Find the selector for a named fw_cfg file by enumerating the directory.
/// Returns the selector value, or 0 if not found.
unsafe fn fwcfg_find_file(name: &[u8]) -> u16 {
    fwcfg_select(FW_CFG_FILE_DIR);

    // Directory header: 4-byte big-endian count
    let count = fwcfg_read_be32();

    // Each entry: 4 bytes size + 2 bytes selector + 2 bytes reserved + 56 bytes name
    for _ in 0..count {
        let _size = fwcfg_read_be32();
        let selector = fwcfg_read_be16();
        let _reserved = fwcfg_read_be16();

        // Read the 56-byte name
        let mut entry_name = [0u8; 56];
        fwcfg_read_bytes(&mut entry_name);

        // Compare (NUL-terminated)
        let entry_len = entry_name.iter().position(|&b| b == 0).unwrap_or(56);
        if entry_len == name.len() && entry_name[..entry_len] == *name {
            return selector;
        }
    }

    0
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("ramfb: starting\n");

    // Bootstrap: get devmgr_client from init
    puts("ramfb: bootstrapping...\n");
    let reply = match sys_call_ret4(0, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("ramfb: bootstrap FAILED\n"); halt(); }
    };
    let devmgr_client = reply[0];
    puts("ramfb: bootstrapped\n");

    // Claim fw_cfg device from device manager
    let claim = match sys_call_ret4(devmgr_client, CMD_CLAIM_DEVICE, FW_CFG_HASH, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("ramfb: claim FAILED\n"); halt(); }
    };
    let fwcfg_pageset = claim[0];
    if fwcfg_pageset == 0 {
        puts("ramfb: no fw_cfg device\n");
        halt();
    }
    puts("ramfb: claimed fw_cfg\n");

    // Map fw_cfg MMIO page
    if !sys_map_pages(fwcfg_pageset, FWCFG_VA, MAP_FLAG_DEVICE).is_ok() {
        puts("ramfb: map fw_cfg FAILED\n");
        halt();
    }
    puts("ramfb: fw_cfg mapped\n");

    // Find the "etc/ramfb" selector
    let ramfb_sel = unsafe { fwcfg_find_file(b"etc/ramfb") };
    if ramfb_sel == 0 {
        puts("ramfb: etc/ramfb not found in fw_cfg\n");
        halt();
    }
    puts("ramfb: found etc/ramfb\n");

    // Allocate framebuffer pages
    let fb_ps = match sys_alloc_pages(FB_PAGES) {
        Ok(id) => id,
        Err(_) => { puts("ramfb: alloc fb FAILED\n"); halt(); }
    };
    puts("ramfb: fb allocated\n");

    // Map framebuffer into our address space (normal memory, not device)
    if !sys_map_pages(fb_ps, FB_VA, 0).is_ok() {
        puts("ramfb: map fb FAILED\n");
        halt();
    }
    puts("ramfb: fb mapped\n");

    // Query the physical address of the first framebuffer page.
    // ramfb needs contiguous physical memory. Our page allocator doesn't
    // guarantee contiguity, but for a small 320x240 buffer on a fresh
    // system the pages are likely sequential. We use page 0's address
    // as the DMA base. If pages aren't contiguous, the display will
    // show tearing -- a known limitation until contiguous alloc is added.
    let fb_phys = match sys_query_pageset_phys(fb_ps, 0) {
        Ok(addr) => addr,
        Err(_) => { puts("ramfb: query phys FAILED\n"); halt(); }
    };
    puts("ramfb: fb phys queried\n");

    // Fill framebuffer with a test pattern: vertical color gradient
    unsafe {
        let fb = FB_VA as *mut u8;
        for y in 0..FB_HEIGHT {
            for x in 0..FB_WIDTH {
                let offset = (y * FB_STRIDE + x * FB_BPP) as usize;
                let r = (x * 255 / FB_WIDTH) as u8;
                let g = (y * 255 / FB_HEIGHT) as u8;
                let b = 128u8;
                // XRGB8888: byte order is B, G, R, X (little-endian pixel)
                *fb.add(offset) = b;
                *fb.add(offset + 1) = g;
                *fb.add(offset + 2) = r;
                *fb.add(offset + 3) = 0xFF;
            }
        }
    }
    puts("ramfb: test pattern written\n");

    // Write RAMFBConfig to fw_cfg (28 bytes, all big-endian)
    let mut config = [0u8; 28];
    config[0..8].copy_from_slice(&fb_phys.to_be_bytes());          // addr
    config[8..12].copy_from_slice(&FOURCC_XRGB8888.to_be_bytes()); // fourcc
    config[12..16].copy_from_slice(&0u32.to_be_bytes());           // flags
    config[16..20].copy_from_slice(&FB_WIDTH.to_be_bytes());       // width
    config[20..24].copy_from_slice(&FB_HEIGHT.to_be_bytes());      // height
    config[24..28].copy_from_slice(&FB_STRIDE.to_be_bytes());      // stride

    unsafe {
        fwcfg_select(ramfb_sel);
        fwcfg_write_bytes(&config);
    }
    puts("ramfb: display configured\n");

    // Idle -- the framebuffer is now being rendered by QEMU
    loop { sys_yield(); }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("ramfb: PANIC\n");
    halt();
}
