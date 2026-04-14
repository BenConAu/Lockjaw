/// Minimal Flattened Device Tree (FDT) parser.
///
/// Parses the binary DTB format to extract device information: compatible
/// strings, MMIO addresses (reg property), and interrupt numbers.
/// Pure, no_std, testable on host with a real DTB blob.
///
/// Limitations (sufficient for QEMU virt flat layout):
/// - No address translation (#address-cells inheritance)
/// - No phandle resolution
/// - Assumes root #address-cells=2, #size-cells=2

use crate::device::{DeviceInfo, MAX_DEVICES, compatible_hash};

// FDT structure block tokens
const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE: u32 = 0x00000002;
const FDT_PROP: u32 = 0x00000003;
const FDT_NOP: u32 = 0x00000004;
const FDT_END: u32 = 0x00000009;

// FDT header magic
const FDT_MAGIC: u32 = 0xd00dfeed;

/// Errors from FDT parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdtError {
    /// Data is too small to contain an FDT header.
    TooSmall,
    /// Magic number doesn't match 0xd00dfeed.
    BadMagic,
    /// Structure or strings block offset is out of bounds.
    InvalidOffsets,
}

/// Result of parsing an FDT: a list of discovered devices.
pub struct FdtDevices {
    pub devices: [DeviceInfo; MAX_DEVICES],
    pub count: usize,
}

/// Parse an FDT blob and extract device information.
/// Returns a list of devices with their compatible hashes, MMIO addresses,
/// and interrupt IDs.
pub fn parse_fdt(data: &[u8]) -> Result<FdtDevices, FdtError> {
    if data.len() < 40 {
        return Err(FdtError::TooSmall);
    }

    let magic = read_u32_be(data, 0);
    if magic != FDT_MAGIC {
        return Err(FdtError::BadMagic);
    }

    let off_dt_struct = read_u32_be(data, 8) as usize;
    let off_dt_strings = read_u32_be(data, 12) as usize;

    if off_dt_struct >= data.len() || off_dt_strings >= data.len() {
        return Err(FdtError::InvalidOffsets);
    }

    let mut result = FdtDevices {
        devices: [DeviceInfo {
            compatible_hash: 0,
            mmio_addr: 0,
            mmio_size: 0,
            intid: 0,
            claimed: false,
        }; MAX_DEVICES],
        count: 0,
    };

    // Walk the structure block.
    // Use a per-depth property stack so child nodes don't clobber parent properties.
    const MAX_DEPTH: usize = 8;
    let mut pos = off_dt_struct;
    let mut depth: usize = 0;

    // Per-depth node properties
    let mut compat_hash: [u64; MAX_DEPTH] = [0; MAX_DEPTH];
    let mut mmio_addr: [u64; MAX_DEPTH] = [0; MAX_DEPTH];
    let mut mmio_size: [u64; MAX_DEPTH] = [0; MAX_DEPTH];
    let mut intid: [u32; MAX_DEPTH] = [0; MAX_DEPTH];
    let mut has_compat: [bool; MAX_DEPTH] = [false; MAX_DEPTH];

    loop {
        if pos + 4 > data.len() {
            break;
        }
        let token = read_u32_be(data, pos);
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                // Skip node name (null-terminated, padded to 4 bytes)
                while pos < data.len() && data[pos] != 0 {
                    pos += 1;
                }
                pos += 1; // skip null terminator
                pos = align4(pos);
                // Reset properties for this depth level
                if depth < MAX_DEPTH {
                    compat_hash[depth] = 0;
                    mmio_addr[depth] = 0;
                    mmio_size[depth] = 0;
                    intid[depth] = 0;
                    has_compat[depth] = false;
                }
                depth += 1;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                // If this node had a compatible string, save it as a device
                if depth < MAX_DEPTH && has_compat[depth] && result.count < MAX_DEVICES {
                    result.devices[result.count] = DeviceInfo {
                        compatible_hash: compat_hash[depth],
                        mmio_addr: mmio_addr[depth],
                        mmio_size: mmio_size[depth],
                        intid: intid[depth],
                        claimed: false,
                    };
                    result.count += 1;
                }
            }
            FDT_PROP => {
                if pos + 8 > data.len() {
                    break;
                }
                let prop_len = read_u32_be(data, pos) as usize;
                let name_off = read_u32_be(data, pos + 4) as usize;
                pos += 8;

                let prop_data_start = pos;
                let prop_data_end = pos + prop_len;
                if prop_data_end > data.len() {
                    break;
                }

                // Read property name from strings block.
                // Properties are stored at (depth - 1) since depth was
                // incremented on BEGIN_NODE before properties are read.
                let prop_name = read_string(data, off_dt_strings + name_off);
                let d = depth - 1; // current node's depth index

                if d < MAX_DEPTH && str_eq(prop_name, b"compatible") {
                    // Hash the first compatible string (null-terminated within prop data)
                    let compat_str = read_string(data, prop_data_start);
                    compat_hash[d] = compatible_hash(compat_str);
                    has_compat[d] = true;
                } else if d < MAX_DEPTH && str_eq(prop_name, b"reg") && prop_len >= 16 {
                    // Assumes #address-cells=2, #size-cells=2 (QEMU virt root)
                    // reg = <addr_hi addr_lo size_hi size_lo>
                    let addr_hi = read_u32_be(data, prop_data_start) as u64;
                    let addr_lo = read_u32_be(data, prop_data_start + 4) as u64;
                    let size_hi = read_u32_be(data, prop_data_start + 8) as u64;
                    let size_lo = read_u32_be(data, prop_data_start + 12) as u64;
                    mmio_addr[d] = (addr_hi << 32) | addr_lo;
                    mmio_size[d] = (size_hi << 32) | size_lo;
                } else if d < MAX_DEPTH && str_eq(prop_name, b"interrupts") && prop_len >= 12 {
                    // GICv3 interrupt specifier: <type spi_number flags>
                    // type 0 = SPI, INTID = spi_number + 32
                    let int_type = read_u32_be(data, prop_data_start);
                    let spi_num = read_u32_be(data, prop_data_start + 4);
                    if int_type == 0 {
                        intid[d] = spi_num + 32;
                    }
                }

                pos = align4(prop_data_end);
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => break, // unknown token, stop
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
    ])
}

