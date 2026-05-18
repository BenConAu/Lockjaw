//! Memory barrier wrappers.
//!
//! Safe to call from `#![forbid(unsafe_code)]` contexts. Each
//! function wraps a single `asm!` instruction. The `unsafe` blocks
//! here are audited individually — one line each.
//!
//! # CRITICAL — asm options
//!
//! Use `options(nostack, preserves_flags)`. **Never** add `nomem`.
//! `nomem` tells LLVM the inline asm doesn't touch memory, which
//! would license the optimizer to reorder the very memory accesses
//! the barrier exists to order — silently defeating it. The existing
//! correct wrappers in `user/lockjaw-userlib/src/virtqueue.rs:21-43`
//! use this exact options shape; copy it for any new variant added
//! here.
//!
//! # Variants
//!
//! - `dsb_sy()` — full system DSB. Ensures all earlier memory
//!   accesses (from any agent) complete before any later access.
//! - `dmb_ish()` — Inner Shareable DMB. Orders memory accesses by
//!   any observer in the Inner Shareable domain (all CPUs in
//!   Lockjaw's SMP setup).
//! - `dmb_ishst()` — Inner Shareable DMB, store-store only. Cheaper
//!   than full `dmb_ish` when only store ordering matters
//!   (the common case for "publish to a device after writing
//!   descriptor data").
//! - `dmb_ishld()` — Inner Shareable DMB, load-load + load-store.
//!   Used when you need to ensure earlier loads complete before
//!   later memory accesses (e.g., reading a device-written
//!   completion flag before dereferencing a pointer it gates).
//! - `isb()` — Instruction Synchronization Barrier. Flushes the
//!   pipeline; rarely needed in driver code but provided for
//!   parity with kernel/MMU work.
//!
//! Adding a new variant (`dsb_st`, `dsb_ld`, etc.) is a one-line
//! addition; resist the urge to make a generic interface.

// On non-aarch64 targets (host tests), provide compiler-fence-based
// fallbacks. These don't model the hardware barrier (there isn't
// one to model on x86_64), but they keep the function signatures
// available so host tests can call into code that uses them.
#[cfg(target_arch = "aarch64")]
use core::arch::asm;

/// Full system DSB.
#[inline(always)]
pub fn dsb_sy() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: dsb sy has no side effects beyond ordering. The asm
    // options correctly omit `nomem` so LLVM treats this as a
    // memory barrier.
    unsafe {
        asm!("dsb sy", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// Inner Shareable DMB (full).
#[inline(always)]
pub fn dmb_ish() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as dsb_sy.
    unsafe {
        asm!("dmb ish", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// Inner Shareable DMB, store-store ordering.
#[inline(always)]
pub fn dmb_ishst() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as dsb_sy.
    unsafe {
        asm!("dmb ishst", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
}

/// Inner Shareable DMB, load-load + load-store ordering.
#[inline(always)]
pub fn dmb_ishld() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as dsb_sy.
    unsafe {
        asm!("dmb ishld", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
}

/// Instruction Synchronization Barrier.
#[inline(always)]
pub fn isb() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: ISB has no side effects beyond pipeline synchronization.
    unsafe {
        asm!("isb", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: calling each barrier helper from safe code must
    /// compile and not panic. We can't verify hardware barrier
    /// semantics on host (no MMIO, no SMP ordering effect we can
    /// observe), but proving the signatures are callable from a
    /// `#![forbid(unsafe_code)]` context indirectly via a test fn
    /// is the contract that matters for drivers.
    #[test]
    fn barriers_are_callable_from_safe_code() {
        // No #![forbid(unsafe_code)] on the test module itself, but
        // each call site below is safe Rust by construction.
        dsb_sy();
        dmb_ish();
        dmb_ishst();
        dmb_ishld();
        isb();
    }
}
