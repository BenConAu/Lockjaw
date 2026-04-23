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
// fw_cfg DMA control flags (in the FWCfgDmaAccess.control BE u32)
// ---------------------------------------------------------------------------
// Since QEMU v2.4 PIO writes to the DATA register are no-ops; writes must go
// through the DMA interface (reinstated in v2.9). That's what drives ramfb's
// write-callback. See QEMU docs/specs/fw_cfg.rst.
const DMA_CTRL_ERROR:  u32 = 1 << 0;  // set by device on failure
const DMA_CTRL_SKIP:   u32 = 1 << 2;  // skip bytes without reading/writing
const DMA_CTRL_SELECT: u32 = 1 << 3;  // selector in bits 16..31 of control
const DMA_CTRL_WRITE:  u32 = 1 << 4;  // transfer is guest -> device

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


// ---------------------------------------------------------------------------
// fw_cfg MMIO helpers
// ---------------------------------------------------------------------------

unsafe fn fwcfg_select(base: u64, selector: u16) {
    ptr::write_volatile((base + FWCFG_SEL) as *mut u16, selector.to_be());
}

unsafe fn fwcfg_read8(base: u64) -> u8 {
    ptr::read_volatile((base + FWCFG_DATA) as *const u8)
}

unsafe fn fwcfg_read_bytes(base: u64, buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = fwcfg_read8(base);
    }
}

unsafe fn fwcfg_read_be32(base: u64) -> u32 {
    let mut buf = [0u8; 4];
    fwcfg_read_bytes(base, &mut buf);
    u32::from_be_bytes(buf)
}

unsafe fn fwcfg_read_be16(base: u64) -> u16 {
    let mut buf = [0u8; 2];
    fwcfg_read_bytes(base, &mut buf);
    u16::from_be_bytes(buf)
}

// ---------------------------------------------------------------------------
// fw_cfg directory search
// ---------------------------------------------------------------------------

