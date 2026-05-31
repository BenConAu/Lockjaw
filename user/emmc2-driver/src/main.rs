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
// `grep -rn 'allow(unsafe_code)' user/emmc2-driver/src/`
// MUST return nothing.
#![deny(unsafe_code)]

use lockjaw_userlib::*;
use lockjaw_userlib::clock::{ClockClient, ClockError};
use lockjaw_userlib::block::{
    BlockEngine, BlockError, BlockInfo, run_block_server,
};
use lockjaw_userlib::driver_runtime::{
    standard_driver_init_level, LevelDriverCtx, LevelDriverInitError,
};
use lockjaw_userlib::dma::{
    alloc_dma_backing_dma_pool, close_dma_backing, BorrowedDmaMapping,
    DmaBacking, DmaMappingView, DmaPoolOrigin, OwnedDmaMapping,
};
use lockjaw_userlib::dma_transfer::{
    run_dma_transfer, DmaCompletion, DmaDir, DmaTransferError, Immediate,
};
use lockjaw_mmio::region::MappedRegs;
use lockjaw_regs::sdhci::{
    Command, ErrorIntSignalEnable, ErrorIntStatus, ErrorIntStatusEnable,
    HostControlDmaSel, NormalIntSignalEnable, NormalIntStatus, NormalIntStatusEnable,
    PowerControlBusVoltage, PresentState, Sdhci, TransferMode,
};
// O2: temporary escape for the SdhciCommandInit migration window.
// Every gated-setter call site mints a token via this fn until O4/O5
// migrate them to `SdhciCommandInit::open()` and the init helpers in
// `lockjaw_userlib::sdhci`. O5 deletes `__temp_unguarded_mint` from
// `lockjaw-regs` and the temporary xtask exemption that allows the
// emmc2-driver crate to name it.
#[allow(deprecated)]
use lockjaw_regs::sdhci::__temp_unguarded_mint;
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_userlib::time::{cntfreq_hz, monotonic_now, sleep_for, Nanos};
// `monotonic_now` returns `MonoTicks`, which is `Ord`; the comparisons in the
// poll helpers don't need the type imported by name. `sleep_for` is used only
// for pure-time waits (regulator settle, post-clock idle) — never inside
// status polls, which busy-wait with `core::hint::spin_loop()` between MMIO
// reads to avoid quantizing 200µs hardware events to a 10ms scheduler tick.
use lockjaw_types::device::BCM2711_EMMC2_HASH;
use lockjaw_types::sdhci::{
    Capabilities, CsdV2,
    CMD8_IF_COND_ARG, ACMD41_ARG_HCS,
    SDHCI_SPEC_300,
    SDHCI_INT_CMD_TIMEOUT, SDHCI_INT_CMD_CRC,
    SDHCI_INT_CMD_END_BIT, SDHCI_INT_CMD_INDEX,
    SDHCI_INT_DATA_TIMEOUT, SDHCI_INT_DATA_CRC, SDHCI_INT_DATA_END_BIT,
    SDHCI_CMD_CRC, SDHCI_CMD_INDEX, SDHCI_CMD_DATA,
    SDHCI_CMD_RESP_SHORT,
    SDHCI_TRNS_READ, SDHCI_TRNS_DMA, SDHCI_TRNS_BLK_CNT_EN, SDHCI_TRNS_MULTI, SDHCI_TRNS_AUTO_CMD23,
    SdCommand, compute_clock_divisor, sd_command_word,
};
use lockjaw_types::sdhci::response::{R0, R1, R1b, R2, R3, R6, R7};
use lockjaw_types::wire::sdhci::Adma2Descriptor;
// O4: framework-side SDHCI envelope + init helpers. ID-phase + init
// callsites flow through this surface; data-phase still uses the
// O2 temp-mint imports (removed in O5).
use lockjaw_userlib::sdhci::{
    self as sdhci_lib, SdhciCommandError, SdhciCommandInit, SdhciInitError,
};

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

