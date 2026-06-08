//! SDHCI operation-layer envelope and init-time helpers.
//!
//! This module is the SDHCI family's `lockjaw-userlib` surface — the
//! single legal path drivers use to talk to the controller. It owns
//! the `SdhciOpToken` mint, wraps gated register accessors behind
//! typed operations, and re-exports the safe pieces of
//! `lockjaw_regs::sdhci` so driver crates never name `lockjaw_regs`
//! directly. (The xtask `check-driver-unsafe` regime enforces the
//! ban on `lockjaw_regs` import in `user/*-driver/` source — landing
//! in O7 of the plan.)
//!
//! Provided in O3 (this commit):
//!   - [`SdhciCommandInit`] typestate envelope for ID-phase commands
//!     plus [`issue_no_data`](SdhciCommandInit::issue_no_data) which
//!     wraps the poll-based CMD_INHIBIT → write_argument →
//!     combined_trigger → poll-CMD_COMPLETE sequence behind a single
//!     typed call parametrized over [`ResponseShape`].
//!   - Init-time free function helpers ([`soft_reset_all`],
//!     [`configure_clock`], [`set_power_on`], [`set_int_enable_masks`],
//!     [`set_timeout_dat_counter`]) that mint a token internally for
//!     one-shot non-command writes the controller needs at boot.
//!   - Re-exports of the safe `lockjaw_regs::sdhci` surface (Sdhci,
//!     SdhciOpToken, register-field newtypes) so drivers `use
//!     lockjaw_userlib::sdhci::*`.
//!
//! Coming later in the plan:
//!   - O5: `issue_data_transfer` wrapping `run_dma_transfer` + the
//!     `SdhciDataCompletion` IRQ wait + B4.1 drain that currently
//!     lives in `user/emmc2-driver/src/main.rs`.
//!   - O6: `MmcCard<'a, S>` outer typestate (Idle→Ready→Ident→Stby→Tran)
//!     that consumes `SdhciCommandInit` internally to drive the ID
//!     phase. Engine constructors then take a `CardInfo` (only
//!     mintable via `MmcCard::<Tran>::into_parts()`) as compile-time
//!     proof the card reached `Tran`.

use core::convert::Infallible;
use core::marker::PhantomData;

// Internal use only — `__sdhci_internal_mint` is the operation
// envelope's mint path. NOT re-exported (drivers cannot name it).
use lockjaw_regs::sdhci::__sdhci_internal_mint;

// Re-export the safe `lockjaw_regs::sdhci` surface under the original
// names so drivers consume `lockjaw_userlib::sdhci::*` and never
// reference `lockjaw_regs` directly. The xtask `check-driver-unsafe`
// regime (O7) bans `lockjaw_regs` imports in driver crates, making
// `__sdhci_internal_mint` and `__temp_unguarded_mint` (the gated mint
// paths, not re-exported here) unreachable from driver source.
pub use lockjaw_regs::sdhci::{
    ClockControl, Command, ErrorIntSignalEnable, ErrorIntStatus, ErrorIntStatusEnable,
    HostControlDmaSel, NormalIntSignalEnable, NormalIntStatus, NormalIntStatusEnable,
    PowerControl, PowerControlBusVoltage, PresentState, Sdhci, SdhciOpToken,
    SoftwareReset, TimeoutControl, TransferMode,
};

use lockjaw_types::sdhci::{
    compute_clock_divisor, response::ResponseShape, sd_command_word,
    CMD8_IF_COND_ARG, CsdV2, NotCsdV2, OcrRegister, SdCommand, SDHCI_CMD_CRC,
    SDHCI_CMD_DATA, SDHCI_CMD_INDEX, SDHCI_CMD_RESP_SHORT, SDHCI_TRNS_DMA, SDHCI_TRNS_READ,
};
use lockjaw_types::sdhci::card_state::{
    BusWidth, CardLifecycleState, Ident, Idle, Ready, Stby, Tran, Uninit,
};
use lockjaw_types::sdhci::operation::{OpIdle, OpState};
use lockjaw_types::sdhci::response::{R0, R1, R1b, R2, R3, R6, R6Response, R7};

use crate::dma_transfer::{run_dma_transfer, DmaCompletion, DmaRegion, DmaTransferError};
use crate::irq::BoundIrq;
use crate::time::{monotonic_now, sleep_for, spin_until_or_deadline, Nanos};

use lockjaw_types::syscall::SyscallError;

// ---------------------------------------------------------------------------
// SdhciCommandError — generic SDHCI command-issue failure shapes.
// ---------------------------------------------------------------------------

/// Failure shapes from [`SdhciCommandInit::issue_no_data`]. Driver-side
/// error types (e.g. emmc2's `Emmc2Error`) map these into their own
/// variants — `SdhciCommandError` is the family-generic surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdhciCommandError {
    /// `PRESENT_STATE.CMD_INHIBIT` did not clear before the pre-issue
    /// deadline. No command was issued on the bus.
    InhibitStuck { present_state: u32 },
    /// `NORMAL_INT_STATUS.ERROR` fired; `err_int_status` holds the
    /// `ERROR_INT_STATUS` bits captured before W1C.
    ControllerError { err_int_status: u16 },
    /// Neither `CMD_COMPLETE` nor `ERROR` fired before the post-issue
    /// deadline.
    NoResponse,
}

// ---------------------------------------------------------------------------
// SdhciCommandInit<'a, S> — operation-layer typestate envelope.
// ---------------------------------------------------------------------------

/// Typestate operation envelope for SDHCI command issuance.
///
/// Opened with [`SdhciCommandInit::open`] against a borrowed `&Sdhci`;
/// each issue method consumes `self` and returns the envelope back in
/// `OpIdle` so multiple commands can run through one open envelope.
/// The `<'a>` lifetime ties the captured `SdhciOpToken` to the same
/// `&'a Sdhci` borrow — a token cannot leak across the envelope's
/// scope.
///
/// The `<S>` parameter is bounded by [`lockjaw_types::sdhci::operation::OpState`]
/// so only the four legal state markers (OpIdle/OpArmed/OpKicked/
/// OpCompleted) can fill the slot. In O3 the public API surface
/// exposes only `OpIdle` — the internal states are reserved for the
/// O5 `issue_data_transfer` implementation that needs to gate its
/// register-program → kick → await sequence at the type level.
pub struct SdhciCommandInit<'a, S: OpState> {
    sdhci: &'a Sdhci,
    tk: SdhciOpToken<'a>,
    _state: PhantomData<S>,
}