/// Find the selector for a named fw_cfg file by enumerating the directory.
/// Returns the selector value, or 0 if not found.
unsafe fn fwcfg_find_file(base: u64, name: &[u8]) -> u16 {
    fwcfg_select(base, FW_CFG_FILE_DIR);

    let count = fwcfg_read_be32(base);

    for _ in 0..count {
        let _size = fwcfg_read_be32(base);
        let selector = fwcfg_read_be16(base);
        let _reserved = fwcfg_read_be16(base);

        let mut entry_name = [0u8; 56];
        fwcfg_read_bytes(base, &mut entry_name);

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

    // Allocate our Reply object for outbound sys_call (bootstrap + claim).
    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("ramfb: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: get devmgr_client from init
    puts("ramfb: bootstrapping...\n");
    let reply = match sys_call_ret4(0, reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("ramfb: bootstrap FAILED\n"); halt(); }
    };
    let devmgr_client = reply[0];
    puts("ramfb: bootstrapped\n");

    // Claim fw_cfg device from device manager
    let claim = match sys_call_ret4(devmgr_client, reply_obj, CMD_CLAIM_DEVICE, FW_CFG_HASH, 0, 0) {
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
    let fwcfg_va = VMEM.alloc(1).expect("VA exhausted for fw_cfg");
    if !sys_map_pages(fwcfg_pageset, fwcfg_va, MAP_FLAG_DEVICE).is_ok() {
        puts("ramfb: map fw_cfg FAILED\n");
        halt();
    }
    puts("ramfb: fw_cfg mapped\n");

    // Find the "etc/ramfb" selector
    let ramfb_sel = unsafe { fwcfg_find_file(fwcfg_va, b"etc/ramfb") };
    if ramfb_sel == 0 {
        puts("ramfb: etc/ramfb not found in fw_cfg\n");
        halt();
    }
    puts("ramfb: found etc/ramfb\n");

    // Allocate framebuffer pages
    let fb_ps = match sys_alloc_pages_contiguous(FB_PAGES) {
        Ok(id) => id,
        Err(_) => { puts("ramfb: alloc fb FAILED\n"); halt(); }
    };
    puts("ramfb: fb allocated\n");

    // Map framebuffer into our address space (normal memory, not device)
    let fb_va = VMEM.alloc(FB_PAGES as usize).expect("VA exhausted for framebuffer");
    if !sys_map_pages(fb_ps, fb_va, 0).is_ok() {
        puts("ramfb: map fb FAILED\n");
        halt();
    }
    puts("ramfb: fb mapped\n");

    // Framebuffer pages are physically contiguous; page 0's address
    // is the DMA base.
    let fb_phys = match sys_query_pageset_phys(fb_ps, 0) {
        Ok(addr) => addr,
        Err(_) => { puts("ramfb: query phys FAILED\n"); halt(); }
    };
    puts("ramfb: fb phys queried\n");

    // Fill framebuffer with a test pattern: vertical color gradient
    unsafe {
        let fb = fb_va as *mut u8;
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

    // Allocate a scratch page for the DMA control header + the inline
    // RAMFBConfig. We need both in guest RAM at known physical addresses
    // because QEMU reads them by phys addr during the DMA transfer.
    let dma_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("ramfb: alloc dma FAILED\n"); halt(); }
    };
    let dma_va = VMEM.alloc(1).expect("VA exhausted for DMA");
    if !sys_map_pages(dma_ps, dma_va, 0).is_ok() {
        puts("ramfb: map dma FAILED\n");
        halt();
    }
    let dma_phys = match sys_query_pageset_phys(dma_ps, 0) {
        Ok(p) => p,
        Err(_) => { puts("ramfb: query dma phys FAILED\n"); halt(); }
    };

    // Layout inside the DMA page:
    //   [0..16]   FWCfgDmaAccess header (control, length, address — all BE)
    //   [16..44]  RAMFBConfig  (addr, fourcc, flags, width, height, stride — all BE)
    let header_va = dma_va as *mut u8;
    let config_va = (dma_va + 16) as *mut u8;
    let config_phys = dma_phys + 16;

    unsafe {
        // Fill RAMFBConfig.
        let mut cfg = [0u8; 28];
        cfg[0..8].copy_from_slice(&fb_phys.to_be_bytes());
        cfg[8..12].copy_from_slice(&FOURCC_XRGB8888.to_be_bytes());
        cfg[12..16].copy_from_slice(&0u32.to_be_bytes());
        cfg[16..20].copy_from_slice(&FB_WIDTH.to_be_bytes());
        cfg[20..24].copy_from_slice(&FB_HEIGHT.to_be_bytes());
        cfg[24..28].copy_from_slice(&FB_STRIDE.to_be_bytes());
        for (i, b) in cfg.iter().enumerate() {
            ptr::write_volatile(config_va.add(i), *b);
        }

        // Fill FWCfgDmaAccess: control, length, address.
        let control: u32 =
            ((ramfb_sel as u32) << 16) | DMA_CTRL_SELECT | DMA_CTRL_WRITE;
        for (i, b) in control.to_be_bytes().iter().enumerate() {
            ptr::write_volatile(header_va.add(i), *b);
        }
        for (i, b) in (28u32).to_be_bytes().iter().enumerate() {
            ptr::write_volatile(header_va.add(4 + i), *b);
        }
        for (i, b) in config_phys.to_be_bytes().iter().enumerate() {
            ptr::write_volatile(header_va.add(8 + i), *b);
        }

        // Full system barrier: make sure the header is observable in RAM
        // before we trigger the DMA via the MMIO register.
        asm!("dsb sy");                          // finish prior stores before MMIO

        // Trigger DMA: write the guest-physical address of the header,
        // big-endian, to the 64-bit DMA address register.
        ptr::write_volatile((fwcfg_va + FWCFG_DMA) as *mut u64, dma_phys.to_be());

        // Poll the control field. QEMU clears the READ/SKIP/SELECT/WRITE
        // bits when the transfer finishes; ERROR (bit 0) means failure.
        loop {
            let cb = [
                ptr::read_volatile(header_va.add(0)),
                ptr::read_volatile(header_va.add(1)),
                ptr::read_volatile(header_va.add(2)),
                ptr::read_volatile(header_va.add(3)),
            ];
            let ctl = u32::from_be_bytes(cb);
            if ctl & DMA_CTRL_ERROR != 0 {
                puts("ramfb: DMA error\n");
                halt();
            }
            if ctl & (DMA_CTRL_SELECT | DMA_CTRL_SKIP | DMA_CTRL_WRITE) == 0 {
                break;
            }
        }
    }
    puts("ramfb: display configured\n");

    // Framebuffer is now being rendered by QEMU. Nothing left to do.
    sys_exit();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn halt() -> ! {
    sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("ramfb: PANIC\n");
    halt();
}