/// Deadline-bounded busy-wait until `pred()` returns true. Returns
/// `Err(())` if the deadline expires first.
///
/// Single closure-based helper that survives the per-width typed-
/// accessor transition (P9.5 / P9.6 / P9.8). Pre-typed-accessor the
/// driver had three per-width variants (poll_until_clear_8 / set_16 /
/// clear_32) that each baked in a raw `(offset, mask, read_width)`
/// triple; once the predicates moved to typed snapshots
/// (sdhci.software_reset().contains(...),
/// sdhci.clock_control().int_clk_stable(), etc.) the per-width
/// versions became four-line copies of this same loop with one
/// differing line. The closure version collapses them.
///
/// Busy-poll with `core::hint::spin_loop()` between checks rather
/// than yielding via `sleep_for`: hardware events of interest happen
/// in microseconds to milliseconds (SDHCI command response,
/// SOFTWARE_RESET clear, INT_CLK_STABLE), and the scheduler's
/// per-tick granularity (~10 ms) would quantize those waits to far
/// longer than the actual event budget.
fn poll_until<F: FnMut() -> bool>(mut pred: F, timeout: Nanos) -> Result<(), ()> {
    let freq = cntfreq_hz();
    let deadline = monotonic_now().deadline_in(timeout, freq);
    loop {
        if pred() {
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
/// Deadline-bounded busy poll (see the status-bit-poll comment
/// block above for why we busy-spin between MMIO reads instead of
/// yielding via `sleep_for`). The earlier 1_000_000-iteration spin
/// tied correctness to CPU clock and codegen; with a real-time
/// deadline the timeout is what it says regardless of platform.
// O4: local soft_reset_all deleted — emmc2_entry now calls
// `lockjaw_userlib::sdhci::soft_reset_all(sdhci)` which mints the
// internal token, performs the W1S + 200ms poll, and returns
// `Result<(), SdhciInitError>`. The framework helper IS the local
// shape: same SW_RST_ALL write, same poll predicate, same deadline.

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
// O4: local configure_clock deleted — emmc2_entry now calls
// `lockjaw_userlib::sdhci::configure_clock(sdhci, base_hz, target_hz)`
// which performs the exact same sequence (gate SD_CLK → write
// divisor + INT_CLK_EN → 100ms INT_CLK_STABLE poll → re-enable
// SD_CLK) under an internal token.

// ---------------------------------------------------------------------------
// Command issue
// ---------------------------------------------------------------------------

// O4: `issue_command` + `CmdResult` deleted. The ID-phase command
// pattern is now `SdhciCommandInit::open(sdhci).issue_no_data::<R>(cmd,
// arg)` from `lockjaw_userlib::sdhci`. The envelope wraps the exact
// poll sequence the old issue_command implemented (CMD_INHIBIT poll
// → ARGUMENT → TRANSFER_MODE+COMMAND single-store → NORMAL_INT_STATUS
// poll → W1C → typed response decode) with byte-identical wire effects,
// parameterized over the `ResponseShape` (R0/R1/R1b/R2/R3/R6/R7). The
// local `issue_or_die` helper below shapes the SdhciCommandError into
// the existing log-and-sys_exit error pattern for the strict ID-phase
// commands.

/// Issue an ID-phase command via the operation envelope and either
/// return its decoded response or log + sys_exit on failure. The
/// strict pattern used by every M2/M3 command except CMD0 (which is
/// lenient — see emmc2_entry).
fn issue_or_die<R: lockjaw_types::sdhci::response::ResponseShape>(
    sdhci: &Sdhci,
    label: &str,
    cmd: SdCommand,
    arg: u32,
) -> R::Decoded {
    match SdhciCommandInit::open(sdhci).issue_no_data::<R>(cmd, arg) {
        Ok((decoded, _)) => decoded,
        Err(SdhciCommandError::InhibitStuck { .. }) => {
            puts("[EMMC2:IDPHASE] ");
            puts(label);
            puts(" FAILED: CMD_INHIBIT did not clear before deadline\n");
            sys_exit()
        }
        Err(SdhciCommandError::ControllerError { err_int_status }) => {
            puts("[EMMC2:IDPHASE] ");
            puts(label);
            puts(" FAILED: ");
            put_error_int_status(err_int_status);
            puts("\n");
            sys_exit()
        }
        Err(SdhciCommandError::NoResponse) => {
            puts("[EMMC2:IDPHASE] ");
            puts(label);
            puts(" FAILED: no CMD_COMPLETE/ERROR within 1s\n");
            sys_exit()
        }
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
    /// PRESENT_STATE.DAT_INHIBIT did not clear within 10 ms after
    /// DATA_COMPLETE fired (B4.1). The DATA_COMPLETE interrupt
    /// signals card-side transfer end, but PRESENT_STATE.DAT_INHIBIT
    /// is the controller's own "data path genuinely idle / writes
    /// drained" signal. Polling it after DATA_COMPLETE is what makes
    /// a standalone single-block read produce committed DRAM by the
    /// time it returns — without it, isolated reads (e.g. the
    /// emmc2 selftest) can race ahead of the controller's outbound
    /// AXI writes.
    DatInhibitStuck { present_state: u32 },
    /// Allocating + mapping the descriptor table via
    /// `OwnedDmaMapping::<DmaPoolOrigin>::alloc_dma_pool()` failed
    /// (alloc, phys query, VA reservation, or map — the typed mapping
    /// collapses these into a single SyscallError).
    DescAllocFailed,
    /// The coherence envelope's descriptor clean (a `ToDevice` region
    /// sync, per-transfer after writing the descriptor and before
    /// kicking the controller) failed.
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
            Self::DatInhibitStuck { .. }       => "dat_inhibit stuck post-completion",
            Self::DescAllocFailed              => "desc alloc failed",
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
        Emmc2Error::InhibitStuck { present_state }
        | Emmc2Error::DatInhibitStuck { present_state } => {
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
        | Emmc2Error::DescSyncFailed => {}
    }
}

// ---------------------------------------------------------------------------
// Entry point — Tier-A escape valve (boot_stub! + manual
// standard_driver_init_level call).
// ---------------------------------------------------------------------------
//
// P9.4c: replaced the raw _start + sys_alloc_pages +
// sys_create_reply + bootstrap_endpoint + CMD_CLAIM_DEVICE +
// bind_irq_level + VMEM.alloc + sys_map_pages + unpack_clock_ref
// boilerplate with the Tier-A escape-valve shape: `boot_stub!`
// emits the `_start` thunk + `LOCKJAW_HASH_SECTION` static, and
// `emmc2_entry` calls `standard_driver_init_level::<Sdhci>` (P9.4b)
// directly so it can match on the specific failure variant.
//
// The framework helper owns:
//   - driver_bootstrap (the sys_alloc_pages + sys_create_reply +
//     bootstrap_endpoint sys_call dance) → server_ep / devmgr_ep /
//     reply_obj
//   - probe_by_hash → CMD_PROBE_DEVICE walks the DTB-derived device
//     list; returns ProbeError::NotFound when no device matches
//     BCM2711_EMMC2_HASH (the QEMU graceful-exit path).
//   - claim_typed::<Sdhci> → CMD_CLAIM_BY_ADDR followed by the VMEM
//     alloc + sys_map_pages + intra-page-offset MappedRegs<Sdhci>
//     construction. Driver source contains zero `unsafe {
//     MappedRegs::new(...) }` and zero CMD_CLAIM_DEVICE call sites.
//   - bind_irq_level(intid) → BoundIrq with wait_until + unmask.
//   - unpack_clock_ref(claim[3]) → ctx.clock_ref (P9.4a).
//
// emmc2_entry's body keeps only the emmc2-specific protocol:
// ClockClient::acquire (driver-policy decision — which clock?), then
// the soft_reset → configure_clock → CMD0..CMD7 → engine init → run
// sequence, which is identical to pre-P9.4c (modulo sourcing the
// typed `&Sdhci` from ctx.regs.regs() rather than from a raw u64
// derived from sys_map_pages). Per-width typed accessors land in
// P9.5-P9.8.
//
// Why Tier-A escape valve and not the Tier-C `driver_main!(level =
// true)` macro arm: the integration-test gate requires a specific
// failure log line when bcm2711-emmc2 is absent on QEMU
// (`[EMMC2:INIT] no bcm2711-emmc2 device on this platform (QEMU)`).
// `driver_main!`'s macro arm hardcodes a generic `<name>: init
// failed` on the Err arm, which wouldn't satisfy that gate.
// Matching on `LevelDriverInitError::Probe` directly in driver
// source lets the driver emit its own gated log line — same pattern
// cprman / ramfb use for their probe-failure handling.
boot_stub! {
    hash = LOCKJAW_SOURCE_HASH,
    main = emmc2_entry,
}

fn emmc2_entry() -> ! {
    puts("emmc2: starting\n");
    let ctx: LevelDriverCtx<Sdhci> = match standard_driver_init_level::<Sdhci>(
        "emmc2", BCM2711_EMMC2_HASH,
    ) {
        Ok(c) => c,
        Err(LevelDriverInitError::Probe(_)) => {
            // QEMU virt has no bcm2711-emmc2 — exit cleanly so the
            // integration test can assert the graceful-fail path.
            puts("[EMMC2:INIT] no bcm2711-emmc2 device on this platform (QEMU); exiting\n");
            sys_exit();
        }
        Err(_) => {
            puts("emmc2: framework init failed (bootstrap / claim / bind_irq_level)\n");
            sys_exit();
        }
    };

    // Reborrow the typed MMIO handle as `&Sdhci` for the ID-phase
    // commands (issue_no_data via SdhciCommandInit) and init helpers
    // (soft_reset_all / configure_clock / set_power_on_and_settle /
    // set_int_enable_masks / set_timeout_dat_counter / set_bus_width_4bit).
    // `ctx.regs` is the MappedRegs<Sdhci> the framework claim_typed
    // handed back; `regs()` yields `&T` for as long as ctx lives.
    let sdhci = ctx.regs.regs();

    // The DTB binding for bcm2711-emmc2 includes a clocks reference;
    // M0a's parser populated it and the device-manager packed it into
    // claim[3], which P9.4a's claim_typed unpacked into
    // ctx.clock_ref. If it's absent the driver can't proceed safely
    // (the controller's base clock is whatever VC firmware last set,
    // which may be wrong). Surface and exit rather than operate on a
    // clock we don't own.
    let clock_ref = match ctx.clock_ref {
        Some(c) => c,
        None => {
            puts("emmc2: bcm2711-emmc2 DTB node has no clocks property — refusing to proceed\n");
            sys_exit();
        }
    };

    // Acquire the clock handle through device-manager (M0c proxy).
    // M2 calls set_rate / enable to drive the SDHCI base clock. The
    // ClockClient is held in scope; drop closes the Endpoint per RAII.
    let clk = match ClockClient::acquire(
        ctx.devmgr_ep, ctx.reply_obj, clock_ref.controller_phandle, clock_ref.clock_id,
    ) {
        Ok(c) => c,
        Err(e) => {
            puts("emmc2: clock acquire FAILED: ");
            put_clock_error(e);
            puts("\n");
            sys_exit();
        }
    };

    // Server endpoint passthrough (renamed to match the pre-P9.4c
    // local). Init exports the same handle to other processes that
    // need block storage on Pi 4B.
    let blk_srv_ep = ctx.server_ep;

    // Soft-reset the controller. SW_RST_ALL puts every internal block
    // back to the post-power-on state; required before any further
    // configuration touches CLOCK_CONTROL or POWER_CONTROL.
    if let Err(SdhciInitError::ResetTimeout) = sdhci_lib::soft_reset_all(sdhci) {
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
    // SIGNAL_ENABLE (NORMAL_INT_SIGNAL_ENABLE 0x038 /
    // ERROR_INT_SIGNAL_ENABLE 0x03A) is intentionally NOT touched
    // here. STATUS_ENABLE gates whether the controller latches the
    // bit; SIGNAL_ENABLE gates whether a latched bit asserts the
    // GIC line. The ID phase (CMD0/8/41/2/3/7) below uses polling
    // — it reads NORMAL_INT_STATUS directly without going through
    // the GIC — so STATUS_ENABLE must be on and SIGNAL_ENABLE must
    // stay off, otherwise the controller would assert the GIC line
    // before any IRQ binding exists and the kernel would have no
    // notification to signal.
    //
    // After ID phase completes, Emmc2BlockEngine::new (1) clears any
    // STATUS bits that the ID-phase polling left latched (notably
    // DATA_COMPLETE from CMD7's R1b busy release, which issue_command
    // does not W1C) and then (2) flips SIGNAL_ENABLE on for
    // CMD_COMPLETE / DATA_COMPLETE / ERROR. From that point the
    // data-path CMD17 in adma2_single_block_read waits on the IRQ
    // via lockjaw_userlib::irq::BoundIrq::wait_until rather than
    // polling. See Engine::new for the full sequencing rationale.
    // P9.6: typed STATUS_ENABLE writes. Enabling every bit
    // (0xFFFF) is the existing behaviour — STATUS_ENABLE gates
    // which bits the controller latches, and we want to see
    // everything during the ID phase polling. SIGNAL_ENABLE
    // stays off here (Engine::new flips it on after ID phase).
    sdhci_lib::set_int_enable_masks(
        sdhci,
        NormalIntStatusEnable(0xFFFF),
        ErrorIntStatusEnable(0xFFFF),
    );

    // Read CAPABILITIES (low 32 bits at 0x040) and CAPABILITIES_HI
    // (high 32 bits at 0x044). Decoded view lives in lockjaw-types
    // so the bit layout has host tests; the driver just dispatches
    // the two volatile reads.
    let caps_lo = sdhci.capabilities().bits();
    let caps_hi = sdhci.read_capabilities_hi();
    let caps = Capabilities::decode(caps_lo, caps_hi);

    // HOST_VERSION (0x0fe) is a u16: bits[7:0] = spec version
    // (0=v1, 1=v2, 2=v3), bits[15:8] = vendor version. SDHCI_SPEC_300
    // is the constant 2. This is distinct from bit 28 of CAPABILITIES
    // (64-bit addressing support, a per-capability flag, not the spec
    // revision number).
    // P9.6: typed read_host_version (regspec does not declare named
    // bit fields on this register, so the generated accessor returns
    // raw u16). The bit-7:0 spec-version decoding stays here.
    let host_version = sdhci.read_host_version();
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
    // `set_power_on_and_settle` does the typed PowerControl write +
    // the 2ms regulator-settle sleep that the SD spec implies (Pi 4B
    // has a fixed 3.3 V rail; ~1 ms is enough). Tick-quantized —
    // actual wait is ≥ one scheduler tick (~10 ms), trivially over
    // spec minimum.
    sdhci_lib::set_power_on_and_settle(sdhci, PowerControlBusVoltage::V33);
    // Set max data timeout (TMCLK × 2^27 = 0x0E).
    sdhci_lib::set_timeout_dat_counter(sdhci, 0x0E);

    // Enable the SDHCI internal clock at ID-mode rate (≤ 400 kHz), wait
    // for the oscillator to stabilise, then gate the clock to the card.
    if sdhci_lib::configure_clock(sdhci, actual_base_hz, 400_000).is_err() {
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
    // still useful diagnostic context for what follows. CMD0's lenient
    // handling is the one ID-phase divergence from `issue_or_die`.
    match SdhciCommandInit::open(sdhci).issue_no_data::<R0>(SdCommand::GoIdleState, 0) {
        Ok(_) => puts("[EMMC2:IDPHASE] CMD0 acknowledged\n"),
        Err(SdhciCommandError::InhibitStuck { .. }) => {
            puts("[EMMC2:IDPHASE] CMD0: CMD_INHIBIT stuck (controller not responding)\n")
        }
        Err(SdhciCommandError::ControllerError { err_int_status }) => {
            puts("[EMMC2:IDPHASE] CMD0 controller error: ");
            put_error_int_status(err_int_status);
            puts("\n");
        }
        Err(SdhciCommandError::NoResponse) => {
            puts("[EMMC2:IDPHASE] CMD0: no CMD_COMPLETE within 1s (suspicious)\n")
        }
    }

    // CMD8 — SEND_IF_COND. Arg: VHS=1 (2.7–3.6 V) + check pattern 0xAA.
    // R7 response echoes VHS and the check pattern back. A correct echo
    // proves the card is SD Physical Layer Spec v2.0+ (SDv2+). Pre-SDv2
    // cards don't respond to CMD8; UHS and SDXC cards need it for ACMD41.
    let cmd8_echo: u32 = issue_or_die::<R7>(sdhci, "CMD8", SdCommand::SendIfCond, CMD8_IF_COND_ARG);

    // CMD8 R7: bits[11:8] = voltage accepted (echoes VHS=1), bits[7:0]
    // = check pattern (echoes 0xAA). Together bits[11:0] = 0x1AA.
    if cmd8_echo & 0xFFF != 0x1AA {
        puts("emmc2: CMD8 bad echo=");
        put_hex((cmd8_echo & 0xFFF) as u64);
        puts("\n");
        sys_exit();
    }

    puts("[EMMC2:IDPHASE] CMD8 echo=0x1AA — card is SDv2+ (clk via cprman)\n");

    // -----------------------------------------------------------------------
    // M3 — Full SD identification: ACMD41 → CMD2 → CMD3 → CMD9 →
    //       CMD7 → ACMD6 → HOST_CONTROL → 25 MHz
    // -----------------------------------------------------------------------

    // ACMD41 loop — SD spec § 4.2.3.1. Each iteration: CMD55 (sets
    // APP_CMD mode for the next command), then ACMD41 with HCS=1.
    // Loops until OCR.power_up_done = true (card finished init) or the
    // 1-second timeout expires.
    //
    // CMD55 in broadcast mode (arg=0, no RCA assigned yet). ACMD41
    // uses R3 (no CRC/index — spec §4.9.3); the typed envelope's
    // `R3::FLAGS = SDHCI_CMD_RESP_SHORT` encodes that.
    let freq = cntfreq_hz();
    let acmd41_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
    let ocr = loop {
        let _: u32 = issue_or_die::<R1>(sdhci, "CMD55", SdCommand::AppCmd, 0);
        let ocr = issue_or_die::<R3>(sdhci, "ACMD41", SdCommand::SdSendOpCond, ACMD41_ARG_HCS);
        if ocr.power_up_done {
            break ocr;
        }
        if monotonic_now() >= acmd41_deadline {
            puts("[EMMC2:IDPHASE] ACMD41 timeout: card never became ready\n");
            sys_exit();
        }
        // Short delay between retries — SD spec doesn't require one
        // but gives the card breathing room between polls.
        let _ = sleep_for(Nanos::from_millis(10));
    };
    puts("[EMMC2:IDPHASE] ACMD41 ready ccs=");
    put_decimal(ocr.ccs as u64);
    puts("\n");

    // CMD2 — ALL_SEND_CID. R2 (136-bit) carries the card's unique CID.
    // We log the success but don't decode CID in M3 — capacity and
    // addressing come from CSD (CMD9). R2 returns the raw four-word
    // view; we discard it because the CID decoder isn't on the M3 path.
    let _cid: [u32; 4] = issue_or_die::<R2>(sdhci, "CMD2", SdCommand::AllSendCid, 0);
    puts("[EMMC2:IDPHASE] CMD2 CID received\n");

    // CMD3 — SEND_RELATIVE_ADDR. Card publishes its RCA; the host
    // stores it and uses it to address the card from here on. R6
    // response: rca (u16) + 16 bits of card status; the typed envelope
    // splits both into the `R6Response` struct.
    let r6 = issue_or_die::<R6>(sdhci, "CMD3", SdCommand::SendRelativeAddr, 0);
    let rca: u16 = r6.rca;
    puts("[EMMC2:IDPHASE] CMD3 rca=");
    put_hex(rca as u64);
    puts("\n");

    // CMD9 — SEND_CSD. R2 (136-bit) response carries the CSD register.
    // CSD v2 (SDHC/SDXC) encodes capacity in the C_SIZE field; the pure
    // decoder in lockjaw-types computes capacity_bytes from the four
    // RESPONSE words. The R2 envelope returns the four-word view
    // directly — no separate manual RESPONSE_1..3 reads needed.
    let rca_arg = (rca as u32) << 16;
    let csd_resp: [u32; 4] = issue_or_die::<R2>(sdhci, "CMD9", SdCommand::SendCsd, rca_arg);
    let capacity_bytes: u64 = match CsdV2::decode(csd_resp) {
        Ok(csd) => csd.capacity_bytes,
        Err(e) => {
            puts("[EMMC2:IDPHASE] CMD9 CSD_STRUCTURE=");
            put_decimal(e.csd_structure as u64);
            puts(" (expected 1 for SDHC/SDXC)\n");
            sys_exit();
        }
    };

    // CMD7 — SELECT_CARD. Moves the card from Stand-by to Transfer
    // state. R1b: CMD_COMPLETE fires immediately; controller holds
    // CMD_COMPLETE until DAT0 deasserts. Wait for both CMD_INHIBIT and
    // DAT_INHIBIT before issuing the next command.
    let _: u32 = issue_or_die::<R1b>(sdhci, "CMD7", SdCommand::SelectCard, rca_arg);
    // Wait for DAT0 to deassert — the card signals "ready" by releasing it.
    if poll_until(
        || {
            let ps = sdhci.present_state();
            !ps.contains(PresentState::CMD_INHIBIT) && !ps.contains(PresentState::DAT_INHIBIT)
        },
        Nanos::from_millis(500),
    ).is_err() {
        puts("[EMMC2:IDPHASE] CMD7 DAT_INHIBIT did not clear (card busy timeout)\n");
        sys_exit();
    }
    puts("[EMMC2:IDPHASE] CMD7 card selected\n");

    // ACMD6 — SET_BUS_WIDTH. Switch the card to 4-bit DAT bus. Must
    // be preceded by CMD55 addressed to the selected card (RCA).
    let _: u32 = issue_or_die::<R1>(sdhci, "CMD55 (pre-ACMD6)", SdCommand::AppCmd, rca_arg);
    // ACMD6 argument: 0x2 = 4-bit bus (bits[1:0] = 10).
    let _: u32 = issue_or_die::<R1>(sdhci, "ACMD6", SdCommand::SetBusWidth, 0x2);
    // Mirror the 4-bit bus width in HOST_CONTROL_1 immediately after
    // the card acknowledges ACMD6 — the host side must match the card.
    // The framework helper `set_bus_width_4bit` does the typed
    // modify_host_control with the internal token.
    sdhci_lib::set_bus_width_4bit(sdhci);

    // Raise the SD bus clock from 400 kHz (ID mode) to 25 MHz (data
    // transfer mode). SDHCI spec § 3.2.4: disable SD_CLK_EN first,
    // then reprogram divisor — `sdhci_lib::configure_clock` does this.
    if sdhci_lib::configure_clock(sdhci, actual_base_hz, 25_000_000).is_err() {
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
    // Drop the `sdhci` borrow on ctx.regs before moving it into the
    // engine. After this point any further direct SDHCI access in
    // emmc2_entry must go through `engine.regs.regs()` (used by the
    // selftest below).
    let _ = sdhci;
    let engine = match Emmc2BlockEngine::new(ctx.regs, capacity_sectors, ctx.irq) {
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
    // Adopt the engine-owned buffer pageset for local inspection. The
    // BorrowedDmaMapping unmaps its VA on Drop but does NOT close the
    // pageset — engine.free_buffer (below) owns the close.
    let selftest_map = match BorrowedDmaMapping::map_existing(selftest_buf, 1) {
        Ok(m) => m,
        Err(_) => { puts("[EMMC2:BLK] selftest map FAILED\n"); halt(); }
    };
    // Diagnostic: print buf_phys (the mapping's first-page PA) and
    // desc_phys so the post-read buffer contents can be correlated
    // with the descriptor wire encoding the controller saw. The same
    // PA was queried in alloc_buffer to stash buf.pa; if these differ,
    // that would explain a silent zero-data read (DMA goes to a
    // different PA than the CPU's mapping).
    puts("[EMMC2:DIAG] buf_phys=");
    put_hex(selftest_map.pa());
    puts(" desc_phys=");
    put_hex(engine.desc_map.pa());
    puts("\n");
    // Zero only the 512-byte range this selftest reads + syncs (one
    // sector). Zeroing the whole mapped page would dirty cache lines
    // past the synced range that are never cleaned/invalidated before
    // the buffer is closed — a writeback hazard on the freed DmaPool page.
    selftest_map.zero_range(0, 512);
    if engine.read(0, 1, selftest_buf).is_err() {
        puts("[EMMC2:BLK] selftest read FAILED\n");
        halt();
    }
    // MBR boot signature lives at bytes 510-511 = 0x55 0xAA (little-endian).
    // Read each byte through the mapping's bounds-checked cell accessor.
    let lo = selftest_map.cell::<u8>(510).read() as u16;
    let hi = selftest_map.cell::<u8>(511).read() as u16;
    let sig = (hi << 8) | lo;
    if sig != 0xAA55 {
        puts("[EMMC2:BLK] selftest MBR signature=");
        put_hex(sig as u64);
        puts(" BAD (expected 0xAA55)\n");
        halt();
    }
    // Drop the borrowed mapping (unmaps the VA, returns it to VMEM).
    // The pageset stays open — engine.free_buffer closes it next.
    drop(selftest_map);
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
//     diagnosed (see docs/tracking/tech-debt.md: "CMD18+Auto-CMD23 cold-boot
//     validation"), no production path may dispatch through it.
//     `#[allow(dead_code)]` keeps it as a reference but the compiler
//     enforces no callers.
// ---------------------------------------------------------------------------

/// Single-block ADMA2 read of `sector`. CMD17 + READ|DMA only. No
/// MULTI, no BLK_CNT_EN, no Auto-CMD23 — those belong to the
/// multi-block path. Mirrors the M6 sub-commit 2b sequence.
///
/// SDHCI data-transfer completion — the device-done signal the
/// coherence envelope awaits between the `kick` and the post-transfer
/// invalidate. Wraps the IRQ-driven CMD_COMPLETE/DATA_COMPLETE wait
/// plus the B4.1 post-DATA_COMPLETE `DAT_INHIBIT` drain. This is the
/// one piece of the transfer only the driver knows; supplying it as a
/// `DmaCompletion` is what makes "invalidate before the device
/// finished" unrepresentable. Device-specific by nature — it lives in
/// the driver/family layer (the seed of a future `SdhciCommandInit`),
/// never in the generic `dma_transfer` module.
struct SdhciDataCompletion<'a> {
    sdhci: &'a Sdhci,
    bound_irq: &'a mut lockjaw_userlib::irq::BoundIrq,
}

impl DmaCompletion for SdhciDataCompletion<'_> {
    type Error = Emmc2Error;

    fn await_complete(self) -> Result<(), Emmc2Error> {
        #[allow(deprecated)]
        let tk = __temp_unguarded_mint(self.sdhci);
        // IRQ-driven completion (m7-irq-experiment graft).
        //
        // SDHCI is configured LEVEL_HIGH. Each IRQ delivery:
        //   1. Kernel ACKs+EOIRs in GIC, masks the intid (level-triggered),
        //      signals our notification (counter += 1).
        //   2. bound_irq.wait_until wakes the driver and advances the
        //      threshold by 1 so the next call expects a fresh IRQ.
        //   3. We read NORMAL_INT_STATUS to see what fired (CMD_COMPLETE,
        //      DATA_COMPLETE, ERROR — could be one, could be all).
        //   4. We clear the latched bits (W1C). DEVICE-SPECIFIC; stays
        //      here. The framework owns the wait/unmask wiring; the
        //      driver owns its own status-register protocol.
        //   5. bound_irq.unmask so the GIC re-enables delivery.
        //
        // The loop handles both "two separate IRQs" (CMD then DATA arrive
        // far apart) and "one IRQ with both bits set" (fast transfer where
        // CMD and DATA latch close together). cmd_complete_seen tracks
        // whether we've already cleared CMD_COMPLETE so an ERROR mid-data
        // is reported as DataError, not CmdError.
        //
        // Deadline-bounded wait: a wedged IRQ path would hang the block
        // server forever. BoundIrq::wait_until returns IrqWaitError::Timeout
        // when the deadline expires; we surface it as
        // CmdCompleteTimeout / TransferCompleteTimeout exactly like the
        // original polling shape did. Single 1-second budget covers both
        // CMD_COMPLETE (typical < 1 ms) and DATA_COMPLETE (typical < 100 ms
        // for one block).
        let freq = cntfreq_hz();
        let mut cmd_complete_seen = false;
        let deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
        loop {
            if self.bound_irq.wait_until(deadline).is_err() {
                return Err(if cmd_complete_seen {
                    Emmc2Error::TransferCompleteTimeout
                } else {
                    Emmc2Error::CmdCompleteTimeout
                });
            }

            // P9.6: typed NormalIntStatus snapshot + clear methods.
            let status = self.sdhci.normal_int_status(&tk);

            if status.contains(NormalIntStatus::ERROR) {
                let err_int_status = self.sdhci.error_int_status(&tk).bits();
                self.sdhci.clear_error_int_status(ErrorIntStatus(0xFFFF), &tk);
                self.sdhci.clear_normal_int_status(NormalIntStatus(0xFFFF), &tk);
                let _ = self.bound_irq.unmask();
                return Err(if cmd_complete_seen {
                    Emmc2Error::DataError { err_int_status }
                } else {
                    Emmc2Error::CmdError { err_int_status }
                });
            }

            if status.contains(NormalIntStatus::CMD_COMPLETE) {
                self.sdhci.clear_normal_int_status(NormalIntStatus::CMD_COMPLETE, &tk);
                cmd_complete_seen = true;
            }

            if status.contains(NormalIntStatus::DATA_COMPLETE) {
                self.sdhci.clear_normal_int_status(NormalIntStatus::DATA_COMPLETE, &tk);
                let _ = self.bound_irq.unmask();
                // Fall through to the B4.1 DAT_INHIBIT drain below before
                // returning Ok.
                break;
            }

            // Status had no relevant bits set (spurious wake, or
            // CMD_COMPLETE alone with DATA still in flight). Unmask the
            // GIC so the next IRQ can be delivered and loop back to
            // wait_until — the deadline still bounds total time spent.
            let _ = self.bound_irq.unmask();
        }

        // B4.1 — post-DATA_COMPLETE DAT_INHIBIT drain.
        //
        // DATA_COMPLETE signals card-side transfer end, but the BCM2711
        // Arasan controller can keep outbound AXI writes in flight to
        // DRAM for a tail period after asserting it. The post-completion
        // invalidate (the envelope's sync_for_cpu) only orders CPU-cache
        // operations; it does not arbitrate against the controller's
        // outstanding bus writes. Returning Ok before those writes have
        // committed lets the caller (e.g. emmc2 selftest) read the buffer
        // and see pre-DMA zeros.
        //
        // PRESENT_STATE.DAT_INHIBIT is the controller's own "data
        // path genuinely idle" bit: it stays set while the data path
        // has any outstanding activity — including the post-DATA_COMPLETE
        // AXI write tail. Polling it here forces the read call to wait
        // for the controller to actually drain.
        //
        // Pre-B4.1 history: the existing pre-command CMD_INHIBIT |
        // DAT_INHIBIT poll at the top of adma2_single_block_read (kept in
        // place as defensive double-coverage) implicitly drained the
        // PREVIOUS transfer on every subsequent read — which is why long
        // FAT32 / posix read chains worked while the standalone selftest
        // failed. M7-era diagnostic dumps that "fixed" the selftest
        // worked for the same reason: their MMIO reads serialised
        // against the controller's outstanding writes. This is the
        // principled replacement.
        //
        // 10 ms deadline matches the plan's B4.1 budget; the actual
        // drain is microseconds in normal operation.
        if poll_until(
            || !self.sdhci.present_state().contains(PresentState::DAT_INHIBIT),
            Nanos::from_millis(10),
        ).is_err() {
            return Err(Emmc2Error::DatInhibitStuck {
                present_state: self.sdhci.present_state().bits(),
            });
        }

        Ok(())
    }
}

/// `desc_map` is the `OwnedDmaMapping` backing the descriptor table.
/// The coherence envelope cleans the descriptor write down to DRAM
/// (a `ToDevice` region) before the controller's DMA fetches it
/// (cacheable-DMA migration C1: the descriptor mapping is Normal
/// Cacheable post-migration, so a `dsb sy` alone is not enough to
/// flush the cache line — the envelope's clean does `dc cvac` + dsb).
fn adma2_single_block_read(
    sdhci: &Sdhci,
    sector: u64,
    buf_phys: u64,
    desc_map: &OwnedDmaMapping<DmaPoolOrigin>,
    bound_irq: &mut lockjaw_userlib::irq::BoundIrq,
) -> Result<(), Emmc2Error> {
    #[allow(deprecated)]
    let tk = __temp_unguarded_mint(sdhci);
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
    if poll_until(
        || {
            let ps = sdhci.present_state();
            !ps.contains(PresentState::CMD_INHIBIT) && !ps.contains(PresentState::DAT_INHIBIT)
        },
        Nanos::from_millis(100),
    ).is_err() {
        return Err(Emmc2Error::InhibitStuck {
            present_state: sdhci.present_state().bits(),
        });
    }

    // One descriptor covers the whole 512-byte transfer. Same descriptor
    // table memory is reused across calls; the controller has either not
    // started yet (first call) or has fully released (inhibit cleared
    // above), so we can safely overwrite the previous descriptor.
    // Typed Adma2Descriptor wire DTO (tran_end = VALID | END | ACT_TRAN,
    // a construction-safe shape per lockjaw_types::sdhci) written through
    // the OwnedDmaMapping's `cell()` accessor — bounds-checked and
    // alignment-checked, so the write is fully safe (this removed the
    // lone remaining `DmaCell::at` unsafe on this path).
    desc_map
        .cell::<Adma2Descriptor>(0)
        .write(Adma2Descriptor::tran_end(buf_phys as u32, 512));
    // Coherence envelope: clean the descriptor down to DRAM (a
    // `ToDevice` region — post-C1 the descriptor mapping is Normal
    // Cacheable, so the clean does `dc cvac` + dsb to write the line
    // back before the controller's DMA fetches it), then `kick` the
    // controller, then await the SDHCI data completion (IRQ wait +
    // B4.1 drain). The framework owns the clean -> kick -> await
    // ordering; this driver no longer hand-sequences sync calls.
    let desc_region = desc_map.dma_region(0, 8, DmaDir::ToDevice);
    run_dma_transfer(
        &[desc_region],
        SdhciDataCompletion { sdhci, bound_irq },
        || -> Result<(), Emmc2Error> {
            // Re-select ADMA2-32 in HOST_CONTROL_1. Preserve the 4-bit bus
            // width bit (set during M3 ACMD6); replace the DMA_SEL field
            // bits only. Idempotent (engine.new already set it) but kept for
            // safety against any other code path that might have changed it.
            // P9.5: typed modify-in-place via the generated enum field
            // accessor — `with_dma_sel(HostControlDmaSel::Adma2_32)` masks
            // and shifts the DMA_SEL field correctly without driver-side
            // hand-rolled mask arithmetic.
            sdhci.modify_host_control(|hc| hc.with_dma_sel(HostControlDmaSel::Adma2_32), &tk);

            sdhci.write_adma_address(desc_map.pa() as u32, &tk);

            // Typed BLOCK_SIZE/BLOCK_COUNT writes (regspec generates raw u16
            // write_* accessors — the registers have no named bit fields).
            sdhci.write_block_size(512, &tk);
            sdhci.write_block_count(1, &tk);
            // ARGUMENT must be latched before the COMMAND half of the
            // combined write triggers the command on the bus.
            sdhci.write_argument(sector as u32, &tk);

            let cmd17 = sd_command_word(
                SdCommand::ReadSingleBlock.index(),
                SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
            );
            // P9.7: single-u32 combined_trigger write of TRANSFER_MODE
            // (TRNS_READ | TRNS_DMA) + COMMAND (cmd17). This makes the M7
            // controller-ordering fix mechanical: the BCM2711 Arasan
            // controller REQUIRES TRANSFER_MODE and COMMAND to arrive as one
            // 32-bit store or it silently drops the command. #131 stabilised
            // this by hand-disciplining the write order; the generated
            // set_transfer_mode_command setter now enforces the single-store
            // shape structurally (P9.0 emitter; verified by the codegen test
            // transfer_mode_command_combined_trigger_single_u32_store).
            sdhci.set_transfer_mode_command(
                TransferMode(SDHCI_TRNS_READ | SDHCI_TRNS_DMA),
                Command(cmd17),
                &tk,
            );
            Ok(())
        },
    )
    .map_err(|e| match e {
        // The pre-kick clean of the descriptor region failed.
        DmaTransferError::CleanFailed(_) => Emmc2Error::DescSyncFailed,
        // The `kick` (register programming) returned an error.
        DmaTransferError::Kick(k) => k,
        // The SDHCI completion (IRQ wait / B4.1 drain) failed.
        DmaTransferError::Completion(c) => c,
        // The descriptor region is `ToDevice`, never invalidated, so
        // this arm is unreachable; map it for totality.
        DmaTransferError::InvalidateFailed(_) => Emmc2Error::DescSyncFailed,
    })
}

/// Result of an ADMA2 multi-block read with phase-split timing.
/// `cmd_to_complete` is the controller's response latency; the rest
/// is card-side data delivery. Useful for distinguishing controller
/// bugs from card-side stalls — see `docs/tracking/tech-debt.md` for the
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
fn adma2_transfer(
    sdhci: &Sdhci,
    direction: AdmaDirection,
    sector: u64,
    n_blocks: u16,
    buf_phys: u64,
    desc_map: &OwnedDmaMapping<DmaPoolOrigin>,
) -> Result<AdmaTiming, Emmc2Error> {
    #[allow(deprecated)]
    let tk = __temp_unguarded_mint(sdhci);
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
    if poll_until(
        || {
            let ps = sdhci.present_state();
            !ps.contains(PresentState::CMD_INHIBIT) && !ps.contains(PresentState::DAT_INHIBIT)
        },
        Nanos::from_millis(100),
    ).is_err() {
        return Err(Emmc2Error::InhibitStuck {
            present_state: sdhci.present_state().bits(),
        });
    }

    // One descriptor covers the whole multi-block transfer. Descriptor
    // table from the single-block path is reused (still mapped via
    // desc_map); overwrite its 8 bytes for this transfer. Written
    // through the OwnedDmaMapping's bounds-checked `cell()` accessor
    // (safe — mirrors adma2_single_block_read).
    desc_map
        .cell::<Adma2Descriptor>(0)
        .write(Adma2Descriptor::tran_end(buf_phys as u32, bytes as u16));
    // Coherence envelope: clean the reused descriptor down to DRAM (a
    // `ToDevice` region — same C1 rationale as adma2_single_block_read),
    // then `kick` the controller and run the polled CMD/TRANSFER
    // completion. Dead path (CMD18+Auto-CMD23 disabled), kept on the
    // envelope so a future re-enable inherits the clean -> kick ordering
    // and the single-store TRANSFER_MODE+COMMAND discipline. Completion
    // here is polled inside `kick` (so `Immediate`), not IRQ-driven like
    // the single-block path.
    let desc_region = desc_map.dma_region(0, 8, DmaDir::ToDevice);
    run_dma_transfer(
        &[desc_region],
        Immediate,
        || -> Result<AdmaTiming, Emmc2Error> {
            sdhci.write_adma_address(desc_map.pa() as u32, &tk);
            sdhci.write_argument2(n_blocks as u32, &tk);

            // P9.6: typed BLOCK_SIZE/BLOCK_COUNT writes (mirror of the
            // single-block path).
            sdhci.write_block_size(512, &tk);
            sdhci.write_block_count(n_blocks, &tk);
            let trns_dir = match direction {
                AdmaDirection::Read => SDHCI_TRNS_READ,
                AdmaDirection::Write => 0, // absence of TRNS_READ = write
            };
            let trns = trns_dir | SDHCI_TRNS_DMA | SDHCI_TRNS_BLK_CNT_EN
                | SDHCI_TRNS_MULTI | SDHCI_TRNS_AUTO_CMD23;
            // CMD18/CMD25 argument is the LBA (SDHC/SDXC blocks-as-units).
            // ARGUMENT before the combined TRANSFER_MODE+COMMAND write
            // (P9.7 reorder — same rationale as adma2_single_block_read).
            sdhci.write_argument(sector as u32, &tk);

            let cmd = sd_command_word(
                match direction {
                    AdmaDirection::Read => SdCommand::ReadMultipleBlock.index(),
                    AdmaDirection::Write => SdCommand::WriteMultipleBlock.index(),
                },
                SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA,
            );

            let freq = cntfreq_hz();
            let t0 = monotonic_now();
            // P9.7: single-u32 combined_trigger write (TRANSFER_MODE +
            // COMMAND). Mirror of the single-block path; keeps the
            // multi-block dead-code path on the same write discipline so a
            // future CMD18 re-enable doesn't reintroduce the split-write
            // Arasan errata exposure.
            sdhci.set_transfer_mode_command(TransferMode(trns), Command(cmd), &tk);

            // Poll CMD_COMPLETE (P9.6: typed status snapshot + clear).
            let cmd_deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
            loop {
                let status = sdhci.normal_int_status(&tk);
                if status.contains(NormalIntStatus::ERROR) {
                    let err_int_status = sdhci.error_int_status(&tk).bits();
                    sdhci.clear_error_int_status(ErrorIntStatus(0xFFFF), &tk);
                    sdhci.clear_normal_int_status(
                        NormalIntStatus::ERROR | NormalIntStatus::CMD_COMPLETE,
                        &tk,
                    );
                    return Err(Emmc2Error::CmdError { err_int_status });
                }
                if status.contains(NormalIntStatus::CMD_COMPLETE) {
                    sdhci.clear_normal_int_status(NormalIntStatus::CMD_COMPLETE, &tk);
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
                let status = sdhci.normal_int_status(&tk);
                if status.contains(NormalIntStatus::ERROR) {
                    let err_int_status = sdhci.error_int_status(&tk).bits();
                    sdhci.clear_error_int_status(ErrorIntStatus(0xFFFF), &tk);
                    sdhci.clear_normal_int_status(
                        NormalIntStatus::ERROR | NormalIntStatus::DATA_COMPLETE,
                        &tk,
                    );
                    return Err(Emmc2Error::DataError { err_int_status });
                }
                if status.contains(NormalIntStatus::DATA_COMPLETE) {
                    sdhci.clear_normal_int_status(NormalIntStatus::DATA_COMPLETE, &tk);
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
        },
    )
    .map_err(|e| match e {
        DmaTransferError::CleanFailed(_) => Emmc2Error::DescSyncFailed,
        DmaTransferError::Kick(k) => k,
        // Immediate completion is infallible.
        DmaTransferError::Completion(inf) => match inf {},
        DmaTransferError::InvalidateFailed(_) => Emmc2Error::DescSyncFailed,
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
// CMD18+Auto-CMD23 cold-boot issue is diagnosed (see docs/tracking/tech-debt.md).
// Plus per-buffer PageSet tracking.
// ---------------------------------------------------------------------------

const MAX_DMA_BUFFERS: usize = 8;

/// A DMA buffer slot. Owns the DmaPool-origin backing — move-only,
/// because it holds a closable pageset handle and is the only token
/// that can mint a coherence `DmaRegion` over the buffer. Stored as
/// `Option<DmaBuf>` per slot; the `None` slot replaces the old all-zero
/// `DmaBuf` sentinel, which a move-only owner cannot have.
struct DmaBuf {
    backing: DmaBacking<DmaPoolOrigin>,
    sector_count: u64,
}

struct Emmc2BlockEngine {
    /// Typed MMIO handle for the SDHCI register region. Replaces the
    /// pre-P9.4c raw `mmio_va: u64` field — `regs.regs()` yields
    /// `&Sdhci` for the per-call shim API, and future P9.5-P9.8
    /// commits will move per-width accesses to typed accessors on
    /// the same handle.
    regs: MappedRegs<Sdhci>,
    capacity_sectors: u64,
    /// Persistent ADMA2-32 descriptor table. One 8-byte tran+end
    /// descriptor is rewritten per transfer. Allocated from the
    /// DMA pool and (post cacheable-DMA migration C1) mapped
    /// Normal Cacheable. The `OwnedDmaMapping` retains the pageset
    /// so each per-transfer descriptor write feeds a `ToDevice`
    /// coherence region in the envelope, which cleans the cache line
    /// down to DRAM before the controller fetches the descriptor. Its
    /// `Drop` unmaps the VA and closes
    /// the pageset when the engine is torn down.
    desc_map: OwnedDmaMapping<DmaPoolOrigin>,
    dma_buffers: [Option<DmaBuf>; MAX_DMA_BUFFERS],
    dma_count: usize,
    /// IRQ-driven completion state (m7-irq-experiment graft).
    /// Framework-owned: the BoundIrq bundles the notification, the
    /// hardware INTID, and the threshold counter. Driver code calls
    /// `wait_until` to block for the next IRQ and `unmask()` to
    /// re-arm the GIC after clearing the device-side status latch.
    irq: lockjaw_userlib::irq::BoundIrq,
}

impl Emmc2BlockEngine {
    /// Allocate the descriptor table page, switch the controller to
    /// ADMA2-32, enable NORMAL/ERROR_INT_SIGNAL_ENABLE for the IRQ
    /// path, return a ready-to-serve engine. Caller hands over the
    /// typed MMIO handle (via P9.4c framework entry) and the bound
    /// IRQ, and is responsible for keeping the controller
    /// initialized through enumeration before calling this. Signal-
    /// enables are turned on HERE (not at boot) so the ID-phase
    /// polling path in `issue_command` continues to work without IRQ
    /// interference.
    fn new(
        regs: MappedRegs<Sdhci>,
        capacity_sectors: u64,
        irq: lockjaw_userlib::irq::BoundIrq,
    ) -> Result<Self, Emmc2Error>
    {
        // Reborrow as &Sdhci for the rest of init (passed to
        // sdhci_read*/write* shims). regs is moved into the engine
        // at the end of this function.
        let sdhci = regs.regs();
        #[allow(deprecated)]
        let tk = __temp_unguarded_mint(sdhci);
        // Descriptor table: 1 page from the DMA pool, allocated and
        // mapped via the typed OwnedDmaMapping (Normal Cacheable
        // post-C1 of the cacheable-DMA migration). The matching
        // descriptor clean is the `ToDevice` region of the coherence
        // envelope inside adma2_single_block_read. The mapping owns the
        // pageset; its Drop unmaps + closes it when the engine is torn down.
        let desc_map = OwnedDmaMapping::alloc_dma_pool().map_err(|_| Emmc2Error::DescAllocFailed)?;
        if desc_map.pa() >= (1u64 << 32) {
            return Err(Emmc2Error::DescPhysAbove4Gib(desc_map.pa()));
        }
        // Switch HOST_CONTROL_1.DMA_SEL to ADMA2-32; preserve the
        // 4-bit bus width from ACMD6 (P9.5 — typed
        // `with_dma_sel(HostControlDmaSel::Adma2_32)`).
        sdhci.modify_host_control(|hc| hc.with_dma_sel(HostControlDmaSel::Adma2_32), &tk);
        // Program the descriptor table address once; the same table
        // is reused across every transfer.
        sdhci.write_adma_address(desc_map.pa() as u32, &tk);
        // Clear any latched STATUS bits before enabling signaling.
        //
        // ID-phase polling can leave NORMAL_INT_STATUS bits set —
        // notably DATA_COMPLETE after CMD7 (RESP_SHORT_BUSY / R1b):
        // the card holds DAT0 busy while it transitions to the
        // selected state, and the release latches DATA_COMPLETE.
        // issue_command's W1C only clears CMD_COMPLETE; the
        // subsequent PRESENT_STATE.DAT_INHIBIT poll waits for the
        // busy release but does not touch NORMAL_INT_STATUS.
        //
        // Per SDHCI v3 §2.2.24, the GIC line is asserted whenever
        // (STATUS_ENABLE & SIGNAL_ENABLE & STATUS) is non-zero —
        // combinatorial, no edge detection. So if SIGNAL_ENABLE is
        // flipped on with STATUS.DATA_COMPLETE still latched, the
        // controller asserts the GIC line IMMEDIATELY, the kernel
        // takes the IRQ + masks the SPI + signals the notification,
        // and BoundIrq.threshold (= 1 after bind) is satisfied
        // before CMD17 has even been issued. The first wait_until
        // returns instantly with stale STATUS, the driver "succeeds"
        // on a transfer that never happened, and the buffer reads
        // garbage.
        //
        // Clearing STATUS via W1C-0xFFFF here is the structural fix
        // — it's a superset of B4.2's per-command stale-bit guard,
        // but at this specific site it's REQUIRED to make the
        // ID-phase → data-phase IRQ-mode transition safe.
        // P9.6: typed clear via clear_*_int_status with the full
        // mask wrapped in the typed newtype.
        sdhci.clear_normal_int_status(NormalIntStatus(0xFFFF), &tk);
        sdhci.clear_error_int_status(ErrorIntStatus(0xFFFF), &tk);

        // Enable IRQ signaling for the data path. STATUS_ENABLE was
        // set during emmc2_entry's post-bootstrap init (right after
        // soft_reset_all, before the ID phase); SIGNAL_ENABLE gates
        // whether a latched STATUS bit asserts the GIC line. Turned
        // on HERE, AFTER the ID phase (issue_command polls — no
        // signal enable during ID = no IRQ overlap during
        // enumeration).
        //
        // The set is the three events the IRQ loop in
        // adma2_single_block_read handles: CMD_COMPLETE,
        // DATA_COMPLETE, and ERROR. ERROR (bit 15) is the master
        // gate for error IRQ delivery (SDHCI 3.0 §2.2.21 Table 2-32):
        // it AND-gates the per-error ERROR_INT_SIGNAL_ENABLE bits, so
        // without it set the IRQ loop never wakes on a data-path
        // CRC/timeout — those would surface only as the 1-second
        // TransferCompleteTimeout fallback. The three bits here must
        // match the three the loop's status decode checks; see
        // docs/tracking/tech-debt.md "emmc2 error-IRQ enable is Pi-fault-path-
        // only" for why make test cannot guard this set.
        let normal_sig = NormalIntSignalEnable::CMD_COMPLETE
            | NormalIntSignalEnable::DATA_COMPLETE
            | NormalIntSignalEnable::ERROR;
        sdhci.set_normal_int_signal_enable(normal_sig, &tk);
        sdhci.set_error_int_signal_enable(ErrorIntSignalEnable(0xFFFF), &tk);
        // Drop the `sdhci` borrow so we can move `regs` into the
        // struct (NLL should already handle this, but the explicit
        // shadow keeps the intent obvious in review).
        let _ = sdhci;
        Ok(Self {
            regs,
            capacity_sectors,
            desc_map,
            dma_buffers: core::array::from_fn(|_| None),
            dma_count: 0,
            irq,
        })
    }

    fn find_buf(&self, ps: PageSetHandle) -> Result<&DmaBuf, BlockError> {
        for slot in self.dma_buffers.iter().flatten() {
            if slot.backing.pageset.0 == ps.0 {
                return Ok(slot);
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
            // emmc2 allocs buffers via `alloc_dma_backing_dma_pool` (DmaPool
            // origin). Post cacheable-DMA migration C1 the kernel
            // accepts only Normal (Cacheable) mappings for DmaPool
            // PageSets. Cross-process coherence is the ENGINE's
            // responsibility: `Engine::read` wraps the transfer in the
            // coherence envelope, which invalidates the read range (a
            // `FromDevice` region) before returning, so by the time
            // control reaches the client (over IPC) the buffer's cache
            // lines for that range are already invalidated and the next
            // CPU load reads fresh DRAM. Clients DO NOT need to issue
            // their own sync after `read` returns; a future write path
            // would clean its `ToDevice` region through the same
            // envelope (not exercised today — writes are disabled until
            // CMD18+Auto-CMD23 cold-boot is validated).
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
        let backing = alloc_dma_backing_dma_pool(pages).map_err(|_| BlockError::AllocFailed)?;
        // ADMA2-32 programs a 32-bit ADMA_ADDRESS, so the buffer's
        // physical base must sit below 4 GiB. Close the backing on the
        // reject path so the rejected allocation doesn't leak (the
        // PageSetGuard that previously auto-closed on early return is
        // subsumed by alloc_dma_backing_dma_pool's own success-only handoff).
        if backing.pa >= (1u64 << 32) {
            close_dma_backing(backing.pageset);
            return Err(BlockError::AllocFailed);
        }
        // Track this buffer in the slot table; on success the slot owns
        // the backing (its pageset is closed in free_buffer). On failure
        // (table full) close immediately so the allocation doesn't leak.
        let ps = backing.pageset;
        if let Some(slot) = self.dma_buffers.iter_mut().find(|s| s.is_none()) {
            *slot = Some(DmaBuf { backing, sector_count });
            self.dma_count += 1;
            return Ok(ps);
        }
        close_dma_backing(backing.pageset);
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
        let sync_bytes = count * 512;
        // The destination buffer is a `FromDevice` coherence region; the
        // envelope owns its clean-before / invalidate-after ordering:
        //
        // - B2.2 (docs/history/post-c1-fix-plan.md): clean any pre-DMA dirty
        //   cache lines to DRAM BEFORE the controller writes. Lines
        //   dirtied by an earlier CPU write (e.g. the selftest's pre-DMA
        //   `zero_range(0, 512)`, or a previous read's partial overwrite)
        //   would otherwise be written back over the device's DMA-
        //   deposited bytes, leaving stale zeros. This is the load-bearing
        //   fix for the C1 Pi-flash `0xAA55` gate; B2.1's kernel `dc
        //   civac` upgrade alone cannot recover dirty-pre-DMA lines.
        // - C1: the buffer is Normal Cacheable, so AFTER the controller's
        //   DMA write the envelope invalidates the lines so subsequent
        //   CPU loads read fresh DRAM, not stale pre-DMA cache state
        //   (the principled replacement for the M7-era incidental drain).
        //
        // Mirrors Linux's dma_map_single(DMA_FROM_DEVICE) +
        // dma_sync_sg_for_cpu contract; the driver no longer issues
        // sync_for_{device,cpu} by hand. One region covers count*512 bytes.
        let buf_region = buf.backing.dma_region(0, sync_bytes, DmaDir::FromDevice);
        let buf_pa = buf.backing.pa;
        // `buf`'s borrow of self.dma_buffers ends here; take the disjoint
        // controller / descriptor / IRQ borrows for the per-block loop.
        let regs = self.regs.regs();
        let desc_map = &self.desc_map;
        let irq = &mut self.irq;
        run_dma_transfer(
            &[buf_region],
            Immediate,
            || -> Result<(), BlockError> {
                // Reads always use CMD17 (single-block). Multi-sector reads
                // loop one block at a time, advancing buf_pa by 512 bytes per
                // sector. The CMD18+Auto-CMD23 path in adma2_transfer is dead
                // for reads until the cold-boot CMD18 question is settled;
                // we want every fat32 / partition-manager read to exercise
                // the proven M6 sub-commit 2b sequence. Performance cost on
                // Pi: one CMD17 round-trip per sector. Each block runs its
                // own inner descriptor envelope (IRQ wait + B4.1 drain), so
                // this kick blocks until every block finishes and the outer
                // buffer completion is `Immediate`.
                for i in 0..count {
                    let buf_sector_pa = buf_pa + i * 512;
                    let res = adma2_single_block_read(
                        regs, sector + i, buf_sector_pa, desc_map, irq,
                    );
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
                Ok(())
            },
        )
        .map_err(|e| match e {
            // Pre-kick clean (B2.2) of the buffer region failed.
            DmaTransferError::CleanFailed(_) => {
                puts("[EMMC2:READ] sync_for_device FAILED\n");
                BlockError::IoError
            }
            // A per-block read failed (already logged in the loop).
            DmaTransferError::Kick(be) => be,
            // Immediate completion is infallible.
            DmaTransferError::Completion(inf) => match inf {},
            // Post-transfer invalidate (C1) of the buffer region failed.
            DmaTransferError::InvalidateFailed(_) => {
                puts("[EMMC2:READ] sync_for_cpu FAILED\n");
                BlockError::IoError
            }
        })
    }

    fn write(&mut self, _sector: u64, _count: u64, _buffer: PageSetHandle)
        -> Result<(), BlockError>
    {
        // Writes are disabled until CMD18+Auto-CMD23 (read) and
        // CMD25+Auto-CMD23 (write) are validated on cold boot — see
        // docs/tracking/tech-debt.md "CMD18+Auto-CMD23 cold-boot validation".
        // The Pi 4B end-to-end target (#131) is FAT32 read through
        // POSIX, which does not write; rejecting writes prevents
        // accidental dispatch through the multi-block path while the
        // cold-boot diagnosis is open.
        Err(BlockError::Unsupported)
    }

    fn free_buffer(&mut self, buffer: PageSetHandle) {
        for slot in self.dma_buffers.iter_mut() {
            if matches!(slot.as_ref(), Some(b) if b.backing.pageset.0 == buffer.0) {
                if let Some(buf) = slot.take() {
                    close_dma_backing(buf.backing.pageset);
                }
                self.dma_count -= 1;
                return;
            }
        }
    }
}
