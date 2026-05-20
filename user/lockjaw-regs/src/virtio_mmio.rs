//! VirtIO MMIO transport registers + virtio-blk config space
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/regspecs/virtio-mmio.toml
//! Regenerate with: `cargo xtask gen-regs`.
//! Drift is caught by: `cargo xtask gen-regs --check` (CI).

#![allow(dead_code, missing_docs)]

use lockjaw_mmio::cell::{Ro, Rw, W1c, Wo};

//
// verify_against: lockjaw_types::virtio
// Coverage: 22/24 registers cross-checked against constants.
// Unmatched (no constant binding in [[verify_offsets]]):
//   - blk_capacity_low
//   - blk_capacity_high
//

/// VirtIO MMIO transport registers + virtio-blk config space
#[repr(C)]
pub struct VirtioMmio {
    /// Magic 0x74726976 ('virt' LE) — bad value = not a virtio device
    magic_value: Ro<u32>,
    /// Transport version (2 for modern devices)
    version: Ro<u32>,
    /// DeviceID (1=net, 2=block, 16=gpu, 0=no device)
    device_id: Ro<u32>,
    /// VendorID
    vendor_id: Ro<u32>,
    /// Device features (window selected by device_features_sel)
    device_features: Ro<u32>,
    /// Window selector for device_features (0 = low 32, 1 = high 32)
    device_features_sel: Wo<u32>,
    _pad0: [u8; 0x8],
    /// Accepted driver features (window selected by driver_features_sel)
    driver_features: Wo<u32>,
    /// Window selector for driver_features
    driver_features_sel: Wo<u32>,
    _pad1: [u8; 0x8],
    /// Select virtqueue index for subsequent queue_* operations
    queue_sel: Wo<u32>,
    /// Maximum queue size for the selected virtqueue (0 = unavailable)
    queue_num_max: Ro<u32>,
    /// Negotiated queue size for the selected virtqueue
    queue_num: Wo<u32>,
    _pad2: [u8; 0x8],
    /// 1 = queue armed; write 0 to reset (driver-controlled)
    queue_ready: Rw<u32>,
    _pad3: [u8; 0x8],
    /// Notify the device of new descriptors (value = queue index)
    queue_notify: Wo<u32>,
    _pad4: [u8; 0xc],
    /// Pending interrupt cause bits (bit 0 = used-buffer, bit 1 = config-change)
    interrupt_status: Ro<u32>,
    /// Acknowledge bits set in interrupt_status (write the same value back)
    interrupt_ack: W1c<u32>,
    _pad5: [u8; 0x8],
    /// Device status flags — drives the init state machine
    status: Rw<u32>,
    _pad6: [u8; 0xc],
    /// Descriptor table PA, low 32 bits
    queue_desc_low: Wo<u32>,
    /// Descriptor table PA, high 32 bits
    queue_desc_high: Wo<u32>,
    _pad7: [u8; 0x8],
    /// Available ring PA, low 32 bits (driver-area)
    queue_driver_low: Wo<u32>,
    /// Available ring PA, high 32 bits
    queue_driver_high: Wo<u32>,
    _pad8: [u8; 0x8],
    /// Used ring PA, low 32 bits (device-area)
    queue_device_low: Wo<u32>,
    /// Used ring PA, high 32 bits
    queue_device_high: Wo<u32>,
    _pad9: [u8; 0x58],
    /// Block device capacity, low 32 bits (sectors of 512 bytes)
    blk_capacity_low: Ro<u32>,
    /// Block device capacity, high 32 bits
    blk_capacity_high: Ro<u32>,
}

// ---------- Status ----------

/// Status register — typed snapshot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Status(pub u32);

