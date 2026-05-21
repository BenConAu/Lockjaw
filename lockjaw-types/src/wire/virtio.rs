//! VirtIO split-virtqueue + virtio-blk wire DTOs (spec 2.7, 5.2.6)
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/wirespecs/virtio.toml
//! Regenerate with: `cargo xtask gen-wires`.
//! Drift is caught by: `cargo xtask gen-wires --check` (CI).

#![allow(dead_code, missing_docs)]

use crate::dma_value_impl;

// ---------- VirtqDesc ----------

/// Split virtqueue descriptor (spec 2.7.5).
/// Wire layout: 16 bytes, LE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct VirtqDesc([u8; 16]);

impl VirtqDesc {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(addr: u64, len: u32, flags: u16, next: u16) -> Self {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..16].copy_from_slice(&next.to_le_bytes());
        Self(b)
    }

    /// Guest-physical address of the buffer.
    #[inline(always)]
    pub fn addr(&self) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.0[0..8]);
        u64::from_le_bytes(buf)
    }

    /// Length of the buffer in bytes.
    #[inline(always)]
    pub fn len(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[8..12]);
        u32::from_le_bytes(buf)
    }

    /// Descriptor flags (VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE, VIRTQ_DESC_F_INDIRECT).
    #[inline(always)]
    pub fn flags(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[12..14]);
        u16::from_le_bytes(buf)
    }

    /// Next-descriptor index if VIRTQ_DESC_F_NEXT is set.
    #[inline(always)]
    pub fn next(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[14..16]);
        u16::from_le_bytes(buf)
    }
}

dma_value_impl!(VirtqDesc, size = 16);

// ---------- VirtqAvail ----------

/// Available ring header (spec 2.7.6). Ring entries follow in memory.
/// Wire layout: 4 bytes, LE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct VirtqAvail([u8; 4]);

impl VirtqAvail {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(flags: u16, idx: u16) -> Self {
        let mut b = [0u8; 4];
        b[0..2].copy_from_slice(&flags.to_le_bytes());
        b[2..4].copy_from_slice(&idx.to_le_bytes());
        Self(b)
    }

    /// Ring flags (VIRTQ_AVAIL_F_NO_INTERRUPT).
    #[inline(always)]
    pub fn flags(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[0..2]);
        u16::from_le_bytes(buf)
    }

    /// Index of the next available ring entry to be written.
    #[inline(always)]
    pub fn idx(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[2..4]);
        u16::from_le_bytes(buf)
    }
}

dma_value_impl!(VirtqAvail, size = 4);

// ---------- VirtqUsed ----------

/// Used ring header (spec 2.7.8). Ring entries follow in memory.
/// Wire layout: 4 bytes, LE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct VirtqUsed([u8; 4]);

impl VirtqUsed {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(flags: u16, idx: u16) -> Self {
        let mut b = [0u8; 4];
        b[0..2].copy_from_slice(&flags.to_le_bytes());
        b[2..4].copy_from_slice(&idx.to_le_bytes());
        Self(b)
    }

    /// Ring flags (VIRTQ_USED_F_NO_NOTIFY).
    #[inline(always)]
    pub fn flags(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[0..2]);
        u16::from_le_bytes(buf)
    }

    /// Index of the next used ring entry the device will write.
    #[inline(always)]
    pub fn idx(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[2..4]);
        u16::from_le_bytes(buf)
    }
}

dma_value_impl!(VirtqUsed, size = 4);

// ---------- VirtqUsedElem ----------

/// Single used ring element (spec 2.7.8).
/// Wire layout: 8 bytes, LE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct VirtqUsedElem([u8; 8]);

impl VirtqUsedElem {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(id: u32, len: u32) -> Self {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&id.to_le_bytes());
        b[4..8].copy_from_slice(&len.to_le_bytes());
        Self(b)
    }

    /// Descriptor chain head index that completed.
    #[inline(always)]
    pub fn id(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[0..4]);
        u32::from_le_bytes(buf)
    }

    /// Total bytes the device wrote into the chain.
    #[inline(always)]
    pub fn len(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[4..8]);
        u32::from_le_bytes(buf)
    }
}

dma_value_impl!(VirtqUsedElem, size = 8);

// ---------- VirtioBlkReqHeader ----------

/// virtio-blk request header (spec 5.2.6).
/// Wire layout: 16 bytes, LE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct VirtioBlkReqHeader([u8; 16]);

impl VirtioBlkReqHeader {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(req_type: u32, sector: u64) -> Self {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&req_type.to_le_bytes());
        b[4..8].copy_from_slice(&0u32.to_le_bytes());
        b[8..16].copy_from_slice(&sector.to_le_bytes());
        Self(b)
    }

    /// Request type (VIRTIO_BLK_T_IN read, VIRTIO_BLK_T_OUT write).
    #[inline(always)]
    pub fn req_type(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[0..4]);
        u32::from_le_bytes(buf)
    }

    /// Reserved field (spec mandates 0). Constructor omits this; accessor returns 0.
    #[inline(always)]
    pub fn reserved(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[4..8]);
        u32::from_le_bytes(buf)
    }

    /// Starting sector in 512-byte units.
    #[inline(always)]
    pub fn sector(&self) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.0[8..16]);
        u64::from_le_bytes(buf)
    }
}

