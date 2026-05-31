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
    run_dma_transfer, DmaDir, DmaTransferError, Immediate,
};
use lockjaw_mmio::region::MappedRegs;
// O7: driver consumes Sdhci + register-field newtypes via
// `lockjaw-userlib::sdhci`'s re-export rather than naming `lockjaw_regs`
// directly. The xtask `check-driver-unsafe` ban (also added in O7)
// forbids `lockjaw_regs` import in `user/*-driver` source; this import
// goes through the framework's safe surface and stays compliant.
use lockjaw_userlib::sdhci::{
    ErrorIntSignalEnable, ErrorIntStatusEnable,
    NormalIntSignalEnable, NormalIntStatusEnable,
    PowerControlBusVoltage, Sdhci,
};
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_userlib::time::{sleep_for, Nanos};
// `monotonic_now` returns `MonoTicks`, which is `Ord`; the comparisons in the
// poll helpers don't need the type imported by name. `sleep_for` is used only
// for pure-time waits (regulator settle, post-clock idle) — never inside
// status polls, which busy-wait with `core::hint::spin_loop()` between MMIO
// reads to avoid quantizing 200µs hardware events to a 10ms scheduler tick.
use lockjaw_types::device::BCM2711_EMMC2_HASH;
use lockjaw_types::sdhci::{
    Capabilities, ACMD41_ARG_HCS, SdCommand,
    SDHCI_SPEC_300,
    SDHCI_INT_CMD_TIMEOUT, SDHCI_INT_CMD_CRC,
    SDHCI_INT_CMD_END_BIT, SDHCI_INT_CMD_INDEX,
    SDHCI_INT_DATA_TIMEOUT, SDHCI_INT_DATA_CRC, SDHCI_INT_DATA_END_BIT,
    compute_clock_divisor,
};
use lockjaw_types::wire::sdhci::Adma2Descriptor;
// O6: framework-side SDHCI surface. Driver consumes the MmcCard
// outer typestate (ID phase + bus-width init), the per-operation
// envelopes (SdhciCommandInit + issue_data_transfer for data phase),
// and the init-time helpers (soft_reset_all / configure_clock /
// set_power_on_and_settle / set_int_enable_masks /
// set_timeout_dat_counter / init_adma2_32 / enable_irq_signaling).
use lockjaw_userlib::sdhci::{
    self as sdhci_lib, SdhciCommandInit, SdhciInitError,
};

// O6: `poll_until` deleted — every status-bit poll the driver used to
// drive directly now lives inside the framework (CMD_INHIBIT poll in
// SdhciCommandInit::issue_no_data, INT_CLK_STABLE poll in
// configure_clock, SW_RST_ALL poll in soft_reset_all, DAT_INHIBIT
// drain in SdhciDataCompletion + MmcCard::select). The driver no
// longer hand-runs any deadline-bounded busy poll.

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

