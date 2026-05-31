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
    /// IPC caller-token of the driver that claimed this device
    /// (0 if unclaimed). The device-manager uses this to enforce
    /// that only the original claimant can issue `CMD_RELEASE_BY_ADDR`
    /// — otherwise any process that knew the MMIO address could
    /// steal another driver's claim.
    pub claim_token: u64,
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

/// Pre-computed hash for "brcm,bcm2711-emmc2" — Pi 4B SDHCI v3 controller
/// behind the microSD slot. emmc2-driver claims this in M1.
pub const BCM2711_EMMC2_HASH: u64 = compatible_hash(b"brcm,bcm2711-emmc2");

// ---------------------------------------------------------------------------
// Device manager IPC protocol commands
// ---------------------------------------------------------------------------

/// Claim the first unclaimed device matching a compatible hash.
/// Request:  msg = [CMD_CLAIM_DEVICE, compatible_hash, 0, 0]
/// Response: msg = [status, exported_handle, intid, packed_clock_ref]
///   status: CLAIM_OK on success, CLAIM_ERR on failure.
///   Handle 0 is a valid handle-table index — check status, not handle.
///   packed_clock_ref: the device's first DTB clocks reference (if
///     any), packed via `pack_clock_ref(controller_phandle,
///     clock_id)`. Lets drivers immediately call
///     `CMD_GET_CLOCK_HANDLE` without a separate query. 0 means the
///     node had no `clocks` property.
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
/// Response: msg = [status, exported_handle, intid, packed_clock_ref]
///   status: CLAIM_OK on success, CLAIM_ERR on failure.
///   packed_clock_ref: same shape as CMD_CLAIM_DEVICE.
pub const CMD_CLAIM_BY_ADDR: u64 = 3;

/// Claim response status codes.
pub const CLAIM_OK:  u64 = 0;
pub const CLAIM_ERR: u64 = 1;

/// Release a previously claimed device, clearing the device-manager's
/// `claimed` bit so the same `mmio_addr` becomes claimable again.
/// Intended exclusively for the `claim_typed` error path today.
///
/// **Caller obligation:** the exported MMIO pageset handle MUST be
/// closed BEFORE issuing this RPC. The device-manager has no
/// kernel-side reference tracking, so it cannot itself verify the
/// driver has actually relinquished the mapping. If a release fires
/// while the original claimant still holds the exported handle, a
/// second driver can reclaim and map the same device — a real race.
/// `claim_typed` enforces the ordering (drops the guard, then
/// releases); other callers must follow the same discipline.
///
/// The verified release path is tracked in `docs/tracking/tech-debt.md`.
/// Request:  msg = [CMD_RELEASE_BY_ADDR, mmio_addr, 0, 0]
/// Response: msg = [status, 0, 0, 0] where status = CLAIM_OK if the
///   address matched a claimed device whose claim_token matches the
///   caller's IPC token (now released), CLAIM_ERR otherwise.
pub const CMD_RELEASE_BY_ADDR: u64 = 4;

// ---------------------------------------------------------------------------
// Packed clock-reference encoding for claim replies
// ---------------------------------------------------------------------------
//
// `CMD_CLAIM_DEVICE` and `CMD_CLAIM_BY_ADDR` reply with the device's
// first DTB `clocks = <&phandle id>` reference (if any) in the last
// message word. Both halves are u32; packing them into one u64
// avoids growing the IPC reply layout. 0 means the node had no
// clocks property.

/// Pack a `(controller_phandle, clock_id)` pair into one u64 for
/// the claim-reply word. controller_phandle in the high 32 bits,
/// clock_id in the low 32 bits — order chosen so that 0 unpacks
/// cleanly into "no provider, no clock id."
pub const fn pack_clock_ref(controller_phandle: u32, clock_id: u32) -> u64 {
    ((controller_phandle as u64) << 32) | (clock_id as u64)
}

/// Inverse of `pack_clock_ref`. Returns `None` if the packed value
/// is 0 (no clocks reference); `Some((controller_phandle,
/// clock_id))` otherwise.
pub const fn unpack_clock_ref(packed: u64) -> Option<(u32, u32)> {
    if packed == 0 {
        None
    } else {
        Some(((packed >> 32) as u32, packed as u32))
    }
}

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
    fn pack_clock_ref_round_trip() {
        // Realistic Pi 4B emmc2 case: cprman phandle = 8,
        // BCM2835_CLOCK_EMMC2 = 51.
        let packed = pack_clock_ref(8, 51);
        assert_eq!(unpack_clock_ref(packed), Some((8, 51)));
        // Boundaries: phandle and clock_id can each take any u32.
        let packed = pack_clock_ref(u32::MAX, u32::MAX);
        assert_eq!(unpack_clock_ref(packed), Some((u32::MAX, u32::MAX)));
        let packed = pack_clock_ref(0, 1);
        assert_eq!(unpack_clock_ref(packed), Some((0, 1)));
    }

    #[test]
    fn pack_clock_ref_zero_is_none() {
        // 0 means "no clocks reference present" — the only
        // round-trip that yields None on unpack.
        assert_eq!(pack_clock_ref(0, 0), 0);
        assert_eq!(unpack_clock_ref(0), None);
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
    fn bcm2711_emmc2_hash_distinct_from_other_devices() {
        // CPRMAN and EMMC2 must each have a unique compatible hash;
        // device-manager dispatches claims by hash, so a collision
        // would silently route a clock claim to the storage driver
        // (or vice-versa).
        assert_eq!(compatible_hash(b"brcm,bcm2711-emmc2"), BCM2711_EMMC2_HASH);
        assert_ne!(BCM2711_EMMC2_HASH, BCM2711_CPRMAN_HASH);
        assert_ne!(BCM2711_EMMC2_HASH, PL011_HASH);
        assert_ne!(BCM2711_EMMC2_HASH, FW_CFG_HASH);
        assert_ne!(BCM2711_EMMC2_HASH, VIRTIO_MMIO_HASH);
    }

    #[test]
    fn device_manager_commands_distinct() {
        assert_ne!(CMD_CLAIM_DEVICE, CMD_PROBE_DEVICE);
        assert_ne!(CMD_CLAIM_DEVICE, CMD_CLAIM_BY_ADDR);
        assert_ne!(CMD_PROBE_DEVICE, CMD_CLAIM_BY_ADDR);
    }
}
