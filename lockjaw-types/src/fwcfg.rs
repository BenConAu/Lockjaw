//! QEMU fw_cfg protocol DTOs (shared-memory layouts).
//!
//! These are the structures the device reads from / writes to in
//! guest RAM via the DMA interface. They are NOT MMIO registers —
//! those live in `lockjaw_regs::fw_cfg`. The split:
//!
//! - `lockjaw_regs::fw_cfg::FwCfg` — typed MMIO accessors (data
//!   stream port, BE selector, BE DMA trigger).
//! - `lockjaw_types::fwcfg` (this module) — DMA-shared structs the
//!   device reads from a guest-allocated scratch page after the
//!   driver writes that page's PA to the DMA trigger.
//!
//! All multi-byte fields are big-endian on the wire. The helper
//! constructors take host-order arguments and apply `to_be()` so
//! callers never hand-pack bytes.

/// Well-known selector for the fw_cfg file directory.
/// Driver enumerates this to discover dynamic items (e.g. `etc/ramfb`).
pub const FW_CFG_FILE_DIR: u16 = 0x0019;

// ---------------------------------------------------------------------------
// FwCfgDmaAccess — DMA control header.
//
// Layout (16 bytes, BE on the wire):
//   u32 control  (bit 0 = ERROR set by device on failure,
//                 bit 1 = READ  (device -> guest, default),
//                 bit 2 = SKIP  (skip bytes without R/W),
//                 bit 3 = SELECT (selector in bits 16..31 of control),
//                 bit 4 = WRITE (guest -> device),
//                 bits 16..31 = selector when SELECT is set)
//   u32 length
//   u64 address  (guest-phys of the payload buffer)
//
// Driver writes the header to a scratch page, then writes that
// page's PA to FWCFG_DMA (offset 0x10) — the device DMA-reads the
// header and acts on it. Driver then polls control: when the
// SELECT/SKIP/WRITE bits clear (and ERROR stays unset), the
// transfer is complete.
// ---------------------------------------------------------------------------

/// DMA control bits (in the BE u32 `control` field — apply
/// `from_be` to inspect after a poll read).
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

/// `FwCfgDmaAccess` header — `#[repr(C)]` with three BE-on-wire
/// fields. Constructed via `new()`; readback via `control_raw()`
/// (returns the BE bits) or `control()` (returns host order).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct FwCfgDmaAccess {
    /// Control word — BE on the wire. Use `control()` to inspect.
    pub control_be: u32,
    /// Length in bytes — BE on the wire.
    pub length_be: u32,
    /// Payload buffer guest-phys address — BE on the wire.
    pub address_be: u64,
}

impl FwCfgDmaAccess {
    /// Construct from host-order values. Applies `to_be()` to each
    /// field so callers never hand-pack bytes.
    ///
    /// `control` should be a bitwise-OR of the `DMA_CTRL_*`
    /// constants plus the selector shifted into bits 16..31 when
    /// `DMA_CTRL_SELECT` is set.
    pub fn new(control: u32, length: u32, address: u64) -> Self {
        Self {
            control_be: control.to_be(),
            length_be: length.to_be(),
            address_be: address.to_be(),
        }
    }

    /// Convenience builder for a "write payload to selected item"
    /// request (the ramfb case). Combines SELECT + WRITE with the
    /// selector in the top half of the control word.
    pub fn write_to_selector(selector: u16, length: u32, address: u64) -> Self {
        let control = ((selector as u32) << 16) | DMA_CTRL_SELECT | DMA_CTRL_WRITE;
        Self::new(control, length, address)
    }

