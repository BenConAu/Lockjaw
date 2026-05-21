//! QEMU fw_cfg DMA-shared wire DTOs (BE on the wire)
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/wirespecs/fwcfg.toml
//! Regenerate with: `cargo xtask gen-wires`.
//! Drift is caught by: `cargo xtask gen-wires --check` (CI).

#![allow(dead_code, missing_docs)]

use crate::dma_value_impl;

// ---------- FwCfgDmaAccess ----------

/// fw_cfg DMA control header — driver writes, device acts on it.
/// Wire layout: 16 bytes, BE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct FwCfgDmaAccess([u8; 16]);

impl FwCfgDmaAccess {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(control: u32, length: u32, address: u64) -> Self {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&control.to_be_bytes());
        b[4..8].copy_from_slice(&length.to_be_bytes());
        b[8..16].copy_from_slice(&address.to_be_bytes());
        Self(b)
    }

    /// DMA control word — bitwise OR of DMA_CTRL_* plus selector in bits 16..31 when SELECT is set.
    #[inline(always)]
    pub fn control(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[0..4]);
        u32::from_be_bytes(buf)
    }

    /// Transfer length in bytes.
    #[inline(always)]
    pub fn length(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[4..8]);
        u32::from_be_bytes(buf)
    }

    /// Guest-physical address of the payload buffer.
    #[inline(always)]
    pub fn address(&self) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.0[8..16]);
        u64::from_be_bytes(buf)
    }
}

dma_value_impl!(FwCfgDmaAccess, size = 16);

// ---------- RamfbConfig ----------

/// ramfb per-frame configuration (28 bytes, BE).
/// Wire layout: 28 bytes, BE-endian default.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct RamfbConfig([u8; 28]);

impl RamfbConfig {
    /// Construct from host-order values. Byte-order
    /// conversion to the on-wire layout is applied
    /// per field at construction time; the resulting
    /// bytes can be written through `DmaCell::write` to
    /// reach the device.
    pub fn new(addr: u64, fourcc: u32, width: u32, height: u32, stride: u32) -> Self {
        let mut b = [0u8; 28];
        b[0..8].copy_from_slice(&addr.to_be_bytes());
        b[8..12].copy_from_slice(&fourcc.to_be_bytes());
        b[12..16].copy_from_slice(&0u32.to_be_bytes());
        b[16..20].copy_from_slice(&width.to_be_bytes());
        b[20..24].copy_from_slice(&height.to_be_bytes());
        b[24..28].copy_from_slice(&stride.to_be_bytes());
        Self(b)
    }

    /// Framebuffer guest-physical address.
    #[inline(always)]
    pub fn addr(&self) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.0[0..8]);
        u64::from_be_bytes(buf)
    }

    /// Pixel format fourcc (e.g. RAMFB_FORMAT_XRGB8888).
    #[inline(always)]
    pub fn fourcc(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[8..12]);
        u32::from_be_bytes(buf)
    }

    /// Reserved flags (spec mandates 0). Constructor omits this; accessor returns 0.
    #[inline(always)]
    pub fn flags(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[12..16]);
        u32::from_be_bytes(buf)
    }

    /// Width in pixels.
    #[inline(always)]
    pub fn width(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[16..20]);
        u32::from_be_bytes(buf)
    }

    /// Height in pixels.
    #[inline(always)]
    pub fn height(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[20..24]);
        u32::from_be_bytes(buf)
    }

    /// Bytes per row (= width × bytes_per_pixel for packed formats).
    #[inline(always)]
    pub fn stride(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.0[24..28]);
        u32::from_be_bytes(buf)
    }
}

dma_value_impl!(RamfbConfig, size = 28);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fw_cfg_dma_access_size_and_align() {
        assert_eq!(core::mem::size_of::<FwCfgDmaAccess>(), 16);
        assert_eq!(core::mem::align_of::<FwCfgDmaAccess>(), 1);
    }

    #[test]
    fn fw_cfg_dma_access_roundtrip() {
        let v = FwCfgDmaAccess::new(0x12345678u32, 0x12355678u32, 0x1122334655667788u64);

        // control field — BE-endian raw bytes
        let mut expected_control = [0u8; 4];
        expected_control.copy_from_slice(&0x12345678u32.to_be_bytes());
        assert_eq!(&v.0[0..4], &expected_control[..]);
        // length field — BE-endian raw bytes
        let mut expected_length = [0u8; 4];
        expected_length.copy_from_slice(&0x12355678u32.to_be_bytes());
        assert_eq!(&v.0[4..8], &expected_length[..]);
        // address field — BE-endian raw bytes
        let mut expected_address = [0u8; 8];
        expected_address.copy_from_slice(&0x1122334655667788u64.to_be_bytes());
        assert_eq!(&v.0[8..16], &expected_address[..]);

        assert_eq!(v.control(), 0x12345678u32);
        assert_eq!(v.length(), 0x12355678u32);
        assert_eq!(v.address(), 0x1122334655667788u64);
    }

    #[test]
    fn ramfb_config_size_and_align() {
        assert_eq!(core::mem::size_of::<RamfbConfig>(), 28);
        assert_eq!(core::mem::align_of::<RamfbConfig>(), 1);
    }

    #[test]
    fn ramfb_config_roundtrip() {
        let v = RamfbConfig::new(0x1122334455667788u64, 0x12355678u32, 0x12365678u32, 0x12375678u32, 0x12385678u32);

        // addr field — BE-endian raw bytes
        let mut expected_addr = [0u8; 8];
        expected_addr.copy_from_slice(&0x1122334455667788u64.to_be_bytes());
        assert_eq!(&v.0[0..8], &expected_addr[..]);
        // fourcc field — BE-endian raw bytes
        let mut expected_fourcc = [0u8; 4];
        expected_fourcc.copy_from_slice(&0x12355678u32.to_be_bytes());
        assert_eq!(&v.0[8..12], &expected_fourcc[..]);
        // flags field — BE-endian raw bytes
        let mut expected_flags = [0u8; 4];
        expected_flags.copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(&v.0[12..16], &expected_flags[..]);
        // width field — BE-endian raw bytes
        let mut expected_width = [0u8; 4];
        expected_width.copy_from_slice(&0x12365678u32.to_be_bytes());
        assert_eq!(&v.0[16..20], &expected_width[..]);
        // height field — BE-endian raw bytes
        let mut expected_height = [0u8; 4];
        expected_height.copy_from_slice(&0x12375678u32.to_be_bytes());
        assert_eq!(&v.0[20..24], &expected_height[..]);
        // stride field — BE-endian raw bytes
        let mut expected_stride = [0u8; 4];
        expected_stride.copy_from_slice(&0x12385678u32.to_be_bytes());
        assert_eq!(&v.0[24..28], &expected_stride[..]);

        assert_eq!(v.addr(), 0x1122334455667788u64);
        assert_eq!(v.fourcc(), 0x12355678u32);
        assert_eq!(v.flags(), 0u32);
        assert_eq!(v.width(), 0x12365678u32);
        assert_eq!(v.height(), 0x12375678u32);
        assert_eq!(v.stride(), 0x12385678u32);
    }

}
