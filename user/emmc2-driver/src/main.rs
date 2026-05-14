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
use lockjaw_userlib::time::{cntfreq_hz, monotonic_now, sleep_for, Nanos};
// `monotonic_now` returns `MonoTicks`, which is `Ord`; the comparisons in the
// poll helpers don't need the type imported by name. `sleep_for` is used only
// for pure-time waits (regulator settle, post-clock idle) — never inside
// status polls, which busy-wait with `core::hint::spin_loop()` between MMIO
// reads to avoid quantizing 200µs hardware events to a 10ms scheduler tick.
use lockjaw_types::device::{
    BCM2711_EMMC2_HASH, CMD_CLAIM_DEVICE, CLAIM_OK, unpack_clock_ref,
};
use lockjaw_types::sdhci::{
    Capabilities,
    SDHCI_SOFTWARE_RESET, SW_RST_ALL,
    SDHCI_CAPABILITIES, SDHCI_CAPABILITIES_HI,
    SDHCI_HOST_VERSION, SDHCI_SPEC_300,
    SDHCI_CLOCK_CONTROL, SDHCI_CLOCK_INT_EN, SDHCI_CLOCK_INT_STABLE, SDHCI_CLOCK_CARD_EN,
    SDHCI_POWER_CONTROL, SDHCI_POWER_ON, SDHCI_POWER_330,
    SDHCI_TIMEOUT_CONTROL,
    SDHCI_PRESENT_STATE, SDHCI_CMD_INHIBIT,
    SDHCI_ARGUMENT, SDHCI_COMMAND,
    SDHCI_NORMAL_INT_STATUS, SDHCI_ERROR_INT_STATUS,
    SDHCI_INT_CMD_COMPLETE, SDHCI_INT_ERROR,
    SDHCI_RESPONSE_0,
    SDHCI_CMD_CRC, SDHCI_CMD_INDEX, SDHCI_CMD_RESP_NONE, SDHCI_CMD_RESP_SHORT,
    SdCommand, compute_clock_divisor, sd_command_word,
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

/// Read a 16-bit SDHCI register at `base + offset`. Offset must be
/// 2-byte aligned. Used for CLOCK_CONTROL, NORMAL_INT_STATUS, etc.
unsafe fn sdhci_read16(base: u64, offset: usize) -> u16 {
    ptr::read_volatile((base + offset as u64) as *const u16)
}

/// Write a 16-bit SDHCI register at `base + offset`. CLOCK_CONTROL
/// (0x02c), COMMAND (0x00e), NORMAL_INT_STATUS (0x030) are u16 writes.
unsafe fn sdhci_write16(base: u64, offset: usize, value: u16) {
    ptr::write_volatile((base + offset as u64) as *mut u16, value);
}

/// Read a 32-bit SDHCI register at `base + offset`. Caller is
/// responsible for 4-byte alignment.
unsafe fn sdhci_read32(base: u64, offset: usize) -> u32 {
    ptr::read_volatile((base + offset as u64) as *const u32)
}

/// Write a 32-bit SDHCI register at `base + offset`. ARGUMENT (0x008)
/// is the primary u32 write in M2; offset must be 4-byte aligned.
unsafe fn sdhci_write32(base: u64, offset: usize, value: u32) {
    ptr::write_volatile((base + offset as u64) as *mut u32, value);
}

// ---------------------------------------------------------------------------
// Status-bit polls (deadline-bounded, busy)
// ---------------------------------------------------------------------------
//
// Pattern: check the register, return immediately if the condition
// is satisfied; otherwise `core::hint::spin_loop()` between MMIO
// reads. The loop is bounded by an absolute monotonic deadline
// computed from `timeout` so termination doesn't depend on CPU
// clock or codegen — that's the bug-class the new sleep primitive
// fixed (iteration-count-as-time), and a real-time deadline
// preserves the fix without yielding.
//
// We deliberately do *not* `sleep_for` between checks here, even
// though the sleep primitive exists. The hardware events these
// polls watch fire on the order of microseconds (CMD_INHIBIT,
// CMD_COMPLETE) to a few milliseconds (SW_RST_ALL,
// INT_CLK_STABLE). Yielding for a tick (~10 ms) between every MMIO
// read would push a typical 200 µs CMD_COMPLETE into the 10 ms
// regime and turn a "100 ms timeout" into a "~110 ms in the worst
// case, with the driver descheduled the whole time" path. The two
// rules from the sleep-plan principle are different shapes:
//   - pure-time wait (regulator settle, 74 SD-clock idle):
//     `sleep_for` — yield to the scheduler.
//   - hardware-event poll: deadline + `spin_loop`.
// The polls return Err(()) on timeout; the caller logs the failure.

/// Read an 8-bit status register and busy-poll until `mask` clears,
/// or `timeout` elapses. Used by SOFTWARE_RESET (SW_RST_ALL).
unsafe fn poll_until_clear_8(base: u64, offset: usize, mask: u8, timeout: Nanos) -> Result<(), ()> {
    let freq = cntfreq_hz();
    let deadline = monotonic_now().deadline_in(timeout, freq);
    loop {
        if sdhci_read8(base, offset) & mask == 0 {
            return Ok(());
        }
        if monotonic_now() >= deadline {
            return Err(());
        }
        core::hint::spin_loop();
    }
}

/// Read a 16-bit status register and busy-poll until `mask` becomes
/// set, or `timeout` elapses. Used by CLOCK_CONTROL.INT_STABLE.
unsafe fn poll_until_set_16(base: u64, offset: usize, mask: u16, timeout: Nanos) -> Result<(), ()> {
    let freq = cntfreq_hz();
    let deadline = monotonic_now().deadline_in(timeout, freq);
    loop {
        if sdhci_read16(base, offset) & mask != 0 {
            return Ok(());
        }
        if monotonic_now() >= deadline {
            return Err(());
        }
        core::hint::spin_loop();
    }
}

/// Read a 32-bit status register and busy-poll until `mask` clears,
/// or `timeout` elapses. Used by PRESENT_STATE.CMD_INHIBIT.
unsafe fn poll_until_clear_32(base: u64, offset: usize, mask: u32, timeout: Nanos) -> Result<(), ()> {
    let freq = cntfreq_hz();
    let deadline = monotonic_now().deadline_in(timeout, freq);
    loop {
        if sdhci_read32(base, offset) & mask == 0 {
            return Ok(());
        }
        if monotonic_now() >= deadline {
            return Err(());
        }
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Soft reset
// ---------------------------------------------------------------------------

/// Issue SDHCI `SW_RST_ALL`: write the bit to SOFTWARE_RESET, then
/// poll until the controller clears it. Spec § 2.2.16: hardware
/// guarantees the bit clears within ~100 ms once the reset completes.
/// Returns Err if the bit hasn't cleared by the deadline.
///
/// Deadline-bounded busy poll (see `poll_until_clear_8` and the
/// status-bit-poll comment block above for why we busy-spin between
/// MMIO reads instead of yielding via `sleep_for`). The earlier
/// 1_000_000-iteration spin tied correctness to CPU clock and
/// codegen; with a real-time deadline the timeout is what it says
/// regardless of platform.
unsafe fn soft_reset_all(base: u64) -> Result<(), ()> {
    sdhci_write8(base, SDHCI_SOFTWARE_RESET, SW_RST_ALL);
    poll_until_clear_8(base, SDHCI_SOFTWARE_RESET, SW_RST_ALL, Nanos::from_millis(200))
}

// ---------------------------------------------------------------------------
// ID-mode clock setup
// ---------------------------------------------------------------------------

/// Configure the SDHCI internal clock for SD identification mode (≤ 400 kHz).
///
/// Sequence per SDHCI spec § 3.2.3:
///   1. Write divisor fields + INT_CLK_EN to CLOCK_CONTROL.
///   2. Poll INT_CLK_STABLE.
///   3. Write SD_CLK_EN to gate the clock through to the card.
///
/// `base_hz` is the SDHCI base clock (the CPRMAN-provided rate the
/// controller uses as its reference). Returns Err if the clock doesn't
/// stabilise within a generous spin limit.
unsafe fn configure_clock_id_mode(base: u64, base_hz: u64) -> Result<(), ()> {
    let (lo, hi) = compute_clock_divisor(base_hz, 400_000);
    // Write divisor + internal clock enable in one shot.
    let ctrl = (lo as u16) << 8 | (hi as u16) << 6 | SDHCI_CLOCK_INT_EN;
    sdhci_write16(base, SDHCI_CLOCK_CONTROL, ctrl);
    // SDHCI spec gives a typical lock time well under 1 ms; 100 ms
    // is generous enough that a misconfigured base clock surfaces
    // as a clean error rather than a hang.
    poll_until_set_16(base, SDHCI_CLOCK_CONTROL, SDHCI_CLOCK_INT_STABLE, Nanos::from_millis(100))?;
    // Stable: enable the clock output to the card slot.
    let ctrl_en = sdhci_read16(base, SDHCI_CLOCK_CONTROL) | SDHCI_CLOCK_CARD_EN;
    sdhci_write16(base, SDHCI_CLOCK_CONTROL, ctrl_en);
    Ok(())
}

// ---------------------------------------------------------------------------
// Command issue
// ---------------------------------------------------------------------------

/// Issue one SD command and return `RESPONSE_0` (the low 32 bits of the
/// response register).
///
/// Sequence per SDHCI spec § 3.7:
///   1. Poll PRESENT_STATE.CMD_INHIBIT until clear.
///   2. Write ARGUMENT.
///   3. Write COMMAND (triggers the transfer on the bus).
///   4. Poll NORMAL_INT_STATUS for CMD_COMPLETE or ERROR.
///   5. Clear CMD_COMPLETE (write-1-to-clear).
///   6. Return RESPONSE_0.
///
/// Returns `Err(())` on timeout, CMD_INHIBIT stuck, or controller error.
unsafe fn issue_command(base: u64, arg: u32, cmd_word: u16) -> Result<u32, ()> {
    // Wait for CMD line to be free. Should clear within microseconds
    // on a healthy controller; 100 ms is generous bound.
    poll_until_clear_32(base, SDHCI_PRESENT_STATE, SDHCI_CMD_INHIBIT, Nanos::from_millis(100))?;
    sdhci_write32(base, SDHCI_ARGUMENT, arg);
    // Writing COMMAND triggers the command on the bus.
    sdhci_write16(base, SDHCI_COMMAND, cmd_word);
    // Poll NORMAL_INT_STATUS for CMD_COMPLETE or ERROR. Manual loop
    // (not the generic helper) because we need to distinguish three
    // outcomes (success / error / timeout) and clear different
    // status bits on each. Busy-poll with `spin_loop` between
    // checks: a command at 400 kHz responds in ~120 µs (R7) and
    // even DAT-line transfers complete in milliseconds — yielding a
    // whole scheduler tick between checks would dominate that
    // budget. Deadline keeps the loop bounded regardless of CPU.
    let freq = cntfreq_hz();
    let deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    loop {
        let status = sdhci_read16(base, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            // Clear underlying error bits first (w1c), then the summary.
            // Leaving ERROR_INT_STATUS set causes the controller to re-assert
            // the NORMAL_INT_STATUS ERROR summary bit on the next command.
            sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            return Err(());
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE);
            return Ok(sdhci_read32(base, SDHCI_RESPONSE_0));
        }
        if monotonic_now() >= deadline {
            return Err(()); // timeout
        }
        core::hint::spin_loop();
    }
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
    // M2 calls set_rate / enable to drive the SDHCI base clock. The
    // ClockClient is held in scope; drop closes the Endpoint per RAII.
    let clk = match ClockClient::acquire(
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

    // M1 success line.
    puts("[EMMC2:INIT] caps=");
    put_hex(caps.bits);
    puts(" base_clk=");
    put_decimal(caps.base_clock_mhz as u64);
    puts("MHz adma2=");
    put_decimal(caps.adma2_supported as u64);
    puts(" v3=");
    put_decimal((spec_version == SDHCI_SPEC_300) as u64);
    puts(" clk_handle=ok\n");

    // -----------------------------------------------------------------------
    // M2 — clock programming, bus power, CMD0 + CMD8
    // -----------------------------------------------------------------------

    // Honor the CAPABILITIES contract: per `lockjaw_types::sdhci`,
    // `base_clock_mhz == 0` means "controller did not advertise its
    // base clock — driver must source the value elsewhere." Until
    // M2 follow-up adds that fallback (DTB property or CPRMAN-as-
    // source-of-truth — see Pi log discrepancy between CAPABILITIES
    // 100 MHz and CPRMAN 200 MHz), the only safe behavior is to
    // refuse to proceed rather than program the clock to 0 Hz and
    // derive a divide-by-zero divisor.
    if caps.base_clock_mhz == 0 {
        puts("emmc2: CAPABILITIES.base_clock_mhz == 0 — controller advertises no base clock\n");
        puts("emmc2: M2 fallback (DTB-sourced base clock) not yet implemented; refusing to proceed\n");
        sys_exit();
    }

    // Program CPRMAN to produce the SDHCI base clock. The CAPABILITIES
    // register reports what the controller expects; ask CPRMAN to match it.
    let base_hz = caps.base_clock_mhz as u64 * 1_000_000;
    match clk.set_rate(base_hz) {
        Ok(actual) => {
            puts("emmc2: CPRMAN set_rate=");
            put_decimal(actual / 1_000_000);
            puts("MHz\n");
        }
        Err(e) => {
            puts("emmc2: set_rate FAILED: ");
            put_clock_error(e);
            puts("\n");
            halt();
        }
    }
    if let Err(e) = clk.enable() {
        puts("emmc2: clk enable FAILED: ");
        put_clock_error(e);
        puts("\n");
        halt();
    }

    // Power on the SD bus at 3.3 V before enabling the SD clock.
    // Set max data timeout (TMCLK × 2^27 = 0x0E) while we're here.
    unsafe {
        sdhci_write8(mmio_va, SDHCI_POWER_CONTROL, SDHCI_POWER_330 | SDHCI_POWER_ON);
        sdhci_write8(mmio_va, SDHCI_TIMEOUT_CONTROL, 0x0E);
    }

    // Wait for card power to stabilise before enabling the SD clock.
    // Pi 4B has a fixed 3.3 V rail; ~1 ms is enough for the regulator
    // to settle. Tick-quantized — actual wait is ≥ one scheduler
    // tick (~10 ms), which trivially satisfies the spec minimum.
    let _ = sleep_for(Nanos::from_millis(2));

    // Enable the SDHCI internal clock at ID-mode rate (≤ 400 kHz), wait
    // for the oscillator to stabilise, then gate the clock to the card.
    if unsafe { configure_clock_id_mode(mmio_va, base_hz) }.is_err() {
        puts("emmc2: SDHCI INT_CLK_STABLE not set within timeout\n");
        halt();
    }

    // SD spec § 6.4.1.1: ≥ 74 SD clock cycles (185 µs at 400 kHz) must
    // elapse after SD_CLK_EN before the host issues CMD0. We document
    // the spec requirement here as 200 µs; tick-quantized sleep makes
    // the actual wait one scheduler tick (~10 ms), which trivially
    // satisfies the "≥ 185 µs" minimum.
    let _ = sleep_for(Nanos::from_micros(200));

    // CMD0 — GO_IDLE_STATE. No response. Resets all cards on the bus to
    // idle state. We issue and ignore the result: no response means no
    // success indicator, and it's safe for the card to miss CMD0.
    let cmd0 = sd_command_word(SdCommand::GoIdleState.index(), SDHCI_CMD_RESP_NONE);
    let _ = unsafe { issue_command(mmio_va, 0, cmd0) };

    // CMD8 — SEND_IF_COND. Arg: VHS=1 (2.7–3.6 V) + check pattern 0xAA.
    // R7 response echoes VHS and the check pattern back. A correct echo
    // proves the card is SD Physical Layer Spec v2.0+ (SDv2+). Pre-SDv2
    // cards don't respond to CMD8; UHS and SDXC cards need it for ACMD41.
    let cmd8_flags = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX;
    let cmd8 = sd_command_word(SdCommand::SendIfCond.index(), cmd8_flags);
    let resp = match unsafe { issue_command(mmio_va, 0x0000_01AA, cmd8) } {
        Ok(r) => r,
        Err(()) => {
            puts("emmc2: CMD8 FAILED — no card or card not SDv2+\n");
            sys_exit();
        }
    };

    // CMD8 R7: bits[11:8] = voltage accepted (echoes VHS=1), bits[7:0]
    // = check pattern (echoes 0xAA). Together bits[11:0] = 0x1AA.
    if resp & 0xFFF != 0x1AA {
        puts("emmc2: CMD8 bad echo=0x");
        put_hex((resp & 0xFFF) as u64);
        puts("\n");
        sys_exit();
    }

    puts("[EMMC2:IDPHASE] CMD8 echo=0x1AA — card is SDv2+ (clk via cprman)\n");
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