    /// Inspect the control bits in host order. Strips `to_be()`.
    pub fn control(&self) -> u32 {
        u32::from_be(self.control_be)
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

// FwCfgDmaAccess is #[repr(C)] of u32 + u32 + u64 = 16 bytes.
// `dma_value_impl!`'s const_assert verifies no padding at compile
// time — if the struct ever changes shape, the build fails before
// any DmaCell write can leak undef bytes.
crate::dma_value_impl!(FwCfgDmaAccess, size = 16);

// ---------------------------------------------------------------------------
// RAMFBConfig — written by the driver to a guest-RAM buffer that
// the device reads on each ramfb frame.
//
// On-wire layout (28 bytes, BE):
//   u64 addr      — framebuffer guest-phys
//   u32 fourcc    — pixel format ("XR24" etc.)
//   u32 flags     — must be 0
//   u32 width
//   u32 height
//   u32 stride    — bytes per row (= width * bytes_per_pixel for packed
//                   formats)
//
// **`#[repr(C, packed)]` is load-bearing:** a plain `#[repr(C)]` of
// u64 + 5×u32 gets 4 bytes of trailing padding (struct alignment 8
// from the leading u64 forces size to be a multiple of 8 = 32).
// That padding would violate `DmaValue`'s "no padding bytes that
// could be undef" safety contract — a typed DMA write through
// `DmaCell::<RamfbConfig>::write` would write 32 bytes including
// 4 undef bytes into shared memory, technically UB even though
// QEMU only reads the first 28 per the `length` field. Packing
// eliminates the trailing padding so `size_of::<RamfbConfig>() = 28
// = RAMFB_CONFIG_WIRE_SIZE` and the struct is sound as a DmaValue.
//
// Field access discipline: packed structs forbid `&self.field`
// references (alignment isn't guaranteed). All access in this
// module is by value (`self.addr_be` returns the u64; constructor
// `Self { addr_be: ..., ... }` writes by value) so the constraint
// doesn't bite. External code should construct via `new()` and
// write via `DmaCell::write` — never take field references.
// ---------------------------------------------------------------------------

/// Pixel format fourcc for 32-bit XRGB (X in the high byte).
/// Matches DRM's `DRM_FORMAT_XRGB8888`.
pub const RAMFB_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// Number of bytes QEMU reads from the `RamfbConfig` buffer on
/// each ramfb frame — the value the driver passes as `length` in
/// the DMA header. Equal to `size_of::<RamfbConfig>()` after the
/// `#[repr(C, packed)]` fix above. Constant so drivers don't
/// hand-code 28.
pub const RAMFB_CONFIG_WIRE_SIZE: u32 = 28;

/// `RAMFBConfig` — `#[repr(C, packed)]` with BE-on-wire fields.
/// Packed so size matches the 28-byte wire layout (no trailing
/// padding); this is required for soundness of the `DmaValue`
/// impl below.
///
/// Construct via `new()`; write via `DmaCell::<RamfbConfig>::write`.
/// Do not take field references — packed alignment isn't guaranteed.
#[derive(Clone, Copy)]
#[repr(C, packed)]
pub struct RamfbConfig {
    /// Framebuffer guest-phys address — BE on the wire.
    pub addr_be: u64,
    /// Pixel format fourcc — BE on the wire.
    pub fourcc_be: u32,
    /// Reserved flags (must be 0) — BE on the wire.
    pub flags_be: u32,
    /// Width in pixels — BE on the wire.
    pub width_be: u32,
    /// Height in pixels — BE on the wire.
    pub height_be: u32,
    /// Stride in bytes — BE on the wire.
    pub stride_be: u32,
}

impl RamfbConfig {
    /// Construct from host-order values. Applies `to_be()` to each
    /// field.
    pub fn new(fb_phys: u64, fourcc: u32, width: u32, height: u32, stride: u32) -> Self {
        Self {
            addr_be: fb_phys.to_be(),
            fourcc_be: fourcc.to_be(),
            flags_be: 0u32.to_be(),
            width_be: width.to_be(),
            height_be: height.to_be(),
            stride_be: stride.to_be(),
        }
    }
}

// RamfbConfig is #[repr(C, packed)] of u64 + 5 u32s = 28 bytes.
// Packed eliminates the trailing padding a plain #[repr(C)] would
// have (alignment 8 from the leading u64 would force size 32).
// `dma_value_impl!`'s const_assert verifies size == 28 at compile
// time — if a future edit drops the `packed` attribute, the
// build fails before any DmaCell write can leak undef bytes.
crate::dma_value_impl!(RamfbConfig, size = 28);

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
// Read 64 bytes at a time from the data port after writing
// FW_CFG_FILE_DIR as the selector. The first 4 bytes are a BE u32
// count of entries.
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
    fn dma_access_constructs_be() {
        let h = FwCfgDmaAccess::new(0x1234_5678, 0xabcd, 0x1_0000_0000);
        assert_eq!(h.control_be, 0x1234_5678u32.to_be());
        assert_eq!(h.length_be, 0xabcdu32.to_be());
        assert_eq!(h.address_be, 0x1_0000_0000u64.to_be());
    }

    #[test]
    fn dma_access_write_to_selector() {
        let h = FwCfgDmaAccess::write_to_selector(0x1234, 28, 0xabcd_0000);
        let ctl = h.control();
        assert_eq!(ctl >> 16, 0x1234);
        assert!(ctl & DMA_CTRL_SELECT != 0);
        assert!(ctl & DMA_CTRL_WRITE != 0);
        assert!(ctl & DMA_CTRL_ERROR == 0);
    }

    #[test]
    fn dma_access_completion_detection() {
        let busy = FwCfgDmaAccess::write_to_selector(0x42, 16, 0);
        assert!(!busy.is_complete());
        assert!(!busy.is_error());

        // Simulate device clearing the SELECT/WRITE bits but leaving
        // the selector in bits 16..31 intact.
        let done = FwCfgDmaAccess {
            control_be: (0x42_0000u32).to_be(),
            length_be: 0,
            address_be: 0,
        };
        assert!(done.is_complete());

        let failed = FwCfgDmaAccess {
            control_be: DMA_CTRL_ERROR.to_be(),
            length_be: 0,
            address_be: 0,
        };
        assert!(failed.is_error());
    }

    #[test]
    fn ramfb_config_be_layout() {
        let c = RamfbConfig::new(0x4000_0000, RAMFB_FORMAT_XRGB8888, 320, 240, 1280);
        // Copy packed fields to locals so the assert_eq! comparisons
        // don't take field references (packed structs forbid that —
        // alignment isn't guaranteed). This is the documented access
        // discipline: read by value, never by reference.
        let addr = c.addr_be;
        let fourcc = c.fourcc_be;
        let flags = c.flags_be;
        let width = c.width_be;
        let height = c.height_be;
        let stride = c.stride_be;
        assert_eq!(addr, 0x4000_0000u64.to_be());
        assert_eq!(fourcc, RAMFB_FORMAT_XRGB8888.to_be());
        assert_eq!(flags, 0u32);
        assert_eq!(width, 320u32.to_be());
        assert_eq!(height, 240u32.to_be());
        assert_eq!(stride, 1280u32.to_be());
    }

    #[test]
    fn ramfb_config_layout_offsets_match_wire() {
        // Field offsets must match QEMU's expected layout exactly.
        // With #[repr(C, packed)], the struct has no padding so
        // offsets are the simple field-sum chain.
        use core::mem::offset_of;
        assert_eq!(offset_of!(RamfbConfig, addr_be), 0);
        assert_eq!(offset_of!(RamfbConfig, fourcc_be), 8);
        assert_eq!(offset_of!(RamfbConfig, flags_be), 12);
        assert_eq!(offset_of!(RamfbConfig, width_be), 16);
        assert_eq!(offset_of!(RamfbConfig, height_be), 20);
        assert_eq!(offset_of!(RamfbConfig, stride_be), 24);
    }

    // The no-padding invariant on RamfbConfig is enforced at
    // COMPILE TIME by `dma_value_impl!(RamfbConfig, size = 28)`
    // above (search for the macro invocation in this file). If a
    // future edit drops `#[repr(C, packed)]`, the build fails at
    // the macro invocation — no runtime test needed and no
    // driver write can leak undef bytes.
    //
    // The packed-alignment runtime check moved into the same
    // place: const_assert size = 28 also implies align = 1
    // (the only way size matches the field sum for this layout).

    /// The wire size constant matches the actual struct size.
    /// Used by ramfb-driver as the `length` field of the
    /// FwCfgDmaAccess header.
    #[test]
    fn ramfb_wire_size_constant() {
        assert_eq!(RAMFB_CONFIG_WIRE_SIZE as usize, core::mem::size_of::<RamfbConfig>());
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