impl Status {
    /// Empty (no bits set).
    pub const fn empty() -> Self { Self(0) }
    /// Underlying bit pattern.
    pub const fn bits(self) -> u32 { self.0 }
    /// True if every bit set in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    /// Driver has noticed the device
    pub const ACKNOWLEDGE: Self = Self(1 << 0);
    /// Driver knows how to drive the device
    pub const DRIVER: Self = Self(1 << 1);
    /// Driver is ready to drive
    pub const DRIVER_OK: Self = Self(1 << 2);
    /// Features negotiation complete
    pub const FEATURES_OK: Self = Self(1 << 3);
    /// Device wants reset
    pub const NEEDS_RESET: Self = Self(1 << 6);
    /// Fatal init error
    pub const FAILED: Self = Self(1 << 7);
}

impl core::ops::BitOr for Status {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}
impl core::ops::BitAnd for Status {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
}
impl core::ops::Not for Status {
    type Output = Self;
    fn not(self) -> Self { Self(!self.0) }
}

/// Returned when an enum decode sees a bit pattern that does not
/// correspond to any declared variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReservedBits(pub u64);

// ---------- VirtioMmio accessors ----------

impl VirtioMmio {
    /// Volatile read of `magic_value` as `u32`.
    #[inline(always)]
    pub fn read_magic_value(&self) -> u32 { self.magic_value.read() }
    /// Volatile read of `version` as `u32`.
    #[inline(always)]
    pub fn read_version(&self) -> u32 { self.version.read() }
    /// Volatile read of `device_id` as `u32`.
    #[inline(always)]
    pub fn read_device_id(&self) -> u32 { self.device_id.read() }
    /// Volatile read of `vendor_id` as `u32`.
    #[inline(always)]
    pub fn read_vendor_id(&self) -> u32 { self.vendor_id.read() }
    /// Volatile read of `device_features` as `u32`.
    #[inline(always)]
    pub fn read_device_features(&self) -> u32 { self.device_features.read() }
    /// Volatile write of `device_features_sel`.
    #[inline(always)]
    pub fn write_device_features_sel(&self, v: u32) { self.device_features_sel.write(v); }
    /// Volatile write of `driver_features`.
    #[inline(always)]
    pub fn write_driver_features(&self, v: u32) { self.driver_features.write(v); }
    /// Volatile write of `driver_features_sel`.
    #[inline(always)]
    pub fn write_driver_features_sel(&self, v: u32) { self.driver_features_sel.write(v); }
    /// Volatile write of `queue_sel`.
    #[inline(always)]
    pub fn write_queue_sel(&self, v: u32) { self.queue_sel.write(v); }
    /// Volatile read of `queue_num_max` as `u32`.
    #[inline(always)]
    pub fn read_queue_num_max(&self) -> u32 { self.queue_num_max.read() }
    /// Volatile write of `queue_num`.
    #[inline(always)]
    pub fn write_queue_num(&self, v: u32) { self.queue_num.write(v); }
    /// Volatile read of `queue_ready` as `u32`.
    #[inline(always)]
    pub fn read_queue_ready(&self) -> u32 { self.queue_ready.read() }
    /// Volatile write of `queue_ready`.
    #[inline(always)]
    pub fn write_queue_ready(&self, v: u32) { self.queue_ready.write(v); }
    /// Read-modify-write `queue_ready`.
    #[inline(always)]
    pub fn modify_queue_ready<F: FnOnce(u32) -> u32>(&self, f: F) {
        self.queue_ready.modify(f);
    }
    /// Volatile write of `queue_notify`.
    #[inline(always)]
    pub fn write_queue_notify(&self, v: u32) { self.queue_notify.write(v); }
    /// Volatile read of `interrupt_status` as `u32`.
    #[inline(always)]
    pub fn read_interrupt_status(&self) -> u32 { self.interrupt_status.read() }
    /// Clear bits in `interrupt_ack` (write-1-to-clear).
    #[inline(always)]
    pub fn clear_interrupt_ack(&self, mask: u32) { self.interrupt_ack.clear(mask); }
    /// Read a typed snapshot of `status`.
    #[inline(always)]
    pub fn status(&self) -> Status { Status(self.status.read()) }
    /// Write the value back to `status`.
    #[inline(always)]
    pub fn set_status(&self, v: Status) { self.status.write(v.0); }
    /// Read-modify-write `status` via a typed closure.
    #[inline(always)]
    pub fn modify_status<F: FnOnce(Status) -> Status>(&self, f: F) {
        self.status.modify(|v| f(Status(v)).0);
    }
    /// Volatile write of `queue_desc_low`.
    #[inline(always)]
    pub fn write_queue_desc_low(&self, v: u32) { self.queue_desc_low.write(v); }
    /// Volatile write of `queue_desc_high`.
    #[inline(always)]
    pub fn write_queue_desc_high(&self, v: u32) { self.queue_desc_high.write(v); }
    /// Volatile write of `queue_driver_low`.
    #[inline(always)]
    pub fn write_queue_driver_low(&self, v: u32) { self.queue_driver_low.write(v); }
    /// Volatile write of `queue_driver_high`.
    #[inline(always)]
    pub fn write_queue_driver_high(&self, v: u32) { self.queue_driver_high.write(v); }
    /// Volatile write of `queue_device_low`.
    #[inline(always)]
    pub fn write_queue_device_low(&self, v: u32) { self.queue_device_low.write(v); }
    /// Volatile write of `queue_device_high`.
    #[inline(always)]
    pub fn write_queue_device_high(&self, v: u32) { self.queue_device_high.write(v); }
    /// Volatile read of `blk_capacity_low` as `u32`.
    #[inline(always)]
    pub fn read_blk_capacity_low(&self) -> u32 { self.blk_capacity_low.read() }
    /// Volatile read of `blk_capacity_high` as `u32`.
    #[inline(always)]
    pub fn read_blk_capacity_high(&self) -> u32 { self.blk_capacity_high.read() }
}

