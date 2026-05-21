#![no_std]
#![no_main]
// Driver-crate body writes zero `unsafe` blocks AND zero
// `#[allow(unsafe_code)]` attributes. The macro-generated boot
// stub in `lockjaw_userlib::boot_stub!` is the single audited
// location for the boot-entry attributes.
//
// `#![deny]` (not `#![forbid]`) so the macro-emitted per-item
// allows on `#[no_mangle]` and `#[link_section]` are honoured.
// Acceptance grep:
// `grep -rn 'allow(unsafe_code)' user/ramfb-driver/src/`
// MUST return nothing.
#![deny(unsafe_code)]

use lockjaw_userlib::devmgr::claim_typed;
use lockjaw_userlib::display::{
    run_display_server, DisplayEngine, DisplayError, ModeInfo, PIXEL_FORMAT_XRGB8888,
};
use lockjaw_userlib::dma::{
    alloc_dma_backing, close_dma_backing, DmaMappingView, OwnedDmaMapping,
};
use lockjaw_userlib::driver_runtime::{driver_bootstrap, probe_by_hash};
use lockjaw_userlib::fwcfg::{dma_write, find_file};
use lockjaw_userlib::handle::PageSetHandle;
use lockjaw_userlib::{boot_stub, puts, sys_exit, FW_CFG_HASH};
use lockjaw_mmio::region::MappedRegs;
use lockjaw_regs::fw_cfg::FwCfg;
use lockjaw_types::fwcfg::{RamfbConfig, RAMFB_CONFIG_WIRE_SIZE, RAMFB_FORMAT_XRGB8888};

// ---------------------------------------------------------------------------
// Display mode table (first = preferred)
// ---------------------------------------------------------------------------

const MODES: [ModeInfo; 2] = [
    ModeInfo { width: 320, height: 240, format: PIXEL_FORMAT_XRGB8888, refresh_millihz: 60000 },
    ModeInfo { width: 640, height: 480, format: PIXEL_FORMAT_XRGB8888, refresh_millihz: 60000 },
];

// ---------------------------------------------------------------------------
// Scratch-page layout: FwCfgDmaAccess header at offset 0, RamfbConfig
// at offset 16. Both written via typed DmaCell so the driver never
// hand-packs bytes. `dma_write` from lockjaw_userlib::fwcfg owns
// the barrier + trigger + poll discipline; this driver only owns
// the ramfb-specific payload (RamfbConfig).
// ---------------------------------------------------------------------------

const DMA_HEADER_OFFSET: u64 = 0;
const RAMFB_CONFIG_OFFSET: u64 = 16;

// ---------------------------------------------------------------------------
// Self-test configuration (used once at startup to verify hw works)
// ---------------------------------------------------------------------------

const SELFTEST_WIDTH: u32 = 320;
const SELFTEST_HEIGHT: u32 = 240;
const SELFTEST_BPP: u32 = 4; // 32-bit XRGB8888
const SELFTEST_STRIDE: u32 = SELFTEST_WIDTH * SELFTEST_BPP;
const SELFTEST_SIZE_BYTES: u64 = SELFTEST_STRIDE as u64 * SELFTEST_HEIGHT as u64;

