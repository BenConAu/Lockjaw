//! MBR partition table parsing. Pure logic; allocation-free; host-tested.
//!
//! Callers hand in sector 0 (exactly 512 bytes) and the upstream disk's
//! reported capacity in sectors; `parse_disk` identifies whether LBA 0 is a
//! bare FAT32 BPB or an MBR partition table and returns the corresponding
//! `DiskLayout`. The caller then picks the partition it wants and builds a
//! `PartitionBlockEngine` in `user/partition-manager`.

pub const MBR_SIGNATURE: u16 = 0xAA55;
pub const MAX_MBR_PARTITIONS: usize = 4;

/// Byte offset of the first MBR partition entry within sector 0.
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
/// Each MBR partition entry is 16 bytes.
const MBR_ENTRY_SIZE: usize = 16;
/// Byte offset of the 0xAA55 signature within sector 0.
const MBR_SIGNATURE_OFFSET: usize = 510;

/// A single entry from an MBR partition table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbrPartition {
    /// Partition type byte (e.g. 0x0B = FAT32 CHS, 0x0C = FAT32 LBA).
    /// 0x00 means the slot is empty.
    pub partition_type: u8,
    /// First LBA of the partition (little-endian u32 in the table).
    pub start_lba: u32,
    /// Length of the partition in sectors.
    pub sector_count: u32,
}

/// The layout interpretation of LBA 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskLayout {
    /// LBA 0 is a FAT32 BPB — the whole disk is one FAT volume starting
    /// at LBA 0. `sector_count` is the upstream's reported capacity.
    BareFat { sector_count: u64 },
    /// LBA 0 is an MBR with a valid 0xAA55 signature. `partitions[i]` is
    /// the i-th 16-byte entry; `partition_type == 0` means the slot is
    /// empty (callers should skip those).
    Mbr { partitions: [MbrPartition; MAX_MBR_PARTITIONS] },
}

/// Error returned when `parse_disk` cannot identify a usable layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionError {
    /// 0xAA55 missing at bytes 510-511, OR present but not a bare FAT32
    /// volume AND at least one non-empty MBR entry is malformed: either
    /// has `sector_count == 0` (typed but empty geometry) or extends
    /// past the upstream capacity (including the u32-wrap-around case
    /// caught by widening both fields to u64 before comparing). Refusing
    /// to partially advertise a malformed table.
    Unrecognised,
    /// 0xAA55 present, all four MBR entries have `partition_type == 0`.
    NoPartitions,
}

/// True for the two FAT32 partition-type codes used on modern SD cards.
///
/// 0x0B = FAT32 with CHS addressing, 0x0C = FAT32 with LBA addressing.
/// Real SD cards formatted by modern tools use 0x0C.
pub fn is_fat32(partition_type: u8) -> bool {
    matches!(partition_type, 0x0B | 0x0C)
}

