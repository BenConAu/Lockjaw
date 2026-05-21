//! QEMU fw_cfg control + DMA
//!
//! GENERATED FILE — do not edit by hand.
//! Source: user/regspecs/fw-cfg.toml
//! Regenerate with: `cargo xtask gen-regs`.
//! Drift is caught by: `cargo xtask gen-regs --check` (CI).

#![allow(dead_code, missing_docs)]

use lockjaw_mmio::cell::{Rw, Wo};

/// QEMU fw_cfg control + DMA
#[repr(C)]
pub struct FwCfg {
    /// Data port — stream of bytes from the selected item
    data: Rw<u8>,
    _pad0: [u8; 0x7],
    /// Item selector (BE)
    selector: Wo<u16>,
    _pad1: [u8; 0x6],
    /// DMA control header guest-phys address (BE) — write triggers transfer
    dma_addr: Wo<u64>,
}

/// Returned when an enum decode sees a bit pattern that does not
/// correspond to any declared variant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReservedBits(pub u64);

// ---------- FwCfg accessors ----------

impl FwCfg {
    /// Volatile read of `data` as `u8`.
    #[inline(always)]
    pub fn read_data(&self) -> u8 { self.data.read() }
    /// Volatile write of `data`.
    #[inline(always)]
    pub fn write_data(&self, v: u8) { self.data.write(v); }
    /// Volatile write of `selector` (big-endian on the wire).
    #[inline(always)]
    pub fn write_selector(&self, v: u16) { self.selector.write(v.to_be()); }
    /// Volatile write of `dma_addr` (big-endian on the wire).
    #[inline(always)]
    pub fn write_dma_addr(&self, v: u64) { self.dma_addr.write(v.to_be()); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use lockjaw_mmio::mock::MockMmioRegion;

    #[test]
    fn layout_offsets() {
        assert_eq!(offset_of!(FwCfg, data), 0x0, "data offset");
        assert_eq!(offset_of!(FwCfg, selector), 0x8, "selector offset");
        assert_eq!(offset_of!(FwCfg, dma_addr), 0x10, "dma_addr offset");
    }

    #[test]
    fn selector_be_roundtrip() {
        let region = MockMmioRegion::for_layout::<FwCfg>();
        let regs = region.as_mapped_regs::<FwCfg>();
        regs.regs().write_selector(0x1234u16);
        // Underlying memory holds the BE byte pattern (write applied to_be()).
        assert_eq!(region.peek_u16(0x8), 0x3412u16);
    }

    #[test]
    fn dma_addr_be_roundtrip() {
        let region = MockMmioRegion::for_layout::<FwCfg>();
        let regs = region.as_mapped_regs::<FwCfg>();
        regs.regs().write_dma_addr(0x1122_3344_5566_7788u64);
        // Underlying memory holds the BE byte pattern (write applied to_be()).
        assert_eq!(region.peek_u64(0x10), 0x8877_6655_4433_2211u64);
    }

}