// O6: `issue_or_die` deleted. The ID-phase command pattern is now the
// MmcCard<'a, S> typestate chain from `lockjaw_userlib::sdhci` — each
// transition consumes the previous state and produces the next
// (Uninit → Idle → Ready → Ident → Stby → Tran), with typed
// MmcCardError variants carrying the failure detail. emmc2_entry
// dispatches log-and-sys_exit per transition via match (CMD0 lenient,
// others strict). The framework owns the SdhciCommandInit::open +
// issue_no_data::<R> mechanics; the driver expresses card-state
// intent only.


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

    // ID-phase: drive the card through CMD0 → CMD8 → ACMD41 → CMD2 →
    // CMD3 → CMD7 (folds CMD9 inside per O6 review) → ACMD6 via the
    // MmcCard typestate chain. Each transition consumes the previous
    // state and produces the next; a typo that calls these out of
    // order (e.g. `select()` before `publish_rca()`) is a compile
    // error. Per-step failure logs and sys_exits inline — the framework
    // returns MmcCardError, the driver decides logging/exit policy.
    // Errors carry typed diagnostic info (Cmd8EchoMismatch,
    // Acmd41Timeout, Cmd9NotV2, Cmd7BusyStuck, or the underlying
    // SdhciCommandError).
    let card = sdhci_lib::MmcCard::uninit(sdhci);
    // CMD0 — strict-exit. Every failure mode (InhibitStuck /
    // ControllerError / NoResponse) signals a controller in bad state,
    // and the typestate chain requires a real `MmcCard<Idle>` for the
    // next step. Pre-O6 history: the hand-rolled CMD0 was tolerant
    // (logged and continued), but the typestate now leaves no path to
    // proceed without an `Ok(_)` from go_idle.
    let card = match card.go_idle() {
        Ok(c) => { puts("[EMMC2:IDPHASE] CMD0 acknowledged\n"); c }
        Err(e) => {
            puts("[EMMC2:IDPHASE] CMD0 FAILED, halting: ");
            put_mmc_card_error(&e);
            puts("\n");
            sys_exit();
        }
    };

    // CMD8 — SEND_IF_COND with the standard 0x1AA check pattern.
    // R7 echo verifies SDv2+. Pre-SDv2 cards / clock-mis-configured
    // bus surface as Acmd8EchoMismatch or Sdhci(NoResponse).
    let card = match card.verify_sdv2_if_cond() {
        Ok(c) => {
            puts("[EMMC2:IDPHASE] CMD8 echo=0x1AA — card is SDv2+ (clk via cprman)\n");
            c
        }
        Err(e) => { puts("[EMMC2:IDPHASE] CMD8 FAILED: "); put_mmc_card_error(&e); puts("\n"); sys_exit(); }
    };

    // -----------------------------------------------------------------------
    // M3 — Full SD identification: ACMD41 → CMD2 → CMD3 → CMD9 →
    //       CMD7 → ACMD6 → HOST_CONTROL → 25 MHz
    // -----------------------------------------------------------------------

    // ACMD41 loop — internally retries CMD55+ACMD41 with a 1-second
    // deadline + 10ms inter-retry sleep. Captures the OCR for CardInfo.
    let card = match card.power_up_to_ready(ACMD41_ARG_HCS) {
        Ok(c) => c,
        Err(e) => { puts("[EMMC2:IDPHASE] ACMD41 FAILED: "); put_mmc_card_error(&e); puts("\n"); sys_exit(); }
    };
    puts("[EMMC2:IDPHASE] ACMD41 ready\n");

    // CMD2 — ALL_SEND_CID. Captures CID for CardInfo.
    let card = match card.identify() {
        Ok(c) => c,
        Err(e) => { puts("[EMMC2:IDPHASE] CMD2 FAILED: "); put_mmc_card_error(&e); puts("\n"); sys_exit(); }
    };
    puts("[EMMC2:IDPHASE] CMD2 CID received\n");

    // CMD3 — SEND_RELATIVE_ADDR. Captures RCA for CardInfo + the
    // subsequent RCA-addressed commands (CMD7, CMD9).
    let card = match card.publish_rca() {
        Ok(c) => c,
        Err(e) => { puts("[EMMC2:IDPHASE] CMD3 FAILED: "); put_mmc_card_error(&e); puts("\n"); sys_exit(); }
    };

    // CMD9 + CMD7 — Stby → Tran. `select()` folds the two together
    // (per O6 review): runs CMD9 to capture the CSD-v2 capacity first,
    // then CMD7 to select the card and polls DAT_INHIBIT (500ms). The
    // fold makes "`CardInfo.csd` is captured" a structural property of
    // reaching `MmcCard<Tran>` rather than a runtime expect. Cmd9NotV2
    // surfaces for legacy SDSC; Cmd7BusyStuck for a card that never
    // releases DAT0 busy.
    let card = match card.select() {
        Ok(c) => c,
        Err(e) => { puts("[EMMC2:IDPHASE] CMD9/CMD7 FAILED: "); put_mmc_card_error(&e); puts("\n"); sys_exit(); }
    };
    puts("[EMMC2:IDPHASE] CMD9+CMD7 card selected with CSD captured\n");

    // ACMD6 — SET_BUS_WIDTH 4-bit. Internally CMD55+ACMD6 +
    // host_control flip via set_bus_width_4bit.
    let card = match card.set_bus_width_4bit() {
        Ok(c) => c,
        Err(e) => { puts("[EMMC2:IDPHASE] ACMD6 FAILED: "); put_mmc_card_error(&e); puts("\n"); sys_exit(); }
    };

    // Raise the SD bus clock from 400 kHz (ID mode) to 25 MHz (data
    // transfer mode). SDHCI spec § 3.2.4: disable SD_CLK_EN first,
    // then reprogram divisor — `sdhci_lib::configure_clock` does this.
    if sdhci_lib::configure_clock(sdhci, actual_base_hz, 25_000_000).is_err() {
        puts("[EMMC2:READY] clock-to-25MHz FAILED: INT_CLK_STABLE timeout\n");
        sys_exit();
    }

    // Surrender the &Sdhci borrow + drop the typestate. CardInfo is
    // the proof token Emmc2BlockEngine::new requires — no other path
    // to construct one (Tier 3 #13).
    let card_info = card.into_parts();
    let rca = card_info.rca();
    let capacity_bytes = card_info.csd().capacity_bytes;

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
    // MmcCard<Tran>::into_parts() above already dropped the &Sdhci
    // borrow chain — `sdhci` (= ctx.regs.regs()) is no longer
    // live-borrowed by anything, so `ctx.regs` is movable into the
    // engine. The CardInfo is the structural proof token; the engine
    // ctor extracts capacity_sectors from card_info.csd() internally.
    let engine = match Emmc2BlockEngine::new(ctx.regs, card_info, ctx.irq) {
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
    put_decimal(capacity_bytes / 512);
    puts(" blocks; selftest read OK\n");

    run_block_server(&mut engine, blk_srv_ep);
}

