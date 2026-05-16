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
    SDHCI_BLOCK_SIZE, SDHCI_BLOCK_COUNT, SDHCI_BUFFER_DATA_PORT,
    SDHCI_NORMAL_INT_STATUS, SDHCI_ERROR_INT_STATUS,
    SDHCI_NORMAL_INT_STATUS_ENABLE, SDHCI_ERROR_INT_STATUS_ENABLE,
    SDHCI_INT_CMD_COMPLETE, SDHCI_INT_DATA_COMPLETE,
    SDHCI_INT_BUF_RD_READY, SDHCI_INT_ERROR,
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

// ---------------------------------------------------------------------------
// PIO block read
// ---------------------------------------------------------------------------

/// Failure modes for `read_block_pio`.
#[derive(Clone, Copy)]
enum PioError {
    /// CMD_INHIBIT or DAT_INHIBIT didn't clear before the deadline.
    InhibitStuck,
    /// ERROR_INT_STATUS fired during the command phase.
    CmdError(u16),
    /// Neither CMD_COMPLETE nor ERROR fired before the 1s deadline.
    NoResponse,
    /// ERROR_INT_STATUS fired while waiting for BUF_RD_READY or TRANSFER_COMPLETE.
    DataError(u16),
    /// BUF_RD_READY did not fire before the deadline.
    DataTimeout,
    /// TRANSFER_COMPLETE did not arrive after draining the buffer.
    XferTimeout,
}

/// Issue CMD17 (READ_SINGLE_BLOCK) and drain the 512-byte block via PIO.
///
/// `lba` is the block address. For SDHC/SDXC (CCS=1) this is the block
/// number directly. For SDSC (CCS=0) the caller must pass `lba * 512`
/// (byte address). For LBA 0 both encodings are 0; the distinction
/// becomes load-bearing at M5+ when non-zero LBAs are requested.
///
/// Returns the block as 128 LE u32 words. BUFFER_DATA_PORT serves bytes
/// in card-native order: word[0] = bytes 0–3, word[127] = bytes 508–511.
/// MBR signature check: `(block[127] >> 16) & 0xFFFF == 0xAA55`.
unsafe fn read_block_pio(base: u64, lba: u32) -> Result<[u32; 128], PioError> {
    // Data commands occupy both CMD and DAT lines from issue through
    // TRANSFER_COMPLETE. Waiting for only CMD_INHIBIT (as in
    // `issue_command`) would let the command fire while DAT is still
    // busy from a prior R1b, corrupting the bus.
    if poll_until_clear_32(
        base, SDHCI_PRESENT_STATE,
        SDHCI_CMD_INHIBIT | SDHCI_DAT_INHIBIT,
        Nanos::from_millis(100),
    ).is_err() {
        return Err(PioError::InhibitStuck);
    }

    // SDHCI spec §3.7: BLOCK_SIZE, BLOCK_COUNT, TRANSFER_MODE must be
    // written before the COMMAND write that triggers the bus transaction.
    sdhci_write16(base, SDHCI_BLOCK_SIZE, 512);
    sdhci_write16(base, SDHCI_BLOCK_COUNT, 1);
    // PIO single-block read: no DMA, no multi-block, direction = card→host.
    sdhci_write16(base, SDHCI_TRANSFER_MODE, SDHCI_TRNS_READ);

    // Write ARGUMENT then COMMAND. COMMAND write triggers the bus.
    sdhci_write32(base, SDHCI_ARGUMENT, lba);
    let cmd17 = sd_command_word(
        SdCommand::ReadSingleBlock.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
    );
    sdhci_write16(base, SDHCI_COMMAND, cmd17);

    // Poll for CMD_COMPLETE or ERROR. CMD_COMPLETE fires after the response
    // phase; data transfer starts in parallel and fires BUF_RD_READY once
    // the internal buffer is full.
    let freq = cntfreq_hz();
    let cmd_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    loop {
        let status = sdhci_read16(base, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err = sdhci_read16(base, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            return Err(PioError::CmdError(err));
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE);
            break;
        }
        if monotonic_now() >= cmd_deadline {
            return Err(PioError::NoResponse);
        }
        core::hint::spin_loop();
    }

    // Poll for BUF_RD_READY. At 25 MHz with 4-bit DAT: 512 bytes /
    // (25M/8 bytes/s) ≈ 164 µs for the card to fill the internal buffer.
    // 100 ms deadline is generous; PRESENT_STATE is logged on timeout
    // so DAT_INHIBIT state is visible in the log.
    let data_deadline = monotonic_now().deadline_in(Nanos::from_millis(100), freq);
    loop {
        let status = sdhci_read16(base, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err = sdhci_read16(base, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_BUF_RD_READY);
            return Err(PioError::DataError(err));
        }
        if status & SDHCI_INT_BUF_RD_READY != 0 {
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_BUF_RD_READY);
            break;
        }
        if monotonic_now() >= data_deadline {
            return Err(PioError::DataTimeout);
        }
        core::hint::spin_loop();
    }

    // Drain all 128 u32 words (512 bytes) from BUFFER_DATA_PORT. SDHCI
    // spec §3.7.2: PIO reads must consume the entire block in one pass —
    // leaving data in the buffer leaves the controller in an inconsistent
    // state that wedges subsequent commands.
    let mut block = [0u32; 128];
    for word in block.iter_mut() {
        *word = sdhci_read32(base, SDHCI_BUFFER_DATA_PORT);
    }

    // TRANSFER_COMPLETE arrives after the card transmits the CRC trailer;
    // typically fires immediately after the buffer drain.
    let xfer_deadline = monotonic_now().deadline_in(Nanos::from_millis(100), freq);
    loop {
        let status = sdhci_read16(base, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err = sdhci_read16(base, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_DATA_COMPLETE);
            return Err(PioError::DataError(err));
        }
        if status & SDHCI_INT_DATA_COMPLETE != 0 {
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_DATA_COMPLETE);
            return Ok(block);
        }
        if monotonic_now() >= xfer_deadline {
            return Err(PioError::XferTimeout);
        }
        core::hint::spin_loop();
    }
}

