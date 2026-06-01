//! PL011 family's `lockjaw-userlib` surface — the legal path drivers
//! use to talk to the controller. Re-exports the safe pieces of
//! `lockjaw_regs::pl011` so driver crates never name `lockjaw_regs`
//! directly. The xtask `check-driver-unsafe` regime enforces the ban
//! on `lockjaw_regs::pl011` imports in `user/*-driver/` source (the
//! `(lockjaw_regs, pl011)` entry in `BANNED_DRIVER_MODULE_PATHS`).
//!
//! Provided in P1 (this commit): re-exports only.
//!
//! Coming later in the plan:
//!   - P2: `write_byte_deadline` deadline-bounded TX wrapper +
//!     `TxTimeout` error. Closes the unbounded-spin tech-debt entry
//!     at `docs/tracking/tech-debt.md` (PL011 TX wait is unbounded).
//!   - P3: `set_interrupt_masks` write-replace IMSC helper (closes
//!     the non-atomic RMW correctness gap by construction) +
//!     `drain_rx_fifo` RX FIFO drainer.

pub use lockjaw_regs::pl011::{Control, Flag, Icr, Imsc, Lcrh, Pl011};
