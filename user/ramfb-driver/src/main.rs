#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;
use core::arch::asm;
use core::ptr;
use lockjaw_userlib::*;
use lockjaw_userlib::display::{
    DisplayEngine, run_display_server, ModeInfo, DisplayError, PIXEL_FORMAT_XRGB8888,
};

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
// Display mode table (first = preferred)
// ---------------------------------------------------------------------------

const MODES: [ModeInfo; 2] = [
    ModeInfo { width: 320, height: 240, format: PIXEL_FORMAT_XRGB8888, refresh_millihz: 60000 },
    ModeInfo { width: 640, height: 480, format: PIXEL_FORMAT_XRGB8888, refresh_millihz: 60000 },
];

// ---------------------------------------------------------------------------
// Self-test configuration (used once at startup to verify hw works)
// ---------------------------------------------------------------------------

const SELFTEST_WIDTH: u32 = 320;
const SELFTEST_HEIGHT: u32 = 240;
const SELFTEST_BPP: u32 = 4;            // 32-bit XRGB8888
const SELFTEST_STRIDE: u32 = SELFTEST_WIDTH * SELFTEST_BPP;
const SELFTEST_SIZE: u64 = SELFTEST_STRIDE as u64 * SELFTEST_HEIGHT as u64;
const SELFTEST_PAGES: u64 = (SELFTEST_SIZE + PAGE_SIZE - 1) / PAGE_SIZE;

// ---------------------------------------------------------------------------
// fw_cfg MMIO helpers
// ---------------------------------------------------------------------------

unsafe fn fwcfg_select(base: u64, selector: u16) {
    ptr::write_volatile((base + FWCFG_SEL) as *mut u16, selector.to_be());
}

unsafe fn fwcfg_read8(base: u64) -> u8 {
    ptr::read_volatile((base + FWCFG_DATA) as *const u8)
}

/// Read N bytes from the currently selected fw_cfg item.
unsafe fn fwcfg_read_bytes(base: u64, buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = fwcfg_read8(base);
    }
}

/// Read a big-endian u32 from the current fw_cfg item.
unsafe fn fwcfg_read_be32(base: u64) -> u32 {
    let mut buf = [0u8; 4];
    fwcfg_read_bytes(base, &mut buf);
    u32::from_be_bytes(buf)
}

/// Read a big-endian u16 from the current fw_cfg item.
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

    // Directory header: 4-byte big-endian count
    let count = fwcfg_read_be32(base);

    // Each entry: 4 bytes size + 2 bytes selector + 2 bytes reserved + 56 bytes name
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
// fw_cfg DMA programming
// ---------------------------------------------------------------------------

