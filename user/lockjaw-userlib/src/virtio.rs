//! Typed VirtIO MMIO transport — init builder + post-init runtime wrapper.
//!
//! Encodes spec 3.1.1 ("Device Initialization") as a linear typestate
//! builder. Each step returns the next state's type, so a driver
//! cannot skip ACKNOWLEDGE before DRIVER, cannot set DRIVER_OK before
//! FEATURES_OK was acked, and cannot issue I/O against an un-init
//! device (the runtime wrapper is constructed only by `.driver_ok()`).
//!
//! Naming discipline: the generated register layout is
//! `lockjaw_regs::virtio_mmio::VirtioMmio`. This module's runtime
//! wrapper is `VirtioTransport` and its builder is
//! `VirtioTransportInit<S>` — distinct from the generated type so
//! `use` imports don't collide. The transport name reflects the
//! VirtIO spec's terminology ("MMIO transport layer").
//!
//! The builder owns `MappedRegs<VirtioMmio>` until `.driver_ok()`
//! consumes it into `VirtioTransport`. Driver methods that issue
//! I/O take `&VirtioTransport`, so the type system enforces "no
//! I/O before init complete."

use crate::virtqueue::Virtqueue;
use core::marker::PhantomData;
use lockjaw_mmio::region::MappedRegs;
use lockjaw_regs::virtio_mmio::{Status, VirtioMmio};
use lockjaw_types::virtio::VIRTIO_MMIO_MAGIC_VALUE;

/// Errors a VirtIO init step can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioInitError {
    /// MAGIC_VALUE register did not match the spec sentinel.
    NotVirtio { magic: u32 },
    /// VERSION register was not 2 (we only target modern devices).
    UnsupportedVersion(u32),
    /// The driver asked for features the device does not offer.
    /// Spec says we MUST NOT proceed past FEATURES_OK in this case.
    UnsupportedFeatures { wanted: u64, offered: u64 },
    /// Wrote FEATURES_OK, but the device cleared it on readback,
    /// meaning the device rejected our subset. Spec says we must
    /// abandon and set FAILED.
    DeviceRejectedFeatures,
    /// Selected queue index has `queue_num_max == 0` (queue not
    /// available on this device).
    QueueUnavailable { index: u16 },
    /// The factory closure passed to `setup_queue` /
    /// `setup_additional_queue` failed to allocate the virtqueue
    /// backing region. Distinct from `QueueUnavailable` so
    /// host-side OOM does not surface in logs as a false
    /// device-state message.
    BackingAllocFailed { pages: u64 },
}

// ---------------------------------------------------------------------------
// State markers — zero-sized. Each step returns a distinct state type
// so the compiler enforces ordering at the call site.
// ---------------------------------------------------------------------------

/// Initial state — fresh from `MappedRegs<VirtioMmio>`, magic/version
/// already verified by `reset()` so it cannot be reached for a
/// non-virtio mapping.
pub struct Reset;
/// After writing STATUS = ACKNOWLEDGE.
pub struct Acknowledged;
/// After writing STATUS = ACKNOWLEDGE | DRIVER.
pub struct DriverSelected;
/// After writing accepted features into driver_features windows.
/// FEATURES_OK has NOT been asserted yet — `.features_ok()` is the
/// next transition.
pub struct FeaturesWritten;
/// After writing FEATURES_OK and confirming the device kept it set
/// (i.e., the device accepted our feature subset).
pub struct FeaturesAccepted;
/// After programming at least one virtqueue's descriptor/avail/used
/// PAs and writing QUEUE_READY = 1.
pub struct QueuesReady;

// ---------------------------------------------------------------------------
// Builder: VirtioTransportInit<S>.
// ---------------------------------------------------------------------------

/// Typestate-driven VirtIO MMIO init builder.
pub struct VirtioTransportInit<S> {
    regs: MappedRegs<VirtioMmio>,
    _state: PhantomData<S>,
}

