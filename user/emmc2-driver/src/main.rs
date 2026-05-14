#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::arch::asm;
use core::ptr;
use lockjaw_userlib::*;
use lockjaw_userlib::clock::{ClockClient, ClockError};
use lockjaw_types::device::{
    BCM2711_EMMC2_HASH, CMD_CLAIM_DEVICE, CLAIM_OK, unpack_clock_ref,
};
use lockjaw_types::sdhci::{
    Capabilities,
    SDHCI_SOFTWARE_RESET, SW_RST_ALL,
    SDHCI_CAPABILITIES, SDHCI_CAPABILITIES_HI,
    SDHCI_HOST_VERSION, SDHCI_SPEC_300,
};

// ---------------------------------------------------------------------------
// MMIO helpers
// ---------------------------------------------------------------------------
//
// SDHCI assigns specific access widths per register; mismatched widths
// can fault on real silicon. SOFTWARE_RESET (0x02f) is a single byte;
// CAPABILITIES / CAPABILITIES_HI (0x040 / 0x044) are 32-bit reads.

/// Read an 8-bit SDHCI register at `base + offset`.
unsafe fn sdhci_read8(base: u64, offset: usize) -> u8 {
    ptr::read_volatile((base + offset as u64) as *const u8)
}

/// Write an 8-bit SDHCI register at `base + offset`.
unsafe fn sdhci_write8(base: u64, offset: usize, value: u8) {
    ptr::write_volatile((base + offset as u64) as *mut u8, value);
}

/// Read a 16-bit SDHCI register at `base + offset`. HOST_VERSION
/// (0x0fe) is the only u16 we read in M1; offset must be 2-byte aligned.
unsafe fn sdhci_read16(base: u64, offset: usize) -> u16 {
    ptr::read_volatile((base + offset as u64) as *const u16)
}

/// Read a 32-bit SDHCI register at `base + offset`. Caller is
/// responsible for the offset being 4-byte aligned (the spec
/// guarantees this for every 32-bit register listed in sdhci.rs).
unsafe fn sdhci_read32(base: u64, offset: usize) -> u32 {
    ptr::read_volatile((base + offset as u64) as *const u32)
}

// ---------------------------------------------------------------------------
// Soft reset
// ---------------------------------------------------------------------------