impl<'a> SdhciCommandInit<'a, OpIdle> {
    /// Open an operation envelope against `sdhci`. Mints an internal
    /// `SdhciOpToken<'a>` whose lifetime is bound to the borrow.
    /// Cheap: no MMIO, just a phantom mint.
    #[inline]
    pub fn open(sdhci: &'a Sdhci) -> Self {
        Self {
            sdhci,
            tk: __sdhci_internal_mint(sdhci),
            _state: PhantomData,
        }
    }

    /// Issue an ID-phase command and return its typed response.
    ///
    /// `R` selects the [`ResponseShape`] (R0/R1/R1b/R2/R3/R6/R7); the
    /// shape's `FLAGS` bits get OR'd into the COMMAND register and
    /// `decode` parses the four RESPONSE registers into `R::Decoded`.
    /// Picking the wrong response shape is a compile error at the
    /// call site (`let r: R6Response = ...issue_no_data::<R3>(...)?;`
    /// won't typecheck).
    ///
    /// Sequence per SDHCI v3 § 3.7:
    ///   1. Poll `PRESENT_STATE.CMD_INHIBIT` until clear (`InhibitStuck`
    ///      on timeout).
    ///   2. Write `ARGUMENT`.
    ///   3. Write `TRANSFER_MODE + COMMAND` via the combined-trigger
    ///      setter (single u32 store — BCM2711 Arasan controller
    ///      silently drops commands written as two u16 halves).
    ///   4. Poll `NORMAL_INT_STATUS` for `CMD_COMPLETE` or `ERROR`
    ///      (`NoResponse` on timeout; `ControllerError` on ERROR).
    ///   5. On CMD_COMPLETE: read RESPONSE_0..3 as needed by `R`,
    ///      decode, W1C the CMD_COMPLETE bit, return.
    ///   6. Return the envelope in `OpIdle` so the next command can
    ///      issue without reopening.
    ///
    /// Polling shape (vs. IRQ): ID-phase commands at 400 kHz return
    /// in ~120 µs; a scheduler-tick yield would dominate. Busy-poll
    /// with `core::hint::spin_loop` between checks, deadline-bounded
    /// at 1 second.
    pub fn issue_no_data<R: ResponseShape>(
        self,
        cmd: SdCommand,
        arg: u32,
    ) -> Result<(R::Decoded, Self), SdhciCommandError> {
        // Pre-issue inhibit poll. Healthy controllers clear within
        // microseconds; the 100 ms budget keeps a wedged controller
        // from hanging the caller.
        let inhibit_deadline =
            monotonic_now().deadline_in(Nanos::from_millis(100), crate::time::cntfreq_hz());
        loop {
            let ps = self.sdhci.present_state();
            if !ps.contains(PresentState::CMD_INHIBIT) {
                break;
            }
            if inhibit_deadline.has_expired(monotonic_now()) {
                return Err(SdhciCommandError::InhibitStuck {
                    present_state: ps.bits(),
                });
            }
            core::hint::spin_loop();
        }

        self.sdhci.write_argument(arg, &self.tk);

        // Compose the COMMAND register from the SD command index +
        // the response shape's flag bits.
        let cmd_word = sd_command_word(cmd.index(), R::FLAGS);
        // ID-phase commands carry no data; TRANSFER_MODE stays 0.
        // Single-u32 combined-trigger write — BCM2711 silently drops
        // split writes.
        self.sdhci
            .set_transfer_mode_command(TransferMode(0), Command(cmd_word), &self.tk);

        // Poll for CMD_COMPLETE or ERROR. Deadline-bounded; busy-poll.
        let freq = crate::time::cntfreq_hz();
        let deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
        loop {
            let status = self.sdhci.normal_int_status(&self.tk);
            if status.contains(NormalIntStatus::ERROR) {
                // Capture the per-error bits BEFORE W1C — caller needs
                // them to decode the cause. Then clear underlying +
                // summary; leaving ERROR_INT_STATUS set causes the
                // controller to re-assert NORMAL_INT_STATUS.ERROR on
                // the next command.
                let err_int_status = self.sdhci.error_int_status(&self.tk).bits();
                self.sdhci.clear_error_int_status(ErrorIntStatus(0xFFFF), &self.tk);
                self.sdhci.clear_normal_int_status(
                    NormalIntStatus::ERROR | NormalIntStatus::CMD_COMPLETE,
                    &self.tk,
                );
                return Err(SdhciCommandError::ControllerError { err_int_status });
            }
            if status.contains(NormalIntStatus::CMD_COMPLETE) {
                // W1C CMD_COMPLETE *before* reading RESPONSE — matches
                // the existing `issue_command` in emmc2 (W1C then read)
                // so O4's migration is wire-effects-identical. SDHCI
                // v3: response registers stay latched after W1C of
                // CMD_COMPLETE until the next command issues.
                self.sdhci
                    .clear_normal_int_status(NormalIntStatus::CMD_COMPLETE, &self.tk);
                // Read only what the response shape needs — short
                // responses (R0/R1/R1b/R3/R6/R7) consume RESPONSE_0
                // alone; the long response (R2) spans all four. The
                // existing `issue_command` reads RESPONSE_0 only; the
                // long-response 4-read path is the new contribution
                // (CMD2 / CMD9 in the driver currently re-issue and
                // read all four manually — O4 collapses that into
                // `issue_no_data::<R2>`).
                let r0 = self.sdhci.read_response_0(&self.tk);
                let r = if R::READS_LONG_RESPONSE {
                    [
                        r0,
                        self.sdhci.read_response_1(&self.tk),
                        self.sdhci.read_response_2(&self.tk),
                        self.sdhci.read_response_3(&self.tk),
                    ]
                } else {
                    [r0, 0, 0, 0]
                };
                let decoded = R::decode(r);
                return Ok((decoded, self));
            }
            if deadline.has_expired(monotonic_now()) {
                return Err(SdhciCommandError::NoResponse);
            }
            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Init-time helpers — one-shot wrappers around the non-command gated
// setters the controller needs at boot.
// ---------------------------------------------------------------------------
//
// Each helper mints an internal token, does its job, drops the token.
// Drivers call these at init time instead of touching the raw setters
// (which are gated and only reachable via the operation envelope or
// via `lockjaw_regs` direct import — the latter banned by xtask in O7).

/// Failure shapes from the init-time helpers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdhciInitError {
    /// `SOFTWARE_RESET.SW_RST_ALL` did not auto-clear within the
    /// 200 ms deadline.
    ResetTimeout,
    /// `CLOCK_CONTROL.INT_CLK_STABLE` did not assert within the
    /// 100 ms deadline.
    ClockUnstable,
}

/// Issue a full SDHCI software reset (SW_RST_ALL) and poll until the
/// hardware auto-clears the bit.
///
/// Per SDHCI v3 § 3.2.1: writing SW_RST_ALL starts a hardware-managed
/// reset that clears the bit on completion. The poll deadline is
/// 200 ms — generous for a healthy controller (typical < 10 ms),
/// firm enough to surface a wedged controller as a clean error.
pub fn soft_reset_all(sdhci: &Sdhci) -> Result<(), SdhciInitError> {
    let tk = __sdhci_internal_mint(sdhci);
    sdhci.set_software_reset(SoftwareReset::SW_RST_ALL, &tk);
    let deadline =
        monotonic_now().deadline_in(Nanos::from_millis(200), crate::time::cntfreq_hz());
    spin_until_or_deadline(
        || {
            !sdhci
                .software_reset(&tk)
                .contains(SoftwareReset::SW_RST_ALL)
        },
        deadline,
    )
    .map_err(|_| SdhciInitError::ResetTimeout)
}

/// Configure the SD clock to `target_hz` from a `base_hz` reference,
/// per SDHCI v3 § 3.2.3 / § 3.2.4 (gate off SD_CLK_EN, write divisor +
/// INT_CLK_EN, poll INT_CLK_STABLE, re-enable SD_CLK_EN).
///
/// Used for both ID-mode (400 kHz) and data-transfer mode (25 MHz+).
/// `INT_CLK_STABLE` poll deadline is 100 ms — well over the spec
/// typical of < 1 ms.
pub fn configure_clock(
    sdhci: &Sdhci,
    base_hz: u64,
    target_hz: u64,
) -> Result<(), SdhciInitError> {
    let tk = __sdhci_internal_mint(sdhci);
    // Gate the clock output before touching the divisor.
    sdhci.modify_clock_control(|cc| cc.with_sd_clk_en(false), &tk);
    // Write divisor + internal clock enable in one shot. Start from
    // zero-default so all bits we don't set stay clear.
    let (lo, hi) = compute_clock_divisor(base_hz, target_hz);
    sdhci.set_clock_control(
        ClockControl::default()
            .with_freq_sel(lo as u16)
            .with_freq_sel_upper(hi as u16)
            .with_int_clk_en(true),
        &tk,
    );
    let deadline =
        monotonic_now().deadline_in(Nanos::from_millis(100), crate::time::cntfreq_hz());
    spin_until_or_deadline(
        || sdhci.clock_control(&tk).int_clk_stable(),
        deadline,
    )
    .map_err(|_| SdhciInitError::ClockUnstable)?;
    // Stable: enable the clock output to the card slot.
    sdhci.modify_clock_control(|cc| cc.with_sd_clk_en(true), &tk);
    Ok(())
}

/// Enable the SD bus power at the given voltage. BCM2711's emmc2 slot
/// is hardwired to 3.3 V; the voltage parameter exists so other SDHCI
/// consumers (e.g. SDIO with voltage switch) can pick.
///
/// Caller should sleep for a regulator-settle period (~1 ms per the
/// SD spec; ~2 ms with margin) before enabling the SD clock. The
/// helper does NOT sleep — the sleep policy is driver-side because
/// it depends on the platform's regulator.
pub fn set_power_on(sdhci: &Sdhci, voltage: PowerControlBusVoltage) {
    let tk = __sdhci_internal_mint(sdhci);
    sdhci.set_power_control(
        PowerControl::default()
            .with_bus_power_on(true)
            .with_bus_voltage(voltage),
        &tk,
    );
}

/// Convenience wrapper around [`set_power_on`] that also sleeps for a
/// platform-typical regulator-settle delay (2 ms with margin over the
/// SD spec minimum of ~1 ms).
pub fn set_power_on_and_settle(sdhci: &Sdhci, voltage: PowerControlBusVoltage) {
    set_power_on(sdhci, voltage);
    let _ = sleep_for(Nanos::from_millis(2));
}

/// Program `NORMAL_INT_STATUS_ENABLE` and `ERROR_INT_STATUS_ENABLE` —
/// the gates that decide which event bits the controller *latches*
/// into the status registers. STATUS_ENABLE is the prerequisite for
/// the driver's polling loops in `issue_no_data` to see anything;
/// SIGNAL_ENABLE (separate, IRQ-line gate) is the prerequisite for
/// IRQ-driven completion in O5's `issue_data_transfer`.
///
/// Drivers typically call this at init with full-mask
/// `NormalIntStatusEnable(0xFFFF) / ErrorIntStatusEnable(0xFFFF)` so
/// every event latches. Per-bit selectivity stays the SIGNAL_ENABLE's
/// job (which gates IRQ delivery, not latching).
pub fn set_int_enable_masks(
    sdhci: &Sdhci,
    normal: NormalIntStatusEnable,
    error: ErrorIntStatusEnable,
) {
    let tk = __sdhci_internal_mint(sdhci);
    sdhci.set_normal_int_status_enable(normal, &tk);
    sdhci.set_error_int_status_enable(error, &tk);
}

/// Program the data-line timeout counter — `value` is the 4-bit
/// exponent in `TIMEOUT_CONTROL.DAT_TIMEOUT_COUNTER`; the controller
/// uses `timeout = base_clk × 2^(13+value)`. Drivers typically write
/// `0x0E` for the max (~10s at 200 MHz base).
pub fn set_timeout_dat_counter(sdhci: &Sdhci, value: u8) {
    let tk = __sdhci_internal_mint(sdhci);
    sdhci.set_timeout_control(
        TimeoutControl::default().with_dat_timeout_counter(value),
        &tk,
    );
}

/// Set `HOST_CONTROL_1.DAT_4BIT` to mirror the 4-bit bus width
/// negotiated with the card via ACMD6 SET_BUS_WIDTH. The host side
/// MUST match the card side or every data transfer drops three
/// quarters of its bits.
///
/// One-shot init helper — typed `modify_host_control(|hc|
/// hc.with_dat_4bit(true))` with the token minted internally.
pub fn set_bus_width_4bit(sdhci: &Sdhci) {
    let tk = __sdhci_internal_mint(sdhci);
    sdhci.modify_host_control(|hc| hc.with_dat_4bit(true), &tk);
}

/// Configure the SDHCI controller for ADMA2-32 data transfers. Sets
/// `HOST_CONTROL_1.DMA_SEL = ADMA2_32` and writes `descriptor_pa` to
/// `ADMA_SYS_ADDR`. Called once at engine init; the descriptor table
/// memory is reused across every transfer. The per-transfer kick path
/// inside `issue_data_transfer` re-asserts both `DMA_SEL` AND
/// `ADMA_ADDRESS` defensively — idempotent against the engine init's
/// writes, but guards against any future code path that might change
/// either before a data transfer fires.
///
/// 32-bit ADMA2 is hardcoded — emmc2 is the only consumer and SDHCI
/// v3 on BCM2711 supports 32-bit only. A 64-bit ADMA2 helper lands
/// when a >4GiB-PA SDHCI consumer surfaces.
pub fn init_adma2_32(sdhci: &Sdhci, descriptor_pa: u32) {
    let tk = __sdhci_internal_mint(sdhci);
    sdhci.modify_host_control(
        |hc| hc.with_dma_sel(HostControlDmaSel::Adma2_32),
        &tk,
    );
    sdhci.write_adma_address(descriptor_pa, &tk);
}

/// Turn on IRQ-driven completion delivery for data-phase transfers.
/// Clears any stale `NORMAL_INT_STATUS` / `ERROR_INT_STATUS` bits
/// LEFT LATCHED FROM THE ID PHASE — then writes `SIGNAL_ENABLE` masks.
///
/// The stale-clear is load-bearing: per SDHCI v3 §2.2.24, the GIC
/// line is asserted whenever `(STATUS_ENABLE & SIGNAL_ENABLE & STATUS)`
/// is non-zero (combinatorial, no edge detection). If `SIGNAL_ENABLE`
/// flips on while `STATUS.DATA_COMPLETE` is still latched from a
/// prior R1b command (e.g. CMD7's busy release), the controller
/// asserts the GIC line IMMEDIATELY — the first `wait_until` in
/// `SdhciDataCompletion::await_complete` returns instantly on stale
/// status, the driver "succeeds" on a transfer that never happened,
/// and the buffer reads garbage. Clearing STATUS via W1C-0xFFFF before
/// the signal-enable flip is the structural fix.
pub fn enable_irq_signaling(
    sdhci: &Sdhci,
    normal: NormalIntSignalEnable,
    error: ErrorIntSignalEnable,
) {
    let tk = __sdhci_internal_mint(sdhci);
    sdhci.clear_normal_int_status(NormalIntStatus(0xFFFF), &tk);
    sdhci.clear_error_int_status(ErrorIntStatus(0xFFFF), &tk);
    sdhci.set_normal_int_signal_enable(normal, &tk);
    sdhci.set_error_int_signal_enable(error, &tk);
}

// ---------------------------------------------------------------------------
// SdhciDataCompletion — the device-done signal for data-phase transfers.
// ---------------------------------------------------------------------------

/// Failure shapes from the data-phase IRQ-driven completion wait.
/// Generic across SDHCI consumers; emmc2's `Emmc2Error` maps these
/// into its own variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdhciDataCompletionError {
    /// The `BoundIrq::wait_until` deadline expired before
    /// `NORMAL_INT_STATUS.CMD_COMPLETE` ever fired. The command
    /// never got past the controller's response phase.
    CmdCompleteTimeout,
    /// `CMD_COMPLETE` fired but `DATA_COMPLETE` did not within the
    /// remaining deadline. The data phase wedged.
    TransferCompleteTimeout,
    /// `NORMAL_INT_STATUS.ERROR` fired before `CMD_COMPLETE`;
    /// `err_int_status` holds the `ERROR_INT_STATUS` snapshot
    /// captured pre-W1C.
    CmdError { err_int_status: u16 },
    /// `NORMAL_INT_STATUS.ERROR` fired after `CMD_COMPLETE` but
    /// before `DATA_COMPLETE`. CRC/timeout in the data phase.
    DataError { err_int_status: u16 },
    /// `DATA_COMPLETE` fired but `PRESENT_STATE.DAT_INHIBIT` did
    /// not clear within the 10ms drain budget — the controller's
    /// post-DATA_COMPLETE AXI write tail wedged. B4.1 plan.
    DatInhibitStuck { present_state: u32 },
}

/// SDHCI data-transfer completion — the device-done signal the
/// coherence envelope awaits between the `kick` and the post-transfer
/// invalidate.
///
/// Wraps the IRQ-driven `CMD_COMPLETE`/`DATA_COMPLETE` wait plus the
/// B4.1 post-`DATA_COMPLETE` `DAT_INHIBIT` drain. SDHCI-family-generic;
/// any SDHCI consumer that runs IRQ-driven ADMA transfers uses this
/// shape. Per-driver error mapping happens at the consumer's
/// `DmaCompletion::Error` boundary.
pub struct SdhciDataCompletion<'a> {
    sdhci: &'a Sdhci,
    bound_irq: &'a mut BoundIrq,
}