dma_value_impl!(VirtioBlkReqHeader, size = 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtq_desc_size_and_align() {
        assert_eq!(core::mem::size_of::<VirtqDesc>(), 16);
        assert_eq!(core::mem::align_of::<VirtqDesc>(), 1);
    }

    #[test]
    fn virtq_desc_roundtrip() {
        let v = VirtqDesc::new(0x1122334455667788u64, 0x12355678u32, 0x1254u16, 0x1264u16);

        // addr field — LE-endian raw bytes
        let mut expected_addr = [0u8; 8];
        expected_addr.copy_from_slice(&0x1122334455667788u64.to_le_bytes());
        assert_eq!(&v.0[0..8], &expected_addr[..]);
        // len field — LE-endian raw bytes
        let mut expected_len = [0u8; 4];
        expected_len.copy_from_slice(&0x12355678u32.to_le_bytes());
        assert_eq!(&v.0[8..12], &expected_len[..]);
        // flags field — LE-endian raw bytes
        let mut expected_flags = [0u8; 2];
        expected_flags.copy_from_slice(&0x1254u16.to_le_bytes());
        assert_eq!(&v.0[12..14], &expected_flags[..]);
        // next field — LE-endian raw bytes
        let mut expected_next = [0u8; 2];
        expected_next.copy_from_slice(&0x1264u16.to_le_bytes());
        assert_eq!(&v.0[14..16], &expected_next[..]);

        assert_eq!(v.addr(), 0x1122334455667788u64);
        assert_eq!(v.len(), 0x12355678u32);
        assert_eq!(v.flags(), 0x1254u16);
        assert_eq!(v.next(), 0x1264u16);
    }

    #[test]
    fn virtq_avail_size_and_align() {
        assert_eq!(core::mem::size_of::<VirtqAvail>(), 4);
        assert_eq!(core::mem::align_of::<VirtqAvail>(), 1);
    }

    #[test]
    fn virtq_avail_roundtrip() {
        let v = VirtqAvail::new(0x1234u16, 0x1244u16);

        // flags field — LE-endian raw bytes
        let mut expected_flags = [0u8; 2];
        expected_flags.copy_from_slice(&0x1234u16.to_le_bytes());
        assert_eq!(&v.0[0..2], &expected_flags[..]);
        // idx field — LE-endian raw bytes
        let mut expected_idx = [0u8; 2];
        expected_idx.copy_from_slice(&0x1244u16.to_le_bytes());
        assert_eq!(&v.0[2..4], &expected_idx[..]);

        assert_eq!(v.flags(), 0x1234u16);
        assert_eq!(v.idx(), 0x1244u16);
    }

    #[test]
    fn virtq_used_size_and_align() {
        assert_eq!(core::mem::size_of::<VirtqUsed>(), 4);
        assert_eq!(core::mem::align_of::<VirtqUsed>(), 1);
    }

    #[test]
    fn virtq_used_roundtrip() {
        let v = VirtqUsed::new(0x1234u16, 0x1244u16);

        // flags field — LE-endian raw bytes
        let mut expected_flags = [0u8; 2];
        expected_flags.copy_from_slice(&0x1234u16.to_le_bytes());
        assert_eq!(&v.0[0..2], &expected_flags[..]);
        // idx field — LE-endian raw bytes
        let mut expected_idx = [0u8; 2];
        expected_idx.copy_from_slice(&0x1244u16.to_le_bytes());
        assert_eq!(&v.0[2..4], &expected_idx[..]);

        assert_eq!(v.flags(), 0x1234u16);
        assert_eq!(v.idx(), 0x1244u16);
    }

    #[test]
    fn virtq_used_elem_size_and_align() {
        assert_eq!(core::mem::size_of::<VirtqUsedElem>(), 8);
        assert_eq!(core::mem::align_of::<VirtqUsedElem>(), 1);
    }

    #[test]
    fn virtq_used_elem_roundtrip() {
        let v = VirtqUsedElem::new(0x12345678u32, 0x12355678u32);

        // id field — LE-endian raw bytes
        let mut expected_id = [0u8; 4];
        expected_id.copy_from_slice(&0x12345678u32.to_le_bytes());
        assert_eq!(&v.0[0..4], &expected_id[..]);
        // len field — LE-endian raw bytes
        let mut expected_len = [0u8; 4];
        expected_len.copy_from_slice(&0x12355678u32.to_le_bytes());
        assert_eq!(&v.0[4..8], &expected_len[..]);

        assert_eq!(v.id(), 0x12345678u32);
        assert_eq!(v.len(), 0x12355678u32);
    }

    #[test]
    fn virtio_blk_req_header_size_and_align() {
        assert_eq!(core::mem::size_of::<VirtioBlkReqHeader>(), 16);
        assert_eq!(core::mem::align_of::<VirtioBlkReqHeader>(), 1);
    }

    #[test]
    fn virtio_blk_req_header_roundtrip() {
        let v = VirtioBlkReqHeader::new(0x12345678u32, 0x1122334555667788u64);

        // req_type field — LE-endian raw bytes
        let mut expected_req_type = [0u8; 4];
        expected_req_type.copy_from_slice(&0x12345678u32.to_le_bytes());
        assert_eq!(&v.0[0..4], &expected_req_type[..]);
        // reserved field — LE-endian raw bytes
        let mut expected_reserved = [0u8; 4];
        expected_reserved.copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(&v.0[4..8], &expected_reserved[..]);
        // sector field — LE-endian raw bytes
        let mut expected_sector = [0u8; 8];
        expected_sector.copy_from_slice(&0x1122334555667788u64.to_le_bytes());
        assert_eq!(&v.0[8..16], &expected_sector[..]);

        assert_eq!(v.req_type(), 0x12345678u32);
        assert_eq!(v.reserved(), 0u32);
        assert_eq!(v.sector(), 0x1122334555667788u64);
    }

}