impl VirtioTransportInit<Reset> {
    /// Wrap a typed MMIO region and verify it really is a virtio
    /// MMIO transport (modern). Performs MAGIC_VALUE + VERSION
    /// checks up front — a non-virtio mapping never reaches
    /// `.acknowledge()` because there is no public `From<MappedRegs>`.
    ///
    /// Also clears STATUS so partial init from a prior owner does
    /// not leak in (spec 3.1.1 step 1: reset the device).
    pub fn reset(regs: MappedRegs<VirtioMmio>) -> Result<Self, VirtioInitError> {
        let magic = regs.regs().read_magic_value();
        if magic != VIRTIO_MMIO_MAGIC_VALUE {
            return Err(VirtioInitError::NotVirtio { magic });
        }
        let version = regs.regs().read_version();
        if version != 2 {
            return Err(VirtioInitError::UnsupportedVersion(version));
        }
        // Spec step 1: reset by writing 0 to STATUS.
        regs.regs().set_status(Status::empty());
        Ok(Self { regs, _state: PhantomData })
    }

    /// Spec step 2: acknowledge that we noticed the device.
    pub fn acknowledge(self) -> VirtioTransportInit<Acknowledged> {
        self.regs.regs().set_status(Status::ACKNOWLEDGE);
        VirtioTransportInit { regs: self.regs, _state: PhantomData }
    }
}

impl VirtioTransportInit<Acknowledged> {
    /// Spec step 3: assert we have a driver.
    pub fn driver(self) -> VirtioTransportInit<DriverSelected> {
        self.regs.regs().set_status(Status::ACKNOWLEDGE.insert(Status::DRIVER));
        VirtioTransportInit { regs: self.regs, _state: PhantomData }
    }
}

impl VirtioTransportInit<DriverSelected> {
    /// Spec steps 4–5: read the device's offered feature bits, accept
    /// `wanted` if a subset of offered, write back the accepted
    /// subset.
    ///
    /// Fails with `UnsupportedFeatures` if `wanted` includes bits the
    /// device does not offer — the spec says the driver MUST NOT
    /// proceed in that case.
    pub fn negotiate(self, wanted: u64)
        -> Result<VirtioTransportInit<FeaturesWritten>, VirtioInitError>
    {
        let offered = self.regs.regs().read_device_features_64();
        if wanted & !offered != 0 {
            return Err(VirtioInitError::UnsupportedFeatures { wanted, offered });
        }
        self.regs.regs().write_driver_features_64(wanted);
        Ok(VirtioTransportInit { regs: self.regs, _state: PhantomData })
    }
}

impl VirtioTransportInit<FeaturesWritten> {
    /// Spec step 7: assert FEATURES_OK, then read STATUS back. If the
    /// device cleared the bit, our feature subset was rejected and we
    /// must set FAILED + abandon init.
    pub fn features_ok(self)
        -> Result<VirtioTransportInit<FeaturesAccepted>, VirtioInitError>
    {
        let cur = self.regs.regs().status();
        self.regs.regs().set_status(cur.insert(Status::FEATURES_OK));
        if !self.regs.regs().status().contains(Status::FEATURES_OK) {
            self.regs.regs().set_status(Status::FAILED);
            return Err(VirtioInitError::DeviceRejectedFeatures);
        }
        Ok(VirtioTransportInit { regs: self.regs, _state: PhantomData })
    }
}

impl VirtioTransportInit<FeaturesAccepted> {
    /// Spec step 8a (queue setup): select queue `index`, read its
    /// `queue_num_max`, hand it to `make_vq` (which allocates the
    /// backing region and constructs the `Virtqueue`), then program
    /// the descriptor/avail/used physical addresses from the returned
    /// vq's layout and assert QUEUE_READY.
    ///
    /// Solves the "init builder + virtqueue allocation" composition
    /// hole: `queue_num_max` is readable only AFTER features_ok, but
    /// `Virtqueue::new` needs that value. The factory closure inverts
    /// the dependency.
    ///
    /// Returns `(builder_in_QueuesReady_state, the_virtqueue)` so the
    /// driver owns the virtqueue while the builder advances toward
    /// `driver_ok()`. For multi-queue drivers (virtio-net etc.), use
    /// `VirtioTransportInit::<QueuesReady>::setup_additional_queue`
    /// to attach further queues without state regression.
    pub fn setup_queue<F>(
        self,
        index: u16,
        make_vq: F,
    ) -> Result<(VirtioTransportInit<QueuesReady>, Virtqueue), VirtioInitError>
    where
        F: FnOnce(u16 /* queue_num_max */) -> Result<Virtqueue, VirtioInitError>,
    {
        let vq = setup_queue_internal(&self.regs, index, make_vq)?;
        Ok((VirtioTransportInit { regs: self.regs, _state: PhantomData }, vq))
    }
}