impl<'a> SdhciDataCompletion<'a> {
    /// Construct a completion bound to `sdhci` + `bound_irq`. The
    /// envelope's `await_complete` consumes self.
    #[inline]
    pub fn new(sdhci: &'a Sdhci, bound_irq: &'a mut BoundIrq) -> Self {
        Self { sdhci, bound_irq }
    }
}

impl DmaCompletion for SdhciDataCompletion<'_> {
    type Error = SdhciDataCompletionError;

    fn await_complete(self) -> Result<(), Self::Error> {
        let tk = __sdhci_internal_mint(self.sdhci);
        // IRQ-driven completion. SDHCI is configured LEVEL_HIGH;
        // each IRQ delivery: kernel ACK+EOIR+mask in GIC, signals our
        // notification (counter += 1). `bound_irq.wait_until` wakes the
        // driver and advances the threshold by 1. We read
        // NORMAL_INT_STATUS, decode (CMD_COMPLETE, DATA_COMPLETE,
        // ERROR — could be any combination), W1C the latched bits, and
        // `bound_irq.unmask` so the GIC re-enables delivery.
        //
        // The loop handles both two-IRQ (CMD then DATA arrive far
        // apart) and one-IRQ-both-bits (fast transfer) cases.
        // `cmd_complete_seen` tracks whether CMD_COMPLETE was already
        // observed so an ERROR mid-data is reported as DataError, not
        // CmdError.
        //
        // Deadline-bounded: 1s covers both CMD_COMPLETE (typical <1ms)
        // and DATA_COMPLETE (typical <100ms for one block).
        let freq = crate::time::cntfreq_hz();
        let mut cmd_complete_seen = false;
        let deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
        loop {
            if self.bound_irq.wait_until(deadline).is_err() {
                return Err(if cmd_complete_seen {
                    SdhciDataCompletionError::TransferCompleteTimeout
                } else {
                    SdhciDataCompletionError::CmdCompleteTimeout
                });
            }

            let status = self.sdhci.normal_int_status(&tk);

            if status.contains(NormalIntStatus::ERROR) {
                let err_int_status = self.sdhci.error_int_status(&tk).bits();
                self.sdhci
                    .clear_error_int_status(ErrorIntStatus(0xFFFF), &tk);
                self.sdhci
                    .clear_normal_int_status(NormalIntStatus(0xFFFF), &tk);
                let _ = self.bound_irq.unmask();
                return Err(if cmd_complete_seen {
                    SdhciDataCompletionError::DataError { err_int_status }
                } else {
                    SdhciDataCompletionError::CmdError { err_int_status }
                });
            }

            if status.contains(NormalIntStatus::CMD_COMPLETE) {
                self.sdhci
                    .clear_normal_int_status(NormalIntStatus::CMD_COMPLETE, &tk);
                cmd_complete_seen = true;
            }

            if status.contains(NormalIntStatus::DATA_COMPLETE) {
                self.sdhci
                    .clear_normal_int_status(NormalIntStatus::DATA_COMPLETE, &tk);
                let _ = self.bound_irq.unmask();
                break;
            }

            // Spurious wake or CMD_COMPLETE alone with DATA in flight.
            // Unmask + loop; the deadline still bounds total time.
            let _ = self.bound_irq.unmask();
        }

        // B4.1 — post-DATA_COMPLETE DAT_INHIBIT drain. DATA_COMPLETE
        // signals card-side end, but the BCM2711 Arasan controller can
        // keep outbound AXI writes in flight to DRAM for a tail period.
        // The envelope's post-completion sync_for_cpu invalidate orders
        // CPU caches but does NOT arbitrate against the controller's
        // outstanding bus writes. PRESENT_STATE.DAT_INHIBIT is the
        // controller's own "data path genuinely idle" bit — polling it
        // here forces the read call to wait for the controller drain.
        //
        // 10ms deadline matches the B4.1 plan budget; the actual drain
        // is microseconds in normal operation.
        let drain_deadline =
            monotonic_now().deadline_in(Nanos::from_millis(10), freq);
        loop {
            let ps = self.sdhci.present_state();
            if !ps.contains(PresentState::DAT_INHIBIT) {
                return Ok(());
            }
            if drain_deadline.has_expired(monotonic_now()) {
                return Err(SdhciDataCompletionError::DatInhibitStuck {
                    present_state: ps.bits(),
                });
            }
            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// issue_data_transfer — the data-phase operation envelope.
// ---------------------------------------------------------------------------

/// Direction selector for a data-phase ADMA2 transfer. Maps to the
/// `TRNS_READ` bit in `TRANSFER_MODE` — set for card→host, clear for
/// host→card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataDirection {
    /// Card → host (CMD17 single-block, CMD18 multi-block). Sets
    /// `TRNS_READ` in `TRANSFER_MODE`.
    Read,
    /// Host → card (CMD24 single-block, CMD25 multi-block). Clears
    /// `TRNS_READ` in `TRANSFER_MODE`.
    Write,
}

/// Parameters for a **single-block** ADMA2-32 data transfer (CMD17
/// read or CMD24 write). Bundles only the per-transfer values that
/// vary between calls — `BLOCK_SIZE = 512` and `BLOCK_COUNT = 1` are
/// hardcoded in the kick because the implementation neither programs
/// `TRANSFER_MODE.{MULTI, BLK_CNT_EN, AUTO_CMD23}` nor writes
/// `ARGUMENT2`. Multi-block (CMD18/CMD25) re-introduction will land
/// the additional fields as part of the same commit that adds the
/// MULTI/BLK_CNT_EN/AUTO_CMD23 + ARGUMENT2 kick logic, so the API
/// surface always matches what the kick actually programs (Tier 3 #13
/// — illegal states unrepresentable).
pub struct SdhciDataTransfer {
    /// The SD command — CMD17 (ReadSingleBlock) for reads, CMD24
    /// (WriteBlock) for writes.
    pub cmd: SdCommand,
    /// Command argument — typically the LBA (SDHC/SDXC blocks-as-units).
    pub arg: u32,
    /// Read vs. write — selects `TRNS_READ` in `TRANSFER_MODE`.
    pub direction: DataDirection,
    /// Physical address of the ADMA2 descriptor table the controller
    /// will fetch. Must be 4-byte aligned and fit in u32 (ADMA2-32
    /// mode). The driver is responsible for writing the descriptor
    /// contents into the backing pageset BEFORE calling
    /// `issue_data_transfer` — the envelope cleans the descriptor's
    /// `DmaRegion` (in `regions`) before kicking the controller.
    pub adma_descriptor_pa: u32,
}

/// Failure shapes from [`SdhciCommandInit::issue_data_transfer`].
/// Mirrors the variants of [`DmaTransferError`] with the family-
/// generic [`SdhciDataCompletionError`] inlined plus the pre-kick
/// inhibit-poll failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdhciDataTransferError {
    /// `PRESENT_STATE.CMD_INHIBIT` or `DAT_INHIBIT` did not clear
    /// before the pre-kick poll deadline. No registers were touched.
    InhibitStuck { present_state: u32 },
    /// A pre-`kick` `sync_for_device` (clean) syscall failed.
    CleanFailed(SyscallError),
    /// The data-phase completion wait failed — IRQ timeout, CRC/error
    /// status, or DAT_INHIBIT drain timeout.
    Completion(SdhciDataCompletionError),
    /// A post-completion `sync_for_cpu` (invalidate) syscall failed.
    InvalidateFailed(SyscallError),
}

