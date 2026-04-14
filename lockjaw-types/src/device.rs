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

/// Device manager IPC protocol commands.
pub const CMD_CLAIM_DEVICE: u64 = 1;

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
}
