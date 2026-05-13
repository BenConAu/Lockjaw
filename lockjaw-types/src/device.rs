/// Device discovery types and compatible string hashing.
///
/// The device manager uses FNV-1a hashes of compatible strings to match
/// drivers to devices. Both sides compute the same hash — no string IPC needed.

/// FNV-1a hash of a compatible string (e.g., "arm,pl011").
/// Deterministic, same result on all platforms.
pub const fn compatible_hash(s: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    let mut i = 0;
    while i < s.len() {
        hash ^= s[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    hash
}

/// Pre-computed hash for "arm,pl011" — used by the UART driver.
pub const PL011_HASH: u64 = compatible_hash(b"arm,pl011");

/// Pre-computed hash for "qemu,fw-cfg-mmio" — used by the ramfb display driver.
pub const FW_CFG_HASH: u64 = compatible_hash(b"qemu,fw-cfg-mmio");

/// Maximum compatible strings per device node in the DTB.
/// Must match fdt.rs MAX_COMPAT.
pub const MAX_COMPAT: usize = 4;

/// Maximum number of devices the device manager can track.
/// QEMU virt has ~50 nodes-with-compatible (32 virtio-mmio +
/// UARTs + GIC + etc.); Pi 4B DTB has ~119 (every SoC peripheral
/// node carries a compatible string). 192 is generous headroom
/// for both platforms.
pub const MAX_DEVICES: usize = 192;

/// Maximum number of clock references per device. The DTB
/// `clocks = <&phandle id ...>` property carries multiple
/// (controller_phandle, clock_id) tuples; this caps how many we
/// resolve per node. emmc2 has 1 clock; nothing in the current
/// Pi 4B / QEMU virt set comes close to 4.
pub const MAX_CLOCK_REFS: usize = 4;

/// A resolved clock reference: which controller, which clock on it.
/// Output of FDT parsing — lifted from `clocks = <&phandle N>` after
/// looking up `#clock-cells` on the controller node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockRef {
    /// Phandle of the clock controller (e.g., CPRMAN's phandle).
    pub controller_phandle: u32,
    /// Per-controller clock identifier (the cell that follows
    /// the phandle in the `clocks` property; meaning is defined
    /// by the controller's binding — e.g., for CPRMAN, this is
    /// the clock leaf index).
    pub clock_id: u32,
}

/// A device entry parsed from the DTB.
#[derive(Clone, Copy)]
pub struct DeviceInfo {
    /// FNV-1a hashes of all compatible strings (up to MAX_COMPAT).
    /// Pi 4B UART has "arm,pl011-axi" first, "arm,pl011" second —
    /// matching must check all entries, not just [0].
    pub compat_hashes: [u64; MAX_COMPAT],
    /// Number of valid entries in compat_hashes.
    pub compat_count: u8,
    /// Physical MMIO base address.
    pub mmio_addr: u64,
    /// MMIO region size in bytes.
    pub mmio_size: u64,
    /// GIC interrupt ID (SPI number + 32 for INTID).
    pub intid: u32,
    /// Whether this device has been claimed by a driver.
    pub claimed: bool,
    /// Resolved `clocks = <&phandle id ...>` references, in DTB
    /// declaration order. Empty if the node had no `clocks` property
    /// or the controller's `#clock-cells` was unresolvable.
    pub clocks: [ClockRef; MAX_CLOCK_REFS],
    /// Number of valid entries in `clocks`.
    pub clock_count: u8,
    /// This node's own DTB phandle (the value other nodes use to
    /// reference it via `<&phandle ...>`). 0 = no phandle declared.
    /// Phandle 0 is reserved by the DTB spec, so 0 is a safe
    /// "absent" sentinel. Used by device-manager's clock-provider
    /// registry to validate `controller_phandle` arguments to
    /// CMD_GET_CLOCK_HANDLE.
    pub phandle: u32,
}

impl DeviceInfo {
    /// Check if any compatible string matches the given hash.
    /// Same semantics as the FDT walker's NodeInfo::has_compat_hash.
    pub fn has_compat(&self, hash: u64) -> bool {
        let mut i = 0;
        while i < self.compat_count as usize {
            if self.compat_hashes[i] == hash {
                return true;
            }
            i += 1;
        }
        false
    }
}

/// Pre-computed hash for "virtio,mmio" — used by virtio drivers.
pub const VIRTIO_MMIO_HASH: u64 = compatible_hash(b"virtio,mmio");

/// Pre-computed hash for "brcm,bcm2711-cprman" — Pi 4B clock manager.
/// CPRMAN driver claims this in M0b.
pub const BCM2711_CPRMAN_HASH: u64 = compatible_hash(b"brcm,bcm2711-cprman");

// ---------------------------------------------------------------------------
// Device manager IPC protocol commands
// ---------------------------------------------------------------------------

/// Claim the first unclaimed device matching a compatible hash.
/// Request:  msg = [CMD_CLAIM_DEVICE, compatible_hash, 0, 0]
/// Response: msg = [status, exported_handle, intid, 0]
///   status: CLAIM_OK on success, CLAIM_ERR on failure.
///   Handle 0 is a valid handle-table index — check status, not handle.
pub const CMD_CLAIM_DEVICE: u64 = 1;

/// Probe a device by absolute index among ALL devices matching a hash.
///
/// The index is over the full DTB-derived device list (including
/// claimed devices), so it is stable regardless of concurrent claims.
///
/// For unclaimed devices: the device-manager temporarily maps the
/// MMIO page, reads the device_id register, unmaps, and returns it.
/// Magic validation is done internally by the device-manager; if
/// magic is wrong, the response is PROBE_ERR.
///
/// Request:  msg = [CMD_PROBE_DEVICE, compatible_hash, index, 0]
/// Response: msg = [status, mmio_addr, intid, device_id]
///   status: PROBE_OK, PROBE_END, PROBE_CLAIMED, or PROBE_ERR.
///   mmio_addr/intid/device_id only meaningful when status = PROBE_OK.
pub const CMD_PROBE_DEVICE: u64 = 2;

/// Probe response status codes.
pub const PROBE_OK:      u64 = 0; // device found, device_id valid
pub const PROBE_END:     u64 = 1; // no device at this index (end of list)
pub const PROBE_CLAIMED: u64 = 2; // device exists but already claimed
pub const PROBE_ERR:     u64 = 3; // internal failure (register/map/bad magic)

/// Claim a device by its exact MMIO physical address (TOCTOU-safe).
/// The driver first uses CMD_PROBE_DEVICE to discover the mmio_addr,
/// then claims by stable identity — no skip_count, no race.
/// Request:  msg = [CMD_CLAIM_BY_ADDR, mmio_addr, 0, 0]
/// Response: msg = [status, exported_handle, intid, 0]
///   status: CLAIM_OK on success, CLAIM_ERR on failure.
pub const CMD_CLAIM_BY_ADDR: u64 = 3;

/// Claim response status codes.
pub const CLAIM_OK:  u64 = 0;
pub const CLAIM_ERR: u64 = 1;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pl011_hash_is_deterministic() {
        assert_eq!(compatible_hash(b"arm,pl011"), PL011_HASH);
    }

    #[test]
    fn fw_cfg_hash_is_deterministic() {
        assert_eq!(compatible_hash(b"qemu,fw-cfg-mmio"), FW_CFG_HASH);
    }

    #[test]
    fn fw_cfg_hash_differs_from_pl011() {
        assert_ne!(FW_CFG_HASH, PL011_HASH);
    }

    #[test]
    fn different_strings_different_hashes() {
        assert_ne!(compatible_hash(b"arm,pl011"), compatible_hash(b"arm,pl110"));
    }

    #[test]
    fn empty_string_hash() {
        // Should not panic, produces the FNV offset basis
        assert_eq!(compatible_hash(b""), 0xcbf29ce484222325);
    }

    #[test]
    fn hash_is_nonzero_for_real_strings() {
        assert_ne!(compatible_hash(b"arm,pl011"), 0);
        assert_ne!(compatible_hash(b"arm,gic-v3"), 0);
    }

    #[test]
    fn device_info_size() {
        // Cap at 128 bytes per entry. MAX_DEVICES (192) entries fits
        // in ~24 KB, well within the userspace device-manager's
        // page-array budget. Layout: 4 compat hashes (32) +
        // compat_count (1) + padding + mmio_addr (8) + mmio_size (8)
        // + intid (4) + claimed (1) + padding + clocks (4 × 8 = 32)
        // + clock_count (1) + phandle (4) + padding ~ 104.
        assert!(core::mem::size_of::<DeviceInfo>() <= 128);
    }

    #[test]
    fn clock_ref_packs_to_8_bytes() {
        assert_eq!(core::mem::size_of::<ClockRef>(), 8);
    }

    #[test]
    fn virtio_mmio_hash_is_deterministic() {
        assert_eq!(compatible_hash(b"virtio,mmio"), VIRTIO_MMIO_HASH);
    }

    #[test]
    fn virtio_mmio_hash_differs_from_others() {
        assert_ne!(VIRTIO_MMIO_HASH, PL011_HASH);
        assert_ne!(VIRTIO_MMIO_HASH, FW_CFG_HASH);
    }

    #[test]
    fn device_manager_commands_distinct() {
        assert_ne!(CMD_CLAIM_DEVICE, CMD_PROBE_DEVICE);
        assert_ne!(CMD_CLAIM_DEVICE, CMD_CLAIM_BY_ADDR);
        assert_ne!(CMD_PROBE_DEVICE, CMD_CLAIM_BY_ADDR);
    }
}
