//! DMA transfer direction and the pure cache-coherence op policy.
//!
//! Pure types only — no asm, no MMIO, no syscalls. The side-effecting
//! envelope that issues the actual `dc cvac` / `dc civac` syscalls
//! lives in `lockjaw-userlib::dma_transfer`; the asm primitives live
//! kernel-side in `src/arch/aarch64/cache.rs`. This module owns the
//! *decision* — which cache-maintenance op a region needs given its
//! transfer direction — so that decision is host-testable and the
//! envelope is a thin executor of it.
//!
//! Pairs with [`crate::cache`] (the cache-line range math).

/// Direction of a DMA region, from the device's point of view.
///
/// Discriminants start at 1 (ben_principles Tier 3 #14): although a
/// `DmaDir` is an API argument rather than a value read out of memory,
/// the 1-based convention keeps a zero pattern observably non-default
/// should this type ever be serialised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DmaDir {
    /// CPU-written, device-read — e.g. an ADMA2 descriptor or a
    /// write-data buffer. The CPU's writes must be CLEANED (written
    /// back) to DRAM before the device reads them.
    ToDevice = 1,
    /// Device-written, CPU-read — e.g. a read-data buffer. Cleaned
    /// before the transfer (so a pre-DMA dirty CPU line cannot later
    /// be written back over the device's bytes — the B2.2 fix) AND
    /// invalidated after completion (so the CPU reads fresh DRAM, not
    /// a stale cache line — the M0–M7 stale-read class).
    FromDevice = 2,
}

impl DmaDir {
    /// Whether the region is cleaned (written back) before the device
    /// runs. True for BOTH directions: Lockjaw cleans `FromDevice`
    /// regions pre-transfer too (B2.2), unlike the textbook model that
    /// cleans only `ToDevice`. Dropping the `FromDevice` pre-clean
    /// reintroduces the stale-zeros bug the post-C1 work fought, so the
    /// decision is expressed here as data and pinned by a host test.
    #[inline]
    pub const fn cleans_before(self) -> bool {
        true
    }

    /// Whether the region is invalidated after the device finishes.
    /// `FromDevice` only — a `ToDevice` region is never device-written,
    /// so the CPU's cached copy cannot be stale.
    #[inline]
    pub const fn invalidates_after(self) -> bool {
        matches!(self, DmaDir::FromDevice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The load-bearing coherence invariant, pinned as a pure test so a
    // refactor of the envelope cannot silently regress to the textbook
    // "clean ToDevice only" model (which would drop B2.2).
    #[test]
    fn from_device_cleans_before_and_invalidates_after() {
        assert!(DmaDir::FromDevice.cleans_before());
        assert!(DmaDir::FromDevice.invalidates_after());
    }

    #[test]
    fn to_device_cleans_before_only() {
        assert!(DmaDir::ToDevice.cleans_before());
        assert!(!DmaDir::ToDevice.invalidates_after());
    }

    #[test]
    fn discriminants_are_one_based() {
        assert_eq!(DmaDir::ToDevice as u8, 1);
        assert_eq!(DmaDir::FromDevice as u8, 2);
    }
}