impl<'a> SdhciCommandInit<'a, OpIdle> {
    /// Issue an ADMA2-32 data-phase command (today: single-block read
    /// CMD17, parameterized for the obvious extensions) wrapped in
    /// the DMA coherence envelope.
    ///
    /// The driver writes the ADMA2 descriptor into a coherent pageset
    /// before calling this; the envelope's pre-kick clean ensures the
    /// descriptor is visible to the controller's DMA, and the post-
    /// completion invalidate ensures CPU reads of the data buffer see
    /// fresh DRAM rather than stale cache lines. `regions` should
    /// include both the descriptor (as `ToDevice`) and the data
    /// buffer (as `FromDevice` for reads, `ToDevice` for writes).
    ///
    /// Sequence:
    ///   1. Pre-kick `PRESENT_STATE` poll — `CMD_INHIBIT` and
    ///      `DAT_INHIBIT` both clear (100ms deadline). Writing
    ///      `ADMA_ADDRESS` or `HOST_CONTROL.DMA_SEL` while a transfer
    ///      is active wedges the BCM2711 emmc2 controller; this poll
    ///      is what `Linux's sdhci_send_command` does.
    ///   2. `run_dma_transfer` envelope:
    ///        - clean every `DmaRegion` (both directions per B2.2)
    ///        - kick: program HOST_CONTROL.DMA_SEL=ADMA2-32,
    ///          ADMA_ADDRESS, BLOCK_SIZE, BLOCK_COUNT, ARGUMENT, then
    ///          single-store TRANSFER_MODE+COMMAND combined trigger
    ///        - await `SdhciDataCompletion` (IRQ wait + B4.1 drain)
    ///        - invalidate every `FromDevice` region
    ///   3. Return the envelope in `OpIdle` so the next transfer can
    ///      issue without reopening.
    pub fn issue_data_transfer(
        self,
        params: SdhciDataTransfer,
        regions: &[DmaRegion],
        completion: SdhciDataCompletion<'_>,
    ) -> Result<Self, SdhciDataTransferError> {
        // 1. Pre-kick inhibit poll. Same shape as adma2_single_block_read's
        //    pre-kick poll — 100ms deadline.
        let inhibit_deadline =
            monotonic_now().deadline_in(Nanos::from_millis(100), crate::time::cntfreq_hz());
        loop {
            let ps = self.sdhci.present_state();
            if !ps.contains(PresentState::CMD_INHIBIT)
                && !ps.contains(PresentState::DAT_INHIBIT)
            {
                break;
            }
            if inhibit_deadline.has_expired(monotonic_now()) {
                return Err(SdhciDataTransferError::InhibitStuck {
                    present_state: ps.bits(),
                });
            }
            core::hint::spin_loop();
        }

        // 2. run_dma_transfer wraps the clean → kick → await →
        //    invalidate envelope. The kick programs the controller in
        //    the order BCM2711 expects: BLOCK_SIZE/COUNT/ARGUMENT before
        //    the combined TRANSFER_MODE+COMMAND store. DMA_SEL+
        //    ADMA_ADDRESS first so the controller has the descriptor
        //    table address before fetch starts.
        let kick_result = run_dma_transfer::<(), Infallible, _, _>(
            regions,
            completion,
            || -> Result<(), Infallible> {
                self.sdhci.modify_host_control(
                    |hc| hc.with_dma_sel(HostControlDmaSel::Adma2_32),
                    &self.tk,
                );
                self.sdhci
                    .write_adma_address(params.adma_descriptor_pa, &self.tk);
                // Single-block-only: BLOCK_SIZE = 512 (SDHC/SDXC) and
                // BLOCK_COUNT = 1 are hardcoded. Multi-block variants
                // would also program TRNS_MULTI / TRNS_BLK_CNT_EN /
                // TRNS_AUTO_CMD23 + ARGUMENT2 — see SdhciDataTransfer
                // doc for the API extension plan.
                self.sdhci.write_block_size(512, &self.tk);
                self.sdhci.write_block_count(1, &self.tk);
                self.sdhci.write_argument(params.arg, &self.tk);

                let trns_dir = match params.direction {
                    DataDirection::Read => SDHCI_TRNS_READ,
                    DataDirection::Write => 0,
                };
                let trns = trns_dir | SDHCI_TRNS_DMA;

                // Data-phase R1 command word: R1 (CRC+INDEX) + data bit.
                let cmd_word = sd_command_word(
                    params.cmd.index(),
                    SDHCI_CMD_RESP_SHORT
                        | SDHCI_CMD_CRC
                        | SDHCI_CMD_INDEX
                        | SDHCI_CMD_DATA,
                );
                // Single-store combined trigger — BCM2711 Arasan silently
                // drops the command if split into two halves.
                self.sdhci.set_transfer_mode_command(
                    TransferMode(trns),
                    Command(cmd_word),
                    &self.tk,
                );
                Ok(())
            },
        );

        match kick_result {
            Ok(()) => Ok(self),
            Err(DmaTransferError::CleanFailed(s)) => {
                Err(SdhciDataTransferError::CleanFailed(s))
            }
            Err(DmaTransferError::Kick(infallible)) => match infallible {},
            Err(DmaTransferError::Completion(c)) => {
                Err(SdhciDataTransferError::Completion(c))
            }
            Err(DmaTransferError::InvalidateFailed(s)) => {
                Err(SdhciDataTransferError::InvalidateFailed(s))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MmcCard<'a, S> — outer card-state typestate (O6).
// ---------------------------------------------------------------------------
//
// Linear lifecycle from card power-up to data-transfer-ready: Uninit
// → Idle (CMD0) → Ready (ACMD41 loop) → Ident (CMD2) → Stby (CMD3, with
// CMD9 staying in Stby) → Tran (CMD7 + DAT_INHIBIT drain, with ACMD6 +
// host_control flip staying in Tran). Each transition consumes self,
// runs the required ID-phase command(s) via `SdhciCommandInit`, and
// returns the next state. Card state is encoded in the type system —
// `engine.read()` against a `MmcCard<Stby>` is a compile error because
// `Emmc2BlockEngine::new` requires a `CardInfo`, whose only mint path
// is `MmcCard::<Tran>::into_parts()`.
//
// `MmcCard<'a, S>` borrows `&'a Sdhci` for its lifetime — the typestate
// chain is a sequence of consumed values, all sharing the same borrow.
// `into_parts()` drops the borrow and returns the captured `CardInfo`
// (rca/ocr/cid/csd/bus_width) so the engine can take ownership of
// `MappedRegs<Sdhci>` separately.

/// Failures from `MmcCard<S>` state transitions. Family-generic across
/// SDHCI consumers; drivers map to their own error type (e.g. emmc2's
/// `Emmc2Error`) at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmcCardError {
    /// The underlying SDHCI command failed (poll inhibit timeout, no
    /// response, controller error). Wraps the per-command failure for
    /// caller decode.
    Sdhci(SdhciCommandError),
    /// CMD8 R7 echo did not match the issued check pattern. The card is
    /// either pre-SDv2 (won't speak CMD8 at all — typically surfaces as
    /// `Sdhci(NoResponse)`), or the bus is mis-clocked / mis-voltaged
    /// and the response framing is corrupt.
    Cmd8EchoMismatch { echo: u32 },
    /// The ACMD41 loop ran for its full 1-second deadline without the
    /// card asserting `OCR.power_up_done`. Card is dead, missing, or
    /// has a power-supply problem.
    Acmd41Timeout,
    /// CMD9's R2 response decoded as something other than CSD v2
    /// (SDHC/SDXC). Lockjaw doesn't support legacy SDSC (CSD v1).
    /// Carries the actual `CSD_STRUCTURE` value seen.
    Cmd9NotV2 { csd_structure: u8 },
    /// CMD7's R1b busy phase did not release `PRESENT_STATE.DAT_INHIBIT`
    /// within the 500ms deadline — card never transitioned to Tran.
    Cmd7BusyStuck { present_state: u32 },
}

impl From<SdhciCommandError> for MmcCardError {
    #[inline]
    fn from(e: SdhciCommandError) -> Self {
        MmcCardError::Sdhci(e)
    }
}

/// Captured card metadata from the ID phase. The only mint path is
/// [`MmcCard::<Tran>::into_parts`], so requiring `CardInfo` as an
/// `Emmc2BlockEngine::new` parameter is compile-time proof that the
/// card reached `Tran`. Fields are `pub` for caller read; the
/// constructor is `pub(crate)` so nothing outside `lockjaw-userlib`
/// can fabricate one.
#[derive(Clone, Copy, Debug)]
pub struct CardInfo {
    rca: u16,
    ocr: OcrRegister,
    cid: [u32; 4],
    csd: CsdV2,
    bus_width: BusWidth,
}

impl CardInfo {
    /// Crate-private constructor — only `MmcCard::<Tran>::into_parts()`
    /// mints a `CardInfo`. Drivers cannot construct one directly.
    pub(crate) fn new(
        rca: u16,
        ocr: OcrRegister,
        cid: [u32; 4],
        csd: CsdV2,
        bus_width: BusWidth,
    ) -> Self {
        Self { rca, ocr, cid, csd, bus_width }
    }

    /// Relative Card Address published by CMD3.
    pub fn rca(&self) -> u16 { self.rca }
    /// OCR snapshot from the final ACMD41.
    pub fn ocr(&self) -> OcrRegister { self.ocr }
    /// CID raw words (R2 response from CMD2).
    pub fn cid(&self) -> [u32; 4] { self.cid }
    /// CSD v2 decoded capacity from CMD9.
    pub fn csd(&self) -> CsdV2 { self.csd }
    /// Current bus width on the controller side.
    pub fn bus_width(&self) -> BusWidth { self.bus_width }
}

/// Outer card-state typestate envelope. `S: CardLifecycleState`
/// constrains the phantom to one of the six legal markers (Uninit /
/// Idle / Ready / Ident / Stby / Tran).
pub struct MmcCard<'a, S: CardLifecycleState> {
    sdhci: &'a Sdhci,
    // Captured incrementally as each transition succeeds. Each Option
    // is filled at exactly one state transition and read by
    // `into_parts()` at `<Tran>`; the typestate chain guarantees every
    // field is `Some` by the time the card reaches Tran.
    ocr: Option<OcrRegister>,
    cid: Option<[u32; 4]>,
    rca: Option<u16>,
    csd: Option<CsdV2>,
    bus_width: BusWidth,
    _state: PhantomData<S>,
}

impl<'a> MmcCard<'a, Uninit> {
    /// Open a new card lifecycle envelope against `sdhci`. The card has
    /// not been touched yet; the first transition is `go_idle()` (CMD0).
    pub fn uninit(sdhci: &'a Sdhci) -> Self {
        Self {
            sdhci,
            ocr: None,
            cid: None,
            rca: None,
            csd: None,
            bus_width: BusWidth::Bit1,
            _state: PhantomData,
        }
    }

    /// CMD0 — GO_IDLE_STATE. Resets the card to the Idle state. No
    /// response. Tolerant: if CMD0 has any error path the caller can
    /// inspect via `Sdhci(_)`, but the card may simply have been Idle
    /// already (CMD0-on-Idle is a no-op).
    pub fn go_idle(self) -> Result<MmcCard<'a, Idle>, MmcCardError> {
        SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R0>(SdCommand::GoIdleState, 0)
            .map_err(MmcCardError::from)?;
        Ok(self.transition())
    }
}

impl<'a> MmcCard<'a, Idle> {
    /// CMD8 — SEND_IF_COND. Verifies SDv2+ compatibility by echoing
    /// the check pattern. Returns the original `MmcCard<Idle>` on
    /// success. Stays in Idle — CMD8 is a one-shot identification
    /// probe, not a state-changing command. Driver typically calls
    /// this before `power_up_to_ready` to verify the card is SDHC/SDXC
    /// capable.
    pub fn verify_sdv2_if_cond(self) -> Result<Self, MmcCardError> {
        let echo: u32 = SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R7>(SdCommand::SendIfCond, CMD8_IF_COND_ARG)
            .map_err(MmcCardError::from)?
            .0;
        // R7: bits[11:8] = VHS echo, bits[7:0] = check pattern echo.
        // Combined bits[11:0] must equal 0x1AA (matching CMD8_IF_COND_ARG).
        if echo & 0xFFF != 0x1AA {
            return Err(MmcCardError::Cmd8EchoMismatch { echo });
        }
        Ok(self)
    }