/// Issue CMD18 (READ_MULTIPLE_BLOCK) with Auto-CMD23, drain `blocks.len()`
/// blocks via PIO. Each block is 512 bytes = 128 LE u32 words.
///
/// Auto-CMD23 (per SDHCI §3.7.2 and `docs/emmc2-block-storage-plan.md` Q4):
/// the controller automatically issues CMD23 (SET_BLOCK_COUNT) before CMD18,
/// using the value at SDHCI_ARGUMENT2 (offset 0x000) as its argument. No
/// post-data CMD12 (STOP_TRANSMISSION) is needed because the card stops
/// after the negotiated count. BCM2711 advertises Auto-CMD23 in CAPABILITIES
/// bit 30 and Linux/Circle drivers use this path.
///
/// `lba` is the starting block address (block-addressed for SDHC/SDXC).
unsafe fn read_blocks_pio(
    base: u64, lba: u32, blocks: &mut [[u32; 128]],
) -> Result<(), PioError> {
    let count = blocks.len() as u16;

    // Auto-CMD23 argument — block count for the SET_BLOCK_COUNT the
    // controller will issue before CMD18. Must be written before
    // TRANSFER_MODE so the controller sees it when it triggers CMD23.
    sdhci_write32(base, SDHCI_ARGUMENT2, count as u32);

    if poll_until_clear_32(
        base, SDHCI_PRESENT_STATE,
        SDHCI_CMD_INHIBIT | SDHCI_DAT_INHIBIT,
        Nanos::from_millis(100),
    ).is_err() {
        return Err(PioError::InhibitStuck);
    }

    sdhci_write16(base, SDHCI_BLOCK_SIZE, 512);
    sdhci_write16(base, SDHCI_BLOCK_COUNT, count);
    // Multi-block read with Auto-CMD23: READ direction, block-count enabled,
    // multi-block, controller auto-issues CMD23 with the SDHCI_ARGUMENT2
    // value before CMD18.
    sdhci_write16(base, SDHCI_TRANSFER_MODE,
        SDHCI_TRNS_READ | SDHCI_TRNS_BLK_CNT_EN
            | SDHCI_TRNS_MULTI | SDHCI_TRNS_AUTO_CMD23);

    sdhci_write32(base, SDHCI_ARGUMENT, lba);
    let cmd18 = sd_command_word(
        SdCommand::ReadMultipleBlock.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
    );
    sdhci_write16(base, SDHCI_COMMAND, cmd18);

    let freq = cntfreq_hz();
    let cmd_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    loop {
        let status = sdhci_read16(base, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err = sdhci_read16(base, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            return Err(PioError::CmdError(err));
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE);
            break;
        }
        if monotonic_now() >= cmd_deadline {
            return Err(PioError::NoResponse);
        }
        core::hint::spin_loop();
    }

    // Drain one block at a time. Each block's BUF_RD_READY edge fires
    // when the controller has filled its internal buffer; we clear the
    // bit after draining so the next block's edge is observable.
    for block in blocks.iter_mut() {
        let data_deadline = monotonic_now().deadline_in(Nanos::from_millis(100), freq);
        loop {
            let status = sdhci_read16(base, SDHCI_NORMAL_INT_STATUS);
            if status & SDHCI_INT_ERROR != 0 {
                let err = sdhci_read16(base, SDHCI_ERROR_INT_STATUS);
                sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
                sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_BUF_RD_READY);
                return Err(PioError::DataError(err));
            }
            if status & SDHCI_INT_BUF_RD_READY != 0 {
                sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_BUF_RD_READY);
                break;
            }
            if monotonic_now() >= data_deadline {
                return Err(PioError::DataTimeout);
            }
            core::hint::spin_loop();
        }
        for word in block.iter_mut() {
            *word = sdhci_read32(base, SDHCI_BUFFER_DATA_PORT);
        }
    }

    // After the last block, TRANSFER_COMPLETE arrives once the card
    // transmits the final CRC trailer.
    let xfer_deadline = monotonic_now().deadline_in(Nanos::from_millis(100), freq);
    loop {
        let status = sdhci_read16(base, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err = sdhci_read16(base, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(base, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_DATA_COMPLETE);
            return Err(PioError::DataError(err));
        }
        if status & SDHCI_INT_DATA_COMPLETE != 0 {
            sdhci_write16(base, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_DATA_COMPLETE);
            return Ok(());
        }
        if monotonic_now() >= xfer_deadline {
            return Err(PioError::XferTimeout);
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
    // M4 — Single-block PIO read of LBA 0
    // -----------------------------------------------------------------------
    // CMD17 argument: for SDHC/SDXC (ocr.ccs == true) the argument is the
    // block number directly. For SDSC it would be a byte address (lba * 512).
    // Both encodings produce 0 for LBA 0, so the ccs branch is degenerate
    // here. The distinction becomes load-bearing at M5+ when non-zero LBAs
    // are requested.
    let block = match unsafe { read_block_pio(mmio_va, 0) } {
        Ok(b) => b,
        Err(PioError::InhibitStuck) => {
            puts("[EMMC2:M4] CMD17 FAILED: CMD/DAT_INHIBIT stuck\n");
            sys_exit();
        }
        Err(PioError::CmdError(bits)) => {
            puts("[EMMC2:M4] CMD17 FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        Err(PioError::NoResponse) => {
            puts("[EMMC2:M4] CMD17 FAILED: no CMD_COMPLETE within 1s\n");
            sys_exit();
        }
        Err(PioError::DataError(bits)) => {
            puts("[EMMC2:M4] data FAILED: ");
            put_error_int_status(bits);
            puts("\n");
            sys_exit();
        }
        Err(PioError::DataTimeout) => {
            let pstate = unsafe { sdhci_read32(mmio_va, SDHCI_PRESENT_STATE) };
            puts("[EMMC2:M4] data FAILED: BUF_RD_READY timeout PRESENT_STATE=");
            put_hex(pstate as u64);
            puts("\n");
            sys_exit();
        }
        Err(PioError::XferTimeout) => {
            puts("[EMMC2:M4] data FAILED: TRANSFER_COMPLETE timeout\n");
            sys_exit();
        }
    };

    // Log first 64 bytes (16 words) of LBA 0 as a hex dump.
    puts("[EMMC2:M4] LBA 0 first 64 bytes:\n");
    for i in 0..16usize {
        if i % 4 == 0 { puts("  "); }
        put_hex(block[i] as u64);
        puts(if i % 4 == 3 { "\n" } else { " " });
    }

    // MBR boot signature: bytes 510–511 must be 0x55 0xAA.
    // BUFFER_DATA_PORT returns LE u32: word[127] holds bytes 508–511,
    // with byte 510 in bits[23:16] and byte 511 in bits[31:24].
    // (block[127] >> 16) extracts 0xAA55 when the signature is valid.
    let sig = (block[127] >> 16) & 0xFFFF;
    puts("[EMMC2:M4] MBR signature=");
    put_hex(sig as u64);
    if sig != 0xAA55 {
        puts(" BAD (expected 0xAA55) — byte-lane or decode error\n");
        sys_exit();
    }
    puts(" OK\n");

    puts("[EMMC2:M4] LBA 0 read OK\n");

    // -----------------------------------------------------------------------
    // M5 — Multi-block PIO read (CMD18 + Auto-CMD23) at boot.
    //      The single-block write path (`write_block_pio`) is implemented
    //      and verified, but is NOT exercised from `_start` because the
    //      driver runs unconditionally on every boot and no LBA is
    //      universally safe across MBR / GPT / bootable images (LBA 0 is
    //      MBR, LBAs 1–33 are the GPT primary header, last LBA is the
    //      GPT backup header). A crash between "write pattern" and
    //      "restore original" would persist as data corruption. Real
    //      write coverage moves to M7's BlockEngine, where the LBA is
    //      caller-controlled and tests run against a known-safe target.
    //      The Pi log captured before this restructure (commit message)
    //      confirms write_block_pio + read_block_pio + reset_cmd_dat_lines
    //      have been exercised end-to-end with verified roundtrip.
    // -----------------------------------------------------------------------

    // Multi-block read of LBAs [0..4] via CMD18 + Auto-CMD23.
    // Non-destructive. The first block must still carry the MBR signature
    // we saw in M4 — mismatch would indicate a multi-block-specific
    // decode failure (Auto-CMD23 handshake, BUF_RD_READY edge sequencing).
    let mut blocks: [[u32; 128]; 4] = [[0u32; 128]; 4];
    if let Err(e) = unsafe { read_blocks_pio(mmio_va, 0, &mut blocks) } {
        report_pio_error(mmio_va, "M5", "CMD18 (4 blocks)", e);
        sys_exit();
    }
    let multi_sig = (blocks[0][127] >> 16) & 0xFFFF;
    if multi_sig != 0xAA55 {
        puts("[EMMC2:M5] CMD18 block[0] signature=");
        put_hex(multi_sig as u64);
        puts(" BAD (expected 0xAA55)\n");
        sys_exit();
    }
    puts("[EMMC2:M5] CMD18 read 4 blocks OK\n");

    // -----------------------------------------------------------------------
    // M6 — Single-block ADMA2-32 read of LBA 0
    // -----------------------------------------------------------------------
    // Consumes the M6 sub-commit 2a substrate: sys_alloc_dma_pages
    // returns DmaPool-origin PageSets that can ONLY be mapped
    // NormalNonCacheable. The pool is physically reserved at boot and
    // excluded from the kernel direct map, so the SDHCI DMA engine
    // writing this buffer cannot race the CPU's cached view of the
    // same PA (the alias never exists).
    let freq = cntfreq_hz();

    // Descriptor table: 1 page from the DMA pool (8-byte aligned by
    // virtue of being page-aligned). Only the first 8 bytes hold a
    // single tran+end descriptor; remainder is unused this milestone.
    let desc_ps = match sys_alloc_dma_pages(1) {
        Ok(h) => h,
        Err(e) => {
            puts("[EMMC2:M6] sys_alloc_dma_pages(desc) FAILED: ");
            put_decimal(e.0);
            puts("\n");
            sys_exit();
        }
    };
    let desc_phys = match sys_query_pageset_phys(desc_ps, 0) {
        Ok(p) => p,
        Err(e) => {
            puts("[EMMC2:M6] sys_query_pageset_phys(desc) FAILED: ");
            put_decimal(e.0);
            puts("\n");
            sys_exit();
        }
    };
    let desc_va = match VMEM.alloc(1) {
        Some(va) => va,
        None => { puts("[EMMC2:M6] VA exhausted for desc table\n"); sys_exit(); }
    };
    if !sys_map_pages(desc_ps, desc_va, MapMemoryAttribute::NormalNonCacheable).is_ok() {
        puts("[EMMC2:M6] sys_map_pages(desc, NC) FAILED\n");
        sys_exit();
    }

    // Data buffer: 1 page from the DMA pool (4 KiB; we use only the
    // first 512 bytes for one block, plenty of slack).
    let buf_ps = match sys_alloc_dma_pages(1) {
        Ok(h) => h,
        Err(e) => {
            puts("[EMMC2:M6] sys_alloc_dma_pages(buf) FAILED: ");
            put_decimal(e.0);
            puts("\n");
            sys_exit();
        }
    };
    let buf_phys = match sys_query_pageset_phys(buf_ps, 0) {
        Ok(p) => p,
        Err(e) => {
            puts("[EMMC2:M6] sys_query_pageset_phys(buf) FAILED: ");
            put_decimal(e.0);
            puts("\n");
            sys_exit();
        }
    };
    let buf_va = match VMEM.alloc(1) {
        Some(va) => va,
        None => { puts("[EMMC2:M6] VA exhausted for buffer\n"); sys_exit(); }
    };
    if !sys_map_pages(buf_ps, buf_va, MapMemoryAttribute::NormalNonCacheable).is_ok() {
        puts("[EMMC2:M6] sys_map_pages(buf, NC) FAILED\n");
        sys_exit();
    }

    // The controller programs ADMA descriptors using 32-bit PAs.
    // Pi 4B and QEMU virt RAM both fit under 4 GiB so this truncation
    // is safe; the DMA pool sits below ram_end which is below 4 GiB.
    if buf_phys >= (1u64 << 32) || desc_phys >= (1u64 << 32) {
        puts("[EMMC2:M6] phys addr beyond 32-bit ADMA2 range\n");
        sys_exit();
    }

    // Build the single tran+end descriptor: 8 bytes describing a
    // 512-byte transfer ending at this descriptor. Write through the
    // NC mapping so the value reaches RAM (the controller reads
    // descriptors via DMA, NOT via the CPU's cacheable mapping).
    let desc = adma2_tran_end_descriptor(buf_phys as u32, 512);
    unsafe { ptr::write_volatile(desc_va as *mut u64, desc); }

    // DMA publication barrier: the descriptor write above must reach
    // RAM (and be visible to the SDHCI controller via its DMA path)
    // BEFORE the MMIO writes that kick the transfer. write_volatile
    // alone doesn't impose this ordering on AArch64. `dsb sy` waits
    // for all preceding memory accesses to complete to the full
    // system shareability domain — covering the controller, which
    // sits in the outer-shareable domain on Pi 4B (matches the
    // SH_OUTER attribute the NC mapping uses). Same pattern as
    // ramfb-driver/main.rs:174.
    unsafe { asm!("dsb sy", options(nomem, nostack)); }

    // Switch HOST_CONTROL_1.DMA_SEL to ADMA2 32-bit. Preserve the
    // 4-bit bus width bit (set during M3 ACMD6); replace the DMA_SEL
    // field bits only.
    unsafe {
        let hc = sdhci_read8(mmio_va, SDHCI_HOST_CONTROL);
        let hc_new = (hc & !SDHCI_HOST_CTRL_DMA_SEL_MASK)
            | SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32;
        sdhci_write8(mmio_va, SDHCI_HOST_CONTROL, hc_new);
    }

    // Program the ADMA descriptor table address.
    unsafe { sdhci_write32(mmio_va, SDHCI_ADMA_ADDRESS, desc_phys as u32); }

    // Wait for both inhibit bits and program block params.
    if unsafe { poll_until_clear_32(
        mmio_va, SDHCI_PRESENT_STATE,
        SDHCI_CMD_INHIBIT | SDHCI_DAT_INHIBIT,
        Nanos::from_millis(100),
    )}.is_err() {
        puts("[EMMC2:M6] CMD/DAT_INHIBIT stuck pre-ADMA2 CMD17\n");
        sys_exit();
    }
    unsafe {
        sdhci_write16(mmio_va, SDHCI_BLOCK_SIZE, 512);
        sdhci_write16(mmio_va, SDHCI_BLOCK_COUNT, 1);
        // Single-block ADMA2 read: READ direction + DMA enable. No
        // block-count enable, no Auto-CMD23 — those are for multi-
        // block transfers; one block doesn't need them.
        sdhci_write16(mmio_va, SDHCI_TRANSFER_MODE, SDHCI_TRNS_READ | SDHCI_TRNS_DMA);
        sdhci_write32(mmio_va, SDHCI_ARGUMENT, 0);
    }
    let cmd17 = sd_command_word(
        SdCommand::ReadSingleBlock.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
    );

    let t0 = monotonic_now();
    unsafe { sdhci_write16(mmio_va, SDHCI_COMMAND, cmd17); }

    // Poll for CMD_COMPLETE (response phase) — DMA fires in parallel.
    let cmd_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    loop {
        let status = unsafe { sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS) };
        if status & SDHCI_INT_ERROR != 0 {
            let err = unsafe { sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS) };
            unsafe {
                sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
                sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            }
            puts("[EMMC2:M6] CMD17 controller error: ");
            put_error_int_status(err);
            puts("\n");
            sys_exit();
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            unsafe { sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE); }
            break;
        }
        if monotonic_now() >= cmd_deadline {
            puts("[EMMC2:M6] CMD17 CMD_COMPLETE timeout\n");
            sys_exit();
        }
        core::hint::spin_loop();
    }

    // Poll for TRANSFER_COMPLETE — controller signals when the DMA
    // engine drains the descriptor (one ACT=tran, END=1) and the
    // card delivers the CRC trailer. No per-block BUF_RD_READY draining
    // for ADMA2 — that's what makes this faster than PIO.
    let xfer_deadline = monotonic_now().deadline_in(Nanos::from_millis(100), freq);
    loop {
        let status = unsafe { sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS) };
        if status & SDHCI_INT_ERROR != 0 {
            let err = unsafe { sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS) };
            unsafe {
                sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
                sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_ERROR | SDHCI_INT_DATA_COMPLETE);
            }
            puts("[EMMC2:M6] ADMA2 data error: ");
            put_error_int_status(err);
            puts("\n");
            sys_exit();
        }
        if status & SDHCI_INT_DATA_COMPLETE != 0 {
            unsafe { sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_DATA_COMPLETE); }
            break;
        }
        if monotonic_now() >= xfer_deadline {
            puts("[EMMC2:M6] ADMA2 TRANSFER_COMPLETE timeout\n");
            sys_exit();
        }
        core::hint::spin_loop();
    }
    let t1 = monotonic_now();
    let adma_ticks = t1.0.saturating_sub(t0.0);

    // Verify MBR signature from the NC-mapped buffer. The DMA engine
    // wrote 512 bytes into the buffer's PA; the NC mapping lets us
    // read those bytes without cache coherence concerns. word[127]
    // (bytes 508-511) carries the signature in little-endian.
    let sig_word = unsafe { ptr::read_volatile((buf_va + 4 * 127) as *const u32) };
    let sig = (sig_word >> 16) & 0xFFFF;
    if sig != 0xAA55 {
        puts("[EMMC2:M6] ADMA2 MBR signature=");
        put_hex(sig as u64);
        puts(" BAD (expected 0xAA55)\n");
        sys_exit();
    }

    // ADMA2 done. Re-run the PIO single-block read of LBA 0 for a
    // wall-clock comparison. PIO drains 128 u32 words through the
    // controller's internal buffer one at a time; ADMA2 has the
    // engine DMA all 512 bytes directly into RAM, so ADMA2 should be
    // measurably faster at SDHCI 25 MHz on Pi 4B.
    let t2 = monotonic_now();
    let _pio_block = match unsafe { read_block_pio(mmio_va, 0) } {
        Ok(b) => b,
        Err(e) => {
            report_pio_error(mmio_va, "M6", "PIO compare read", e);
            sys_exit();
        }
    };
    let t3 = monotonic_now();
    let pio_ticks = t3.0.saturating_sub(t2.0);

    // Restore HOST_CONTROL.DMA_SEL to its prior state so the PIO M4
    // path (and any future caller) doesn't see ADMA2 selected. M4's
    // read_block_pio above ran while DMA_SEL was still ADMA2 — the
    // PIO path doesn't set TRNS_DMA so the engine stays idle, but the
    // bit being wrong is still confusing for future readers.
    unsafe {
        let hc = sdhci_read8(mmio_va, SDHCI_HOST_CONTROL);
        sdhci_write8(mmio_va, SDHCI_HOST_CONTROL, hc & !SDHCI_HOST_CTRL_DMA_SEL_MASK);
    }

    // M6 success line: descriptor count + ADMA2 elapsed in microseconds.
    // The PIO comparison is logged on its own line so a regression in
    // either path is visible in isolation.
    let adma_us = ticks_to_us(adma_ticks, freq);
    let pio_us = ticks_to_us(pio_ticks, freq);
    puts("[EMMC2:ADMA] LBA0 read via ADMA2-32, descriptors=1, t=");
    put_decimal(adma_us);
    puts("us\n");
    puts("[EMMC2:ADMA] PIO compare t=");
    put_decimal(pio_us);
    puts("us\n");

    // -----------------------------------------------------------------------
    // M6 perf sweep — multi-block ADMA2 vs multi-block PIO
    // -----------------------------------------------------------------------
    // Pi 4B single-block ADMA2 measured slower than PIO (5.6ms vs 0.22ms)
    // on the first M6 boot — overhead-dominated for one block. This sweep
    // measures the crossover. Each iteration:
    //   - Allocates an N-page DmaPool buffer (8 blocks per page).
    //   - Builds a single ADMA2 tran+end descriptor of N*4096 bytes
    //     (well under the 65535-byte single-descriptor cap).
    //   - Runs CMD18 + Auto-CMD23 + DMA, polls TRANSFER_COMPLETE.
    //   - Compares against PIO read_blocks_pio with the same block count.
    //
    // The buffer page is leaked each iteration (no PageSet handle close
    // path exercised in this driver) — acceptable for a one-shot boot
    // probe; the M6 plan's DMA pool sizing (512 pages) covers it with
    // room to spare.
    perf_sweep_adma_vs_pio(mmio_va, freq, desc_va, desc_phys);

    sys_exit();
}