/// Parse LBA 0 to determine the disk layout.
///
/// `sector_zero` must be exactly 512 bytes (the full first sector).
/// `upstream_capacity_sectors` is the upstream driver's reported sector count;
/// it is only stored in the `BareFat` variant and used to bounds-check each
/// non-empty MBR entry.
pub fn parse_disk(
    sector_zero: &[u8; 512],
    upstream_capacity_sectors: u64,
) -> Result<DiskLayout, PartitionError> {
    // Both MBR and FAT32 BPB carry 0xAA55 at bytes 510-511.
    // Absence means we don't recognise the sector at all.
    if sector_zero[MBR_SIGNATURE_OFFSET] != 0x55
        || sector_zero[MBR_SIGNATURE_OFFSET + 1] != 0xAA
    {
        return Err(PartitionError::Unrecognised);
    }

    // Strong bare-FAT32 discriminator: the filesystem-type string at
    // offset 82 must be exactly "FAT32   " (8 bytes including trailing
    // spaces). A jump-bytes-only check (0xEB ?? 0x90) would
    // misclassify MBR boot sectors whose boot code starts with a short
    // jump. This is the same field fat32::parse_bpb checks at
    // lockjaw-types/src/fat32.rs:140.
    if &sector_zero[82..90] == b"FAT32   " {
        return Ok(DiskLayout::BareFat { sector_count: upstream_capacity_sectors });
    }

    // Parse the four 16-byte MBR partition entries.
    let mut partitions = [MbrPartition { partition_type: 0, start_lba: 0, sector_count: 0 };
        MAX_MBR_PARTITIONS];
    let mut any_nonempty = false;

    for i in 0..MAX_MBR_PARTITIONS {
        let base = MBR_PARTITION_TABLE_OFFSET + i * MBR_ENTRY_SIZE;
        let entry = &sector_zero[base..base + MBR_ENTRY_SIZE];

        // Byte 4 is the partition type; 0x00 = empty.
        let partition_type = entry[4];
        // Bytes 8-11: first LBA (little-endian).
        let start_lba = read_u32_le(entry, 8);
        // Bytes 12-15: sector count (little-endian).
        let sector_count = read_u32_le(entry, 12);

        if partition_type != 0 {
            any_nonempty = true;
            // A typed entry with sector_count == 0 is malformed: the type
            // byte claims a real partition, the geometry says it has no
            // sectors. Same "don't half-advertise" spirit as the past-disk
            // rejection — fat32-server would fail on its first read anyway.
            if sector_count == 0 {
                return Err(PartitionError::Unrecognised);
            }
            // Validate that the partition fits within the disk. Widen both
            // fields to u64 before adding — the sum of two u32 values never
            // overflows u64, but a naïve u32 add could wrap around and
            // silently pass the capacity check (e.g. u32::MAX + 1 = 0).
            let end = (start_lba as u64) + (sector_count as u64);
            if end > upstream_capacity_sectors {
                return Err(PartitionError::Unrecognised);
            }
        }

        partitions[i] = MbrPartition { partition_type, start_lba, sector_count };
    }

    if !any_nonempty {
        return Err(PartitionError::NoPartitions);
    }

    Ok(DiskLayout::Mbr { partitions })
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a zeroed 512-byte sector with 0xAA55 at offset 510.
    fn sector_with_sig() -> [u8; 512] {
        let mut s = [0u8; 512];
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    /// Write a 16-byte MBR partition entry into sector `s` at slot `i`.
    fn write_mbr_entry(s: &mut [u8; 512], slot: usize, ptype: u8, start: u32, count: u32) {
        let base = MBR_PARTITION_TABLE_OFFSET + slot * MBR_ENTRY_SIZE;
        s[base + 4] = ptype;
        let start_b = start.to_le_bytes();
        let count_b = count.to_le_bytes();
        s[base + 8..base + 12].copy_from_slice(&start_b);
        s[base + 12..base + 16].copy_from_slice(&count_b);
    }

    #[test]
    fn parse_disk_bare_fat_recognised_by_fat32_string() {
        let mut s = sector_with_sig();
        s[82..90].copy_from_slice(b"FAT32   ");
        let cap = 131_072u64;
        assert_eq!(parse_disk(&s, cap), Ok(DiskLayout::BareFat { sector_count: cap }));
    }

    // THE regression test (per codex): a sector whose first three bytes look
    // like MBR boot code (0xEB ?? 0x90 — a short jump instruction) but which
    // does NOT carry the "FAT32   " string must be classified as Mbr, not
    // BareFat. A jump-bytes-only discriminator would have failed here.
    #[test]
    fn parse_disk_mbr_boot_code_with_eb_xx_90_not_classified_as_fat() {
        let mut s = sector_with_sig();
        // Realistic MBR boot code prefix.
        s[0] = 0xEB;
        s[1] = 0x5A; // arbitrary displacement
        s[2] = 0x90;
        // No "FAT32   " at offset 82 — leave those bytes as 0x00.
        // One valid FAT32 entry so the result is Mbr, not NoPartitions.
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 65536);
        let layout = parse_disk(&s, 131_072).unwrap();
        assert!(matches!(layout, DiskLayout::Mbr { .. }));
    }

    #[test]
    fn parse_disk_mbr_signature_only_no_entries() {
        let s = sector_with_sig(); // all partition entries are type=0
        assert_eq!(parse_disk(&s, 131_072), Err(PartitionError::NoPartitions));
    }

    #[test]
    fn parse_disk_mbr_single_fat32_lba_partition() {
        let mut s = sector_with_sig();
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 65536);
        let layout = parse_disk(&s, 131_072).unwrap();
        match layout {
            DiskLayout::Mbr { partitions } => {
                assert_eq!(partitions[0].partition_type, 0x0C);
                assert_eq!(partitions[0].start_lba, 2048);
                assert_eq!(partitions[0].sector_count, 65536);
                assert_eq!(partitions[1].partition_type, 0);
                assert_eq!(partitions[2].partition_type, 0);
                assert_eq!(partitions[3].partition_type, 0);
            }
            _ => panic!("expected Mbr"),
        }
    }

    #[test]
    fn parse_disk_mbr_four_partitions_finds_all() {
        let mut s = sector_with_sig();
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 10000);
        write_mbr_entry(&mut s, 1, 0x82, 12048, 10000);
        write_mbr_entry(&mut s, 2, 0x0B, 22048, 10000);
        write_mbr_entry(&mut s, 3, 0x83, 32048, 10000);
        let cap = 100_000u64;
        let layout = parse_disk(&s, cap).unwrap();
        match layout {
            DiskLayout::Mbr { partitions } => {
                assert_eq!(partitions[0].partition_type, 0x0C);
                assert_eq!(partitions[1].partition_type, 0x82);
                assert_eq!(partitions[2].partition_type, 0x0B);
                assert_eq!(partitions[3].partition_type, 0x83);
            }
            _ => panic!("expected Mbr"),
        }
    }

    #[test]
    fn parse_disk_mbr_partition_extends_past_disk_rejected() {
        let mut s = sector_with_sig();
        // start_lba=2048 + sector_count=131072 = 133120 > capacity=131072
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 131_072);
        assert_eq!(parse_disk(&s, 131_072), Err(PartitionError::Unrecognised));
    }

    // start_lba = u32::MAX, sector_count = 1: naïve u32 addition wraps to 0,
    // which would (wrongly) pass a capacity check. Casting to u64 first gives
    // 4_294_967_296 which exceeds any reasonable capacity.
    #[test]
    fn parse_disk_mbr_partition_arithmetic_overflow_rejected() {
        let mut s = sector_with_sig();
        write_mbr_entry(&mut s, 0, 0x0C, u32::MAX, 1);
        let cap = 1_000_000u64;
        assert_eq!(parse_disk(&s, cap), Err(PartitionError::Unrecognised));
    }

    #[test]
    fn parse_disk_random_data_returns_unrecognised() {
        let s = [0xA5u8; 512]; // no 0xAA55, no FAT32 string
        assert_eq!(parse_disk(&s, 131_072), Err(PartitionError::Unrecognised));
    }

    // Has the FAT32 string at offset 82 but is missing 0xAA55 at 510-511.
    // We check 0xAA55 first so this returns Unrecognised, not BareFat.
    #[test]
    fn parse_disk_no_aa55_signature_unrecognised() {
        let mut s = [0u8; 512];
        s[82..90].copy_from_slice(b"FAT32   ");
        // 0xAA55 deliberately absent.
        assert_eq!(parse_disk(&s, 131_072), Err(PartitionError::Unrecognised));
    }

    // Off-by-one boundary on the `>` capacity check: a partition that
    // exactly ends at the last sector of the disk must be accepted.
    #[test]
    fn parse_disk_mbr_partition_end_equals_capacity_accepted() {
        let mut s = sector_with_sig();
        let cap = 131_072u64;
        // start=2048, count=129024 → end=131072 == capacity (last sector OK).
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 129_024);
        let layout = parse_disk(&s, cap).unwrap();
        match layout {
            DiskLayout::Mbr { partitions } => {
                assert_eq!(partitions[0].start_lba, 2048);
                assert_eq!(partitions[0].sector_count, 129_024);
            }
            _ => panic!("expected Mbr"),
        }
    }

    // "Don't half-advertise a malformed table": one good entry plus a
    // later out-of-range entry must reject the whole table, not silently
    // expose the valid one.
    #[test]
    fn parse_disk_mbr_one_good_entry_plus_out_of_range_rejected() {
        let mut s = sector_with_sig();
        let cap = 200_000u64;
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 10_000); // valid
        write_mbr_entry(&mut s, 1, 0x83, 100_000, 200_000); // extends past disk
        assert_eq!(parse_disk(&s, cap), Err(PartitionError::Unrecognised));
    }

    // Empty slots between non-empty entries are preserved verbatim — the
    // caller decides what to do with `partition_type == 0` holes.
    #[test]
    fn parse_disk_mbr_hole_between_entries_preserved() {
        let mut s = sector_with_sig();
        let cap = 200_000u64;
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 10_000);
        // slot 1 left empty (type == 0)
        write_mbr_entry(&mut s, 2, 0x83, 50_000, 10_000);
        // slot 3 left empty
        let layout = parse_disk(&s, cap).unwrap();
        match layout {
            DiskLayout::Mbr { partitions } => {
                assert_eq!(partitions[0].partition_type, 0x0C);
                assert_eq!(partitions[1].partition_type, 0);
                assert_eq!(partitions[2].partition_type, 0x83);
                assert_eq!(partitions[3].partition_type, 0);
            }
            _ => panic!("expected Mbr"),
        }
    }

    // Real MBRs can leave stale bytes in empty (type=0) slots — wiping
    // the type byte alone is the conventional way tools clear an entry.
    // Emptiness is keyed strictly off `partition_type == 0`, so garbage
    // start_lba / sector_count in those slots must not trigger validation
    // or change the NoPartitions verdict.
    #[test]
    fn parse_disk_mbr_all_empty_with_garbage_in_unused_fields_returns_no_partitions() {
        let mut s = sector_with_sig();
        // type=0 in all four slots, but stuff garbage in start/count to
        // simulate "tool wrote partition table then wiped type bytes".
        // These would be wildly out-of-range if validated.
        for slot in 0..MAX_MBR_PARTITIONS {
            let base = MBR_PARTITION_TABLE_OFFSET + slot * MBR_ENTRY_SIZE;
            s[base + 4] = 0; // partition_type = 0 (empty)
            s[base + 8..base + 12].copy_from_slice(&u32::MAX.to_le_bytes());
            s[base + 12..base + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        assert_eq!(parse_disk(&s, 131_072), Err(PartitionError::NoPartitions));
    }

    // A typed entry with sector_count=0 is malformed: the type byte says
    // "real partition" but the geometry says "no sectors". We reject it
    // (same "don't half-advertise" spirit as the past-disk rejection).
    #[test]
    fn parse_disk_mbr_typed_entry_with_zero_sectors_rejected() {
        let mut s = sector_with_sig();
        write_mbr_entry(&mut s, 0, 0x0C, 2048, 0);
        assert_eq!(parse_disk(&s, 131_072), Err(PartitionError::Unrecognised));
    }

    #[test]
    fn is_fat32_recognises_0b_and_0c() {
        assert!(is_fat32(0x0B));
        assert!(is_fat32(0x0C));
        assert!(!is_fat32(0x00));
        assert!(!is_fat32(0x82)); // Linux swap
        assert!(!is_fat32(0x83)); // Linux ext4
        assert!(!is_fat32(0x0E)); // FAT16 LBA
        assert!(!is_fat32(0xFF));
    }
}