    /// ACMD41 loop — SD spec §4.2.3.1. Each iteration: CMD55 (sets
    /// APP_CMD mode for the next command), then ACMD41 with the
    /// caller-supplied argument (typically `ACMD41_ARG_HCS` =
    /// 0x40FF8000 to request SDHC/SDXC). Loops on a 1-second deadline
    /// until `OCR.power_up_done` is set, with a 10ms inter-retry sleep
    /// (SD spec doesn't require it but gives the card breathing room).
    /// Captures the final OCR for `CardInfo`.
    pub fn power_up_to_ready(
        self,
        acmd41_arg: u32,
    ) -> Result<MmcCard<'a, Ready>, MmcCardError> {
        let freq = crate::time::cntfreq_hz();
        let deadline = monotonic_now().deadline_in(Nanos::from_secs(1), freq);
        loop {
            // CMD55 — broadcast (arg=0); response is R1 from the card.
            SdhciCommandInit::open(self.sdhci)
                .issue_no_data::<R1>(SdCommand::AppCmd, 0)
                .map_err(MmcCardError::from)?;
            // ACMD41 — R3 (no CRC/index check); OCR returns busy bit
            // clear when card finished init.
            let ocr = SdhciCommandInit::open(self.sdhci)
                .issue_no_data::<R3>(SdCommand::SdSendOpCond, acmd41_arg)
                .map_err(MmcCardError::from)?
                .0;
            if ocr.power_up_done {
                let mut next: MmcCard<'a, Ready> = self.transition();
                next.ocr = Some(ocr);
                return Ok(next);
            }
            if deadline.has_expired(monotonic_now()) {
                return Err(MmcCardError::Acmd41Timeout);
            }
            let _ = sleep_for(Nanos::from_millis(10));
        }
    }
}

