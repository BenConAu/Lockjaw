/// VirtIO MMIO transport registers, virtqueue descriptor format,
/// block device types, and feature negotiation model.
///
/// All pure types — no MMIO access, no volatile, no barriers.
/// Host-testable layout and feature logic.
///
/// References: VirtIO spec v1.2
///   - Section 2.7: Split virtqueues
///   - Section 3.1: Device initialization
///   - Section 4.2: Virtio Over MMIO
///   - Section 5.2: Block device

// ---------------------------------------------------------------------------
// VirtIO MMIO register offsets (spec 4.2.2)
// ---------------------------------------------------------------------------

pub const VIRTIO_MMIO_MAGIC:              u64 = 0x000;
pub const VIRTIO_MMIO_VERSION:            u64 = 0x004;
pub const VIRTIO_MMIO_DEVICE_ID:          u64 = 0x008;
pub const VIRTIO_MMIO_VENDOR_ID:          u64 = 0x00c;
pub const VIRTIO_MMIO_DEVICE_FEATURES:    u64 = 0x010;
pub const VIRTIO_MMIO_DEVICE_FEATURES_SEL:u64 = 0x014;
pub const VIRTIO_MMIO_DRIVER_FEATURES:    u64 = 0x020;
pub const VIRTIO_MMIO_DRIVER_FEATURES_SEL:u64 = 0x024;
pub const VIRTIO_MMIO_QUEUE_SEL:          u64 = 0x030;
pub const VIRTIO_MMIO_QUEUE_NUM_MAX:      u64 = 0x034;
pub const VIRTIO_MMIO_QUEUE_NUM:          u64 = 0x038;
pub const VIRTIO_MMIO_QUEUE_READY:        u64 = 0x044;
pub const VIRTIO_MMIO_QUEUE_NOTIFY:       u64 = 0x050;
pub const VIRTIO_MMIO_INTERRUPT_STATUS:   u64 = 0x060;
pub const VIRTIO_MMIO_INTERRUPT_ACK:      u64 = 0x064;
pub const VIRTIO_MMIO_STATUS:             u64 = 0x070;
pub const VIRTIO_MMIO_QUEUE_DESC_LOW:     u64 = 0x080;
pub const VIRTIO_MMIO_QUEUE_DESC_HIGH:    u64 = 0x084;
pub const VIRTIO_MMIO_QUEUE_DRIVER_LOW:   u64 = 0x090;
pub const VIRTIO_MMIO_QUEUE_DRIVER_HIGH:  u64 = 0x094;
pub const VIRTIO_MMIO_QUEUE_DEVICE_LOW:   u64 = 0x0a0;
pub const VIRTIO_MMIO_QUEUE_DEVICE_HIGH:  u64 = 0x0a4;

/// Device-specific config space starts at this offset.
pub const VIRTIO_MMIO_CONFIG: u64 = 0x100;

/// Expected magic value at offset 0x000 ("virt" in little-endian).
pub const VIRTIO_MMIO_MAGIC_VALUE: u32 = 0x7472_6976;

// ---------------------------------------------------------------------------
// Device status bits (spec 3.1)
// ---------------------------------------------------------------------------

pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER:      u32 = 2;
pub const STATUS_DRIVER_OK:   u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_FAILED:      u32 = 128;

// ---------------------------------------------------------------------------
// Device IDs (spec 5)
// ---------------------------------------------------------------------------

pub const DEVICE_ID_NET:   u32 = 1;
pub const DEVICE_ID_BLOCK: u32 = 2;
pub const DEVICE_ID_GPU:   u32 = 16;

// ---------------------------------------------------------------------------
// Feature bits (spec 6)
// ---------------------------------------------------------------------------

/// VirtIO version 1 (modern device). Stored in window 1 (bit 32).
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// Block device feature bits (spec 5.2.3)
pub const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
pub const VIRTIO_BLK_F_SEG_MAX:  u64 = 1 << 2;

/// Phase 1 block driver: require only VERSION_1, no optional features.
pub const BLK_DRIVER_WANTED: u64 = VIRTIO_F_VERSION_1;

// ---------------------------------------------------------------------------
// Feature negotiation model
// ---------------------------------------------------------------------------