// ---------------------------------------------------------------------------
// ADMA2 single-block read primitive. Wraps the framework's
// SdhciCommandInit::issue_data_transfer envelope for the CMD17 path
// that Emmc2BlockEngine::read loops one block at a time. The dead
// CMD18+Auto-CMD23 multi-block path (adma2_transfer / AdmaTiming /
// AdmaDirection) was deleted in O5 — YAGNI, single-block has been
// the only production path since the engine landed.
//
// SdhciDataCompletion (IRQ wait + B4.1 DAT_INHIBIT drain) lives in
// lockjaw-userlib::sdhci as part of the family-generic surface; the
// driver constructs one and hands it to issue_data_transfer.
// ---------------------------------------------------------------------------
/// Single-block ADMA2-32 CMD17 read of `sector` into `buf_phys`.
///
/// Writes the descriptor into `desc_map` then hands the transfer to
/// `SdhciCommandInit::issue_data_transfer`, which owns the inhibit
/// poll, the coherence-envelope clean→kick→await→invalidate, and the
/// gated-setter sequence (DMA_SEL → ADMA_ADDRESS → BLOCK_SIZE →
/// BLOCK_COUNT → ARGUMENT → single-store TRANSFER_MODE+COMMAND).
/// `SdhciDataCompletion` (IRQ wait + B4.1 DAT_INHIBIT drain) lives in
/// `lockjaw-userlib::sdhci` and is constructed inline.
fn adma2_single_block_read(
    sdhci: &Sdhci,
    sector: u64,
    buf_phys: u64,
    desc_map: &OwnedDmaMapping<DmaPoolOrigin>,
    bound_irq: &mut lockjaw_userlib::irq::BoundIrq,
) -> Result<(), Emmc2Error> {
    if buf_phys >= (1u64 << 32) {
        return Err(Emmc2Error::BufferPhysAbove4Gib(buf_phys));
    }

    // Write the descriptor before the envelope's clean fires — same
    // typed Adma2Descriptor wire DTO as before. The single tran_end
    // shape (VALID | END | ACT_TRAN) is construction-safe per
    // lockjaw_types::sdhci's named constructor; bounds + alignment
    // are checked by the OwnedDmaMapping cell accessor.
    desc_map
        .cell::<Adma2Descriptor>(0)
        .write(Adma2Descriptor::tran_end(buf_phys as u32, 512));

    // ToDevice region for the descriptor — the envelope cleans it
    // down to DRAM before the controller's DMA fetches.
    let desc_region = desc_map.dma_region(0, 8, DmaDir::ToDevice);

    let params = sdhci_lib::SdhciDataTransfer {
        cmd: SdCommand::ReadSingleBlock,
        arg: sector as u32,
        direction: sdhci_lib::DataDirection::Read,
        adma_descriptor_pa: desc_map.pa() as u32,
    };
    let completion = sdhci_lib::SdhciDataCompletion::new(sdhci, bound_irq);

    SdhciCommandInit::open(sdhci)
        .issue_data_transfer(params, &[desc_region], completion)
        .map(|_| ())
        .map_err(|e| match e {
            sdhci_lib::SdhciDataTransferError::InhibitStuck { present_state } => {
                Emmc2Error::InhibitStuck { present_state }
            }
            sdhci_lib::SdhciDataTransferError::CleanFailed(_) => Emmc2Error::DescSyncFailed,
            // The descriptor region is `ToDevice`, never invalidated, so
            // this arm is unreachable; map it for totality.
            sdhci_lib::SdhciDataTransferError::InvalidateFailed(_) => Emmc2Error::DescSyncFailed,
            sdhci_lib::SdhciDataTransferError::Completion(c) => match c {
                sdhci_lib::SdhciDataCompletionError::CmdCompleteTimeout => {
                    Emmc2Error::CmdCompleteTimeout
                }
                sdhci_lib::SdhciDataCompletionError::TransferCompleteTimeout => {
                    Emmc2Error::TransferCompleteTimeout
                }
                sdhci_lib::SdhciDataCompletionError::CmdError { err_int_status } => {
                    Emmc2Error::CmdError { err_int_status }
                }
                sdhci_lib::SdhciDataCompletionError::DataError { err_int_status } => {
                    Emmc2Error::DataError { err_int_status }
                }
                sdhci_lib::SdhciDataCompletionError::DatInhibitStuck { present_state } => {
                    Emmc2Error::DatInhibitStuck { present_state }
                }
            },
        })
}

