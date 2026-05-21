//! QEMU fw_cfg protocol — well-known selectors, DMA control bits,
//! ramfb pixel-format constants, and the `FwCfgFile` directory-entry
//! decoder. Hand-written and host-testable.
//!
//! Wire DTOs (`FwCfgDmaAccess`, `RamfbConfig`) live in
//! `crate::wire::fwcfg`, generated from `user/wirespecs/fwcfg.toml`
//! by `cargo xtask gen-wires`. This module re-exports them so the
//! historical import paths (`lockjaw_types::fwcfg::FwCfgDmaAccess`,
//! etc.) keep working, and attaches the semantic convenience
//! methods (`write_to_selector`, `is_complete`, `is_error`) to the
//! generated types via `impl` blocks. Those methods would belong in
//! the wirespec if every fw_cfg consumer needed identical helpers,
//! but they're fw_cfg-protocol-specific layered on top of the raw
//! wire layout — exactly the kind of helper the codegen excludes.
//!
//! All multi-byte wire fields are big-endian. The generated
//! constructors and accessors handle byte order; consumers pass and
//! receive host-order values.

pub use crate::wire::fwcfg::*;

/// Well-known selector for the fw_cfg file directory.
/// Driver enumerates this to discover dynamic items (e.g. `etc/ramfb`).
pub const FW_CFG_FILE_DIR: u16 = 0x0019;

// ---------------------------------------------------------------------------
// DMA control bits — composed into the `control` field of an
// `FwCfgDmaAccess` (the generated constructor takes a host-order u32
// and applies `to_be_bytes` internally; consumers compose the bits
// without thinking about byte order).
// ---------------------------------------------------------------------------

/// Device-set ERROR bit. After polling completion, set means the
/// device rejected the transfer.
pub const DMA_CTRL_ERROR:  u32 = 1 << 0;
/// Read transfer (device -> guest). Implicit (default if none of
/// SELECT/SKIP/WRITE is set).
pub const DMA_CTRL_READ:   u32 = 1 << 1;
/// Skip bytes without transferring (advance the stream cursor).
pub const DMA_CTRL_SKIP:   u32 = 1 << 2;
/// Selector is present in bits 16..31 of `control`; setting this
/// makes the transfer pick a new item without a separate selector
/// write.
pub const DMA_CTRL_SELECT: u32 = 1 << 3;
/// Write transfer (guest -> device). Required for ramfb (which is
/// a write-only "configuration" item).
pub const DMA_CTRL_WRITE:  u32 = 1 << 4;

// ---------------------------------------------------------------------------
// FwCfgDmaAccess convenience methods.
//
// The generated `FwCfgDmaAccess::new(control, length, address)` is
// the raw constructor. `write_to_selector` composes the SELECT +
// WRITE bits with the selector field in one call — every fw_cfg
// write-to-named-item path uses this exact pattern.
//
// `is_complete` / `is_error` inspect the device-updated `control`
// word after polling; both fall out of the raw DMA_CTRL_* semantics.
// ---------------------------------------------------------------------------

impl FwCfgDmaAccess {
    /// Convenience builder for a "write payload to selected item"
    /// request (the ramfb case). Combines SELECT + WRITE with the
    /// selector in the top half of the control word.
    pub fn write_to_selector(selector: u16, length: u32, address: u64) -> Self {
        let control = ((selector as u32) << 16) | DMA_CTRL_SELECT | DMA_CTRL_WRITE;
        Self::new(control, length, address)
    }

    /// True if the device set the ERROR bit.
    pub fn is_error(&self) -> bool {
        self.control() & DMA_CTRL_ERROR != 0
    }

    /// True if the SELECT / SKIP / READ / WRITE direction bits have
    /// all been cleared — i.e. the device finished the transfer.
    /// (The poll-completion condition.)
    pub fn is_complete(&self) -> bool {
        let c = self.control();
        c & (DMA_CTRL_SELECT | DMA_CTRL_SKIP | DMA_CTRL_READ | DMA_CTRL_WRITE) == 0
    }
}

// ---------------------------------------------------------------------------
// RAMFBConfig — pixel-format constants. Constructor + accessors are
// generated; this module just exports the well-known fourcc.
// ---------------------------------------------------------------------------

/// Pixel format fourcc for 32-bit XRGB (X in the high byte).
/// Matches DRM's `DRM_FORMAT_XRGB8888`.
pub const RAMFB_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// Number of bytes QEMU reads from the `RamfbConfig` buffer on each
/// ramfb frame — the value the driver passes as `length` in the
/// FwCfgDmaAccess header. Equal to `size_of::<RamfbConfig>()`. The
/// constant exists so drivers don't hand-code 28 and so a future
/// wire-format change shows up in one place rather than spread
/// across drivers.
pub const RAMFB_CONFIG_WIRE_SIZE: u32 = 28;

