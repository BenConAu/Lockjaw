//! Correct-by-construction DMA coherence envelope.
//!
//! A driver describes a transfer declaratively and the framework owns
//! the cache-coherence ordering, so a driver cannot forget a sync,
//! sync the wrong direction or range, or invalidate before the device
//! is done. This is the volatile-MMIO-accessor analogue for DMA
//! coherence: the discipline is exposed only here, never copied into
//! driver source (no driver may call `sys_dma_sync_*` directly).
//!
//! The driver supplies the three things only it knows:
//! - the memory [`DmaRegion`]s and their [`DmaDir`],
//! - a [`DmaCompletion`] that says WHEN the device has finished,
//! - a `kick` closure that programs the device and issues the op.
//!
//! The envelope runs:
//!
//! ```text
//! clean every region (both directions — B2.2)
//!   -> kick (driver programs + issues)
//!   -> completion.await_complete()
//!   -> invalidate every FromDevice region
//! ```
//!
//! Device-specific completion semantics (e.g. SDHCI's `DATA_COMPLETE`
//! IRQ followed by the `DAT_INHIBIT` drain) live in the driver/family
//! layer as a [`DmaCompletion`] impl — never in this generic module
//! (layer separation: this file carries no device policy).
//!
//! Scope note: the device-programmable PA is still held by the driver
//! (it builds the descriptor / programs the address register inside
//! `kick`). Making the PA unobtainable outside the envelope — full
//! no-bypass — is the future `SdhciCommandInit<S>` operation layer,
//! which will *hold* this envelope. This module owns the coherence
//! ordering, which is the bug class that has actually bitten emmc2.

use crate::dma::{DmaBacking, OwnedDmaMapping, SyncCapable};
use crate::dma_sync::{sys_dma_sync_for_cpu, sys_dma_sync_for_device};
use crate::handle::PageSetHandle;
use lockjaw_types::syscall::SyscallError;

pub use lockjaw_types::dma_coherence::DmaDir;

/// A contiguous byte range within a DMA pageset, tagged with the
/// direction of the transfer that will touch it.
///
/// Has no public constructor: a `DmaRegion` is minted only from a
/// `SyncCapable` (DmaPool-origin) mapping or backing via
/// [`OwnedDmaMapping::dma_region`] / [`DmaBacking::dma_region`]. The
/// kernel's `sys_dma_sync_*` reject any non-DmaPool origin, so gating
/// region construction on `SyncCapable` turns "hand a coherence-
/// incapable (Buddy) mapping to a sync" into a compile error rather
/// than a runtime `INVALID_PARAMETER` — a driver cannot fabricate a
/// region from a raw handle.
#[derive(Clone, Copy)]
pub struct DmaRegion {
    pageset: PageSetHandle,
    offset: u64,
    len: u64,
    dir: DmaDir,
}

impl<O: SyncCapable> OwnedDmaMapping<O> {
    /// Mint a coherence region over `[offset, offset+len)` of this
    /// mapping, to hand to [`run_dma_transfer`]. The range is validated
    /// against the pageset size by the kernel sync syscall.
    #[inline]
    pub fn dma_region(&self, offset: u64, len: u64, dir: DmaDir) -> DmaRegion {
        DmaRegion { pageset: self.pageset(), offset, len, dir }
    }
}

impl<O: SyncCapable> DmaBacking<O> {
    /// Mint a coherence region over `[offset, offset+len)` of this
    /// backing -- for the buffer path, which has no local mapping.
    #[inline]
    pub fn dma_region(&self, offset: u64, len: u64, dir: DmaDir) -> DmaRegion {
        DmaRegion { pageset: self.pageset, offset, len, dir }
    }
}

/// The device-completion signal the envelope awaits between the `kick`
/// and the post-transfer invalidate.
///
/// Implementors define WHEN the device is done — an IRQ wait, a
/// register poll, or a composite of both. That is the one piece of the
/// transfer only the driver knows; requiring it as an input is what
/// makes "invalidate before the device finished" unrepresentable.
pub trait DmaCompletion {
    /// Error returned when the device fails or the wait times out.
    type Error;
    /// Block until the device has finished the transfer.
    fn await_complete(self) -> Result<(), Self::Error>;
}

/// Completion for a transfer whose `kick` already blocks until the
/// device is done — e.g. a loop of sub-transfers, each awaiting its
/// own completion. The invalidate runs as soon as `kick` returns.
pub struct Immediate;

impl DmaCompletion for Immediate {
    type Error = core::convert::Infallible;
    #[inline]
    fn await_complete(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// The four distinct failure points of a DMA transfer, kept separate
/// so the driver can map each to its own error variant.
pub enum DmaTransferError<K, C> {
    /// A pre-`kick` `sync_for_device` (clean) syscall failed.
    CleanFailed(SyscallError),
    /// The driver's `kick` (program + issue) failed.
    Kick(K),
    /// The completion wait failed (device error or timeout).
    Completion(C),
    /// A post-completion `sync_for_cpu` (invalidate) syscall failed.
    InvalidateFailed(SyscallError),
}

/// Run a DMA transfer with correct-by-construction cache coherence.
///
/// Order: clean every region (both directions — the `FromDevice`
/// pre-clean is B2.2) -> `kick` -> `completion.await_complete()` ->
/// invalidate every `FromDevice` region. See the module docs for the
/// rationale and the no-bypass scope note.
pub fn run_dma_transfer<T, K, C, F>(
    regions: &[DmaRegion],
    completion: C,
    kick: F,
) -> Result<T, DmaTransferError<K, C::Error>>
where
    C: DmaCompletion,
    F: FnOnce() -> Result<T, K>,
{
    // Clean before the device runs. Both directions are cleaned
    // (B2.2): a FromDevice buffer holding a pre-DMA dirty CPU line
    // would otherwise have that line written back over the device's
    // freshly DMA'd bytes.
    for r in regions {
        if r.dir.cleans_before() {
            let e = sys_dma_sync_for_device(r.pageset, r.offset, r.len);
            if !e.is_ok() {
                return Err(DmaTransferError::CleanFailed(e));
            }
        }
    }

    // The driver programs the device and issues the operation.
    let value = kick().map_err(DmaTransferError::Kick)?;

    // Wait for the device to finish, per the driver-declared signal.
    completion
        .await_complete()
        .map_err(DmaTransferError::Completion)?;

    // Invalidate device-written regions so subsequent CPU loads read
    // fresh DRAM rather than stale pre-transfer cache lines.
    for r in regions {
        if r.dir.invalidates_after() {
            let e = sys_dma_sync_for_cpu(r.pageset, r.offset, r.len);
            if !e.is_ok() {
                return Err(DmaTransferError::InvalidateFailed(e));
            }
        }
    }

    Ok(value)
}