impl<'a> MmcCard<'a, Ready> {
    /// CMD2 — ALL_SEND_CID. Card returns 136-bit CID in R2 response.
    /// Captures the four-word view for `CardInfo`.
    pub fn identify(self) -> Result<MmcCard<'a, Ident>, MmcCardError> {
        let cid = SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R2>(SdCommand::AllSendCid, 0)
            .map_err(MmcCardError::from)?
            .0;
        let mut next: MmcCard<'a, Ident> = self.transition();
        next.cid = Some(cid);
        Ok(next)
    }
}

impl<'a> MmcCard<'a, Ident> {
    /// CMD3 — SEND_RELATIVE_ADDR. Card publishes its RCA. R6 response
    /// carries `(rca, status)`; captures the RCA for `CardInfo` and
    /// subsequent RCA-targeted commands (CMD7, CMD9, CMD13).
    pub fn publish_rca(self) -> Result<MmcCard<'a, Stby>, MmcCardError> {
        let r6: R6Response = SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R6>(SdCommand::SendRelativeAddr, 0)
            .map_err(MmcCardError::from)?
            .0;
        let mut next: MmcCard<'a, Stby> = self.transition();
        next.rca = Some(r6.rca);
        Ok(next)
    }
}

impl<'a> MmcCard<'a, Stby> {
    /// Transition Stby → Tran by running CMD9 (SEND_CSD) to capture
    /// the card-capacity metadata, then CMD7 (SELECT_CARD) to move
    /// the card into the Transfer state, then polling
    /// `PRESENT_STATE.{CMD,DAT}_INHIBIT` until both clear (500ms
    /// deadline for the R1b busy release).
    ///
    /// CMD9 + CMD7 are folded into one transition because every
    /// production path needs CSD before any data transfer, AND
    /// because folding them makes "`CardInfo` contains a valid CSD"
    /// structural: every `MmcCard<Tran>` was constructed via this
    /// method, which always captures CSD before transitioning. If
    /// they were separate methods, `publish_rca()?.select()?` would
    /// compile but `into_parts()` would panic at runtime when the
    /// consumer reads `csd` — codex + opus called that out as a
    /// proof-property gap in O6 round 1.
    ///
    /// Failure paths:
    ///   - CMD9 fails (`Sdhci`) or returns a CSD that isn't v2
    ///     (`Cmd9NotV2`) → card stays in Stby on the bus, no
    ///     state advance.
    ///   - CMD7 fails (`Sdhci`) → card stays in Stby, no advance.
    ///   - DAT_INHIBIT poll times out (`Cmd7BusyStuck`) → CMD7
    ///     fired but the card never released busy.
    pub fn select(self) -> Result<MmcCard<'a, Tran>, MmcCardError> {
        let rca = self.rca.expect("rca captured at MmcCard<Stby> construction");
        let rca_arg = (rca as u32) << 16;