/// Issue SDHCI `SW_RST_ALL`: write the bit to SOFTWARE_RESET, then
/// poll until the controller clears it. Spec § 2.2.16: hardware
/// guarantees the bit clears within ~100 ms once the reset completes.
/// Returns Err if the bit hasn't cleared after a generous bounded
/// spin — better to surface the hang than wedge forever.
unsafe fn soft_reset_all(base: u64) -> Result<(), ()> {
    sdhci_write8(base, SDHCI_SOFTWARE_RESET, SW_RST_ALL);
    // 1_000_000 iterations of spin_loop is comfortably more than the
    // ~100 ms the spec promises, even at the lowest plausible clock.
    for _ in 0..1_000_000 {
        if sdhci_read8(base, SDHCI_SOFTWARE_RESET) & SW_RST_ALL == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("emmc2: starting\n");

    // Allocate our Reply object — we drive sys_call against
    // device-manager (claim + clock acquire) but never receive on a
    // server endpoint (M1 is a one-shot probe, no server loop).
    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("emmc2: create reply FAILED\n"); halt(); }
    };

    puts("emmc2: bootstrapping...\n");
    let reply = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("emmc2: bootstrap FAILED\n"); halt(); }
    };
    // Reply layout: [devmgr_client, _, _, _]. The driver's only
    // server peer is device-manager (CMD_CLAIM_DEVICE for the SDHCI
    // MMIO and CMD_GET_CLOCK_HANDLE for the clock binding).
    let devmgr_client = EndpointHandle(reply[0]);
    puts("emmc2: bootstrapped\n");

    // Claim the BCM2711 emmc2 device. Reply layout (per
    // CMD_CLAIM_DEVICE in lockjaw_types::device):
    //   [status, mmio_handle, intid, packed_clock_ref]
    // On QEMU virt the device is absent → CLAIM_ERR; we exit cleanly
    // so the integration test can assert the graceful-fail path
    // without us hanging.
    let claim = match sys_call_ret4(
        devmgr_client, reply_obj, CMD_CLAIM_DEVICE, BCM2711_EMMC2_HASH, 0, 0,
    ) {
        Ok(r) => r,
        Err(_) => { puts("emmc2: claim call FAILED\n"); halt(); }
    };
    if claim[0] != CLAIM_OK {
        puts("[EMMC2:INIT] no bcm2711-emmc2 device on this platform (QEMU); exiting\n");
        sys_exit();
    }
    let mmio_pageset = PageSetHandle(claim[1]);
    let packed_clock_ref = claim[3];

    // The DTB binding for bcm2711-emmc2 includes a clocks reference;
    // M0a's parser populated it and the device-manager packed it into
    // the claim reply. If it's absent the driver can't proceed
    // safely (the controller's base clock is whatever VC firmware
    // last set, which may be wrong). Surface and exit rather than
    // operate on a clock we don't own.
    let (controller_phandle, clock_id) = match unpack_clock_ref(packed_clock_ref) {
        Some(pair) => pair,
        None => {
            puts("emmc2: bcm2711-emmc2 DTB node has no clocks property — refusing to proceed\n");
            sys_exit();
        }
    };

    // Acquire the clock handle through device-manager (M0c proxy).
    // M1 only proves the binding is reachable end-to-end; M2 will
    // call set_rate / enable to drive the controller. The returned
    // ClockClient is held in scope so the binding survives until we
    // exit (drop closes the underlying Endpoint per RAII).
    let _clk = match ClockClient::acquire(
        devmgr_client, reply_obj, controller_phandle, clock_id,
    ) {
        Ok(c) => c,
        Err(e) => {
            puts("emmc2: clock acquire FAILED: ");
            put_clock_error(e);
            puts("\n");
            sys_exit();
        }
    };

    // Map the SDHCI register page. The DTB declares the region as
    // 0x100 bytes (one 4 KB page is plenty); the device-manager
    // claim path returns a single-page PageSet. MAP_FLAG_DEVICE
    // selects the Device-nGnRnE MAIR slot so loads/stores aren't
    // reordered or merged by the CPU.
    let mmio_va = match VMEM.alloc(1) {
        Some(va) => va,
        None => { puts("emmc2: VA exhausted for MMIO\n"); halt(); }
    };
    if !sys_map_pages(mmio_pageset, mmio_va, MAP_FLAG_DEVICE).is_ok() {
        puts("emmc2: map MMIO FAILED\n");
        halt();
    }

    // Soft-reset the controller. SW_RST_ALL puts every internal block
    // back to the post-power-on state; required before any further
    // configuration touches CLOCK_CONTROL or POWER_CONTROL.
    if unsafe { soft_reset_all(mmio_va) }.is_err() {
        puts("emmc2: SW_RST_ALL did not clear within timeout\n");
        halt();
    }

    // Read CAPABILITIES (low 32 bits at 0x040) and CAPABILITIES_HI
    // (high 32 bits at 0x044). Decoded view lives in lockjaw-types
    // so the bit layout has host tests; the driver just dispatches
    // the two volatile reads.
    let caps_lo = unsafe { sdhci_read32(mmio_va, SDHCI_CAPABILITIES) };
    let caps_hi = unsafe { sdhci_read32(mmio_va, SDHCI_CAPABILITIES_HI) };
    let caps = Capabilities::decode(caps_lo, caps_hi);

    // HOST_VERSION (0x0fe) is a u16: bits[7:0] = spec version
    // (0=v1, 1=v2, 2=v3), bits[15:8] = vendor version. SDHCI_SPEC_300
    // is the constant 2. This is distinct from bit 28 of CAPABILITIES
    // (64-bit addressing support, a per-capability flag, not the spec
    // revision number).
    let host_version = unsafe { sdhci_read16(mmio_va, SDHCI_HOST_VERSION) };
    let spec_version = (host_version & 0xFF) as u8;

    // Success line per the M1 plan.
    puts("[EMMC2:INIT] caps=");
    put_hex(caps.bits);
    puts(" base_clk=");
    put_decimal(caps.base_clock_mhz as u64);
    puts("MHz adma2=");
    put_decimal(caps.adma2_supported as u64);
    puts(" v3=");
    put_decimal((spec_version == SDHCI_SPEC_300) as u64);
    puts(" clk_handle=ok\n");

    sys_exit();
}

// ---------------------------------------------------------------------------
// Diagnostics helpers
// ---------------------------------------------------------------------------

fn put_clock_error(e: ClockError) {
    match e {
        ClockError::NotSupported(id) => { puts("NotSupported("); put_decimal(id as u64); puts(")"); }
        ClockError::OutOfRange       => puts("OutOfRange"),
        ClockError::Hardware         => puts("Hardware"),
        ClockError::BadOp            => puts("BadOp"),
        ClockError::NoProvider       => puts("NoProvider"),
        ClockError::TableFull        => puts("TableFull"),
        ClockError::InvalidHandle    => puts("InvalidHandle"),
        ClockError::IpcFailed        => puts("IpcFailed"),
    }
}

fn halt() -> ! {
    loop { unsafe { asm!("wfi"); } }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("emmc2: PANIC\n");
    halt();
}
