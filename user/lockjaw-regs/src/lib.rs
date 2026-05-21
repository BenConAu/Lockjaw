//! `lockjaw-regs` — generated typed register layers for user-mode
//! device drivers.
//!
//! Every module in this crate is **generated** from a TOML spec in
//! `user/regspecs/`. Do not hand-edit the per-device files; edit the
//! spec and run `cargo xtask gen-regs`. `cargo xtask gen-regs --check`
//! verifies committed code matches the spec; CI gates on it.
//!
//! Drivers consume the generated typed accessors and the underlying
//! `lockjaw-mmio` cells; they never touch raw MMIO. This crate runs
//! `#![forbid(unsafe_code)]` to make that compile-enforced (the
//! generated code uses safe `lockjaw-mmio` cell methods).
//!
//! Phase 2 ships PL011 only as the proof-of-concept; Phases 3-7 add
//! one device each (`virtio`, `fwcfg`, `cprman`, `sdhci`).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod cprman;
pub mod fw_cfg;
pub mod pl011;
pub mod virtio_mmio;