impl VirtioTransportInit<QueuesReady> {
    /// Attach an additional queue without leaving `QueuesReady`. Same
    /// semantics as `setup_queue` but does NOT change state, so a
    /// multi-queue driver (virtio-net, virtio-gpu) can call this once
    /// per additional queue then proceed to `driver_ok()`. Implemented
    /// by the same internal helper so the two paths can't drift.
    pub fn setup_additional_queue<F>(
        self,
        index: u16,
        make_vq: F,
    ) -> Result<(Self, Virtqueue), VirtioInitError>
    where
        F: FnOnce(u16 /* queue_num_max */) -> Result<Virtqueue, VirtioInitError>,
    {
        let vq = setup_queue_internal(&self.regs, index, make_vq)?;
        Ok((self, vq))
    }

    /// Spec step 8: assert DRIVER_OK and consume the builder into the
    /// runtime wrapper. After this, I/O may proceed against the
    /// returned `VirtioTransport`.
    pub fn driver_ok(self) -> VirtioTransport {
        self.regs.regs().set_status(
            Status::ACKNOWLEDGE
                .insert(Status::DRIVER)
                .insert(Status::FEATURES_OK)
                .insert(Status::DRIVER_OK)
        );
        VirtioTransport { regs: self.regs }
    }
}

/// Shared implementation for `setup_queue` and `setup_additional_queue`
/// so the two callsites can't drift in spec-ordering.
fn setup_queue_internal<F>(
    regs: &MappedRegs<VirtioMmio>,
    index: u16,
    make_vq: F,
) -> Result<Virtqueue, VirtioInitError>
where
    F: FnOnce(u16 /* queue_num_max */) -> Result<Virtqueue, VirtioInitError>,
{
    let r = regs.regs();
    r.write_queue_sel(index as u32);
    let max = r.read_queue_num_max();
    if max == 0 {
        return Err(VirtioInitError::QueueUnavailable { index });
    }
    let qmax_u16 = if max > u16::MAX as u32 { u16::MAX } else { max as u16 };
    let vq = make_vq(qmax_u16)?;
    debug_assert!(
        vq.queue_size() as u32 <= max,
        "factory returned vq.queue_size() = {} > device's queue_num_max = {} at index {}",
        vq.queue_size(), max, index
    );
    r.write_queue_num(vq.queue_size() as u32);
    // Phase 4A-synthesized u64 writes — replace the four manual
    // low/high pairs that the Phase 3 driver wrote by hand.
    r.write_queue_desc(vq.desc_phys());
    r.write_queue_driver(vq.avail_phys());
    r.write_queue_device(vq.used_phys());
    r.write_queue_ready(1);
    Ok(vq)
}

// ---------------------------------------------------------------------------
// Runtime: VirtioTransport (single concrete type, constructed only by
// `.driver_ok()`).
// ---------------------------------------------------------------------------

/// Initialized VirtIO MMIO transport — the transport-layer surface
/// shared across all virtio devices (block, net, gpu, console).
/// Device-specific config-space access lives in per-family wrappers
/// (e.g. `virtio_blk::VirtioBlkDevice`), not here.
pub struct VirtioTransport {
    regs: MappedRegs<VirtioMmio>,
}

impl VirtioTransport {
    /// Notify the device that queue `index` has new available work.
    #[inline]
    pub fn queue_notify(&self, index: u16) {
        self.regs.regs().write_queue_notify(index as u32);
    }

    /// Read pending interrupt cause bits.
    #[inline]
    pub fn read_interrupt_status(&self) -> u32 {
        self.regs.regs().read_interrupt_status()
    }

    /// Acknowledge interrupt cause bits (write-1-to-clear semantics).
    #[inline]
    pub fn clear_interrupt_ack(&self, mask: u32) {
        self.regs.regs().clear_interrupt_ack(mask);
    }

    /// Direct typed-register access for per-family device wrappers
    /// (e.g. `VirtioBlkDevice::read_capacity`). Crate-private so the
    /// driver-facing surface stays narrow.
    #[inline]
    pub(crate) fn regs(&self) -> &VirtioMmio {
        self.regs.regs()
    }
}