/// Helper: run one ADMA2 multi-block read of `n_blocks` starting at
/// LBA 0 and return elapsed ticks. Allocates a fresh DmaPool buffer
/// (does NOT free — driver is a one-shot probe). Uses CMD18 with
/// Auto-CMD23 and a single descriptor covering the full transfer.
unsafe fn adma2_multiblock_read(
    mmio_va: u64,
    n_blocks: u16,
    n_pages: u64,
    desc_va: u64,
    desc_phys: u64,
) -> Result<u64, &'static str> {
    let buf_ps = sys_alloc_dma_pages(n_pages).map_err(|_| "alloc_dma_pages")?;
    let buf_phys = sys_query_pageset_phys(buf_ps, 0).map_err(|_| "query_phys")?;
    let buf_va = VMEM.alloc(n_pages as usize).ok_or("VA alloc")?;
    if !sys_map_pages(buf_ps, buf_va, MapMemoryAttribute::NormalNonCacheable).is_ok() {
        return Err("map_pages NC");
    }
    if buf_phys >= (1u64 << 32) {
        return Err("phys > 4GiB");
    }
    let bytes = (n_blocks as u32) * 512;
    if bytes > 65535 {
        return Err("descriptor length > 65535");
    }

    // One descriptor covers the whole multi-block transfer. Descriptor
    // table from the single-block path is reused (still mapped at
    // desc_va); overwrite its 8 bytes for this sweep iteration.
    let desc = adma2_tran_end_descriptor(buf_phys as u32, bytes as u16);
    ptr::write_volatile(desc_va as *mut u64, desc);
    asm!("dsb sy", options(nomem, nostack));

    sdhci_write32(mmio_va, SDHCI_ADMA_ADDRESS, desc_phys as u32);
    sdhci_write32(mmio_va, SDHCI_ARGUMENT2, n_blocks as u32);

    if poll_until_clear_32(
        mmio_va, SDHCI_PRESENT_STATE,
        SDHCI_CMD_INHIBIT | SDHCI_DAT_INHIBIT,
        Nanos::from_millis(100),
    ).is_err() {
        return Err("inhibit stuck");
    }
    sdhci_write16(mmio_va, SDHCI_BLOCK_SIZE, 512);
    sdhci_write16(mmio_va, SDHCI_BLOCK_COUNT, n_blocks);
    sdhci_write16(mmio_va, SDHCI_TRANSFER_MODE,
        SDHCI_TRNS_READ | SDHCI_TRNS_DMA | SDHCI_TRNS_BLK_CNT_EN
            | SDHCI_TRNS_MULTI | SDHCI_TRNS_AUTO_CMD23);
    sdhci_write32(mmio_va, SDHCI_ARGUMENT, 0);

    let cmd18 = sd_command_word(
        SdCommand::ReadMultipleBlock.index(),
        SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
    );

    let freq = cntfreq_hz();
    let t0 = monotonic_now();
    sdhci_write16(mmio_va, SDHCI_COMMAND, cmd18);

    // Poll CMD_COMPLETE
    let cmd_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    loop {
        let status = sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err = sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_CMD_COMPLETE);
            puts("    CMD18 controller error: ");
            put_error_int_status(err);
            puts("\n");
            return Err("cmd error");
        }
        if status & SDHCI_INT_CMD_COMPLETE != 0 {
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_CMD_COMPLETE);
            break;
        }
        if monotonic_now() >= cmd_deadline { return Err("cmd_complete timeout"); }
        core::hint::spin_loop();
    }

    // Poll TRANSFER_COMPLETE — for multi-block ADMA, the controller
    // signals once after the FULL descriptor's transfer ends.
    let xfer_deadline = monotonic_now().deadline_in(Nanos::from_millis(500), freq);
    loop {
        let status = sdhci_read16(mmio_va, SDHCI_NORMAL_INT_STATUS);
        if status & SDHCI_INT_ERROR != 0 {
            let err = sdhci_read16(mmio_va, SDHCI_ERROR_INT_STATUS);
            sdhci_write16(mmio_va, SDHCI_ERROR_INT_STATUS, 0xFFFF);
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS,
                SDHCI_INT_ERROR | SDHCI_INT_DATA_COMPLETE);
            puts("    ADMA2 multi data error: ");
            put_error_int_status(err);
            puts("\n");
            return Err("data error");
        }
        if status & SDHCI_INT_DATA_COMPLETE != 0 {
            sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, SDHCI_INT_DATA_COMPLETE);
            break;
        }
        if monotonic_now() >= xfer_deadline { return Err("transfer_complete timeout"); }
        core::hint::spin_loop();
    }
    let t1 = monotonic_now();
    Ok(t1.0.saturating_sub(t0.0))
}

