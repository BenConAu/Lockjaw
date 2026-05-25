#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use core::ptr;
use lockjaw_userlib::*;
use lockjaw_userlib::clock::{ClockClient, ClockError};
use lockjaw_userlib::block::{
    BlockEngine, BlockError, BlockInfo, run_block_server,
};
use lockjaw_userlib::handle::PageSetGuard;
use lockjaw_userlib::dma_sync::{sys_dma_sync_for_cpu, sys_dma_sync_for_device};
use lockjaw_types::addr::PAGE_SIZE;
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
    Capabilities, OcrRegister, r6_rca, CsdV2,
    CMD8_IF_COND_ARG, ACMD41_ARG_HCS,
    SDHCI_SOFTWARE_RESET, SW_RST_ALL,
    SDHCI_CAPABILITIES, SDHCI_CAPABILITIES_HI,
    SDHCI_HOST_VERSION, SDHCI_SPEC_300,
    SDHCI_CLOCK_CONTROL, SDHCI_CLOCK_INT_EN, SDHCI_CLOCK_INT_STABLE, SDHCI_CLOCK_CARD_EN,
    SDHCI_POWER_CONTROL, SDHCI_POWER_ON, SDHCI_POWER_330,
    SDHCI_TIMEOUT_CONTROL,
    SDHCI_PRESENT_STATE, SDHCI_CMD_INHIBIT, SDHCI_DAT_INHIBIT,
    SDHCI_ARGUMENT, SDHCI_ARGUMENT2, SDHCI_TRANSFER_MODE, SDHCI_COMMAND,
    SDHCI_BLOCK_SIZE, SDHCI_BLOCK_COUNT,
    SDHCI_NORMAL_INT_STATUS, SDHCI_ERROR_INT_STATUS,
    SDHCI_NORMAL_INT_STATUS_ENABLE, SDHCI_ERROR_INT_STATUS_ENABLE,
    SDHCI_INT_CMD_COMPLETE, SDHCI_INT_DATA_COMPLETE,
    SDHCI_INT_ERROR,
    SDHCI_INT_CMD_TIMEOUT, SDHCI_INT_CMD_CRC,
    SDHCI_INT_CMD_END_BIT, SDHCI_INT_CMD_INDEX,
    SDHCI_INT_DATA_TIMEOUT, SDHCI_INT_DATA_CRC, SDHCI_INT_DATA_END_BIT,
    SDHCI_RESPONSE_0, SDHCI_RESPONSE_1, SDHCI_RESPONSE_2, SDHCI_RESPONSE_3,
    SDHCI_HOST_CONTROL, SDHCI_HOST_CTRL_DAT_4BIT,
    SDHCI_HOST_CTRL_DMA_SEL_MASK, SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32,
    SDHCI_ADMA_ADDRESS,
    SDHCI_CMD_CRC, SDHCI_CMD_INDEX, SDHCI_CMD_DATA,
    SDHCI_CMD_RESP_NONE, SDHCI_CMD_RESP_SHORT, SDHCI_CMD_RESP_LONG, SDHCI_CMD_RESP_SHORT_BUSY,
    SDHCI_TRNS_READ, SDHCI_TRNS_DMA, SDHCI_TRNS_BLK_CNT_EN, SDHCI_TRNS_MULTI, SDHCI_TRNS_AUTO_CMD23,
    SdCommand, compute_clock_divisor, sd_command_word,
    adma2_tran_end_descriptor,
};

// ---------------------------------------------------------------------------
// MMIO helpers
// ---------------------------------------------------------------------------
//
// SDHCI assigns specific access widths per register; mismatched widths
// can fault on real silicon. SOFTWARE_RESET (0x02f) is a single byte;
// CAPABILITIES / CAPABILITIES_HI (0x040 / 0x044) are 32-bit reads.

/// Read an 8-bit SDHCI register at `base + offset`.
unsafe fn sdhci_read8(base: u64, offset: u64) -> u8 {
    ptr::read_volatile((base + offset) as *const u8)
}

/// Write an 8-bit SDHCI register at `base + offset`.
unsafe fn sdhci_write8(base: u64, offset: u64, value: u8) {
    ptr::write_volatile((base + offset) as *mut u8, value);
}

/// Read a 16-bit SDHCI register at `base + offset`. Offset must be
/// 2-byte aligned. Used for CLOCK_CONTROL, NORMAL_INT_STATUS, etc.
unsafe fn sdhci_read16(base: u64, offset: u64) -> u16 {
    ptr::read_volatile((base + offset) as *const u16)
}

/// Write a 16-bit SDHCI register at `base + offset`. CLOCK_CONTROL
/// (0x02c), COMMAND (0x00e), NORMAL_INT_STATUS (0x030) are u16 writes.
unsafe fn sdhci_write16(base: u64, offset: u64, value: u16) {
    ptr::write_volatile((base + offset) as *mut u16, value);
}

/// Read a 32-bit SDHCI register at `base + offset`. Caller is
/// responsible for 4-byte alignment.
unsafe fn sdhci_read32(base: u64, offset: u64) -> u32 {
    ptr::read_volatile((base + offset) as *const u32)
}

