//! PL011 family's `lockjaw-userlib` surface — the legal path drivers
//! use to talk to the controller. Re-exports the safe pieces of
//! `lockjaw_regs::pl011` so driver crates never name `lockjaw_regs`
//! directly. The xtask `check-driver-unsafe` regime enforces the ban
//! on `lockjaw_regs::pl011` imports in `user/*-driver/` source (the
//! `(lockjaw_regs, pl011)` entry in `BANNED_DRIVER_MODULE_PATHS`).
//!
//! Provided so far:
//!   - P1: re-exports of `Pl011`, `Flag`, `Imsc`, `Lcrh`, `Control`, `Icr`.
//!   - P2: [`write_byte_deadline`] deadline-bounded TX wrapper +
//!     [`TxTimeout`] error. Closes the unbounded-spin tech-debt
//!     entry at `docs/tracking/tech-debt.md` (PL011 TX wait is
//!     unbounded).
//!   - P3 (this commit): [`set_interrupt_masks`] write-replace IMSC
//!     helper (closes the non-atomic RMW correctness gap by
//!     construction) + [`drain_rx_fifo`] RX FIFO drainer.

use crate::time::{spin_until_or_deadline, MonoTicks};

pub use lockjaw_regs::pl011::{Control, Flag, Icr, Imsc, Lcrh, Pl011};

/// Returned by [`write_byte_deadline`] when `Flag::TXFF` did not
/// clear before the deadline expired. Named after the operation
/// (TX) rather than the underlying primitive's `DeadlineExpired`
/// so the failure mode is self-describing at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxTimeout;

/// Write `byte` to the UART once `Flag::TXFF` clears, or fail with
/// [`TxTimeout`] if `deadline` expires first. Wraps
/// [`spin_until_or_deadline`](crate::time::spin_until_or_deadline) so
/// the TX spin is bounded per-board via the driver-side deadline.
pub fn write_byte_deadline(
    regs: &Pl011,
    byte: u8,
    deadline: MonoTicks,
) -> Result<(), TxTimeout> {
    spin_until_or_deadline(|| !regs.flag().contains(Flag::TXFF), deadline)
        .map_err(|_| TxTimeout)?;
    regs.write_data(byte as u32);
    Ok(())
}

/// Write the full interrupt mask in one register write — **not** a
/// read-modify-write. The caller passes the complete desired mask;
/// every IMSC bit not set in `masks` is cleared. This is the Tier 3
/// #13 explicit-init shape: name what you want, do not OR-against-
/// what-was-there. The non-atomic RMW race window (read → modify →
/// write, with no synchronization against a concurrent writer)
/// closes by construction because there is no read step.
///
/// Today's only caller is `uart_main`, which passes `Imsc::RXIM` to
/// enable RX interrupts. TXIM gets cleared, which is deliberate
/// current policy — the driver does TX via polling
/// [`write_byte_deadline`], not via TX interrupts. A future TX-IRQ
/// optimization composes naturally: `Imsc::RXIM | Imsc::TXIM`.
///
/// Mirrors SDHCI's `set_int_enable_masks` at
/// `user/lockjaw-userlib/src/sdhci.rs:374` — same write-replace
/// shape, same SDHCI-init-helper style.
pub fn set_interrupt_masks(regs: &Pl011, masks: Imsc) {
    regs.set_imsc(masks);
}

/// Drain the RX FIFO, invoking `on_byte` once per byte received.
/// Loops until `Flag::RXFE` (FIFO empty) becomes true.
///
/// Two helper-scope assumptions, true for today's echo driver:
/// (i) the low 8 bits of `DATA` are the only semantically relevant
/// payload (framing/parity/break diagnostics in the upper bits are
/// not surfaced to the callback);
/// (ii) draining the FIFO is sufficient for the current IRQ-ack
/// path — no `ICR` (interrupt clear register) participation needed
/// because the RX-interrupt source clears via FIFO-empty rather
/// than via explicit ack.
///
/// A future PL011-mode change (e.g., enabling framing-error reporting)
/// or a cross-vendor `UartTransport` extraction whose hardware needs
/// explicit IRQ ack would need to revisit these assumptions.
pub fn drain_rx_fifo(regs: &Pl011, mut on_byte: impl FnMut(u8)) {
    while !regs.flag().contains(Flag::RXFE) {
        on_byte((regs.read_data() & 0xFF) as u8);
    }
}
