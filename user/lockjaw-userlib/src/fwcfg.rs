//! fw_cfg protocol helpers — directory walk + DMA write.
//!
//! The family-policy module for fw_cfg drivers (analogous to
//! `lockjaw_userlib::virtio` for the virtio family,
//! `lockjaw_userlib::virtio_blk` for virtio-blk). Wraps the typed
//! MMIO accessors from `lockjaw_regs::fw_cfg::FwCfg` and the shared
//! DTOs from `lockjaw_types::fwcfg` with the two patterns every
//! fw_cfg consumer needs:
//!
//! - **Directory walk** (`find_file`): write FW_CFG_FILE_DIR
//!   selector, read 4-byte BE count, iterate N×64-byte entries
//!   matching by name.
//! - **DMA write** (`dma_write`): write payload to scratch, write
//!   FwCfgDmaAccess header, full-system barrier, trigger via
//!   FWCFG_DMA MMIO, poll completion.
//!
//! Both patterns lived in ramfb-driver before Phase 6 review; both
//! are generic fw_cfg-protocol with nothing ramfb-specific. Future
//! fw_cfg consumers (reading etc/edid, etc/system-states,
//! etc/acpi/tables, …) inherit the same shapes by calling these
//! helpers instead of re-implementing.

use crate::dma::{BuddyOrigin, DmaMappingView, OwnedDmaMapping};
use lockjaw_regs::fw_cfg::FwCfg;
use lockjaw_types::fwcfg::{FwCfgDmaAccess, FwCfgFile, FW_CFG_FILE_DIR};

/// Read `N` raw bytes from the currently-selected fw_cfg item via
/// the data-port stream. Each `read_data()` consumes one byte; the
/// caller controls `N` via the const generic so the buffer is
/// stack-allocated.
pub fn read_bytes<const N: usize>(regs: &FwCfg) -> [u8; N] {
    let mut buf = [0u8; N];
    for b in buf.iter_mut() {
        *b = regs.read_data();
    }
    buf
}

/// Walk the fw_cfg file directory looking for `name`. Returns the
/// per-file selector on match, or `None` if the directory is
/// exhausted without finding the name. BE decoding of the
/// directory's count + each entry's header is handled in
/// `lockjaw_types::fwcfg::FwCfgFile::from_stream_bytes` (driver
/// never hand-byte-swaps).
pub fn find_file(regs: &FwCfg, name: &[u8]) -> Option<u16> {
    regs.write_selector(FW_CFG_FILE_DIR);

    let count_be: [u8; 4] = read_bytes(regs);
    let count = u32::from_be_bytes(count_be);

    for _ in 0..count {
        let entry_bytes: [u8; 64] = read_bytes(regs);
        let entry = FwCfgFile::from_stream_bytes(&entry_bytes);
        if entry.name_str() == name {
            return Some(entry.selector);
        }
    }
    None
}

/// Errors `dma_write` can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwCfgDmaError {
    /// QEMU set the ERROR bit in the DMA control word on completion.
    DeviceError,
}

/// Issue a fw_cfg DMA write: payload at `payload_offset` (host-
/// allocated scratch page), `length` bytes from there to the
/// `selector`-named item.
///
/// The function writes a `FwCfgDmaAccess` header at `header_offset`
/// of the same scratch page (pointing at `payload_offset`),
/// inserts a full-system barrier so the header reaches RAM before
/// the MMIO trigger, then writes the header's guest-physical
/// address to the BE 64-bit DMA address register. It polls the
/// header's control word for completion — when QEMU clears the
/// direction bits the transfer is done; if it sets the ERROR bit
/// the function returns `DeviceError`.
///
/// This is the canonical fw_cfg DMA write sequence — the barrier
/// + poll discipline lives here so every fw_cfg consumer inherits
/// it. Lockjaw doesn't have a deadline-bounded spin primitive yet
/// (tracked in docs/tracking/tech-debt.md as "PL011 TX wait is unbounded");
/// when that lands, this poll loop is a candidate user.
pub fn dma_write(
    regs: &FwCfg,
    scratch: &OwnedDmaMapping<BuddyOrigin>,
    header_offset: u64,
    payload_offset: u64,
    selector: u16,
    length: u32,
) -> Result<(), FwCfgDmaError> {
    let payload_phys = scratch.pa_offset(payload_offset);
    let header = FwCfgDmaAccess::write_to_selector(selector, length, payload_phys);
    scratch.cell::<FwCfgDmaAccess>(header_offset).write(header);

    // Full system barrier: the header writes must be visible in
    // RAM before we trigger the DMA via the MMIO register, or
    // QEMU may DMA-read a partial / stale header.
    lockjaw_mmio::barrier::dsb_sy();

    // Trigger DMA. The generated `write_dma_addr` accessor applies
    // `to_be()` internally; we pass host-order PA.
    regs.write_dma_addr(scratch.pa_offset(header_offset));

    // Poll completion. QEMU clears the SELECT / SKIP / READ / WRITE
    // direction bits when the transfer finishes; the ERROR bit
    // (bit 0) signals failure.
    loop {
        let cur = scratch.cell::<FwCfgDmaAccess>(header_offset).read();
        if cur.is_error() {
            return Err(FwCfgDmaError::DeviceError);
        }
        if cur.is_complete() {
            return Ok(());
        }
        core::hint::spin_loop();
    }
}
