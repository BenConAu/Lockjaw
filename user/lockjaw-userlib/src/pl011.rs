//! PL011 family's `lockjaw-userlib` surface — the legal path drivers
//! use to talk to the controller. Re-exports the safe pieces of
//! `lockjaw_regs::pl011` so driver crates never name `lockjaw_regs`
//! directly. The xtask `check-driver-unsafe` regime enforces the ban
//! on `lockjaw_regs::pl011` imports in `user/*-driver/` source (the
//! `(lockjaw_regs, pl011)` entry in `BANNED_DRIVER_MODULE_PATHS`).
//!
//! Provided so far:
//!   - P1: re-exports of `Pl011`, `Flag`, `Imsc`, `Lcrh`, `Control`, `Icr`.
//!   - P2 (this commit): [`write_byte_deadline`] deadline-bounded TX
//!     wrapper + [`TxTimeout`] error. Closes the unbounded-spin
//!     tech-debt entry at `docs/tracking/tech-debt.md` (PL011 TX wait
//!     is unbounded).
//!
//! Coming later in the plan:
//!   - P3: `set_interrupt_masks` write-replace IMSC helper (closes
//!     the non-atomic RMW correctness gap by construction) +
//!     `drain_rx_fifo` RX FIFO drainer.

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
