//! SDHCI ADMA2 descriptor wire DTOs (LE on the wire)
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/wirespecs/sdhci.toml
//! Regenerate with: `cargo xtask gen-wires`.
//! Drift is caught by: `cargo xtask gen-wires --check` (CI).

#![allow(dead_code, missing_docs)]

use crate::dma_value_impl;

// ---------- Adma2Descriptor ----------

/// ADMA2 32-bit-address descriptor (SDHCI v3 §1.13.3.1).
/// Wire layout: 8 bytes, LE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Adma2Descriptor([u8; 8]);

impl Adma2Descriptor {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(attr: u16, length: u16, address: u32) -> Self {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&attr.to_le_bytes());
        b[2..4].copy_from_slice(&length.to_le_bytes());
        b[4..8].copy_from_slice(&address.to_le_bytes());
        Self(b)
    }

    /// Attributes word — OR of ADMA2_ATTR_VALID/END/INT/ACT_TRAN/ACT_LINK (bits 5:0 defined, bits 15:6 reserved).
    #[inline(always)]
    pub fn attr(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[0..2]);
        u16::from_le_bytes(buf)
    }

    /// Transfer length in bytes (max 65535 per descriptor; emmc2 caps here to avoid the length=0=65536 spec quirk).
    #[inline(always)]
    pub fn length(&self) -> u16 {
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&self.0[2..4]);
        u16::from_le_bytes(buf)
    }

    /// Data buffer guest-physical address (4-byte aligned, 32-bit only — ADMA2-32 variant).
    #[inline(always)]
    pub fn address(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[4..8]);
        u32::from_le_bytes(buf)
    }
}

dma_value_impl!(Adma2Descriptor, size = 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adma2_descriptor_size_and_align() {
        assert_eq!(core::mem::size_of::<Adma2Descriptor>(), 8);
        assert_eq!(core::mem::align_of::<Adma2Descriptor>(), 1);
    }

    #[test]
    fn adma2_descriptor_roundtrip() {
        let v = Adma2Descriptor::new(0x1234u16, 0x1244u16, 0x12365678u32);

        // attr field — LE-endian raw bytes
        let mut expected_attr = [0u8; 2];
        expected_attr.copy_from_slice(&0x1234u16.to_le_bytes());
        assert_eq!(&v.0[0..2], &expected_attr[..]);
        // length field — LE-endian raw bytes
        let mut expected_length = [0u8; 2];
        expected_length.copy_from_slice(&0x1244u16.to_le_bytes());
        assert_eq!(&v.0[2..4], &expected_length[..]);
        // address field — LE-endian raw bytes
        let mut expected_address = [0u8; 4];
        expected_address.copy_from_slice(&0x12365678u32.to_le_bytes());
        assert_eq!(&v.0[4..8], &expected_address[..]);

        assert_eq!(v.attr(), 0x1234u16);
        assert_eq!(v.length(), 0x1244u16);
        assert_eq!(v.address(), 0x12365678u32);
    }

}