/// Models the windowed 32-bit feature negotiation protocol.
///
/// The MMIO transport reads/writes features through two 32-bit
/// windows selected by DEVICE_FEATURES_SEL / DRIVER_FEATURES_SEL.
/// This struct assembles the full 64-bit feature set and computes
/// the accepted subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureNegotiation {
    pub device_features: u64,
    pub driver_features: u64,
}

impl FeatureNegotiation {
    /// Build from two 32-bit reads (window 0 at sel=0, window 1 at sel=1).
    pub fn from_device(low: u32, high: u32) -> Self {
        Self {
            device_features: (low as u64) | ((high as u64) << 32),
            driver_features: 0,
        }
    }

    /// Accept the intersection of device features and wanted features.
    /// Returns (low, high) for writing back through the windowed protocol.
    pub fn accept(&mut self, wanted: u64) -> (u32, u32) {
        self.driver_features = self.device_features & wanted;
        (self.driver_features as u32, (self.driver_features >> 32) as u32)
    }

    /// True if VIRTIO_F_VERSION_1 was negotiated (required for modern).
    pub fn is_modern(&self) -> bool {
        self.driver_features & VIRTIO_F_VERSION_1 != 0
    }
}

// ---------------------------------------------------------------------------
// Virtqueue descriptor (spec 2.7.5)
// ---------------------------------------------------------------------------

/// 16-byte split virtqueue descriptor.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct VirtqDesc {
    /// Guest-physical address of the buffer.
    pub addr:  u64,
    /// Length of the buffer in bytes.
    pub len:   u32,
    /// Descriptor flags (NEXT, WRITE, INDIRECT).
    pub flags: u16,
    /// Next descriptor index if VIRTQ_DESC_F_NEXT is set.
    pub next:  u16,
}

/// Descriptor continues in the next field.
pub const VIRTQ_DESC_F_NEXT:     u16 = 1;
/// Buffer is device-writable (device writes, driver reads).
pub const VIRTQ_DESC_F_WRITE:    u16 = 2;
/// Buffer contains a list of indirect descriptors.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

// ---------------------------------------------------------------------------
// Available ring (spec 2.7.6)
// ---------------------------------------------------------------------------

/// Available ring header (ring entries follow immediately).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx:   u16,
    // ring: [u16; queue_size] follows in memory
}

// ---------------------------------------------------------------------------
// Used ring (spec 2.7.8)
// ---------------------------------------------------------------------------

/// A single used ring element.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct VirtqUsedElem {
    /// Descriptor chain head index.
    pub id:  u32,
    /// Total bytes written by the device.
    pub len: u32,
}

/// Used ring header (ring entries follow immediately).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx:   u16,
    // ring: [VirtqUsedElem; queue_size] follows in memory
}

// ---------------------------------------------------------------------------
// Virtqueue layout calculator
// ---------------------------------------------------------------------------

/// Computed byte offsets and total size for a split virtqueue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtqueueLayout {
    pub desc_offset:  usize,
    pub avail_offset: usize,
    pub used_offset:  usize,
    pub total_size:   usize,
    pub queue_size:   u16,
}

/// Compute byte offsets and total allocation for a split virtqueue.
///
/// Modern split virtqueue alignment (spec 2.7):
/// - Descriptor table: 16-byte aligned
/// - Available ring:    2-byte aligned
/// - Used ring:         4-byte aligned
///
/// No 4K alignment — that was a legacy (pre-1.0) requirement.
///
/// Layout for queue_size N:
///   desc:  N * 16 bytes, starts at offset 0
///   avail: 6 + 2*N bytes, starts at align_up(desc_end, 2)
///   used:  6 + 8*N bytes, starts at align_up(avail_end, 4)
///   total: rounded up to page size for allocation convenience
pub fn virtqueue_layout(queue_size: u16) -> VirtqueueLayout {
    let n = queue_size as usize;

    // Descriptor table: N * 16 bytes at offset 0 (already 16-aligned)
    let desc_offset = 0;
    let desc_end = n * core::mem::size_of::<VirtqDesc>();

    // Available ring: 2-byte aligned, size = 4 (flags+idx) + 2*N (ring) + 2 (used_event)
    let avail_offset = align_up(desc_end, 2);
    let avail_size = 4 + 2 * n + 2; // flags(2) + idx(2) + ring(2*N) + used_event(2)
    let avail_end = avail_offset + avail_size;

    // Used ring: 4-byte aligned, size = 4 (flags+idx) + 8*N (ring) + 2 (avail_event)
    let used_offset = align_up(avail_end, 4);
    let used_size = 4 + 8 * n + 2; // flags(2) + idx(2) + ring(8*N) + avail_event(2)
    let used_end = used_offset + used_size;

    // Round total to page size for contiguous allocation
    let total_size = align_up(used_end, 4096);

    VirtqueueLayout {
        desc_offset,
        avail_offset,
        used_offset,
        total_size,
        queue_size,
    }
}

const fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

// ---------------------------------------------------------------------------
// Block device types (spec 5.2)
// ---------------------------------------------------------------------------

/// Block device request header (spec 5.2.6).
/// Placed in the first descriptor of a request chain.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct VirtioBlkReqHeader {
    /// Request type: VIRTIO_BLK_T_IN (read) or VIRTIO_BLK_T_OUT (write).
    pub req_type: u32,
    pub reserved: u32,
    /// Starting sector (512-byte units).
    pub sector:   u64,
}

pub const VIRTIO_BLK_T_IN:  u32 = 0; // read from device
pub const VIRTIO_BLK_T_OUT: u32 = 1; // write to device

/// Block request status byte (written by device in last descriptor).
pub const VIRTIO_BLK_S_OK:     u8 = 0;
pub const VIRTIO_BLK_S_IOERR:  u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

/// Block config: capacity is a le64 at MMIO config offset 0.
pub const VIRTIO_BLK_CFG_CAPACITY: u64 = VIRTIO_MMIO_CONFIG;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- struct sizes ---

    #[test]
    fn virtq_desc_is_16_bytes() {
        assert_eq!(core::mem::size_of::<VirtqDesc>(), 16);
    }

    #[test]
    fn virtq_used_elem_is_8_bytes() {
        assert_eq!(core::mem::size_of::<VirtqUsedElem>(), 8);
    }

    #[test]
    fn virtio_blk_req_header_is_16_bytes() {
        assert_eq!(core::mem::size_of::<VirtioBlkReqHeader>(), 16);
    }

    #[test]
    fn virtq_avail_header_is_4_bytes() {
        assert_eq!(core::mem::size_of::<VirtqAvail>(), 4);
    }

    #[test]
    fn virtq_used_header_is_4_bytes() {
        assert_eq!(core::mem::size_of::<VirtqUsed>(), 4);
    }

    // --- layout ---

    #[test]
    fn layout_queue_128_fits_one_page() {
        let l = virtqueue_layout(128);
        assert_eq!(l.queue_size, 128);
        // desc: 128 * 16 = 2048 bytes
        assert_eq!(l.desc_offset, 0);
        // avail: at 2048 (already 2-aligned), size = 4 + 256 + 2 = 262
        assert_eq!(l.avail_offset, 2048);
        let avail_end = 2048 + 262;
        // used: align_up(2310, 4) = 2312, size = 4 + 1024 + 2 = 1030
        assert_eq!(l.used_offset, align_up(avail_end, 4));
        let used_end = l.used_offset + 4 + 8 * 128 + 2;
        assert!(used_end <= 4096, "queue_size=128 should fit in one page, used_end={}", used_end);
        assert_eq!(l.total_size, 4096);
    }

    #[test]
    fn layout_queue_256_needs_two_pages() {
        let l = virtqueue_layout(256);
        // desc: 256 * 16 = 4096
        assert_eq!(l.desc_offset, 0);
        assert_eq!(l.avail_offset, 4096);
        let avail_end = 4096 + 4 + 512 + 2; // 4614
        let expected_used = align_up(avail_end, 4); // 4616
        assert_eq!(l.used_offset, expected_used);
        let used_end = expected_used + 4 + 8 * 256 + 2; // 4616 + 2054 = 6670
        assert!(l.total_size >= used_end);
        assert_eq!(l.total_size, 8192); // 2 pages
    }

    #[test]
    fn layout_alignments_correct() {
        for &qs in &[16u16, 32, 64, 128, 256] {
            let l = virtqueue_layout(qs);
            assert_eq!(l.desc_offset % 16, 0, "desc not 16-aligned for qs={}", qs);
            assert_eq!(l.avail_offset % 2, 0, "avail not 2-aligned for qs={}", qs);
            assert_eq!(l.used_offset % 4, 0, "used not 4-aligned for qs={}", qs);
            assert_eq!(l.total_size % 4096, 0, "total not page-aligned for qs={}", qs);
        }
    }

    // --- device IDs ---

    #[test]
    fn device_id_block_is_2() {
        assert_eq!(DEVICE_ID_BLOCK, 2);
    }

    #[test]
    fn device_id_net_is_1() {
        assert_eq!(DEVICE_ID_NET, 1);
    }

    // --- status bits ---

    #[test]
    fn status_bits_distinct() {
        let all = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK
                | STATUS_FEATURES_OK | STATUS_FAILED;
        // All 5 bits should be distinct (no overlap)
        assert_eq!(
            all.count_ones(), 5,
            "status bits overlap"
        );
    }

    #[test]
    fn typical_init_status_sequence() {
        let mut s: u32 = 0;
        s |= STATUS_ACKNOWLEDGE;
        assert_eq!(s, 1);
        s |= STATUS_DRIVER;
        assert_eq!(s, 3);
        s |= STATUS_FEATURES_OK;
        assert_eq!(s, 11);
        s |= STATUS_DRIVER_OK;
        assert_eq!(s, 15);
    }

    // --- feature negotiation ---

    #[test]
    fn feature_from_device_round_trip() {
        let neg = FeatureNegotiation::from_device(0xABCD_1234, 0x0000_0001);
        assert_eq!(neg.device_features, 0x0000_0001_ABCD_1234);
    }

    #[test]
    fn feature_accept_masks_correctly() {
        let mut neg = FeatureNegotiation::from_device(0xFFFF_FFFF, 0x0000_0001);
        // Want only VERSION_1 (bit 32)
        let (low, high) = neg.accept(VIRTIO_F_VERSION_1);
        assert_eq!(low, 0);
        assert_eq!(high, 1);
        assert_eq!(neg.driver_features, VIRTIO_F_VERSION_1);
    }

    #[test]
    fn feature_is_modern_requires_bit_32() {
        let mut neg = FeatureNegotiation::from_device(0xFFFF_FFFF, 0x0000_0001);
        neg.accept(VIRTIO_F_VERSION_1);
        assert!(neg.is_modern());

        let mut neg2 = FeatureNegotiation::from_device(0xFFFF_FFFF, 0x0000_0000);
        neg2.accept(VIRTIO_F_VERSION_1);
        assert!(!neg2.is_modern(), "device lacks VERSION_1 → not modern");
    }

    #[test]
    fn feature_accept_without_version_1_not_modern() {
        let mut neg = FeatureNegotiation::from_device(0xFFFF_FFFF, 0x0000_0001);
        // Accept only low bits, not VERSION_1
        neg.accept(0x0000_0003);
        assert!(!neg.is_modern());
    }

    #[test]
    fn blk_driver_wanted_is_version_1_only() {
        assert_eq!(BLK_DRIVER_WANTED, VIRTIO_F_VERSION_1);
        assert_eq!(BLK_DRIVER_WANTED.count_ones(), 1);
    }

    // --- magic ---

    #[test]
    fn magic_value_is_virt_le() {
        assert_eq!(VIRTIO_MMIO_MAGIC_VALUE, 0x7472_6976);
    }

    // --- block types ---

    #[test]
    fn blk_request_types() {
        assert_eq!(VIRTIO_BLK_T_IN, 0);
        assert_eq!(VIRTIO_BLK_T_OUT, 1);
    }

    #[test]
    fn blk_status_values() {
        assert_eq!(VIRTIO_BLK_S_OK, 0);
        assert_eq!(VIRTIO_BLK_S_IOERR, 1);
        assert_eq!(VIRTIO_BLK_S_UNSUPP, 2);
    }

    #[test]
    fn blk_config_capacity_at_0x100() {
        assert_eq!(VIRTIO_BLK_CFG_CAPACITY, 0x100);
    }
}