/// Write a 32-bit SDHCI register at `base + offset`. ARGUMENT (0x008)
/// is the primary u32 write in M2; offset must be 4-byte aligned.
unsafe fn sdhci_write32(base: u64, offset: u64, value: u32) {
    ptr::write_volatile((base + offset) as *mut u32, value);
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
unsafe fn poll_until_clear_8(base: u64, offset: u64, mask: u8, timeout: Nanos) -> Result<(), ()> {
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
unsafe fn poll_until_set_16(base: u64, offset: u64, mask: u16, timeout: Nanos) -> Result<(), ()> {
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
unsafe fn poll_until_clear_32(base: u64, offset: u64, mask: u32, timeout: Nanos) -> Result<(), ()> {
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

/// Configure the SDHCI SD clock to `target_hz` using `base_hz` as the
/// reference (the CPRMAN-provided rate the controller uses).
///
/// Sequence per SDHCI spec § 3.2.3 / § 3.2.4:
///   1. Disable SD_CLK_EN (CLOCK_CARD_EN) before changing the divisor.
///   2. Write new divisor + INT_CLK_EN to CLOCK_CONTROL.
///   3. Poll INT_CLK_STABLE (typ. < 1 ms).
///   4. Re-enable SD_CLK_EN.
///
/// Used for both ID-mode (400 kHz) and data-transfer mode (25 MHz).
unsafe fn configure_clock(base: u64, base_hz: u64, target_hz: u64) -> Result<(), ()> {
    // Gate off the clock output before touching the divisor.
    let cur = sdhci_read16(base, SDHCI_CLOCK_CONTROL);
    sdhci_write16(base, SDHCI_CLOCK_CONTROL, cur & !SDHCI_CLOCK_CARD_EN);
    // Write divisor + internal clock enable in one shot.
    let (lo, hi) = compute_clock_divisor(base_hz, target_hz);
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

/// Outcomes from `issue_command`. Each failure shape is its own
/// variant so the caller can emit a precise diagnostic instead of
/// a generic timeout. `ControllerError` carries the raw
/// `ERROR_INT_STATUS` bits captured before clearing.
#[derive(Clone, Copy)]
enum CmdResult {
    Ok(u32),
    /// CMD_INHIBIT didn't clear before the deadline. No bus transaction issued.
    InhibitStuck,
    /// NORMAL_INT_STATUS.ERROR fired; bits == ERROR_INT_STATUS at the moment of detection.
    ControllerError(u16),
    /// Neither CMD_COMPLETE nor ERROR fired before the 1s deadline.
    NoResponse,
}

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
unsafe fn issue_command(base: u64, arg: u32, cmd_word: u16) -> CmdResult {
    // Wait for CMD line to be free. Should clear within microseconds
    // on a healthy controller; 100 ms is generous bound.
    if poll_until_clear_32(base, SDHCI_PRESENT_STATE, SDHCI_CMD_INHIBIT, Nanos::from_millis(100)).is_err() {
        return CmdResult::InhibitStuck;
    }
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
            // Capture which error bit fired *before* clearing, so the
            // caller can decode the cause. Then clear underlying bits
            // (w1c) followed by the summary bit — leaving
            // ERROR_INT_STATUS set causes the controller to re-assert
            // the NORMAL_INT_STATUS ERROR summary on the next command.
            let err_bits = sdhci_read16(base, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            return CmdResult::ControllerError(err_bits);
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE);
            return CmdResult::Ok(sdhci_read32(base, SDHCI_RESPONSE_0));
        }
        if monotonic_now() >= deadline {
            return CmdResult::NoResponse;
        }
        core::hint::spin_loop();
    }
}


/// Pretty-print ERROR_INT_STATUS bits to the kernel UART. Names match
/// the SDHCI spec § 2.2.18 / Linux's headers so the output line can
/// be grep'd against a reference. Covers both command-phase bits (0–3)
/// and data-phase bits (4–6) since the same register reports both.
fn put_error_int_status(bits: u16) {
    puts("ERROR_INT_STATUS=");
    put_hex(bits as u64);
    if bits & SDHCI_INT_CMD_TIMEOUT  != 0 { puts(" CMD_TIMEOUT"); }
    if bits & SDHCI_INT_CMD_CRC      != 0 { puts(" CMD_CRC"); }
    if bits & SDHCI_INT_CMD_END_BIT  != 0 { puts(" CMD_END_BIT"); }
    if bits & SDHCI_INT_CMD_INDEX    != 0 { puts(" CMD_INDEX"); }
    if bits & SDHCI_INT_DATA_TIMEOUT != 0 { puts(" DATA_TIMEOUT"); }
    if bits & SDHCI_INT_DATA_CRC     != 0 { puts(" DATA_CRC"); }
    if bits & SDHCI_INT_DATA_END_BIT != 0 { puts(" DATA_END_BIT"); }
}

// ---------------------------------------------------------------------------
// Emmc2Error — typed failure enum for ADMA transfers and engine init.
//
// Replaces the old `Result<_, &'static str>` returns: the inner code
// constructs a variant (possibly carrying SDHCI status registers, bad
// physical addresses, etc.) and returns it. The caller decides what to
// log via `put_emmc2_error`. No inner code calls `puts` for error
// formatting — that's the caller's job, so errors that propagate
// silently get one canonical line at the top of the chain.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Emmc2Error {
    /// Buffer physical address exceeds the 32-bit ADMA2-32 limit.
    BufferPhysAbove4Gib(u64),
    /// Descriptor table physical address exceeds the 32-bit ADMA2-32 limit.
    DescPhysAbove4Gib(u64),
    /// Descriptor segment length exceeds 65535-byte single-descriptor cap.
    DescriptorLengthOverflow(u32),
    /// CMD or DAT inhibit didn't clear within the 100ms pre-poll deadline.
    InhibitStuck { present_state: u32 },
    /// Controller-reported error during command phase (CMD17/CMD18/CMD25).
    /// Status word from SDHCI_ERROR_INT_STATUS is attached for decoding.
    CmdError { err_int_status: u16 },
    /// Controller-reported error during data phase.
    DataError { err_int_status: u16 },
    /// CMD_COMPLETE never arrived within the 1-second deadline.
    CmdCompleteTimeout,
    /// TRANSFER_COMPLETE never arrived within the 100ms deadline.
    TransferCompleteTimeout,
    /// sys_alloc_dma_pages for the descriptor table failed.
    DescAllocFailed,
    /// sys_query_pageset_phys for the descriptor table failed.
    DescPhysQueryFailed,
    /// VMEM.alloc(1) for the descriptor table VA returned None.
    DescVaExhausted,
    /// sys_map_pages for the descriptor table failed.
    DescMapFailed,
    /// sys_dma_sync_for_device on the descriptor table failed
    /// (per-transfer, after writing the descriptor and before
    /// kicking the controller).
    DescSyncFailed,
}

impl Emmc2Error {
    /// Short stable identifier — same string for every instance of a
    /// variant, ignoring attached data. Useful when you just want a
    /// label without the per-instance details.
    fn as_str(&self) -> &'static str {
        match self {
            Self::BufferPhysAbove4Gib(_)       => "buffer phys > 4GiB",
            Self::DescPhysAbove4Gib(_)         => "desc phys > 4GiB",
            Self::DescriptorLengthOverflow(_)  => "descriptor length > 65535",
            Self::InhibitStuck { .. }          => "inhibit stuck",
            Self::CmdError { .. }              => "cmd phase error",
            Self::DataError { .. }             => "data phase error",
            Self::CmdCompleteTimeout           => "cmd_complete timeout",
            Self::TransferCompleteTimeout      => "transfer_complete timeout",
            Self::DescAllocFailed              => "desc alloc failed",
            Self::DescPhysQueryFailed          => "desc phys query failed",
            Self::DescVaExhausted              => "desc VA exhausted",
            Self::DescMapFailed                => "desc map failed",
            Self::DescSyncFailed               => "desc sync_for_device failed",
        }
    }
}

/// Print an Emmc2Error: the as_str() label followed by any attached
/// detail (status bits, physical addresses). Does not add a trailing
/// newline — caller decides.
fn put_emmc2_error(err: &Emmc2Error) {
    puts(err.as_str());
    match err {
        Emmc2Error::BufferPhysAbove4Gib(pa) | Emmc2Error::DescPhysAbove4Gib(pa) => {
            puts(" pa=");
            put_hex(*pa);
        }
        Emmc2Error::DescriptorLengthOverflow(len) => {
            puts(" len=");
            put_decimal(*len as u64);
        }
        Emmc2Error::InhibitStuck { present_state } => {
            puts(" present_state=");
            put_hex(*present_state as u64);
        }
        Emmc2Error::CmdError { err_int_status }
        | Emmc2Error::DataError { err_int_status } => {
            puts(" ");
            put_error_int_status(*err_int_status);
        }
        Emmc2Error::CmdCompleteTimeout
        | Emmc2Error::TransferCompleteTimeout
        | Emmc2Error::DescAllocFailed
        | Emmc2Error::DescPhysQueryFailed
        | Emmc2Error::DescVaExhausted
        | Emmc2Error::DescMapFailed
        | Emmc2Error::DescSyncFailed => {}
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
    // Reply layout: [devmgr_client, blk_srv_ep, _, _].
    //   - devmgr_client: used for CMD_CLAIM_DEVICE (SDHCI MMIO) and
    //     CMD_GET_CLOCK_HANDLE (clock binding).
    //   - blk_srv_ep: the endpoint init created for us to receive
    //     BlockEngine IPC requests on (M7). Owned by init outside
    //     this driver; init exports the same handle to other
    //     processes that need block storage on Pi 4B.
    let devmgr_client = EndpointHandle(reply[0]);
    let blk_srv_ep = EndpointHandle(reply[1]);
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
    // claim path returns a single-page PageSet. MapMemoryAttribute::Device
    // selects the Device-nGnRnE MAIR slot so loads/stores aren't
    // reordered or merged by the CPU.
    let mmio_va = match VMEM.alloc(1) {
        Some(va) => va,
        None => { puts("emmc2: VA exhausted for MMIO\n"); halt(); }
    };
    if !sys_map_pages(mmio_pageset, mmio_va, MapMemoryAttribute::Device).is_ok() {
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

    // Enable status reporting in NORMAL_INT_STATUS / ERROR_INT_STATUS.
    // SDHCI spec § 2.2.21: each bit in NORMAL_INT_STATUS_ENABLE
    // (0x034) gates whether the controller updates the corresponding
    // bit in NORMAL_INT_STATUS (0x030). After SW_RST_ALL the enable
    // registers are zero, so polling NORMAL_INT_STATUS for
    // CMD_COMPLETE / ERROR yields nothing forever — every command
    // appears to time out even when the bus runs perfectly. This
    // is the bug the M2 v1 instrumentation surfaced: SD_CLK was
    // 400 kHz, neither CMD_COMPLETE nor ERROR ever fired.
    //
    // We don't enable IRQ *signals* (NORMAL_INT_SIGNAL_ENABLE 0x038
    // / ERROR_INT_SIGNAL_ENABLE 0x03A) — the driver polls today,
    // and signal-enable controls whether the GIC line asserts.
    // M3+ will flip those bits when we wire the IRQ path.
    unsafe {
        sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS_ENABLE, 0xFFFF);
        sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS_ENABLE,  0xFFFF);
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
    let actual_base_hz = match clk.set_rate(base_hz) {
        Ok(actual) => {
            puts("emmc2: CPRMAN set_rate=");
            put_decimal(actual / 1_000_000);
            puts("MHz (requested ");
            put_decimal(base_hz / 1_000_000);
            puts("MHz)\n");
            actual
        }
        Err(e) => {
            puts("emmc2: set_rate FAILED: ");
            put_clock_error(e);
            puts("\n");
            halt();
        }
    };
    // Diagnostic: the divisor we'll program into CLOCK_CONTROL and
    // the resulting SD bus clock. SD ID-mode requires SD_CLK ≤ 400 kHz;
    // anything outside [100kHz, 400kHz] explains a CMD8 failure
    // immediately (CRC mismatch / no response).
    let (lo, hi) = compute_clock_divisor(actual_base_hz, 400_000);
    let n = ((hi as u64) << 8) | (lo as u64);
    let derived_sd_clk = if n == 0 { actual_base_hz } else { actual_base_hz / (2 * n) };
    puts("[EMMC2:CLK] base_actual=");
    put_decimal(actual_base_hz);
    puts(" divisor_N=");
    put_decimal(n);
    puts(" SD_CLK=");
    put_decimal(derived_sd_clk);
    puts("Hz\n");
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
    if unsafe { configure_clock(mmio_va, actual_base_hz, 400_000) }.is_err() {
        puts("emmc2: SDHCI INT_CLK_STABLE not set within timeout\n");
        halt();
    }

    // SD spec § 6.4.1.1: ≥ 74 SD clock cycles (185 µs at 400 kHz) must
    // elapse after SD_CLK_EN before the host issues CMD0. We document
    // the spec requirement here as 200 µs; tick-quantized sleep makes
    // the actual wait one scheduler tick (~10 ms), which trivially
    // satisfies the "≥ 185 µs" minimum.
    let _ = sleep_for(Nanos::from_micros(200));

    // CMD0 — GO_IDLE_STATE. No response (RESP_NONE). Resets all cards
    // on the bus to idle state. We log the outcome but don't bail on
    // failure: it's safe for the card to miss CMD0 if it was already
    // idle. A controller-level error here (e.g. CMD line wedged) is
    // still useful diagnostic context for what follows.
    let cmd0 = sd_command_word(SdCommand::GoIdleState.index(), SDHCI_CMD_RESP_NONE);
    match unsafe { issue_command(mmio_va, 0, cmd0) } {
        CmdResult::Ok(_) => puts("[EMMC2:IDPHASE] CMD0 acknowledged\n"),
        CmdResult::InhibitStuck => puts("[EMMC2:IDPHASE] CMD0: CMD_INHIBIT stuck (controller not responding)\n"),
        CmdResult::ControllerError(bits) => {
            puts("[EMMC2:IDPHASE] CMD0 controller error: ");
            put_error_int_status(bits);
            puts("\n");
        }
        CmdResult::NoResponse => puts("[EMMC2:IDPHASE] CMD0: no CMD_COMPLETE within 1s (suspicious)\n"),
    }

    // CMD8 — SEND_IF_COND. Arg: VHS=1 (2.7–3.6 V) + check pattern 0xAA.
    // R7 response echoes VHS and the check pattern back. A correct echo
    // proves the card is SD Physical Layer Spec v2.0+ (SDv2+). Pre-SDv2
    // cards don't respond to CMD8; UHS and SDXC cards need it for ACMD41.
    let cmd8_flags = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX;
    let cmd8 = sd_command_word(SdCommand::SendIfCond.index(), cmd8_flags);
    let resp = match unsafe { issue_command(mmio_va, CMD8_IF_COND_ARG, cmd8) } {
        CmdResult::Ok(r) => r,
        CmdResult::InhibitStuck => {
            puts("[EMMC2:IDPHASE] CMD8 FAILED: CMD_INHIBIT did not clear before deadline\n");
            sys_exit();
        }
        CmdResult::ControllerError(bits) => {
            // Decoded error bits tell us whether the bus rate, the
            // voltage, or the card itself is the problem:
            //   CMD_TIMEOUT alone → card didn't respond (pre-SDv2,
            //                       no card, or SD_CLK out of spec)
            //   CMD_CRC          → response present but bus-rate /
            //                       signal-integrity issue
            //   CMD_END_BIT      → bus framing wrong
            //   CMD_INDEX        → response received but for a
            //                       different command (controller bug?)
            puts("[EMMC2:IDPHASE] CMD8 FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        CmdResult::NoResponse => {
            puts("[EMMC2:IDPHASE] CMD8 FAILED: no CMD_COMPLETE/ERROR within 1s\n");
            sys_exit();
        }
    };

    // CMD8 R7: bits[11:8] = voltage accepted (echoes VHS=1), bits[7:0]
    // = check pattern (echoes 0xAA). Together bits[11:0] = 0x1AA.
    if resp & 0xFFF != 0x1AA {
        puts("emmc2: CMD8 bad echo=");
        put_hex((resp & 0xFFF) as u64);
        puts("\n");
        sys_exit();
    }

    puts("[EMMC2:IDPHASE] CMD8 echo=0x1AA — card is SDv2+ (clk via cprman)\n");

    // -----------------------------------------------------------------------
    // M3 — Full SD identification: ACMD41 → CMD2 → CMD3 → CMD9 →
    //       CMD7 → ACMD6 → HOST_CONTROL → 25 MHz
    // -----------------------------------------------------------------------

    // ACMD41 loop — SD spec § 4.2.3.1.  Each iteration: CMD55 (sets
    // APP_CMD mode for the next command), then ACMD41 with HCS=1.
    // The card returns OCR with power_up_done = false while still
    // initializing (busy-bit clear); retry until power_up_done = true
    // or the 1-second timeout expires.
    //
    // CMD55 in broadcast mode (arg=0, no RCA assigned yet).
    // ACMD41 uses R3 (no CRC or index check — spec §4.9.3).
    let cmd55_word = sd_command_word(
        SdCommand::AppCmd.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
    );
    let acmd41_word = sd_command_word(
        SdCommand::SdSendOpCond.index(),
        SDHCI_CMD_RESP_SHORT, // R3: no CRC, no index check
    );
    let freq = cntfreq_hz();
    let acmd41_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    let ocr = loop {
        // CMD55 — prefixes ACMD41; response is R1 from the card.
        match unsafe { issue_command(mmio_va, 0, cmd55_word) } {
            CmdResult::Ok(_) => {}
            CmdResult::InhibitStuck => {
                puts("[EMMC2:IDPHASE] CMD55 FAILED: CMD_INHIBIT stuck\n");
                sys_exit();
            }
            CmdResult::ControllerError(bits) => {
                puts("[EMMC2:IDPHASE] CMD55 FAILED: ");
                put_error_int_status(bits);
                puts("\n");
                sys_exit();
            }
            CmdResult::NoResponse => {
                puts("[EMMC2:IDPHASE] CMD55 FAILED: no response within 1s\n");
                sys_exit();
            }
        }
        // ACMD41 — the card returns OCR; busy bit clears when ready.
        let r0 = match unsafe { issue_command(mmio_va, ACMD41_ARG_HCS, acmd41_word) } {
            CmdResult::Ok(r) => r,
            CmdResult::InhibitStuck => {
                puts("[EMMC2:IDPHASE] ACMD41 FAILED: CMD_INHIBIT stuck\n");
                sys_exit();
            }
            CmdResult::ControllerError(bits) => {
                puts("[EMMC2:IDPHASE] ACMD41 FAILED: ");
                put_error_int_status(bits);
                puts("\n");
                sys_exit();
            }
            CmdResult::NoResponse => {
                puts("[EMMC2:IDPHASE] ACMD41 FAILED: no response within 1s\n");
                sys_exit();
            }
        };
        let ocr = OcrRegister::decode(r0);
        if ocr.power_up_done {
            break ocr;
        }
        if monotonic_now() >= acmd41_deadline {
            puts("[EMMC2:IDPHASE] ACMD41 timeout: card never became ready\n");
            sys_exit();
        }
        // Short delay between retries — SD spec doesn't require one but
        // gives the card breathing room between polls.
        let _ = sleep_for(Nanos::from_millis(10));
    };
    puts("[EMMC2:IDPHASE] ACMD41 ready ccs=");
    put_decimal(ocr.ccs as u64);
    puts("\n");

    // CMD2 — ALL_SEND_CID.  R2 (136-bit) response carries the card's
    // unique CID register.  We read all four RESPONSE words (the SDHCI
    // holds them until the next command) but only log, not decode, the
    // CID in M3 — capacity and addressing come from CSD (CMD9).
    let cmd2_word = sd_command_word(
        SdCommand::AllSendCid.index(),
        SDHCI_CMD_RESP_LONG | SDHCI_CMD_CRC,
    );
    match unsafe { issue_command(mmio_va, 0, cmd2_word) } {
        CmdResult::Ok(_) => {}
        CmdResult::InhibitStuck => {
            puts("[EMMC2:IDPHASE] CMD2 FAILED: CMD_INHIBIT stuck\n");
            sys_exit();
        }
        CmdResult::ControllerError(bits) => {
            puts("[EMMC2:IDPHASE] CMD2 FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        CmdResult::NoResponse => {
            puts("[EMMC2:IDPHASE] CMD2 FAILED: no response within 1s\n");
            sys_exit();
        }
    }
    puts("[EMMC2:IDPHASE] CMD2 CID received\n");

    // CMD3 — SEND_RELATIVE_ADDR.  Card publishes its RCA; the host
    // stores it and uses it to address the card from here on.
    // R6 response: bits[31:16] = RCA, bits[15:0] = card status.
    let cmd3_word = sd_command_word(
        SdCommand::SendRelativeAddr.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
    );
    let rca: u16 = match unsafe { issue_command(mmio_va, 0, cmd3_word) } {
        CmdResult::Ok(r) => r6_rca(r),
        CmdResult::InhibitStuck => {
            puts("[EMMC2:IDPHASE] CMD3 FAILED: CMD_INHIBIT stuck\n");
            sys_exit();
        }
        CmdResult::ControllerError(bits) => {
            puts("[EMMC2:IDPHASE] CMD3 FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        CmdResult::NoResponse => {
            puts("[EMMC2:IDPHASE] CMD3 FAILED: no response within 1s\n");
            sys_exit();
        }
    };
    puts("[EMMC2:IDPHASE] CMD3 rca=");
    put_hex(rca as u64);
    puts("\n");

    // CMD9 — SEND_CSD.  R2 (136-bit) response carries the CSD register.
    // CSD v2 (SDHC/SDXC) encodes capacity in the C_SIZE field; the pure
    // decoder in lockjaw-types computes capacity_bytes from the four
    // RESPONSE words.  We re-read all four registers after CMD_COMPLETE
    // because `issue_command` only returns RESPONSE_0.
    let rca_arg = (rca as u32) << 16;
    let cmd9_word = sd_command_word(
        SdCommand::SendCsd.index(),
        SDHCI_CMD_RESP_LONG | SDHCI_CMD_CRC,
    );
    let capacity_bytes: u64 = match unsafe { issue_command(mmio_va, rca_arg, cmd9_word) } {
        CmdResult::Ok(_) => {
            let resp = unsafe {[
                sdhci_read32(mmio_va, SDHCI_RESPONSE_0),
                sdhci_read32(mmio_va, SDHCI_RESPONSE_1),
                sdhci_read32(mmio_va, SDHCI_RESPONSE_2),
                sdhci_read32(mmio_va, SDHCI_RESPONSE_3),
            ]};
            match CsdV2::decode(resp) {
                Ok(csd) => csd.capacity_bytes,
                Err(e) => {
                    puts("[EMMC2:IDPHASE] CMD9 CSD_STRUCTURE=");
                    put_decimal(e.csd_structure as u64);
                    puts(" (expected 1 for SDHC/SDXC)\n");
                    sys_exit();
                }
            }
        }
        CmdResult::InhibitStuck => {
            puts("[EMMC2:IDPHASE] CMD9 FAILED: CMD_INHIBIT stuck\n");
            sys_exit();
        }
        CmdResult::ControllerError(bits) => {
            puts("[EMMC2:IDPHASE] CMD9 FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        CmdResult::NoResponse => {
            puts("[EMMC2:IDPHASE] CMD9 FAILED: no response within 1s\n");
            sys_exit();
        }
    };

    // CMD7 — SELECT_CARD.  Moves the card from Stand-by to Transfer state.
    // R1b response: CMD_COMPLETE fires immediately; controller holds
    // CMD_COMPLETE until DAT0 deasserts (card releases busy indication).
    // Wait for both CMD_INHIBIT and DAT_INHIBIT before the next command.
    let cmd7_word = sd_command_word(
        SdCommand::SelectCard.index(),
        SDHCI_CMD_RESP_SHORT_BUSY | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
    );
    match unsafe { issue_command(mmio_va, rca_arg, cmd7_word) } {
        CmdResult::Ok(_) => {}
        CmdResult::InhibitStuck => {
            puts("[EMMC2:IDPHASE] CMD7 FAILED: CMD_INHIBIT stuck\n");
            sys_exit();
        }
        CmdResult::ControllerError(bits) => {
            puts("[EMMC2:IDPHASE] CMD7 FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        CmdResult::NoResponse => {
            puts("[EMMC2:IDPHASE] CMD7 FAILED: no response within 1s\n");
            sys_exit();
        }
    }
    // Wait for DAT0 to deassert — the card signals "ready" by releasing it.
    if unsafe { poll_until_clear_32(
        mmio_va, SDHCI_PRESENT_STATE,
        SDHCI_CMD_INHIBIT | SDHCI_DAT_INHIBIT,
        Nanos::from_millis(500),
    )}.is_err() {
        puts("[EMMC2:IDPHASE] CMD7 DAT_INHIBIT did not clear (card busy timeout)\n");
        sys_exit();
    }
    puts("[EMMC2:IDPHASE] CMD7 card selected\n");

    // ACMD6 — SET_BUS_WIDTH.  Switch the card to 4-bit DAT bus.
    // Must be preceded by CMD55 addressed to the selected card (RCA).
    let cmd55_rca_word = sd_command_word(
        SdCommand::AppCmd.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
    );
    match unsafe { issue_command(mmio_va, rca_arg, cmd55_rca_word) } {
        CmdResult::Ok(_) => {}
        CmdResult::InhibitStuck => {
            puts("[EMMC2:IDPHASE] CMD55 (pre-ACMD6) FAILED: CMD_INHIBIT stuck\n");
            sys_exit();
        }
        CmdResult::ControllerError(bits) => {
            puts("[EMMC2:IDPHASE] CMD55 (pre-ACMD6) FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        CmdResult::NoResponse => {
            puts("[EMMC2:IDPHASE] CMD55 (pre-ACMD6) FAILED: no response\n");
            sys_exit();
        }
    }
    // ACMD6 argument: 0x2 = 4-bit bus (bits[1:0] = 10).
    let acmd6_word = sd_command_word(
        SdCommand::SetBusWidth.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
    );
    match unsafe { issue_command(mmio_va, 0x2, acmd6_word) } {
        CmdResult::Ok(_) => {}
        CmdResult::InhibitStuck => {
            puts("[EMMC2:IDPHASE] ACMD6 FAILED: CMD_INHIBIT stuck\n");
            sys_exit();
        }
        CmdResult::ControllerError(bits) => {
            puts("[EMMC2:IDPHASE] ACMD6 FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        CmdResult::NoResponse => {
            puts("[EMMC2:IDPHASE] ACMD6 FAILED: no response\n");
            sys_exit();
        }
    }
    // Mirror the 4-bit bus width in HOST_CONTROL_1 immediately after
    // the card acknowledges ACMD6 — the host side must match the card.
    unsafe {
        let hc = sdhci_read8(mmio_va, SDHCI_HOST_CONTROL);
        sdhci_write8(mmio_va, SDHCI_HOST_CONTROL, hc | SDHCI_HOST_CTRL_DAT_4BIT);
    }

    // Raise the SD bus clock from 400 kHz (ID mode) to 25 MHz (data
    // transfer mode).  SDHCI spec § 3.2.4: disable SD_CLK_EN first,
    // then reprogram divisor — `configure_clock` does this.
    if unsafe { configure_clock(mmio_va, actual_base_hz, 25_000_000) }.is_err() {
        puts("[EMMC2:READY] clock-to-25MHz FAILED: INT_CLK_STABLE timeout\n");
        sys_exit();
    }

    // Derive and log the actual clock we set so the Pi log is self-
    // contained for debugging.
    let (lo25, hi25) = compute_clock_divisor(actual_base_hz, 25_000_000);
    let n25 = ((hi25 as u64) << 8) | (lo25 as u64);
    let card_clk_hz = if n25 == 0 { actual_base_hz } else { actual_base_hz / (2 * n25) };

    // M3 success line.
    puts("[EMMC2:READY] rca=");
    put_hex(rca as u64);
    puts(" capacity=");
    put_decimal(capacity_bytes / (1024 * 1024 * 1024));
    puts("GiB bus=4bit card_clk=");
    put_decimal(card_clk_hz / 1_000_000);
    puts("MHz\n");

    // -----------------------------------------------------------------------
    // M7 — block-device server. Set up the persistent ADMA2 descriptor
    // table, select ADMA2-32 in HOST_CONTROL_1, build an Emmc2BlockEngine,
    // run a one-shot selftest against LBA 0, then enter `run_block_server`.
    //
    // M4 / M5 / M6 demos (single-block PIO, multi-block PIO, single-block
    // ADMA, perf sweep) used to run here at boot — that code is now
    // subsumed by the BlockEngine path. The driver-level reads/writes a
    // BlockClient performs exercise the same SDHCI paths under
    // caller-controlled LBAs.
    // -----------------------------------------------------------------------
    let capacity_sectors = capacity_bytes / 512;
    let engine = match unsafe { Emmc2BlockEngine::new(mmio_va, capacity_sectors) } {
        Ok(e) => e,
        Err(err) => {
            puts("[EMMC2:BLK] engine init FAILED: ");
            put_emmc2_error(&err);
            puts("\n");
            halt();
        }
    };
    let mut engine = engine;

    // Selftest: alloc a 1-sector buffer, read LBA 0 via the engine,
    // verify MBR signature. Mirrors virtio-blk-driver's selftest so the
    // success line matches the same shape.
    let selftest_buf = match engine.alloc_buffer(1) {
        Ok(b) => b,
        Err(_) => { puts("[EMMC2:BLK] selftest alloc FAILED\n"); halt(); }
    };
    let selftest_va = VMEM.alloc(1).expect("[EMMC2:BLK] selftest VA alloc FAILED");
    if !sys_map_pages(selftest_buf, selftest_va, MapMemoryAttribute::Normal).is_ok() {
        puts("[EMMC2:BLK] selftest map FAILED\n");
        halt();
    }
    // Diagnostic: print buf_phys (queried via the handle) and desc_phys
    // so the post-read buffer contents can be correlated with the
    // descriptor wire encoding the controller saw. Same handle was
    // queried in alloc_buffer to stash buf.pa; if these differ, that
    // would explain a silent zero-data read (DMA goes to a different
    // PA than the CPU's mapping).
    let buf_phys_check = sys_query_pageset_phys(selftest_buf, 0)
        .unwrap_or(0xDEAD_BEEF);
    puts("[EMMC2:DIAG] buf_phys=");
    put_hex(buf_phys_check);
    puts(" desc_phys=");
    put_hex(engine.desc_phys);
    puts("\n");
    // Zero the buffer to distinguish read data from any prior content.
    unsafe { ptr::write_bytes(selftest_va as *mut u8, 0, 512); }
    if engine.read(0, 1, selftest_buf).is_err() {
        puts("[EMMC2:BLK] selftest read FAILED\n");
        halt();
    }
    // MBR boot signature lives at bytes 510-511 = 0x55 0xAA (little-endian).
    let sig = unsafe {
        let p = selftest_va as *const u8;
        let lo = ptr::read_volatile(p.add(510)) as u16;
        let hi = ptr::read_volatile(p.add(511)) as u16;
        (hi << 8) | lo
    };
    if sig != 0xAA55 {
        puts("[EMMC2:BLK] selftest MBR signature=");
        put_hex(sig as u64);
        puts(" BAD (expected 0xAA55)\n");
        halt();
    }
    // Proof-token teardown: VA returns to VMEM only on successful unmap.
    if let Ok(p) = unmap_pages_tracked(selftest_buf, selftest_va, 1) {
        VMEM.free_unmapped(p);
    }
    engine.free_buffer(selftest_buf);

    puts("[BLOCKDEV] /dev/sd0 ready: 512B x ");
    put_decimal(capacity_sectors);
    puts(" blocks; selftest read OK\n");

    run_block_server(&mut engine, blk_srv_ep);
}

// ---------------------------------------------------------------------------
// ADMA2 transfer primitives. The CMD17 single-block path is the only
// one currently wired into BlockEngine reads:
//
//   * `adma2_single_block_read` — CMD17 + plain TRNS_READ|TRNS_DMA.
//     Mirrors the M6 sub-commit 2b sequence proven on Pi under the
//     perf-sweep path. No Auto-CMD23, no BLK_CNT_EN, no MULTI.
//     `Emmc2BlockEngine::read` loops this one block at a time for
//     multi-sector reads.
//   * `adma2_transfer`          — CMD18 (read) / CMD25 (write) +
//     Auto-CMD23. **Dead code, do not call.** The cold-boot CMD18 +
//     Auto-CMD23 path returned signature=0x0 on first Pi flash with
//     no preceding PIO primer to mask the issue. Until that's
//     diagnosed (see docs/tech-debt.md: "CMD18+Auto-CMD23 cold-boot
//     validation"), no production path may dispatch through it.
//     `#[allow(dead_code)]` keeps it as a reference but the compiler
//     enforces no callers.
// ---------------------------------------------------------------------------

/// Single-block ADMA2 read of `sector`. CMD17 + READ|DMA only. No
/// MULTI, no BLK_CNT_EN, no Auto-CMD23 — those belong to the
/// multi-block path. Mirrors the M6 sub-commit 2b sequence.
///
/// `desc_ps` is the DmaPool PageSet handle backing the descriptor
/// table; needed to sync the descriptor write down to DRAM via
/// `sys_dma_sync_for_device` before kicking the controller
/// (cacheable-DMA migration C1: the descriptor mapping is Normal
/// Cacheable post-migration, so a `dsb sy` alone is not enough to
/// flush the cache line).
unsafe fn adma2_single_block_read(
    mmio_va: u64,
    sector: u64,
    buf_phys: u64,
    desc_ps: PageSetHandle,
    desc_va: u64,
    desc_phys: u64,
) -> Result<(), Emmc2Error> {
    if buf_phys >= (1u64 << 32) {
        return Err(Emmc2Error::BufferPhysAbove4Gib(buf_phys));
    }

    // Inhibit poll FIRST, before touching any controller config registers.
    // Per SDHCI spec, writing HOST_CONTROL.DMA_SEL or ADMA_ADDRESS while a
    // transfer is in progress has undefined behaviour — for back-to-back
    // CMD17s on BCM2711 emmc2 it wedges the state machine such that
    // PRESENT_STATE bits 1/2/9 (CMD_INHIBIT_DAT, DAT_LINE_ACTIVE,
    // READ_TRANSFER_ACTIVE) never clear. Polling inhibit before any
    // writes is what Linux's sdhci_send_command does.
    if poll_until_clear_32(
        mmio_va, SDHCI_PRESENT_STATE,
        SDHCI_CMD_INHIBIT | SDHCI_DAT_INHIBIT,
        Nanos::from_millis(100),
    ).is_err() {
        return Err(Emmc2Error::InhibitStuck {
            present_state: sdhci_read32(mmio_va, SDHCI_PRESENT_STATE),
        });
    }

    // One descriptor covers the whole 512-byte transfer. Same descriptor
    // table memory is reused across calls; the controller has either not
    // started yet (first call) or has fully released (inhibit cleared
    // above), so we can safely overwrite the previous descriptor.
    let desc = adma2_tran_end_descriptor(buf_phys as u32, 512);
    ptr::write_volatile(desc_va as *mut u64, desc);
    // Flush the descriptor's cache line to DRAM before the
    // controller's DMA fetches it. Post cacheable-DMA migration
    // C1 the descriptor mapping is Normal Cacheable, so the
    // M6-era `dsb sy` here is no longer sufficient — DSB orders
    // CPU stores against subsequent CPU operations, but the
    // store may still sit in the L1 cache when the controller
    // reads memory. sys_dma_sync_for_device does `dc cvac` + dsb
    // sy, which writes the line back to DRAM.
    if !sys_dma_sync_for_device(desc_ps, 0, 8).is_ok() {
        return Err(Emmc2Error::DescSyncFailed);
    }

    // Re-select ADMA2-32 in HOST_CONTROL_1. Preserve the 4-bit bus
    // width bit (set during M3 ACMD6); replace the DMA_SEL field
    // bits only. Idempotent (engine.new already set it) but kept for
    // safety against any other code path that might have changed it.
    let hc = sdhci_read8(mmio_va, SDHCI_HOST_CONTROL);
    let hc_new = (hc & !SDHCI_HOST_CTRL_DMA_SEL_MASK)
        | SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32;
    sdhci_write8(mmio_va, SDHCI_HOST_CONTROL, hc_new);

    sdhci_write32(mmio_va, SDHCI_ADMA_ADDRESS, desc_phys as u32);

    sdhci_write16(mmio_va, SDHCI_BLOCK_SIZE, 512);
    sdhci_write16(mmio_va, SDHCI_BLOCK_COUNT, 1);
    sdhci_write16(mmio_va, SDHCI_TRANSFER_MODE, SDHCI_TRNS_READ | SDHCI_TRNS_DMA);
    sdhci_write32(mmio_va, SDHCI_ARGUMENT, sector as u32);

    let cmd17 = sd_command_word(
        SdCommand::ReadSingleBlock.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
    );
    let freq = cntfreq_hz();
    sdhci_write16(mmio_va, SDHCI_COMMAND, cmd17);

    let cmd_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    loop {
        let status = sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err_int_status = sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            return Err(Emmc2Error::CmdError { err_int_status });
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE);
            break;
        }
        if monotonic_now() >= cmd_deadline {
            return Err(Emmc2Error::CmdCompleteTimeout);
        }
        core::hint::spin_loop();
    }

    // M6 used a 100ms TRANSFER_COMPLETE deadline for single-block;
    // matched here so the function is byte-equivalent.
    let xfer_deadline = monotonic_now().deadline_in(Nanos::from_millis(100), freq);
    loop {
        let status = sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err_int_status = sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_DATA_COMPLETE);
            return Err(Emmc2Error::DataError { err_int_status });
        }
        if status & SDHCI_INT_DATA_COMPLETE != 0 {
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_DATA_COMPLETE);
            break;
        }
        if monotonic_now() >= xfer_deadline {
            return Err(Emmc2Error::TransferCompleteTimeout);
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Result of an ADMA2 multi-block read with phase-split timing.
/// `cmd_to_complete` is the controller's response latency; the rest
/// is card-side data delivery. Useful for distinguishing controller
/// bugs from card-side stalls — see `docs/tech-debt.md` for the
/// 2026-05-17 card-stall investigation.
pub struct AdmaTiming {
    pub total: u64,
    pub cmd_to_complete: u64,
    pub data_to_complete: u64,
}

/// Direction selector for ADMA2 transfers. `Read` programs CMD18
/// (READ_MULTIPLE_BLOCK) with TRNS_READ; `Write` programs CMD25
/// (WRITE_MULTIPLE_BLOCK) without TRNS_READ. Both use Auto-CMD23
/// to set BLOCK_COUNT in a single command pair (no post-data CMD12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmaDirection {
    Read,
    Write,
}

#[allow(dead_code)]  // Disabled until CMD18+Auto-CMD23 cold-boot is validated. See doc/tech-debt.md.
unsafe fn adma2_transfer(
    mmio_va: u64,
    direction: AdmaDirection,
    sector: u64,
    n_blocks: u16,
    buf_phys: u64,
    desc_ps: PageSetHandle,
    desc_va: u64,
    desc_phys: u64,
) -> Result<AdmaTiming, Emmc2Error> {
    if buf_phys >= (1u64 << 32) {
        return Err(Emmc2Error::BufferPhysAbove4Gib(buf_phys));
    }
    let bytes = (n_blocks as u32) * 512;
    if bytes > 65535 {
        return Err(Emmc2Error::DescriptorLengthOverflow(bytes));
    }

    // Inhibit poll FIRST — see adma2_single_block_read for rationale.
    // Writing ADMA_ADDRESS or other transfer-affecting registers while
    // a previous transfer is still active wedges the controller.
    if poll_until_clear_32(
        mmio_va, SDHCI_PRESENT_STATE,
        SDHCI_CMD_INHIBIT | SDHCI_DAT_INHIBIT,
        Nanos::from_millis(100),
    ).is_err() {
        return Err(Emmc2Error::InhibitStuck {
            present_state: sdhci_read32(mmio_va, SDHCI_PRESENT_STATE),
        });
    }

    // One descriptor covers the whole multi-block transfer. Descriptor
    // table from the single-block path is reused (still mapped at
    // desc_va); overwrite its 8 bytes for this transfer.
    let desc = adma2_tran_end_descriptor(buf_phys as u32, bytes as u16);
    ptr::write_volatile(desc_va as *mut u64, desc);
    // Sync descriptor down to DRAM — same rationale as in
    // adma2_single_block_read (descriptor mapping is now Normal
    // Cacheable post-C1; dsb sy alone doesn't flush the line).
    if !sys_dma_sync_for_device(desc_ps, 0, 8).is_ok() {
        return Err(Emmc2Error::DescSyncFailed);
    }

    sdhci_write32(mmio_va, SDHCI_ADMA_ADDRESS, desc_phys as u32);
    sdhci_write32(mmio_va, SDHCI_ARGUMENT2, n_blocks as u32);

    sdhci_write16(mmio_va, SDHCI_BLOCK_SIZE, 512);
    sdhci_write16(mmio_va, SDHCI_BLOCK_COUNT, n_blocks);
    let trns_dir = match direction {
        AdmaDirection::Read => SDHCI_TRNS_READ,
        AdmaDirection::Write => 0, // absence of TRNS_READ = write
    };
    sdhci_write16(mmio_va, SDHCI_TRANSFER_MODE,
        trns_dir | SDHCI_TRNS_DMA | SDHCI_TRNS_BLK_CNT_EN
            | SDHCI_TRNS_MULTI | SDHCI_TRNS_AUTO_CMD23);
    // CMD18/CMD25 argument is the LBA (SDHC/SDXC blocks-as-units).
    sdhci_write32(mmio_va, SDHCI_ARGUMENT, sector as u32);

    let cmd = sd_command_word(
        match direction {
            AdmaDirection::Read => SdCommand::ReadMultipleBlock.index(),
            AdmaDirection::Write => SdCommand::WriteMultipleBlock.index(),
        },
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
    );

    let freq = cntfreq_hz();
    let t0 = monotonic_now();
    sdhci_write16(mmio_va, SDHCI_COMMAND, cmd);

    // Poll CMD_COMPLETE
    let cmd_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    loop {
        let status = sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err_int_status = sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            return Err(Emmc2Error::CmdError { err_int_status });
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE);
            break;
        }
        if monotonic_now() >= cmd_deadline {
            return Err(Emmc2Error::CmdCompleteTimeout);
        }
        core::hint::spin_loop();
    }
    let t_cmd = monotonic_now();

    // Poll TRANSFER_COMPLETE — for multi-block ADMA, the controller
    // signals once after the FULL descriptor's transfer ends.
    let xfer_deadline = monotonic_now().deadline_in(Nanos::from_millis(500), freq);
    loop {
        let status = sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err_int_status = sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_DATA_COMPLETE);
            return Err(Emmc2Error::DataError { err_int_status });
        }
        if status & SDHCI_INT_DATA_COMPLETE != 0 {
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_DATA_COMPLETE);
            break;
        }
        if monotonic_now() >= xfer_deadline {
            return Err(Emmc2Error::TransferCompleteTimeout);
        }
        core::hint::spin_loop();
    }
    let t1 = monotonic_now();
    Ok(AdmaTiming {
        total: t1.0.saturating_sub(t0.0),
        cmd_to_complete: t_cmd.0.saturating_sub(t0.0),
        data_to_complete: t1.0.saturating_sub(t_cmd.0),
    })
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

/// Terminate the process. EL0 `wfi`-loops keep the thread `Running`
/// from the scheduler's POV — they don't block; they spin a
/// tick-period each iteration. Use sys_exit so the scheduler removes
/// us from rotation.
fn halt() -> ! {
    sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("emmc2: PANIC\n");
    halt();
}

// ---------------------------------------------------------------------------
// Emmc2BlockEngine — implements lockjaw_userlib::block::BlockEngine
// against the SDHCI controller. `read` loops `adma2_single_block_read`
// (CMD17) one block at a time. `write` returns `Unsupported` until the
// CMD18+Auto-CMD23 cold-boot issue is diagnosed (see docs/tech-debt.md).
// Plus per-buffer PageSet tracking.
// ---------------------------------------------------------------------------

const MAX_DMA_BUFFERS: usize = 8;

#[derive(Clone, Copy)]
struct DmaBuf {
    /// Handle (0 marks the slot empty).
    handle: u64,
    pa: u64,
    sector_count: u64,
}

const DMA_BUF_EMPTY: DmaBuf = DmaBuf { handle: 0, pa: 0, sector_count: 0 };

struct Emmc2BlockEngine {
    mmio_va: u64,
    capacity_sectors: u64,
    /// Persistent ADMA2-32 descriptor table. One 8-byte tran+end
    /// descriptor is rewritten per transfer. Allocated from the
    /// DMA pool and (post cacheable-DMA migration C1) mapped
    /// Normal Cacheable. The `desc_ps` handle is retained so
    /// every per-transfer descriptor write can be followed by a
    /// `sys_dma_sync_for_device` call to flush the cache line
    /// down to DRAM before the controller fetches the descriptor.
    desc_ps: PageSetHandle,
    desc_va: u64,
    desc_phys: u64,
    dma_buffers: [DmaBuf; MAX_DMA_BUFFERS],
    dma_count: usize,
}

impl Emmc2BlockEngine {
    /// Allocate the descriptor table page, switch the controller to
    /// ADMA2-32, return a ready-to-serve engine. Caller still owns
    /// `mmio_va` and is responsible for keeping the controller
    /// initialized through enumeration before calling this.
    unsafe fn new(mmio_va: u64, capacity_sectors: u64)
        -> Result<Self, Emmc2Error>
    {
        // Descriptor table: 1 page from the DMA pool. Post-C1 of
        // the cacheable-DMA migration this is mapped Normal
        // Cacheable; the matching sys_dma_sync_for_device call
        // before each controller kick is in adma2_single_block_read.
        let desc_ps = sys_alloc_dma_pages(1).map_err(|_| Emmc2Error::DescAllocFailed)?;
        let desc_phys = sys_query_pageset_phys(desc_ps, 0)
            .map_err(|_| Emmc2Error::DescPhysQueryFailed)?;
        let desc_va = VMEM.alloc(1).ok_or(Emmc2Error::DescVaExhausted)?;
        if !sys_map_pages(desc_ps, desc_va, MapMemoryAttribute::Normal).is_ok() {
            return Err(Emmc2Error::DescMapFailed);
        }
        if desc_phys >= (1u64 << 32) {
            return Err(Emmc2Error::DescPhysAbove4Gib(desc_phys));
        }
        // Switch HOST_CONTROL_1.DMA_SEL to ADMA2-32; preserve the
        // 4-bit bus width from ACMD6.
        let hc = sdhci_read8(mmio_va, SDHCI_HOST_CONTROL);
        let hc_new = (hc & !SDHCI_HOST_CTRL_DMA_SEL_MASK)
            | SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32;
        sdhci_write8(mmio_va, SDHCI_HOST_CONTROL, hc_new);
        // Program the descriptor table address once; the same table
        // is reused across every transfer.
        sdhci_write32(mmio_va, SDHCI_ADMA_ADDRESS, desc_phys as u32);
        Ok(Self {
            mmio_va,
            capacity_sectors,
            desc_ps,
            desc_va,
            desc_phys,
            dma_buffers: [DMA_BUF_EMPTY; MAX_DMA_BUFFERS],
            dma_count: 0,
        })
    }

    fn find_buf(&self, ps: PageSetHandle) -> Result<DmaBuf, BlockError> {
        for slot in &self.dma_buffers {
            if slot.handle == ps.0 && slot.pa != 0 {
                return Ok(*slot);
            }
        }
        Err(BlockError::InvalidBuffer)
    }
}

impl BlockEngine for Emmc2BlockEngine {
    fn info(&self) -> BlockInfo {
        BlockInfo {
            capacity_sectors: self.capacity_sectors,
            sector_size: 512,
            // emmc2 allocs buffers via `sys_alloc_dma_pages` (DmaPool
            // origin). Post cacheable-DMA migration C1 the kernel
            // accepts only Normal (Cacheable) mappings for DmaPool
            // PageSets. Cross-process coherence is the ENGINE's
            // responsibility: `Engine::read` invokes
            // `sys_dma_sync_for_cpu` over the read range before
            // returning, so by the time control reaches the client
            // (over IPC) the buffer's cache lines for that range
            // are already invalidated and the next CPU load reads
            // fresh DRAM. Clients DO NOT need to issue their own
            // sync after `read` returns; they DO need to call
            // `sys_dma_sync_for_device` before any write-direction
            // request (not exercised today — writes are disabled
            // until CMD18+Auto-CMD23 cold-boot is validated).
            buffer_attribute: MapMemoryAttribute::Normal,
        }
    }

    fn alloc_buffer(&mut self, sector_count: u64) -> Result<PageSetHandle, BlockError> {
        if self.dma_count >= MAX_DMA_BUFFERS {
            return Err(BlockError::AllocFailed);
        }
        // ADMA2-32 single-descriptor max is 65535 bytes = 127 sectors.
        // Larger transfers would need multi-descriptor chains (out of
        // scope for M7 MVP; future work).
        if sector_count == 0 || sector_count > 127 {
            return Err(BlockError::InvalidParameter);
        }
        let pages = (sector_count * 512 + (PAGE_SIZE - 1)) / PAGE_SIZE;
        let guard = PageSetGuard::new(
            sys_alloc_dma_pages(pages).map_err(|_| BlockError::AllocFailed)?
        );
        let pa = sys_query_pageset_phys(guard.handle(), 0)
            .map_err(|_| BlockError::AllocFailed)?;
        if pa >= (1u64 << 32) {
            return Err(BlockError::AllocFailed);
        }
        for slot in self.dma_buffers.iter_mut() {
            if slot.pa == 0 {
                let ps = guard.take();
                *slot = DmaBuf { handle: ps.0, pa, sector_count };
                self.dma_count += 1;
                return Ok(ps);
            }
        }
        Err(BlockError::AllocFailed)
    }

    fn read(&mut self, sector: u64, count: u64, buffer: PageSetHandle)
        -> Result<(), BlockError>
    {
        let buf = self.find_buf(buffer)?;
        if count == 0 || count > buf.sector_count
            || sector.saturating_add(count) > self.capacity_sectors
        {
            return Err(BlockError::InvalidParameter);
        }
        // Reads always use CMD17 (single-block). Multi-sector reads
        // loop one block at a time, advancing buf.pa by 512 bytes per
        // sector. The CMD18+Auto-CMD23 path in adma2_transfer is dead
        // for reads until the cold-boot CMD18 question is settled;
        // we want every fat32 / partition-manager read to exercise
        // the proven M6 sub-commit 2b sequence.
        //
        // Performance cost on Pi: one CMD17 round-trip per sector for
        // multi-sector reads. Acceptable while the read path is being
        // validated end-to-end with the partition-manager layer.
        for i in 0..count {
            let buf_sector_pa = buf.pa + i * 512;
            let res = unsafe {
                adma2_single_block_read(
                    self.mmio_va, sector + i, buf_sector_pa,
                    self.desc_ps, self.desc_va, self.desc_phys,
                )
            };
            if let Err(err) = res {
                // Surface the typed Emmc2Error so silent-return cases
                // (inhibit stuck / cmd or xfer timeout) aren't lost behind
                // the BlockError::IoError facade. One canonical log line
                // per failure, identifying which sector in the loop died.
                puts("[EMMC2:READ] iter=");
                put_decimal(i);
                puts("/");
                put_decimal(count);
                puts(" sector=");
                put_decimal(sector + i);
                puts(" buf_pa=");
                put_hex(buf_sector_pa);
                puts(" FAILED: ");
                put_emmc2_error(&err);
                puts("\n");
                return Err(BlockError::IoError);
            }
        }
        // Cacheable-DMA migration C1: the buffer is now mapped
        // Normal Cacheable. After the controller has finished
        // writing it via DMA, invalidate the CPU cache lines so
        // subsequent CPU loads read fresh DRAM rather than stale
        // pre-DMA cached state. This is the principled replacement
        // for the M7-era incidental drain (32-byte diagnostic dump
        // / sdhci_read32(PRESENT_STATE)) — see
        // docs/cacheable-dma-migration-plan.md and Linux's
        // dma_sync_sg_for_cpu in sdhci_adma_table_post.
        //
        // Sync at engine level (after the inner loop) rather than
        // per-iteration because no caller code touches the buffer
        // between iterations; one sync covers count*512 bytes.
        let sync_bytes = count * 512;
        if !sys_dma_sync_for_cpu(buffer, 0, sync_bytes).is_ok() {
            puts("[EMMC2:READ] sync_for_cpu FAILED\n");
            return Err(BlockError::IoError);
        }
        Ok(())
    }

    fn write(&mut self, _sector: u64, _count: u64, _buffer: PageSetHandle)
        -> Result<(), BlockError>
    {
        // Writes are disabled until CMD18+Auto-CMD23 (read) and
        // CMD25+Auto-CMD23 (write) are validated on cold boot — see
        // docs/tech-debt.md "CMD18+Auto-CMD23 cold-boot validation".
        // The Pi 4B end-to-end target (#131) is FAT32 read through
        // POSIX, which does not write; rejecting writes prevents
        // accidental dispatch through the multi-block path while the
        // cold-boot diagnosis is open.
        Err(BlockError::Unsupported)
    }

    fn free_buffer(&mut self, buffer: PageSetHandle) {
        for slot in self.dma_buffers.iter_mut() {
            if slot.handle == buffer.0 && slot.pa != 0 {
                sys_close_handle(PageSetHandle(slot.handle));
                *slot = DMA_BUF_EMPTY;
                self.dma_count -= 1;
                return;
            }
        }
    }
}