        // CMD9 first — capture CSD before transitioning. If CMD9
        // errors or the CSD isn't v2, the card stays in Stby on the
        // bus and the typestate doesn't advance.
        let words = SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R2>(SdCommand::SendCsd, rca_arg)
            .map_err(MmcCardError::from)?
            .0;
        let csd = CsdV2::decode(words).map_err(|NotCsdV2 { csd_structure }| {
            MmcCardError::Cmd9NotV2 { csd_structure }
        })?;

        // CMD7 — actually select the card.
        SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R1b>(SdCommand::SelectCard, rca_arg)
            .map_err(MmcCardError::from)?;
        // Wait for DAT0 to deassert — the card signals "ready" by
        // releasing it. 500ms covers the worst-case card-internal
        // state-transition latency observed in the field.
        let freq = crate::time::cntfreq_hz();
        let deadline = monotonic_now().deadline_in(Nanos::from_millis(500), freq);
        loop {
            let ps = self.sdhci.present_state();
            if !ps.contains(PresentState::CMD_INHIBIT)
                && !ps.contains(PresentState::DAT_INHIBIT)
            {
                // Transition to Tran with CSD captured — the
                // structural proof for into_parts()'s csd.expect.
                let mut next: MmcCard<'a, Tran> = self.transition();
                next.csd = Some(csd);
                return Ok(next);
            }
            if deadline.has_expired(monotonic_now()) {
                return Err(MmcCardError::Cmd7BusyStuck {
                    present_state: ps.bits(),
                });
            }
            core::hint::spin_loop();
        }
    }
}

