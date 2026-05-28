//! Hand-written (NOT generated) guard for SDHCI register bits that
//! are structurally mandatory for an IRQ-driven SDHCI consumer.
//!
//! Why this exists separately from the generated per-flag value
//! tests in `src/sdhci.rs`: those tests are emitted FROM the regspec
//! flags, so deleting a flag from `user/regspecs/sdhci.toml` also
//! deletes its value test — the regression "a mandatory int-enable
//! bit was dropped from the spec" passes silently because the test
//! that would catch it vanished with the flag. This file lives in
//! `tests/` (a separate crate, not regenerated) and references the
//! mandatory consts by name, so dropping any of them from the
//! regspec turns into a COMPILE error here that survives
//! `cargo xtask gen-regs`.
//!
//! Scope: only bits the SDHCI 3.0 spec marks required for IRQ-driven
//! operation — not every feature bit (modelling those is use-driven
//! per YAGNI; see nifty-rolling-naur.md "Register models are
//! use-driven"). The driver-side regression "the emmc2 driver
//! composes its SIGNAL_ENABLE set without one of these bits" is NOT
//! catchable here (the composition lives in the driver binary, which
//! has no host-test harness, and QEMU does not emulate SDHCI error
//! IRQs) — see docs/tech-debt.md "emmc2 error-IRQ enable is
//! Pi-fault-path-only" for that gap.

use lockjaw_regs::sdhci::{
    ErrorIntSignalEnable, ErrorIntStatusEnable, NormalIntSignalEnable,
    NormalIntStatusEnable,
};

/// NORMAL_INT_STATUS_ENABLE / NORMAL_INT_SIGNAL_ENABLE bit 15 is the
/// master gate for the error summary (latch + IRQ delivery
/// respectively, SDHCI 3.0 §2.2.21 Table 2-32). Both are mandatory
/// for any IRQ-driven SDHCI consumer: without the STATUS_ENABLE bit
/// the error summary never latches into NORMAL_INT_STATUS, and
/// without the SIGNAL_ENABLE bit a latched error never raises the
/// IRQ line regardless of per-error ERROR_INT_SIGNAL_ENABLE bits.
/// Referencing `::ERROR` here makes its deletion from the regspec a
/// compile error.
#[test]
fn normal_int_error_master_gate_bits_present() {
    assert_eq!(NormalIntStatusEnable::ERROR.bits(), 1 << 15);
    assert_eq!(NormalIntSignalEnable::ERROR.bits(), 1 << 15);
}

/// The completion events an IRQ-driven block driver waits on must be
/// signalable: CMD_COMPLETE (command response done) and
/// DATA_COMPLETE (transfer done). Referencing them guards against a
/// regspec edit that renumbers or drops them.
#[test]
fn normal_int_completion_signal_bits_present() {
    assert_eq!(NormalIntSignalEnable::CMD_COMPLETE.bits(), 1 << 0);
    assert_eq!(NormalIntSignalEnable::DATA_COMPLETE.bits(), 1 << 1);
}

/// The per-error class bits the driver decodes when the NORMAL error
/// summary fires must be signalable through ERROR_INT_SIGNAL_ENABLE
/// and latchable through ERROR_INT_STATUS_ENABLE. Guard the data-path
/// error classes the emmc2 IRQ loop reports (CRC / timeout / end-bit)
/// plus the ADMA descriptor-error class.
#[test]
fn error_int_data_path_class_bits_present() {
    for bit in [
        ErrorIntSignalEnable::DATA_TIMEOUT,
        ErrorIntSignalEnable::DATA_CRC,
        ErrorIntSignalEnable::DATA_END_BIT,
        ErrorIntSignalEnable::ADMA,
    ] {
        assert_ne!(bit.bits(), 0);
    }
    for bit in [
        ErrorIntStatusEnable::DATA_TIMEOUT,
        ErrorIntStatusEnable::DATA_CRC,
        ErrorIntStatusEnable::DATA_END_BIT,
        ErrorIntStatusEnable::ADMA,
    ] {
        assert_ne!(bit.bits(), 0);
    }
}
