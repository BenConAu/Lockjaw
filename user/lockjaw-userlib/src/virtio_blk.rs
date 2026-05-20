//! virtio-blk device wrapper.
//!
//! Layers on top of `VirtioTransport` to expose virtio-blk's
//! config-space accessors (capacity, future: block_size, segment
//! limits, etc.) without polluting the generic transport. When
//! virtio-net and virtio-gpu land, each gets its own per-family
//! module here in `lockjaw-userlib::virtio_*` — `VirtioTransport`
//! stays narrow.
//!
//! The driver flow is:
//! 1. `VirtioTransportInit::reset(regs)` → ... → `.driver_ok()` →
//!    `VirtioTransport`
//! 2. `VirtioBlkDevice::new(transport)` → device wrapper with
//!    blk-specific config-space accessors
//! 3. Driver stores `VirtioBlkDevice`; `device.transport()` is the
//!    transport view for `queue_notify` / `wait_for_completion`.

use crate::virtio::VirtioTransport;

/// virtio-blk-specific device wrapper. Owns a `VirtioTransport` and
/// exposes blk config-space accessors.
pub struct VirtioBlkDevice {
    transport: VirtioTransport,
}

impl VirtioBlkDevice {
    /// Wrap a post-init `VirtioTransport`. The driver should have
    /// already negotiated virtio-blk-appropriate features (e.g.
    /// `BLK_DRIVER_WANTED`) before reaching here.
    pub fn new(transport: VirtioTransport) -> Self {
        Self { transport }
    }

    /// Borrow the transport for queue notify / completion-wait paths.
    /// Virtqueue operations consume `&VirtioTransport`, not
    /// `&VirtioBlkDevice`, so virtqueue code stays device-agnostic.
    #[inline]
    pub fn transport(&self) -> &VirtioTransport {
        &self.transport
    }

    /// Block-device capacity in sectors (one logical 64-bit value,
    /// stored as two 32-bit halves in config space at 0x100/0x104).
    /// Uses the Phase 4A-synthesized u64 accessor.
    #[inline]
    pub fn read_capacity(&self) -> u64 {
        self.transport.regs().read_blk_capacity()
    }
}