/// Sweep ADMA2 and PIO across block counts 1, 4, 8, 16, 32, 64, 127 to
/// find the crossover where ADMA2 starts winning (or fails to). Logs
/// elapsed in microseconds per (count, mode) pair. Caps at 127 blocks
/// because that's the single-descriptor length max (65024 bytes;
/// 65535 / 512 = 127).
///
/// Each ADMA iteration overwrites the descriptor table we set up
/// earlier — reads VMEM_DESC_VA / VMEM_DESC_PHYS the single-block
/// path stashed. Each iteration leaks one buffer PageSet (one-shot
/// driver — pool is sized 512 pages = 2 MiB, total sweep usage
/// 1+4+8+16+32+64+127 = 252 pages, fits with headroom).
/// PIO compare buffer for the perf sweep. 127 blocks × 512 bytes = ~65 KiB —
/// way too large for the userspace stack. Static BSS storage; driver is
/// single-threaded so no concurrent access. One-shot use at boot.
///
/// SAFETY: single-threaded one-shot driver — perf_sweep_adma_vs_pio is
/// the sole accessor and runs once during _start.
static mut PIO_SWEEP_BUF: [[u32; 128]; 127] = [[0u32; 128]; 127];

fn perf_sweep_adma_vs_pio(
    mmio_va: u64,
    freq: lockjaw_userlib::time::TickFreq,
    desc_va: u64,
    desc_phys: u64,
) {
    let counts: &[u16] = &[1, 4, 8, 16, 32, 64, 127];

    // Re-select ADMA2-32 in HOST_CONTROL_1. The single-block M6 path
    // restored DMA_SEL=SDMA at its end so PIO read_block_pio could
    // run cleanly; the sweep needs it back at ADMA2 for the ADMA
    // arm of each comparison.
    unsafe {
        let hc = sdhci_read8(mmio_va, SDHCI_HOST_CONTROL);
        let hc_new = (hc & !SDHCI_HOST_CTRL_DMA_SEL_MASK)
            | SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32;
        sdhci_write8(mmio_va, SDHCI_HOST_CONTROL, hc_new);
    }

    puts("[EMMC2:PERF] block-count sweep (ADMA2 / PIO) us\n");
    for &n in counts {
        // n_pages = ceil(n_blocks * 512 / 4096) = ceil(n / 8)
        let n_pages = ((n as u64) * 512 + 4095) / 4096;

        let adma_ticks = match unsafe { adma2_multiblock_read(mmio_va, n, n_pages, desc_va, desc_phys) } {
            Ok(t) => t,
            Err(e) => {
                puts("[EMMC2:PERF]  n="); put_decimal(n as u64);
                puts(" ADMA2 FAILED: "); puts(e); puts("\n");
                continue;
            }
        };

        // PIO compare: pass a sub-slice of the static buffer (64 KiB
        // total in BSS — too large for the userspace stack).
        let t2 = monotonic_now();
        let pio_result = unsafe {
            #[allow(static_mut_refs)]
            read_blocks_pio(mmio_va, 0, &mut PIO_SWEEP_BUF[..n as usize])
        };
        let t3 = monotonic_now();
        let pio_ticks = t3.0.saturating_sub(t2.0);
        if let Err(e) = pio_result {
            puts("[EMMC2:PERF]  n="); put_decimal(n as u64);
            puts(" PIO FAILED: "); report_pio_error(mmio_va, "PERF", "CMD18 PIO", e);
            continue;
        }

        let adma_us = ticks_to_us(adma_ticks, freq);
        let pio_us = ticks_to_us(pio_ticks, freq);
        puts("[EMMC2:PERF]  n="); put_decimal(n as u64);
        puts(" bytes="); put_decimal((n as u64) * 512);
        puts(" ADMA="); put_decimal(adma_us);
        puts("us PIO="); put_decimal(pio_us);
        puts("us ratio_x100="); put_decimal((adma_us * 100) / pio_us.max(1));
        puts("\n");
    }
}