impl<'a> MmcCard<'a, Tran> {
    /// CMD55+ACMD6 — SET_BUS_WIDTH to 4-bit. Must be preceded by CMD55
    /// addressed to the selected card (RCA). After the card
    /// acknowledges ACMD6, mirrors the 4-bit width in
    /// `HOST_CONTROL_1.DAT_4BIT` via [`set_bus_width_4bit`]. Stays in
    /// Tran — bus width is a runtime field on `CardInfo`, not a
    /// typestate slot.
    pub fn set_bus_width_4bit(mut self) -> Result<Self, MmcCardError> {
        let rca = self.rca.expect("rca captured at MmcCard<Tran> construction");
        let rca_arg = (rca as u32) << 16;
        SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R1>(SdCommand::AppCmd, rca_arg)
            .map_err(MmcCardError::from)?;
        SdhciCommandInit::open(self.sdhci)
            .issue_no_data::<R1>(SdCommand::SetBusWidth, 0x2)
            .map_err(MmcCardError::from)?;
        set_bus_width_4bit(self.sdhci);
        self.bus_width = BusWidth::Bit4;
        Ok(self)
    }

    /// Drop the `&Sdhci` borrow and return the captured `CardInfo` —
    /// the structural proof token that crosses into engine
    /// construction. Each field was filled at exactly one state
    /// transition along the typestate chain; the chain guarantees
    /// every `.expect()` here succeeds.
    pub fn into_parts(self) -> CardInfo {
        CardInfo::new(
            self.rca.expect("rca captured at MmcCard<Stby>"),
            self.ocr.expect("ocr captured at MmcCard<Ready>"),
            self.cid.expect("cid captured at MmcCard<Ident>"),
            self.csd.expect("csd captured at MmcCard<Stby> read_csd"),
            self.bus_width,
        )
    }
}

impl<'a, S: CardLifecycleState> MmcCard<'a, S> {
    /// Transition helper — preserves captured metadata across the
    /// typestate change. Private; only the impl blocks above can call
    /// it (each lives in this module).
    #[inline]
    fn transition<N: CardLifecycleState>(self) -> MmcCard<'a, N> {
        MmcCard {
            sdhci: self.sdhci,
            ocr: self.ocr,
            cid: self.cid,
            rca: self.rca,
            csd: self.csd,
            bus_width: self.bus_width,
            _state: PhantomData,
        }
    }
}

