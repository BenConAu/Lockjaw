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
/// QEMU virt has 32 virtio-mmio devices alone, plus UARTs, GIC, etc.
pub const MAX_DEVICES: usize = 64;

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
        // Ensure it's a reasonable size for array storage
        // 4 compat hashes (32) + count (1) + padding + mmio_addr (8) +
        // mmio_size (8) + intid (4) + claimed (1) + padding = 64
        assert!(core::mem::size_of::<DeviceInfo>() <= 64);
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