// ---------------------------------------------------------------------------
// Diagnostics helpers
// ---------------------------------------------------------------------------

/// Print a MmcCardError diagnostic: family-generic SdhciCommandError
/// for command failures, plus the card-state-specific variants
/// (Cmd8EchoMismatch / Acmd41Timeout / Cmd9NotV2 / Cmd7BusyStuck).
/// Caller adds the leading "[EMMC2:IDPHASE] CMD-X FAILED: " prefix
/// and the trailing newline.
fn put_mmc_card_error(err: &sdhci_lib::MmcCardError) {
    match err {
        sdhci_lib::MmcCardError::Sdhci(sc) => match sc {
            sdhci_lib::SdhciCommandError::InhibitStuck { present_state } => {
                puts("CMD_INHIBIT stuck present_state=");
                put_hex(*present_state as u64);
            }
            sdhci_lib::SdhciCommandError::ControllerError { err_int_status } => {
                put_error_int_status(*err_int_status);
            }
            sdhci_lib::SdhciCommandError::NoResponse => puts("no CMD_COMPLETE within 1s"),
        },
        sdhci_lib::MmcCardError::Cmd8EchoMismatch { echo } => {
            puts("CMD8 bad echo=0x"); put_hex((*echo & 0xFFF) as u64);
        }
        sdhci_lib::MmcCardError::Acmd41Timeout => {
            puts("ACMD41 timeout: card never became ready");
        }
        sdhci_lib::MmcCardError::Cmd9NotV2 { csd_structure } => {
            puts("CSD_STRUCTURE="); put_decimal(*csd_structure as u64);
            puts(" (expected 1 for SDHC/SDXC)");
        }
        sdhci_lib::MmcCardError::Cmd7BusyStuck { present_state } => {
            puts("DAT_INHIBIT did not clear present_state=");
            put_hex(*present_state as u64);
        }
    }
}

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
        card_info: sdhci_lib::CardInfo,
        irq: lockjaw_userlib::irq::BoundIrq,
    ) -> Result<Self, Emmc2Error>
    {
        // CardInfo is the structural proof token that the card reached
        // Tran state. Its only mint path is
        // `MmcCard::<Tran>::into_parts()`, so this signature is
        // compile-time proof emmc2_entry walked the typestate chain
        // through every required ID-phase command. Capacity comes from
        // the CSD v2 decode the chain captured.
        let capacity_sectors = card_info.csd().capacity_blocks;
        // Reborrow as &Sdhci for the init-helper calls. regs is moved
        // into the engine at the end of this function.
        let sdhci = regs.regs();
        // Descriptor table: 1 page from the DMA pool, allocated and
        // mapped via the typed OwnedDmaMapping (Normal Cacheable
        // post-C1 of the cacheable-DMA migration). The matching
        // descriptor clean is the `ToDevice` region of the coherence
        // envelope inside `SdhciCommandInit::issue_data_transfer`. The
        // mapping owns the pageset; its Drop unmaps + closes it when
        // the engine is torn down.
        let desc_map = OwnedDmaMapping::alloc_dma_pool().map_err(|_| Emmc2Error::DescAllocFailed)?;
        if desc_map.pa() >= (1u64 << 32) {
            return Err(Emmc2Error::DescPhysAbove4Gib(desc_map.pa()));
        }
        // Configure HOST_CONTROL_1.DMA_SEL = ADMA2-32 + program the
        // descriptor table address once (reused across every transfer).
        // `init_adma2_32` preserves the 4-bit bus width set during
        // M3 ACMD6 — modify_host_control writes the DMA_SEL field
        // only.
        sdhci_lib::init_adma2_32(sdhci, desc_map.pa() as u32);
        // Turn on IRQ signaling for the data path. STATUS_ENABLE was
        // set during emmc2_entry's post-bootstrap init (before the
        // ID phase). enable_irq_signaling clears any stale STATUS
        // bits left latched from the ID phase (notably DATA_COMPLETE
        // from CMD7's R1b busy release) BEFORE flipping signal-enable
        // — without that stale-clear the GIC line asserts immediately
        // on a never-issued transfer, BoundIrq.wait_until returns
        // instantly on stale status, and the first read sees garbage
        // (SDHCI v3 §2.2.24 — STATUS_ENABLE & SIGNAL_ENABLE & STATUS
        // is combinatorial, no edge detection).
        //
        // The normal-signal set is the three events the IRQ loop in
        // SdhciDataCompletion handles: CMD_COMPLETE, DATA_COMPLETE,
        // ERROR. ERROR (bit 15) is the master gate for error IRQ
        // delivery (SDHCI 3.0 §2.2.21 Table 2-32) — it AND-gates the
        // per-error ERROR_INT_SIGNAL_ENABLE bits, so without it set
        // the IRQ loop never wakes on a data-path CRC/timeout. See
        // docs/tracking/tech-debt.md "emmc2 error-IRQ enable is
        // Pi-fault-path-only" for why make test cannot guard this set.
        let normal_sig = NormalIntSignalEnable::CMD_COMPLETE
            | NormalIntSignalEnable::DATA_COMPLETE
            | NormalIntSignalEnable::ERROR;
        sdhci_lib::enable_irq_signaling(sdhci, normal_sig, ErrorIntSignalEnable(0xFFFF));
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
                // sector. CMD18+Auto-CMD23 multi-block was the old
                // adma2_transfer path; O5 deleted it as YAGNI and the future
                // multi-block re-introduction will extend
                // SdhciDataTransfer.block_count, not reintroduce a separate
                // kick. Performance cost on Pi: one CMD17 round-trip per
                // sector. Each block runs its own inner descriptor envelope
                // (IRQ wait + B4.1 drain) inside issue_data_transfer, so this
                // kick blocks until every block finishes and the outer buffer
                // completion is `Immediate`.
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