/// Program fw_cfg to display a framebuffer. The ramfb-specific work
/// is the `RamfbConfig` payload (framebuffer PA, format, geometry);
/// the generic fw_cfg DMA-write sequence (barrier + trigger + poll)
/// lives in `lockjaw_userlib::fwcfg::dma_write`.
fn program_ramfb(
    regs: &FwCfg,
    dma_page: &OwnedDmaMapping,
    ramfb_sel: u16,
    fb_phys: u64,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<(), DisplayError> {
    let cfg = RamfbConfig::new(fb_phys, RAMFB_FORMAT_XRGB8888, width, height, stride);
    dma_page.cell::<RamfbConfig>(RAMFB_CONFIG_OFFSET).write(cfg);
    dma_write(
        regs,
        dma_page,
        DMA_HEADER_OFFSET,
        RAMFB_CONFIG_OFFSET,
        ramfb_sel,
        RAMFB_CONFIG_WIRE_SIZE,
    )
    .map_err(|_| DisplayError::AllocFailed)
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

/// Implements the DDI `DisplayEngine` trait over fw_cfg + ramfb.
///
/// **Self-test buffer is not tracked.** At driver startup, `self_test`
/// allocates a framebuffer, draws a checkerboard, programs the
/// display, and `core::mem::forget`s the backing — the framebuffer
/// keeps scanning out indefinitely so the display shows SOMETHING
/// from boot until the first DDI client calls SetMode. That
/// framebuffer is NOT in `buffers[]` (clients didn't ask for it via
/// `alloc_buffer`) and the engine has no introspection path to
/// enumerate it. If we ever add "list current buffers" introspection
/// to the engine, the self-test buffer won't appear. By design.
struct RamfbEngine {
    regs: MappedRegs<FwCfg>,
    dma_page: OwnedDmaMapping,
    ramfb_sel: u16,
    session_active: bool,
    current_mode: Option<usize>,
    buffers: [Option<EngineBuffer>; MAX_ENGINE_BUFFERS],
}

impl RamfbEngine {
    fn find_buffer(&self, ps_handle: PageSetHandle) -> Option<&EngineBuffer> {
        self.buffers
            .iter()
            .filter_map(|s| s.as_ref())
            .find(|b| b.ps_handle == ps_handle)
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
        // Check engine slot capacity BEFORE allocating pages so we
        // don't leak DMA-backing pages on a tracking failure.
        let slot_idx = self
            .buffers
            .iter()
            .position(|s| s.is_none())
            .ok_or(DisplayError::AllocFailed)?;

        let stride = width * 4; // XRGB8888 = 4 bytes per pixel
        let size = stride * height;
        let pages = ((size as u64) + lockjaw_userlib::PAGE_SIZE - 1)
            / lockjaw_userlib::PAGE_SIZE;

        // Allocate DMA-backing pages (no local mapping — the client
        // maps the pageset). alloc_dma_backing wraps the
        // `sys_alloc_pages_contiguous + sys_query_pageset_phys` pair
        // so the driver doesn't touch raw syscalls.
        let backing = alloc_dma_backing(pages).map_err(|_| DisplayError::AllocFailed)?;
        self.buffers[slot_idx] = Some(EngineBuffer {
            ps_handle: backing.pageset,
            phys_addr: backing.pa,
        });
        puts("ramfb: buffer allocated\n");
        Ok((backing.pageset, stride, size))
    }

    /// Full modeset: program fw_cfg with mode dimensions and buffer PA.
    fn set_mode(&mut self, _session: u32, mode_index: u32, buffer_handle: PageSetHandle)
        -> Result<(), DisplayError>
    {
        let mode = MODES.get(mode_index as usize).ok_or(DisplayError::InvalidMode)?;
        let buf = self.find_buffer(buffer_handle).ok_or(DisplayError::InvalidBuffer)?;
        program_ramfb(
            self.regs.regs(), &self.dma_page, self.ramfb_sel,
            buf.phys_addr, mode.width, mode.height, mode.width * 4,
        )?;
        self.current_mode = Some(mode_index as usize);
        puts("ramfb: mode set\n");
        Ok(())
    }

    /// Page flip: reprogram with the current mode but a new buffer PA.
    fn set_scanout(&mut self, _session: u32, buffer_handle: PageSetHandle)
        -> Result<(), DisplayError>
    {
        let mode_idx = self.current_mode.ok_or(DisplayError::NotConfigured)?;
        let mode = &MODES[mode_idx];
        let buf = self.find_buffer(buffer_handle).ok_or(DisplayError::InvalidBuffer)?;
        program_ramfb(
            self.regs.regs(), &self.dma_page, self.ramfb_sel,
            buf.phys_addr, mode.width, mode.height, mode.width * 4,
        )?;
        Ok(())
    }

    /// Remove a buffer from engine tracking + close the pageset
    /// handle. The underlying physical pages cannot be freed
    /// (Lockjaw has no sys_free_pages); closing the handle releases
    /// our reference, leaving the client (if any) as the sole owner.
    fn free_buffer(&mut self, buffer_handle: PageSetHandle) {
        for slot in self.buffers.iter_mut() {
            if let Some(b) = slot {
                if b.ps_handle == buffer_handle {
                    close_dma_backing(b.ps_handle);
                    *slot = None;
                    return;
                }
            }
        }
    }

    /// Release the session. Display keeps showing the last buffer
    /// (the hardware continues scanning from the last programmed
    /// address).
    fn release_session(&mut self, _session: u32) -> Result<(), DisplayError> {
        self.session_active = false;
        self.current_mode = None;
        puts("ramfb: session released\n");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Self-test: verify display pipeline works before entering server loop
//
// Allocates a framebuffer, draws a checkerboard, programs the display.
// Runs once at startup so the display shows SOMETHING immediately (no
// regression from pre-DDI one-shot behavior). The self-test buffer
// is not tracked by the engine — a DDI client's SetMode will replace it.
// ---------------------------------------------------------------------------

fn self_test(engine: &mut RamfbEngine) {
    // Selftest buffer is one page (320×240×4 = 307200 bytes ≈ 75 pages).
    // We use alloc_dma_backing for the framebuffer itself (no local
    // mapping needed — we write to it via a local borrowed mapping).
    let pages = (SELFTEST_SIZE_BYTES + lockjaw_userlib::PAGE_SIZE - 1)
        / lockjaw_userlib::PAGE_SIZE;
    let backing = match alloc_dma_backing(pages) {
        Ok(b) => b,
        Err(_) => { puts("ramfb: self-test alloc FAILED\n"); return; }
    };

    // Local borrowed mapping for the framebuffer so we can draw
    // through typed cell access (no raw pointer math). The driver
    // never closes the pageset — we keep the backing alive across
    // mapping/unmapping (`close_dma_backing` happens at the very end).
    let fb_map = match lockjaw_userlib::dma::BorrowedDmaMapping::map_existing(
        backing.pageset,
        pages,
    ) {
        Ok(m) => m,
        Err(_) => {
            close_dma_backing(backing.pageset);
            puts("ramfb: self-test map FAILED\n");
            return;
        }
    };

    // Fill with a checkerboard. Each pixel is XRGB8888 (4 bytes).
    // 16-pixel tiles distinguish the self-test pattern from a
    // DDI client's gradient.
    let pixel_count = (SELFTEST_WIDTH * SELFTEST_HEIGHT) as usize;
    let pixel_view = fb_map.slice::<u32>(0, pixel_count);
    for y in 0..SELFTEST_HEIGHT {
        for x in 0..SELFTEST_WIDTH {
            let tile_white = ((x / 16) + (y / 16)) % 2 == 0;
            let v: u32 = if tile_white { 0xFFFF_FFFF } else { 0xFF40_4040 };
            pixel_view.write((y * SELFTEST_WIDTH + x) as usize, v);
        }
    }
    // Drop the borrowed mapping; the backing pageset stays alive.
    drop(fb_map);

    // Program the display via fw_cfg DMA. The framebuffer's first-page
    // PA is `backing.pa` (we allocated contiguous pages).
    match program_ramfb(
        engine.regs.regs(), &engine.dma_page, engine.ramfb_sel,
        backing.pa, SELFTEST_WIDTH, SELFTEST_HEIGHT, SELFTEST_STRIDE,
    ) {
        Ok(()) => puts("ramfb: display configured\n"),
        Err(_) => puts("ramfb: self-test DMA error\n"),
    }

    // Leak the backing intentionally — the display keeps scanning
    // from this PA after self_test returns. The framebuffer must
    // outlive any DDI client's first SetMode call. Lockjaw has no
    // sys_free_pages, so this is a process-lifetime leak (same as
    // pre-DDI behaviour). `core::mem::forget` is the unambiguous
    // idiom for "I'm deliberately not running cleanup on this" —
    // DmaBacking has no Drop today, but if a future edit gives it
    // one, the forget keeps the leak intentional. The engine has
    // no record of this buffer (see the doc comment on RamfbEngine).
    core::mem::forget(backing);
}

// ---------------------------------------------------------------------------
// Driver main — invoked by the boot_stub! macro. Uses Tier-A
// composable pieces (driver_bootstrap + probe_by_hash + claim_typed)
// instead of `driver_main!`'s standard_driver_init because ramfb
// has no IRQ — the standard helper would call bind_irq unconditionally.
// This is the first production user of the escape-valve pattern
// documented in driver_runtime.rs and book-of-lockjaw.
// ---------------------------------------------------------------------------

fn ramfb_entry() -> ! {
    puts("ramfb: starting\n");

    // Tier-A bootstrap: receive devmgr + display server endpoints
    // from init via the bootstrap IPC.
    let boot = match driver_bootstrap() {
        Ok(b) => b,
        Err(_) => { puts("ramfb: bootstrap FAILED\n"); sys_exit(); }
    };
    puts("ramfb: bootstrapped\n");
    let display_ep = match boot.server_ep {
        Some(ep) => ep,
        None => { puts("ramfb: no server endpoint\n"); sys_exit(); }
    };

    // Tier-A probe + claim. ramfb is fw_cfg-shaped (no DeviceID
    // discriminator); first match wins.
    let probe = match probe_by_hash(&boot, FW_CFG_HASH, 0) {
        Ok(p) => p,
        Err(_) => { puts("ramfb: probe FAILED\n"); sys_exit(); }
    };
    let claimed = match claim_typed::<FwCfg>(boot.devmgr_ep, boot.reply_obj, probe.mmio_addr) {
        Ok(c) => c,
        Err(_) => { puts("ramfb: claim FAILED\n"); sys_exit(); }
    };
    puts("ramfb: claimed fw_cfg\n");

    // Find the etc/ramfb selector — fail gracefully when running on
    // a QEMU build without ramfb (the test harness asserts this
    // line on the QEMU-virt machine without `-device ramfb`).
    let ramfb_sel = match find_file(claimed.regs.regs(), b"etc/ramfb") {
        Some(s) => s,
        None => {
            puts("ramfb: etc/ramfb not found in fw_cfg\n");
            sys_exit();
        }
    };
    puts("ramfb: found etc/ramfb\n");

    // Allocate the DMA scratch page (one page, owned mapping —
    // Drop closes the pageset when the engine drops at process exit).
    let dma_page = match OwnedDmaMapping::alloc() {
        Ok(p) => p,
        Err(_) => { puts("ramfb: alloc dma FAILED\n"); sys_exit(); }
    };

    let mut engine = RamfbEngine {
        regs: claimed.regs,
        dma_page,
        ramfb_sel,
        session_active: false,
        current_mode: None,
        buffers: [None; MAX_ENGINE_BUFFERS],
    };

    // Self-test: draw a checkerboard so the display shows something
    // immediately. A DDI client's SetMode replaces it.
    self_test(&mut engine);

    puts("ramfb: entering display server loop\n");
    run_display_server(&mut engine, display_ep)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("ramfb: PANIC\n");
    sys_exit();
}

// ---------------------------------------------------------------------------
// Driver boot — Tier-A `boot_stub!` only (not `driver_main!`), because
// ramfb's shape doesn't fit the standard "claim + bind_irq + return ctx"
// helper. The macro is the single audited site for the boot
// `#[allow(unsafe_code)]` attributes; the driver crate body itself is
// `#![deny(unsafe_code)]` with zero allows.
// ---------------------------------------------------------------------------

boot_stub! {
    hash = LOCKJAW_SOURCE_HASH,
    main = ramfb_entry,
}