// ---------- VirtioMmio u64-pair accessors ----------

impl VirtioMmio {
    /// Composed read of `blk_capacity` (low=blk_capacity_low, high=blk_capacity_high, le).
    #[inline(always)]
    pub fn read_blk_capacity(&self) -> u64 {
        self.read_blk_capacity_low() as u64 | ((self.read_blk_capacity_high() as u64) << 32)
    }
    /// Composed write of `queue_desc` (writes low first, then high; le).
    #[inline(always)]
    pub fn write_queue_desc(&self, v: u64) {
        self.write_queue_desc_low(v as u32);
        self.write_queue_desc_high((v >> 32) as u32);
    }
    /// Composed write of `queue_driver` (writes low first, then high; le).
    #[inline(always)]
    pub fn write_queue_driver(&self, v: u64) {
        self.write_queue_driver_low(v as u32);
        self.write_queue_driver_high((v >> 32) as u32);
    }
    /// Composed write of `queue_device` (writes low first, then high; le).
    #[inline(always)]
    pub fn write_queue_device(&self, v: u64) {
        self.write_queue_device_low(v as u32);
        self.write_queue_device_high((v >> 32) as u32);
    }
}

// ---------- VirtioMmio windowed accessors ----------

impl VirtioMmio {
    /// Walk `device_features` (selector=device_features_sel, value=device_features) across 2 chunks of 32 bits;
    /// compose into a u64 (chunk 0 supplies the least-significant bits).
    #[inline(always)]
    pub fn read_device_features_64(&self) -> u64 {
        let mut acc: u64 = 0;
        for i in 0..2 {
            self.write_device_features_sel(i as u32);
            let chunk = self.read_device_features() as u64;
            acc |= chunk << (i * 32);
        }
        acc
    }
    /// Walk `driver_features` writing 2 chunks of 32 bits;
    /// chunk 0 carries the least-significant bits of `v`.
    #[inline(always)]
    pub fn write_driver_features_64(&self, v: u64) {
        for i in 0..2 {
            self.write_driver_features_sel(i as u32);
            let chunk = (v >> (i * 32)) as u32;
            self.write_driver_features(chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use lockjaw_mmio::mock::MockMmioRegion;

    #[test]
    fn layout_offsets() {
        assert_eq!(offset_of!(VirtioMmio, magic_value), 0x0, "magic_value offset");
        assert_eq!(offset_of!(VirtioMmio, version), 0x4, "version offset");
        assert_eq!(offset_of!(VirtioMmio, device_id), 0x8, "device_id offset");
        assert_eq!(offset_of!(VirtioMmio, vendor_id), 0xc, "vendor_id offset");
        assert_eq!(offset_of!(VirtioMmio, device_features), 0x10, "device_features offset");
        assert_eq!(offset_of!(VirtioMmio, device_features_sel), 0x14, "device_features_sel offset");
        assert_eq!(offset_of!(VirtioMmio, driver_features), 0x20, "driver_features offset");
        assert_eq!(offset_of!(VirtioMmio, driver_features_sel), 0x24, "driver_features_sel offset");
        assert_eq!(offset_of!(VirtioMmio, queue_sel), 0x30, "queue_sel offset");
        assert_eq!(offset_of!(VirtioMmio, queue_num_max), 0x34, "queue_num_max offset");
        assert_eq!(offset_of!(VirtioMmio, queue_num), 0x38, "queue_num offset");
        assert_eq!(offset_of!(VirtioMmio, queue_ready), 0x44, "queue_ready offset");
        assert_eq!(offset_of!(VirtioMmio, queue_notify), 0x50, "queue_notify offset");
        assert_eq!(offset_of!(VirtioMmio, interrupt_status), 0x60, "interrupt_status offset");
        assert_eq!(offset_of!(VirtioMmio, interrupt_ack), 0x64, "interrupt_ack offset");
        assert_eq!(offset_of!(VirtioMmio, status), 0x70, "status offset");
        assert_eq!(offset_of!(VirtioMmio, queue_desc_low), 0x80, "queue_desc_low offset");
        assert_eq!(offset_of!(VirtioMmio, queue_desc_high), 0x84, "queue_desc_high offset");
        assert_eq!(offset_of!(VirtioMmio, queue_driver_low), 0x90, "queue_driver_low offset");
        assert_eq!(offset_of!(VirtioMmio, queue_driver_high), 0x94, "queue_driver_high offset");
        assert_eq!(offset_of!(VirtioMmio, queue_device_low), 0xa0, "queue_device_low offset");
        assert_eq!(offset_of!(VirtioMmio, queue_device_high), 0xa4, "queue_device_high offset");
        assert_eq!(offset_of!(VirtioMmio, blk_capacity_low), 0x100, "blk_capacity_low offset");
        assert_eq!(offset_of!(VirtioMmio, blk_capacity_high), 0x104, "blk_capacity_high offset");
    }

    #[test]
    fn status_flag_bits() {
        assert_eq!(Status::ACKNOWLEDGE.bits(), 1 << 0, "acknowledge");
        assert_eq!(Status::DRIVER.bits(), 1 << 1, "driver");
        assert_eq!(Status::DRIVER_OK.bits(), 1 << 2, "driver_ok");
        assert_eq!(Status::FEATURES_OK.bits(), 1 << 3, "features_ok");
        assert_eq!(Status::NEEDS_RESET.bits(), 1 << 6, "needs_reset");
        assert_eq!(Status::FAILED.bits(), 1 << 7, "failed");
    }

    #[test]
    fn status_flag_compose() {
        let v = Status::ACKNOWLEDGE | Status::DRIVER;
        assert!(v.contains(Status::ACKNOWLEDGE));
        assert!(v.contains(Status::DRIVER));
        assert!(!Status::ACKNOWLEDGE.contains(Status::DRIVER));
    }

    #[test]
    fn blk_capacity_pair_roundtrip() {
        let region = MockMmioRegion::for_layout::<VirtioMmio>();
        let regs = region.as_mapped_regs::<VirtioMmio>();
        let dev_ref = regs.regs();
        region.poke_u32(0x100, 0x1122_3344);
        region.poke_u32(0x104, 0xAABB_CCDD);
        let composed = dev_ref.read_blk_capacity();
        let manual = dev_ref.read_blk_capacity_low() as u64 | ((dev_ref.read_blk_capacity_high() as u64) << 32);
        assert_eq!(composed, manual);
        assert_eq!(composed, 0xAABB_CCDD_1122_3344);
    }

    #[test]
    fn queue_desc_pair_roundtrip() {
        let region = MockMmioRegion::for_layout::<VirtioMmio>();
        let regs = region.as_mapped_regs::<VirtioMmio>();
        let dev_ref = regs.regs();
        dev_ref.write_queue_desc(0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(region.peek_u32(0x80), 0xCAFE_BABE);
        assert_eq!(region.peek_u32(0x84), 0xDEAD_BEEF);
    }

    #[test]
    fn queue_driver_pair_roundtrip() {
        let region = MockMmioRegion::for_layout::<VirtioMmio>();
        let regs = region.as_mapped_regs::<VirtioMmio>();
        let dev_ref = regs.regs();
        dev_ref.write_queue_driver(0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(region.peek_u32(0x90), 0xCAFE_BABE);
        assert_eq!(region.peek_u32(0x94), 0xDEAD_BEEF);
    }

    #[test]
    fn queue_device_pair_roundtrip() {
        let region = MockMmioRegion::for_layout::<VirtioMmio>();
        let regs = region.as_mapped_regs::<VirtioMmio>();
        let dev_ref = regs.regs();
        dev_ref.write_queue_device(0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(region.peek_u32(0xa0), 0xCAFE_BABE);
        assert_eq!(region.peek_u32(0xa4), 0xDEAD_BEEF);
    }

    #[test]
    fn device_features_windowed_read_visits_all_chunks() {
        let region = MockMmioRegion::for_layout::<VirtioMmio>();
        let regs = region.as_mapped_regs::<VirtioMmio>();
        region.poke_u32(0x10, 0xDEAD_BEEF);
        let composed = regs.regs().read_device_features_64();
        // Both chunks read the same value -> composed value has it in every position.
        let expected = (0xDEAD_BEEFu64) | (0xDEAD_BEEFu64 << 32);
        assert_eq!(composed, expected);
        // Selector ended at chunk_count - 1 (proves helper walked all chunks).
        assert_eq!(region.peek_u32(0x14), 1);
    }

    #[test]
    fn driver_features_windowed_write_visits_all_chunks() {
        let region = MockMmioRegion::for_layout::<VirtioMmio>();
        let regs = region.as_mapped_regs::<VirtioMmio>();
        regs.regs().write_driver_features_64(0xDEAD_BEEF_CAFE_BABE);
        // Selector ended at chunk_count - 1 (proves helper walked all chunks).
        assert_eq!(region.peek_u32(0x24), 1);
        // Value register holds the most-significant chunk (last write wins).
        assert_eq!(region.peek_u32(0x20), 0xDEAD_BEEF);
    }

    mod _verify {
        use super::*;
        use static_assertions::const_assert_eq;
        const_assert_eq!(
            offset_of!(VirtioMmio, magic_value) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_MAGIC
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, version) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_VERSION
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, device_id) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_DEVICE_ID
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, vendor_id) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_VENDOR_ID
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, device_features) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_DEVICE_FEATURES
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, device_features_sel) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_DEVICE_FEATURES_SEL
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, driver_features) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_DRIVER_FEATURES
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, driver_features_sel) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_DRIVER_FEATURES_SEL
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_sel) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_SEL
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_num_max) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_NUM_MAX
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_num) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_NUM
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_ready) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_READY
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_notify) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_NOTIFY
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, interrupt_status) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_INTERRUPT_STATUS
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, interrupt_ack) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_INTERRUPT_ACK
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, status) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_STATUS
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_desc_low) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_DESC_LOW
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_desc_high) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_DESC_HIGH
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_driver_low) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_DRIVER_LOW
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_driver_high) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_DRIVER_HIGH
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_device_low) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_DEVICE_LOW
        );
        const_assert_eq!(
            offset_of!(VirtioMmio, queue_device_high) as u64,
            lockjaw_types::virtio::VIRTIO_MMIO_QUEUE_DEVICE_HIGH
        );
    }

}