// ---------------------------------------------------------------------------
// FwCfgFile — directory entry from FW_CFG_FILE_DIR.
//
// Layout (64 bytes, all BE on the wire EXCEPT the name which is
// a NUL-terminated ASCII string):
//   u32 size
//   u16 selector
//   u16 reserved
//   u8[56] name (NUL-terminated)
//
// Not a wirespec entry: it's a stream-decode helper (driver reads
// 64-byte windows off the data port and decodes one at a time) and
// it bundles a NUL-terminated string field that doesn't fit the
// wirespec's uniform per-field width model. Hand-written stays.
// ---------------------------------------------------------------------------

/// Directory entry — driver receives 64 raw bytes from the stream
/// and decodes via `from_stream_bytes`.
#[derive(Clone, Copy)]
pub struct FwCfgFile {
    /// Item size in bytes (host order — already decoded).
    pub size: u32,
    /// Selector to use for subsequent data-port access to this item
    /// (host order — already decoded).
    pub selector: u16,
    /// NUL-terminated name bytes. Use `name_str()` for the trimmed slice.
    pub name: [u8; 56],
}

impl FwCfgFile {
    /// Decode a directory entry from 64 raw stream bytes. The
    /// driver reads 64 bytes per entry from the data port and
    /// hands them here; this keeps the BE decoding in lockjaw-types
    /// instead of letting the driver hand-byte-swap.
    pub fn from_stream_bytes(buf: &[u8; 64]) -> Self {
        let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let selector = u16::from_be_bytes([buf[4], buf[5]]);
        // buf[6..8] is the BE reserved field — ignored.
        let mut name = [0u8; 56];
        name.copy_from_slice(&buf[8..64]);
        Self { size, selector, name }
    }

    /// Name as a byte slice, NUL-terminator stripped.
    pub fn name_str(&self) -> &[u8] {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(56);
        &self.name[..len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_access_write_to_selector_sets_expected_bits() {
        let h = FwCfgDmaAccess::write_to_selector(0x1234, 28, 0xabcd_0000);
        let ctl = h.control();
        assert_eq!(ctl >> 16, 0x1234, "selector lives in bits 16..31");
        assert!(ctl & DMA_CTRL_SELECT != 0);
        assert!(ctl & DMA_CTRL_WRITE != 0);
        assert!(ctl & DMA_CTRL_ERROR == 0);
        assert_eq!(h.length(), 28);
        assert_eq!(h.address(), 0xabcd_0000);
    }

    #[test]
    fn dma_access_completion_detection() {
        let busy = FwCfgDmaAccess::write_to_selector(0x42, 16, 0);
        assert!(!busy.is_complete());
        assert!(!busy.is_error());

        // Device has cleared SELECT + WRITE bits (selector residue
        // in bits 16..31 doesn't count as "in flight"). `is_complete`
        // is true once SELECT/SKIP/READ/WRITE are all zero.
        let done = FwCfgDmaAccess::new(0x42_0000u32, 0, 0);
        assert!(done.is_complete(), "selector residue should not block completion");

        let failed = FwCfgDmaAccess::new(DMA_CTRL_ERROR, 0, 0);
        assert!(failed.is_error());
    }

    #[test]
    fn ramfb_wire_size_constant_matches_struct_size() {
        // The driver passes RAMFB_CONFIG_WIRE_SIZE as the DMA header's
        // `length` field. It must stay aligned with the on-wire layout
        // of RamfbConfig (the generated struct). If the wirespec grows
        // the struct, this constant must move in lockstep.
        assert_eq!(RAMFB_CONFIG_WIRE_SIZE as usize, core::mem::size_of::<RamfbConfig>());
    }

    #[test]
    fn ramfb_format_xrgb8888_is_xr24_in_le() {
        // fourcc literals are byte sequences interpreted as LE u32 by
        // DRM convention; "XR24" decodes to this value.
        assert_eq!(RAMFB_FORMAT_XRGB8888, u32::from_le_bytes(*b"XR24"));
    }

    #[test]
    fn file_decodes_be_fields() {
        let mut buf = [0u8; 64];
        buf[0..4].copy_from_slice(&0x1234u32.to_be_bytes());     // size
        buf[4..6].copy_from_slice(&0x0042u16.to_be_bytes());     // selector
        buf[6..8].copy_from_slice(&0u16.to_be_bytes());          // reserved
        let name = b"etc/ramfb";
        buf[8..8 + name.len()].copy_from_slice(name);
        // Remaining name bytes already zero (NUL-terminator).
        let f = FwCfgFile::from_stream_bytes(&buf);
        assert_eq!(f.size, 0x1234);
        assert_eq!(f.selector, 0x0042);
        assert_eq!(f.name_str(), b"etc/ramfb");
    }
}