/// Program fw_cfg to display a framebuffer via DMA. Writes the RAMFBConfig
/// and DMA header, triggers the transfer, and polls for completion.
/// Returns true on success, false on DMA error.
unsafe fn program_ramfb(
    fwcfg_va: u64,
    dma_va: u64,
    dma_pa: u64,
    ramfb_sel: u16,
    fb_phys: u64,
    width: u32,
    height: u32,
    format: u32,
    stride: u32,
) -> bool {
    // Layout inside the DMA page:
    //   [0..16]   FWCfgDmaAccess header (control, length, address — all BE)
    //   [16..44]  RAMFBConfig  (addr, fourcc, flags, width, height, stride — all BE)
    let header_va = dma_va as *mut u8;
    let config_va = (dma_va + 16) as *mut u8;
    let config_phys = dma_pa + 16;

    // Fill RAMFBConfig.
    let mut cfg = [0u8; 28];
    cfg[0..8].copy_from_slice(&fb_phys.to_be_bytes());
    cfg[8..12].copy_from_slice(&format.to_be_bytes());
    cfg[12..16].copy_from_slice(&0u32.to_be_bytes());
    cfg[16..20].copy_from_slice(&width.to_be_bytes());
    cfg[20..24].copy_from_slice(&height.to_be_bytes());
    cfg[24..28].copy_from_slice(&stride.to_be_bytes());
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
    ptr::write_volatile((fwcfg_va + FWCFG_DMA) as *mut u64, dma_pa.to_be());

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
            return false;
        }
        if ctl & (DMA_CTRL_SELECT | DMA_CTRL_SKIP | DMA_CTRL_WRITE) == 0 {
            break;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// RamfbEngine — implements the DDI DisplayEngine trait
// ---------------------------------------------------------------------------

const MAX_ENGINE_BUFFERS: usize = 8;

#[derive(Clone, Copy)]
struct EngineBuffer {
    ps_handle: PageSetHandle,
    phys_addr: u64,
}

struct RamfbEngine {
    fwcfg_va: u64,
    dma_va: u64,
    dma_pa: u64,
    ramfb_sel: u16,
    session_active: bool,
    current_mode: Option<usize>,
    buffers: [Option<EngineBuffer>; MAX_ENGINE_BUFFERS],
}

impl RamfbEngine {
    fn find_buffer(&self, ps_handle: PageSetHandle) -> Option<&EngineBuffer> {
        self.buffers.iter()
            .filter_map(|s| s.as_ref())
            .find(|b| b.ps_handle.0 == ps_handle.0)
    }
}

impl DisplayEngine for RamfbEngine {
    fn mode_count(&self) -> u32 {
        MODES.len() as u32
    }

    fn get_mode(&self, index: u32) -> Option<ModeInfo> {
        MODES.get(index as usize).copied()
    }

    fn create_session(&mut self) -> Result<(), DisplayError> {
        if self.session_active {
            return Err(DisplayError::SessionBusy);
        }
        self.session_active = true;
        puts("ramfb: session created\n");
        Ok(())
    }

    fn alloc_buffer(&mut self, _session: u32, width: u32, height: u32, format: u32)
        -> Result<(PageSetHandle, u32, u32), DisplayError>
    {
        if format != PIXEL_FORMAT_XRGB8888 {
            return Err(DisplayError::InvalidMode);
        }

        // Check engine buffer capacity BEFORE allocating pages to avoid
        // leaking contiguous pages on a tracking failure. Lockjaw has no
        // sys_free_pages, so allocated pages cannot be reclaimed.
        let slot_idx = self.buffers.iter()
            .position(|s| s.is_none())
            .ok_or(DisplayError::AllocFailed)?;

        let stride = width * 4; // XRGB8888 = 4 bytes per pixel
        let size = stride * height;
        let pages = ((size as u64) + PAGE_SIZE - 1) / PAGE_SIZE;

        // Allocate physically contiguous pages for DMA scanout
        let ps = sys_alloc_pages_contiguous(pages)
            .map_err(|_| DisplayError::AllocFailed)?;

        // Query physical address — needed to program fw_cfg DMA base
        let phys = sys_query_pageset_phys(ps, 0)
            .map_err(|_| DisplayError::AllocFailed)?;

        // Track the buffer so set_mode/set_scanout can look up phys_addr
        self.buffers[slot_idx] = Some(EngineBuffer { ps_handle: ps, phys_addr: phys });

        puts("ramfb: buffer allocated\n");
        Ok((ps, stride, size))
    }

    /// Full modeset: program fw_cfg with mode dimensions and buffer physical address.
    fn set_mode(&mut self, _session: u32, mode_index: u32, buffer_handle: PageSetHandle)
        -> Result<(), DisplayError>
    {
        let mode = MODES.get(mode_index as usize)
            .ok_or(DisplayError::InvalidMode)?;
        let buf = self.find_buffer(buffer_handle)
            .ok_or(DisplayError::InvalidBuffer)?;

        let ok = unsafe {
            program_ramfb(
                self.fwcfg_va, self.dma_va, self.dma_pa, self.ramfb_sel,
                buf.phys_addr, mode.width, mode.height, mode.format,
                mode.width * 4,
            )
        };
        if !ok {
            puts("ramfb: DMA error in set_mode\n");
            return Err(DisplayError::AllocFailed);
        }

        self.current_mode = Some(mode_index as usize);
        puts("ramfb: mode set\n");
        Ok(())
    }

    /// Page flip: reprogram fw_cfg with the current mode dimensions but a
    /// new buffer physical address. No modeset — just changes the scanout source.
    fn set_scanout(&mut self, _session: u32, buffer_handle: PageSetHandle)
        -> Result<(), DisplayError>
    {
        let mode_idx = self.current_mode.ok_or(DisplayError::NotConfigured)?;
        let mode = &MODES[mode_idx];
        let buf = self.find_buffer(buffer_handle)
            .ok_or(DisplayError::InvalidBuffer)?;

        let ok = unsafe {
            program_ramfb(
                self.fwcfg_va, self.dma_va, self.dma_pa, self.ramfb_sel,
                buf.phys_addr, mode.width, mode.height, mode.format,
                mode.width * 4,
            )
        };
        if !ok {
            puts("ramfb: DMA error in set_scanout\n");
            return Err(DisplayError::AllocFailed);
        }

        Ok(())
    }

    /// Remove a buffer from engine tracking. Called by the server loop
    /// on export failure or session teardown. Note: the underlying
    /// physical pages cannot be freed — Lockjaw has no sys_free_pages
    /// syscall. Only the engine's internal tracking is cleared.
    fn free_buffer(&mut self, buffer_handle: PageSetHandle) {
        for slot in self.buffers.iter_mut() {
            if let Some(b) = slot {
                if b.ps_handle.0 == buffer_handle.0 {
                    *slot = None;
                    return;
                }
            }
        }
    }

    /// Release the session. Display keeps showing the last buffer —
    /// the hardware continues scanning from the last programmed address.
    fn release_session(&mut self, _session: u32) -> Result<(), DisplayError> {
        self.session_active = false;
        self.current_mode = None;
        puts("ramfb: session released\n");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Self-test: verify display pipeline works before entering server loop
// ---------------------------------------------------------------------------

/// Allocate a framebuffer, draw a test gradient, and program the display.
/// This runs once at startup so the display shows something immediately
/// (no regression from the pre-DDI one-shot behavior). The self-test
/// buffer is not tracked by the engine — it will be overwritten when a
/// DDI client calls SetMode.
fn self_test(fwcfg_va: u64, dma_va: u64, dma_pa: u64, ramfb_sel: u16) {
    // Allocate framebuffer pages (physically contiguous for DMA)
    let fb_ps = match sys_alloc_pages_contiguous(SELFTEST_PAGES) {
        Ok(id) => id,
        Err(_) => { puts("ramfb: self-test alloc FAILED\n"); return; }
    };
    let fb_va = VMEM.alloc(SELFTEST_PAGES as usize).expect("VA exhausted for self-test fb");
    if !sys_map_pages(fb_ps, fb_va, MapMemoryAttribute::Normal).is_ok() {
        puts("ramfb: self-test map FAILED\n");
        return;
    }

    // Framebuffer pages are physically contiguous; page 0's address
    // is the DMA base.
    let fb_phys = match sys_query_pageset_phys(fb_ps, 0) {
        Ok(addr) => addr,
        Err(_) => { puts("ramfb: self-test query phys FAILED\n"); return; }
    };

    // Fill framebuffer with a 16-pixel checkerboard so the self-test
    // pattern is visually distinct from the DDI client's gradient.
    unsafe {
        let fb = fb_va as *mut u8;
        for y in 0..SELFTEST_HEIGHT {
            for x in 0..SELFTEST_WIDTH {
                let offset = (y * SELFTEST_STRIDE + x * SELFTEST_BPP) as usize;
                let white = ((x / 16) + (y / 16)) % 2 == 0;
                let val = if white { 0xFF } else { 0x40 };
                // XRGB8888: byte order is B, G, R, X (little-endian pixel)
                *fb.add(offset) = val;
                *fb.add(offset + 1) = val;
                *fb.add(offset + 2) = val;
                *fb.add(offset + 3) = 0xFF;
            }
        }
    }

    // Program display via fw_cfg DMA
    let ok = unsafe {
        program_ramfb(
            fwcfg_va, dma_va, dma_pa, ramfb_sel,
            fb_phys, SELFTEST_WIDTH, SELFTEST_HEIGHT,
            PIXEL_FORMAT_XRGB8888, SELFTEST_STRIDE,
        )
    };
    if ok {
        puts("ramfb: display configured\n");
    } else {
        puts("ramfb: self-test DMA error\n");
    }
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

    // Bootstrap: get devmgr_ep + display_ep from init
    puts("ramfb: bootstrapping...\n");
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("ramfb: bootstrap FAILED\n"); halt(); }
    };
    let devmgr_client = EndpointHandle(reply[0]);
    let display_ep = EndpointHandle(reply[1]);
    puts("ramfb: bootstrapped\n");

    // Claim fw_cfg device from device manager
    let claim = match sys_call_ret4(devmgr_client, reply_obj, CMD_CLAIM_DEVICE, FW_CFG_HASH, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("ramfb: claim FAILED\n"); halt(); }
    };
    if claim[0] != lockjaw_userlib::CLAIM_OK {
        puts("ramfb: no fw_cfg device\n");
        halt();
    }
    let fwcfg_pageset = PageSetHandle(claim[1]);
    puts("ramfb: claimed fw_cfg\n");

    // Map fw_cfg MMIO page
    let fwcfg_va = VMEM.alloc(1).expect("VA exhausted for fw_cfg");
    if !sys_map_pages(fwcfg_pageset, fwcfg_va, MapMemoryAttribute::Device).is_ok() {
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

    // Allocate a scratch page for the DMA control header + the inline
    // RAMFBConfig. We need both in guest RAM at known physical addresses
    // because QEMU reads them by phys addr during the DMA transfer.
    let dma_ps = match sys_alloc_pages(1) {
        Ok(id) => id,
        Err(_) => { puts("ramfb: alloc dma FAILED\n"); halt(); }
    };
    let dma_va = VMEM.alloc(1).expect("VA exhausted for DMA");
    if !sys_map_pages(dma_ps, dma_va, MapMemoryAttribute::Normal).is_ok() {
        puts("ramfb: map dma FAILED\n");
        halt();
    }
    let dma_pa = match sys_query_pageset_phys(dma_ps, 0) {
        Ok(p) => p,
        Err(_) => { puts("ramfb: query dma phys FAILED\n"); halt(); }
    };

    // Self-test: draw gradient to verify display pipeline works.
    // The gradient is visible immediately — no regression from pre-DDI behavior.
    // A DDI client's SetMode will overwrite this with its own buffer.
    self_test(fwcfg_va, dma_va, dma_pa, ramfb_sel);

    // Build engine and enter the DDI server loop (never returns).
    let mut engine = RamfbEngine {
        fwcfg_va,
        dma_va,
        dma_pa,
        ramfb_sel,
        session_active: false,
        current_mode: None,
        buffers: [None; MAX_ENGINE_BUFFERS],
    };
    puts("ramfb: entering display server loop\n");
    run_display_server(&mut engine, display_ep);
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