/// Helper: raw timer ticks → microseconds via TickFreq. Drops fractional
/// microseconds (round-down); fine for the millisecond-scale timings
/// the M6 diagnostic prints.
fn ticks_to_us(ticks: u64, freq: lockjaw_userlib::time::TickFreq) -> u64 {
    // us = ticks * 1_000_000 / freq. Order matters: multiply first to
    // keep precision for small tick counts. Saturating against
    // overflow because ticks * 1e6 can exceed u64 for very long
    // intervals; for M6 timings (single-digit ms) it's safe.
    ticks.saturating_mul(1_000_000) / freq.0
}

/// Centralized error-to-log dispatch for PIO operations. `phase` is the
/// milestone tag (e.g. "M4", "M5") and `op` is a human-readable command
/// description. Reading PRESENT_STATE in DataTimeout makes DAT_INHIBIT /
/// CMD_INHIBIT visible at the failure point.
fn report_pio_error(mmio_va: u64, phase: &str, op: &str, e: PioError) {
    puts("[EMMC2:");
    puts(phase);
    puts("] ");
    puts(op);
    puts(" FAILED: ");
    match e {
        PioError::InhibitStuck   => puts("CMD/DAT_INHIBIT stuck"),
        PioError::CmdError(bits) => put_error_int_status(bits),
        PioError::NoResponse     => puts("no CMD_COMPLETE within deadline"),
        PioError::DataError(bits) => put_error_int_status(bits),
        PioError::DataTimeout => {
            let pstate = unsafe { sdhci_read32(mmio_va, SDHCI_PRESENT_STATE) };
            puts("BUF_R/W_READY timeout PRESENT_STATE=");
            put_hex(pstate as u64);
        }
        PioError::XferTimeout => puts("TRANSFER_COMPLETE timeout"),
    }
    puts("\n");
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
