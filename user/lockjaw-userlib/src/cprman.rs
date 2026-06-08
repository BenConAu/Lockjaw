//! CPRMAN (BCM2711 clock provider) family's `lockjaw-userlib`
//! surface — the legal path drivers use to talk to the controller.
//! Re-exports the safe pieces of `lockjaw_regs::cprman` so driver
//! crates never name `lockjaw_regs` directly. The xtask
//! `check-driver-unsafe` regime enforces the ban on
//! `lockjaw_regs::cprman` imports in `user/*-driver/` source (the
//! `(lockjaw_regs, cprman)` entry in `BANNED_DRIVER_MODULE_PATHS`).
//!
//! Provided so far: re-exports of `Cprman`, `CmEmmc2Ctl`,
//! `CmEmmc2CtlSrc`, `CmEmmc2Div` — the surface cprman-driver
//! consumes today.

pub use lockjaw_regs::cprman::{CmEmmc2Ctl, CmEmmc2CtlSrc, CmEmmc2Div, Cprman};
