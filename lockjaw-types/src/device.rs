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

/// Maximum number of devices the device manager can track.
/// QEMU virt has 32 virtio-mmio devices alone, plus UARTs, GIC, etc.
pub const MAX_DEVICES: usize = 64;

/// A device entry parsed from the DTB.
#[derive(Clone, Copy)]
pub struct DeviceInfo {
    /// FNV-1a hash of the first compatible string.
    pub compatible_hash: u64,
    /// Physical MMIO base address.
    pub mmio_addr: u64,
    /// MMIO region size in bytes.
    pub mmio_size: u64,
    /// GIC interrupt ID (SPI number + 32 for INTID).
    pub intid: u32,
    /// Whether this device has been claimed by a driver.
    pub claimed: bool,
}

/// Pre-computed hash for "virtio,mmio" — used by virtio drivers.
pub const VIRTIO_MMIO_HASH: u64 = compatible_hash(b"virtio,mmio");

// ---------------------------------------------------------------------------
// Device manager IPC protocol commands
// ---------------------------------------------------------------------------

/// Claim the first unclaimed device matching a compatible hash.
/// Request:  msg = [CMD_CLAIM_DEVICE, compatible_hash, 0, 0]
/// Response: msg = [exported_handle, intid, 0, 0]
pub const CMD_CLAIM_DEVICE: u64 = 1;

/// Probe a device by absolute index among ALL devices matching a hash.
///
/// The index is over the full DTB-derived device list (including
/// claimed devices), so it is stable regardless of concurrent claims.
///
/// For unclaimed devices: the device-manager temporarily maps the
/// MMIO page, reads u32 at offset 0 (magic) and offset 8 (device_id),
/// unmaps, and returns the values.
///
/// For claimed devices: MMIO cannot be read (another driver owns it),
/// so mmio_device_id = PROBE_DEVICE_CLAIMED (0xFFFF_FFFF).
///
/// Request:  msg = [CMD_PROBE_DEVICE, compatible_hash, index, 0]
///   index: absolute index among all matching devices (0 = first)
/// Response: msg = [mmio_addr, intid, mmio_magic, mmio_device_id]
///   mmio_addr = 0: no device at this index (end of list).
///   mmio_device_id = PROBE_DEVICE_CLAIMED: device exists but is claimed.
pub const CMD_PROBE_DEVICE: u64 = 2;

/// Sentinel value for mmio_device_id when the probed device is claimed.
pub const PROBE_DEVICE_CLAIMED: u64 = 0xFFFF_FFFF;

/// Claim a device by its exact MMIO physical address (TOCTOU-safe).
/// The driver first uses CMD_PROBE_DEVICE to discover the mmio_addr,
/// then claims by stable identity — no skip_count, no race.
/// Request:  msg = [CMD_CLAIM_BY_ADDR, mmio_addr, 0, 0]
/// Response: msg = [exported_handle, intid, 0, 0]
///   exported_handle = 0 means no match or already claimed.
pub const CMD_CLAIM_BY_ADDR: u64 = 3;

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
        assert!(core::mem::size_of::<DeviceInfo>() <= 40);
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