fn align4(pos: usize) -> usize {
    (pos + 3) & !3
}

/// Read a null-terminated string starting at `offset`. Returns the byte slice
/// up to (not including) the null terminator.
fn read_string(data: &[u8], offset: usize) -> &[u8] {
    let mut end = offset;
    while end < data.len() && data[end] != 0 {
        end += 1;
    }
    &data[offset..end]
}

/// Compare a byte slice to a known string.
fn str_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::PL011_HASH;

    // Load the real QEMU virt DTB for testing
    static QEMU_DTB: &[u8] = include_bytes!("../test-data/qemu-virt.dtb");

    #[test]
    fn parse_qemu_dtb_succeeds() {
        let result = parse_fdt(QEMU_DTB);
        assert!(result.is_ok());
    }

    #[test]
    fn finds_pl011_devices() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let pl011_count = devs.devices[..devs.count]
            .iter()
            .filter(|d| d.compatible_hash == PL011_HASH)
            .count();
        assert_eq!(pl011_count, 2, "QEMU virt should have 2 PL011 UARTs");
    }

    #[test]
    fn uart0_has_correct_address() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let uart0 = devs.devices[..devs.count]
            .iter()
            .find(|d| d.compatible_hash == PL011_HASH && d.mmio_addr == 0x0900_0000);
        assert!(uart0.is_some(), "UART0 should be at 0x09000000");
        let uart0 = uart0.unwrap();
        assert_eq!(uart0.mmio_size, 0x1000);
        assert_eq!(uart0.intid, 33); // SPI 1 + 32
    }

    #[test]
    fn uart1_has_correct_address() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let uart1 = devs.devices[..devs.count]
            .iter()
            .find(|d| d.compatible_hash == PL011_HASH && d.mmio_addr == 0x0904_0000);
        assert!(uart1.is_some(), "UART1 should be at 0x09040000");
        let uart1 = uart1.unwrap();
        assert_eq!(uart1.mmio_size, 0x1000);
        assert_eq!(uart1.intid, 40); // SPI 8 + 32
    }

    #[test]
    fn finds_gicv3() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let gic_hash = compatible_hash(b"arm,gic-v3");
        let gic = devs.devices[..devs.count]
            .iter()
            .find(|d| d.compatible_hash == gic_hash);
        assert!(gic.is_some(), "Should find GICv3");
        let gic = gic.unwrap();
        assert_eq!(gic.mmio_addr, 0x0800_0000);
    }

    #[test]
    fn devices_not_claimed_initially() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        for d in &devs.devices[..devs.count] {
            assert!(!d.claimed);
        }
    }

    #[test]
    fn finds_multiple_device_types() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        // Should find more than just PL011s — GIC, virtio, etc.
        assert!(devs.count >= 3, "Should find at least 3 devices, found {}", devs.count);
    }

    #[test]
    fn too_small_input() {
        match parse_fdt(&[0; 10]) {
            Err(FdtError::TooSmall) => {}
            _ => panic!("expected TooSmall"),
        }
    }

    #[test]
    fn bad_magic() {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&[0xBA, 0xAD, 0xF0, 0x0D]);
        match parse_fdt(&data) {
            Err(FdtError::BadMagic) => {}
            _ => panic!("expected BadMagic"),
        }
    }
}
