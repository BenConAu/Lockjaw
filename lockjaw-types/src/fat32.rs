//! FAT32 on-disk format: pure parsing and arithmetic.
//!
//! The personality server's `user/fat32-server` does the side effects
//! (block I/O, handle tables, IPC). Everything in this module is
//! pure — it takes byte slices and returns parsed structs or
//! decoded values, with no allocation or I/O.
//!
//! Phase scope: read-only, 8.3 short names. LFN entries are handled
//! by the dirent parser (silently skipped). Write support, long
//! filenames, and FAT12/16 are out of scope.

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Fat32Error {
    /// Sector 0 doesn't end with the 0x55 0xAA boot signature at offset 510.
    BadBootSignature,
    /// File-system type field at offset 82 isn't "FAT32   " (with two
    /// trailing spaces). Lockjaw doesn't read FAT12 or FAT16.
    NotFat32,
    /// `bytes_per_sector` isn't 512. The FAT32 spec allows 1024,
    /// 2048, and 4096 too, but Lockjaw's block layer (`lockjaw_types::
    /// block::SECTOR_SIZE`), virtio-blk driver, and fat32-server all
    /// hardcode 512 throughout. Accepting other sector sizes here
    /// would let the parser succeed on volumes the rest of the stack
    /// can't actually read; narrowing the parser keeps the layers
    /// consistent.
    InvalidBytesPerSector { value: u16 },
    /// `sectors_per_cluster` isn't a power of two between 1 and 128.
    InvalidSectorsPerCluster { value: u8 },
    /// `reserved_sector_count` is 0. FAT32 needs at least the BPB.
    ZeroReservedSectors,
    /// `num_fats` is 0. The FAT region would be empty.
    ZeroFats,
    /// `sectors_per_fat_32` (the 32-bit FAT32-specific field) is 0.
    ZeroSectorsPerFat,
    /// `root_cluster` < 2 — clusters 0 and 1 are reserved by the spec.
    InvalidRootCluster { value: u32 },
    /// `total_sectors_32` is 0 (the 16-bit field at offset 19 is also
    /// not used on FAT32 per the spec).
    ZeroTotalSectors,
    /// FAT32 requires `root_entry_count` (offset 17) to be 0; otherwise
    /// the volume is FAT12/16 with a fixed root directory.
    NonZeroRootEntries { value: u16 },
    /// FAT32 requires `sectors_per_fat_16` (offset 22) to be 0; otherwise
    /// the volume is FAT12/16.
    NonZeroSectorsPerFat16 { value: u16 },
    /// `reserved_sectors + num_fats * sectors_per_fat` exceeds
    /// `total_sectors` — the FAT region wouldn't fit in the volume,
    /// which would produce a wrap-around `cluster_count` if let through.
    /// Also catches u32 multiplication overflow on `num_fats * sectors_per_fat`.
    LayoutExceedsVolume { data_start: u64, total_sectors: u32 },
    /// The data region has zero clusters after the FATs are placed.
    /// Even a degenerate FAT32 volume must contain at least one
    /// data cluster (the root directory).
    NoDataClusters,
    /// `root_cluster` references a cluster beyond `max_cluster`. The
    /// root directory wouldn't be readable.
    RootClusterOutOfRange { value: u32, max: u32 },
    /// Volume has fewer than [`FAT32_MIN_CLUSTERS`] data clusters.
    /// Per Microsoft's FAT spec, cluster count is the authoritative
    /// gate between FAT12/16/32 — the fs_type string at offset 82 is
    /// informational. A volume with too few clusters is FAT16 (or
    /// FAT12) regardless of what the type field says, and applying
    /// FAT32 root/FAT semantics to it would corrupt reads.
    BelowFat32MinimumClusters { count: u32, minimum: u32 },
    /// `sectors_per_fat * bytes_per_sector` doesn't hold enough 4-byte
    /// entries to cover all data clusters (plus the two reserved
    /// entries 0 and 1). Without this check, FAT lookups for high
    /// cluster numbers would index past the actual FAT region in the
    /// data area and read garbage.
    FatTooSmallForClusterCount { fat_entries: u64, required: u64 },
}

/// Microsoft FAT spec's threshold for FAT32 classification: a volume
/// with fewer than this many data clusters must be FAT12 or FAT16.
pub const FAT32_MIN_CLUSTERS: u32 = 65_525;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Resolved volume geometry, ready for cluster→sector arithmetic.
/// All fields are derived or validated by [`parse_bpb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fat32Geometry {
    /// Bytes per sector (512, 1024, 2048, or 4096).
    pub bytes_per_sector: u16,
    /// Sectors per cluster (power of two, 1..=128).
    pub sectors_per_cluster: u8,
    /// Number of FAT copies on disk (almost always 2).
    pub num_fats: u8,
    /// First sector of the first FAT (== `reserved_sector_count`).
    pub fat_start_sector: u32,
    /// Sectors per FAT (32-bit field; FAT32 ignores the 16-bit one).
    pub sectors_per_fat: u32,
    /// First sector of cluster 2 (the start of the data region).
    pub data_start_sector: u32,
    /// Cluster number of the root directory (usually 2).
    pub root_cluster: u32,
    /// Total volume size in sectors.
    pub total_sectors: u32,
}

impl Fat32Geometry {
    /// Bytes per cluster = bytes_per_sector * sectors_per_cluster.
    pub const fn bytes_per_cluster(&self) -> u32 {
        self.bytes_per_sector as u32 * self.sectors_per_cluster as u32
    }

    /// Total number of data clusters on the volume. Used to validate
    /// FAT entries and to compute "is this cluster in range?"
    pub const fn cluster_count(&self) -> u32 {
        let data_sectors = self.total_sectors - self.data_start_sector;
        data_sectors / self.sectors_per_cluster as u32
    }

    /// Highest valid data-cluster number (clusters 0 and 1 are reserved;
    /// the data region starts at cluster 2).
    pub const fn max_cluster(&self) -> u32 {
        self.cluster_count() + 1
    }
}

// ---------------------------------------------------------------------------
// BPB parser
// ---------------------------------------------------------------------------

/// Parse the Boot Parameter Block from sector 0 (512 bytes).
///
/// Validates the boot signature, FAT32-specific fields, and
/// fundamental geometry constraints. Returns a fully-resolved
/// [`Fat32Geometry`] suitable for cluster arithmetic.
pub fn parse_bpb(sector0: &[u8; 512]) -> Result<Fat32Geometry, Fat32Error> {
    if sector0[510] != 0x55 || sector0[511] != 0xAA {
        return Err(Fat32Error::BadBootSignature);
    }
    if &sector0[82..90] != b"FAT32   " {
        return Err(Fat32Error::NotFat32);
    }

    let bytes_per_sector = read_u16(sector0, 11);
    // Phase 1 only supports 512-byte sectors (matches the rest of the
    // stack — see InvalidBytesPerSector docs).
    if bytes_per_sector != 512 {
        return Err(Fat32Error::InvalidBytesPerSector { value: bytes_per_sector });
    }

    let sectors_per_cluster = sector0[13];
    if sectors_per_cluster == 0
        || !sectors_per_cluster.is_power_of_two()
        || sectors_per_cluster > 128
    {
        return Err(Fat32Error::InvalidSectorsPerCluster { value: sectors_per_cluster });
    }

    let reserved_sectors = read_u16(sector0, 14);
    if reserved_sectors == 0 {
        return Err(Fat32Error::ZeroReservedSectors);
    }

    let num_fats = sector0[16];
    if num_fats == 0 {
        return Err(Fat32Error::ZeroFats);
    }

    let root_entry_count = read_u16(sector0, 17);
    if root_entry_count != 0 {
        return Err(Fat32Error::NonZeroRootEntries { value: root_entry_count });
    }

    let sectors_per_fat_16 = read_u16(sector0, 22);
    if sectors_per_fat_16 != 0 {
        return Err(Fat32Error::NonZeroSectorsPerFat16 { value: sectors_per_fat_16 });
    }

    let total_sectors_32 = read_u32(sector0, 32);
    if total_sectors_32 == 0 {
        return Err(Fat32Error::ZeroTotalSectors);
    }

    let sectors_per_fat = read_u32(sector0, 36);
    if sectors_per_fat == 0 {
        return Err(Fat32Error::ZeroSectorsPerFat);
    }

    let root_cluster = read_u32(sector0, 44);
    if root_cluster < 2 {
        return Err(Fat32Error::InvalidRootCluster { value: root_cluster });
    }

    let fat_start_sector = reserved_sectors as u32;

    // Compute the FAT region size in u64 to avoid wrap if a malicious or
    // corrupted BPB sets sectors_per_fat near u32::MAX. The combined
    // value must also fit in u32 to be storable in `data_start_sector`,
    // and must not exceed total_sectors (otherwise downstream
    // `cluster_count` would underflow).
    let fat_region_64 = (num_fats as u64) * (sectors_per_fat as u64);
    let data_start_64 = (fat_start_sector as u64) + fat_region_64;
    if data_start_64 > total_sectors_32 as u64 {
        return Err(Fat32Error::LayoutExceedsVolume {
            data_start: data_start_64,
            total_sectors: total_sectors_32,
        });
    }
    let data_start_sector = data_start_64 as u32;

    // At least one data cluster must fit. Without this the root
    // directory has nowhere to live and the cluster-arithmetic
    // helpers would all reject every cluster index.
    let data_sectors = total_sectors_32 - data_start_sector;
    let cluster_count = data_sectors / sectors_per_cluster as u32;
    if cluster_count == 0 {
        return Err(Fat32Error::NoDataClusters);
    }

    // FAT32 classification gate. Cluster count below the spec
    // threshold means this is actually a FAT12/16 volume even if the
    // type-string at offset 82 reads "FAT32   " — that string is
    // informational, not authoritative. Reject so downstream code
    // doesn't apply FAT32 semantics to the wrong layout.
    if cluster_count < FAT32_MIN_CLUSTERS {
        return Err(Fat32Error::BelowFat32MinimumClusters {
            count: cluster_count,
            minimum: FAT32_MIN_CLUSTERS,
        });
    }

    // The FAT must hold one 4-byte entry per data cluster plus the
    // two reserved entries (0 and 1), i.e. cluster_count + 2 entries.
    // Without this check, fat_entry_location() would still return
    // sector offsets for high cluster numbers, but those offsets
    // would land in the data region (or past it) and the read would
    // see garbage. u64 arithmetic so a malicious sectors_per_fat near
    // u32::MAX can't wrap into a falsely-large capacity.
    let fat_capacity_bytes = (sectors_per_fat as u64) * (bytes_per_sector as u64);
    let fat_capacity_entries = fat_capacity_bytes / 4;
    let required_entries = (cluster_count as u64) + 2;
    if fat_capacity_entries < required_entries {
        return Err(Fat32Error::FatTooSmallForClusterCount {
            fat_entries: fat_capacity_entries,
            required: required_entries,
        });
    }

    // root_cluster must be a valid data-cluster index. max_cluster =
    // cluster_count + 1 because clusters 0 and 1 are reserved (so
    // valid data clusters are 2..=cluster_count + 1).
    let max_cluster = cluster_count + 1;
    if root_cluster > max_cluster {
        return Err(Fat32Error::RootClusterOutOfRange { value: root_cluster, max: max_cluster });
    }

    Ok(Fat32Geometry {
        bytes_per_sector,
        sectors_per_cluster,
        num_fats,
        fat_start_sector,
        sectors_per_fat,
        data_start_sector,
        root_cluster,
        total_sectors: total_sectors_32,
    })
}

// ---------------------------------------------------------------------------
// Cluster arithmetic
// ---------------------------------------------------------------------------

/// First sector containing data for `cluster`. Cluster 2 is the
/// first data cluster (clusters 0 and 1 are reserved by the FAT32
/// spec).
///
/// Returns `None` if `cluster` is out of range (< 2 or > max_cluster).
pub fn cluster_to_sector(cluster: u32, geom: &Fat32Geometry) -> Option<u32> {
    if cluster < 2 || cluster > geom.max_cluster() {
        return None;
    }
    Some(geom.data_start_sector + (cluster - 2) * geom.sectors_per_cluster as u32)
}

/// Locate FAT entry for `cluster`: returns the sector containing it
/// and the byte offset within that sector. Each entry is 4 bytes on
/// FAT32.
///
/// Returns `None` if `cluster` is out of range. (Clusters 0 and 1 are
/// allowed here — the FAT does have entries for them, used for the
/// media descriptor and the dirty/clean shutdown bit — but they are
/// never valid data clusters.)
pub fn fat_entry_location(cluster: u32, geom: &Fat32Geometry) -> Option<(u32, u32)> {
    if cluster > geom.max_cluster() {
        return None;
    }
    let byte_offset = cluster as u64 * 4;
    let sector_offset = (byte_offset / geom.bytes_per_sector as u64) as u32;
    let in_sector = (byte_offset % geom.bytes_per_sector as u64) as u32;
    Some((geom.fat_start_sector + sector_offset, in_sector))
}

// ---------------------------------------------------------------------------
// FAT entry decode
// ---------------------------------------------------------------------------

/// Classification of one FAT32 entry. Only the low 28 bits of the
/// 32-bit raw value are meaningful — the spec reserves the top 4 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatEntry {
    /// Cluster is not allocated.
    Free,
    /// Cluster is allocated; `next` is the next cluster in the chain
    /// (or follow-on info for end users to interpret).
    Used { next: u32 },
    /// Cluster is reserved (vendor-defined).
    Reserved,
    /// Cluster is marked bad — must not be used.
    Bad,
    /// End of cluster chain. The file/directory ends with this cluster.
    EndOfChain,
}

const FAT32_ENTRY_MASK: u32 = 0x0FFF_FFFF;

/// Decode the FAT entry at byte offset `cluster * 4` into the given
/// FAT byte buffer. The buffer must contain at least the full entry
/// (4 bytes) starting at the appropriate offset.
///
/// Returns `None` if the buffer doesn't have 4 bytes available at
/// `(cluster * 4)`.
pub fn decode_fat_entry(fat_bytes: &[u8], cluster: u32) -> Option<FatEntry> {
    let byte_offset = cluster as usize * 4;
    if byte_offset + 4 > fat_bytes.len() {
        return None;
    }
    let raw = u32::from_le_bytes([
        fat_bytes[byte_offset],
        fat_bytes[byte_offset + 1],
        fat_bytes[byte_offset + 2],
        fat_bytes[byte_offset + 3],
    ]);
    let value = raw & FAT32_ENTRY_MASK;
    Some(classify_entry(value))
}

/// Decode a raw 32-bit FAT entry value (the top 4 bits are masked
/// off internally). Lower-level than [`decode_fat_entry`] for callers
/// that already have just the 4 bytes — e.g. a server reading one
/// FAT sector at a time and extracting the entry at a known offset
/// rather than indexing by cluster.
pub const fn decode_fat_entry_value(raw: u32) -> FatEntry {
    classify_entry(raw & FAT32_ENTRY_MASK)
}

/// Classify a 28-bit FAT32 entry value (already masked).
/// Values use the full 28-bit range — note the constants below use
/// seven-hex-digit form so they line up with the spec exactly.
const fn classify_entry(value: u32) -> FatEntry {
    match value {
        0x0000000 => FatEntry::Free,
        0x0000001 => FatEntry::Reserved,
        0x0000002..=0xFFFFFEF => FatEntry::Used { next: value },
        0xFFFFFF0..=0xFFFFFF6 => FatEntry::Reserved,
        0xFFFFFF7 => FatEntry::Bad,
        // 0xFFFFFF8..=0xFFFFFFF — End-of-chain. Spec recommends
        // 0x0FFFFFF8 but anything in that range is valid EOC.
        _ => FatEntry::EndOfChain,
    }
}

// ---------------------------------------------------------------------------
// Directory entries (8.3 short names; LFN entries are skipped)
// ---------------------------------------------------------------------------

// Directory entry attribute bits (one byte at offset 11 of a dirent).
pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;

/// LFN entries set all four of READ_ONLY | HIDDEN | SYSTEM | VOLUME_ID
/// simultaneously (a value never used by real attributes), so the low
/// 4 bits of `attr` distinguish them. Check this *before* checking
/// `ATTR_VOLUME_ID` alone, since LFN sets that bit too.
pub const ATTR_LFN_MASK: u8 =
    ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

/// One parsed directory entry. The 11-byte `name` is the raw on-disk
/// 8.3 form: bytes 0-7 are the name (space-padded), bytes 8-10 are
/// the extension (space-padded). Byte 0 == 0x05 in the on-disk
/// representation is the escape for files whose real first byte is
/// 0xE5; [`parse_dirent`] decodes that escape into the actual byte
/// before returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry {
    pub name: [u8; 11],
    pub attr: u8,
    pub first_cluster: u32,
    pub size: u32,
}

impl DirEntry {
    /// True iff the entry refers to a subdirectory (rather than a file).
    pub const fn is_directory(&self) -> bool {
        (self.attr & ATTR_DIRECTORY) != 0
    }
}

/// Result of inspecting a 32-byte slot in a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirEntryStatus {
    /// First byte is 0x00. The directory has no more entries past
    /// this slot — stop walking.
    EndOfDir,
    /// Slot should be skipped (deleted file, LFN entry, or volume
    /// label). Continue walking.
    Skip,
    /// A valid file or subdirectory entry.
    Entry(DirEntry),
}

/// Classify and decode a 32-byte directory slot.
pub fn parse_dirent(bytes: &[u8; 32]) -> DirEntryStatus {
    let first = bytes[0];
    if first == 0x00 {
        return DirEntryStatus::EndOfDir;
    }
    if first == 0xE5 {
        return DirEntryStatus::Skip;
    }
    let attr = bytes[11];
    // LFN check before VOLUME_ID — LFN sets the VOLUME_ID bit too.
    if (attr & ATTR_LFN_MASK) == ATTR_LFN_MASK {
        return DirEntryStatus::Skip;
    }
    if (attr & ATTR_VOLUME_ID) != 0 {
        return DirEntryStatus::Skip;
    }
    let mut name = [0u8; 11];
    name.copy_from_slice(&bytes[..11]);
    // 0x05 escape: real first byte is 0xE5 (which would otherwise be
    // misread as "deleted").
    if name[0] == 0x05 {
        name[0] = 0xE5;
    }
    let cluster_hi = u16::from_le_bytes([bytes[20], bytes[21]]) as u32;
    let cluster_lo = u16::from_le_bytes([bytes[26], bytes[27]]) as u32;
    let first_cluster = (cluster_hi << 16) | cluster_lo;
    let size = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    DirEntryStatus::Entry(DirEntry { name, attr, first_cluster, size })
}

/// Iterator over parsed directory entries in a byte buffer. Skips
/// `Skip` slots and stops at `EndOfDir` (or when the buffer is
/// exhausted). Caller provides one or more clusters' worth of bytes
/// concatenated; the iterator processes 32-byte slots in order.
pub struct DirIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    done: bool,
}

impl<'a> DirIter<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0, done: false }
    }
}

impl<'a> Iterator for DirIter<'a> {
    type Item = DirEntry;
    fn next(&mut self) -> Option<DirEntry> {
        while !self.done {
            if self.offset + 32 > self.bytes.len() {
                return None;
            }
            // SAFETY of unwrap: bounds checked just above.
            let chunk: &[u8; 32] = self.bytes[self.offset..self.offset + 32]
                .try_into()
                .unwrap();
            self.offset += 32;
            match parse_dirent(chunk) {
                DirEntryStatus::EndOfDir => {
                    self.done = true;
                    return None;
                }
                DirEntryStatus::Skip => continue,
                DirEntryStatus::Entry(e) => return Some(e),
            }
        }
        None
    }
}

/// Convenience: iterate parsed entries in a directory cluster (or
/// concatenation of clusters). Equivalent to `DirIter::new(bytes)`.
pub fn iter_dir(bytes: &[u8]) -> DirIter<'_> {
    DirIter::new(bytes)
}

// ---------------------------------------------------------------------------
// 8.3 path matching
// ---------------------------------------------------------------------------

/// Case-insensitive ASCII match of a path component against an
/// on-disk 8.3 directory entry. Splits `query` on the first `.` to
/// separate name from extension, uppercases each side, and pads
/// to 8 + 3 with spaces before comparing the 11-byte `entry.name`.
///
/// Returns false if the name part exceeds 8 bytes or the extension
/// exceeds 3 bytes (those queries can't refer to an 8.3 file). The
/// special "." and ".." directory entries match queries `b"."` and
/// `b".."` respectively.
pub fn match_8_3(query: &[u8], entry: &DirEntry) -> bool {
    // "." and ".." are stored on disk as ".          " and "..         "
    // (the dot characters in the name field, no extension). The
    // dot-split logic below would otherwise treat "." as name="" ext="".
    if query == b"." {
        return entry.name[0] == b'.' && entry.name[1] == b' ';
    }
    if query == b".." {
        return entry.name[0] == b'.' && entry.name[1] == b'.' && entry.name[2] == b' ';
    }

    let dot_pos = query.iter().position(|&b| b == b'.');
    let (name_part, ext_part) = match dot_pos {
        Some(p) => (&query[..p], &query[p + 1..]),
        None => (query, &[][..]),
    };
    // Reject empty name (e.g. ".txt", "."). The `.` and `..` cases
    // are handled above as explicit exceptions; every other valid
    // 8.3 entry has a non-empty name part. An all-spaces on-disk
    // name field that happens to share the extension bytes shouldn't
    // be reachable from a POSIX path.
    if name_part.is_empty() {
        return false;
    }
    if name_part.len() > 8 || ext_part.len() > 3 {
        return false;
    }

    let mut canon = [b' '; 11];
    for (i, &b) in name_part.iter().enumerate() {
        canon[i] = ascii_uppercase(b);
    }
    for (i, &b) in ext_part.iter().enumerate() {
        canon[8 + i] = ascii_uppercase(b);
    }
    canon == entry.name
}

const fn ascii_uppercase(b: u8) -> u8 {
    if b >= b'a' && b <= b'z' { b - 32 } else { b }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_u16(bytes: &[u8; 512], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8; 512], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic, plausible BPB sector. Test cases mutate
    /// individual fields to exercise validation paths.
    fn make_bpb() -> [u8; 512] {
        let mut s = [0u8; 512];
        // Jump + OEM (cosmetic; parser doesn't read these).
        s[0] = 0xEB; s[1] = 0x58; s[2] = 0x90;
        s[3..11].copy_from_slice(b"MTOOL043");
        // bytes_per_sector = 512 (LE)
        s[11] = 0x00; s[12] = 0x02;
        // sectors_per_cluster = 1
        s[13] = 1;
        // reserved_sectors = 32 (typical FAT32)
        s[14] = 32; s[15] = 0;
        // num_fats = 2
        s[16] = 2;
        // root_entry_count = 0 (FAT32 requirement)
        s[17] = 0; s[18] = 0;
        // total_sectors_16 = 0 (use 32-bit)
        s[19] = 0; s[20] = 0;
        // media = 0xF8
        s[21] = 0xF8;
        // sectors_per_fat_16 = 0 (FAT32 requirement)
        s[22] = 0; s[23] = 0;
        // sectors_per_track / heads / hidden / total_sectors_16 — 0 fine
        // total_sectors_32 = 131072 (= 64 MiB / 512)
        let total_sectors: u32 = 131072;
        s[32..36].copy_from_slice(&total_sectors.to_le_bytes());
        // sectors_per_fat_32 = 1009 (typical for 64 MiB)
        let sectors_per_fat: u32 = 1009;
        s[36..40].copy_from_slice(&sectors_per_fat.to_le_bytes());
        // root_cluster = 2
        let root_cluster: u32 = 2;
        s[44..48].copy_from_slice(&root_cluster.to_le_bytes());
        // fs_type = "FAT32   "
        s[82..90].copy_from_slice(b"FAT32   ");
        // boot signature
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    // ---- parse_bpb happy path ----

    #[test]
    fn bpb_typical_volume_parses() {
        let g = parse_bpb(&make_bpb()).unwrap();
        assert_eq!(g.bytes_per_sector, 512);
        assert_eq!(g.sectors_per_cluster, 1);
        assert_eq!(g.num_fats, 2);
        assert_eq!(g.fat_start_sector, 32);
        assert_eq!(g.sectors_per_fat, 1009);
        // data_start = 32 + 2 * 1009 = 2050
        assert_eq!(g.data_start_sector, 2050);
        assert_eq!(g.root_cluster, 2);
        assert_eq!(g.total_sectors, 131072);
    }

    #[test]
    fn bpb_derived_bytes_per_cluster() {
        let mut s = make_bpb();
        s[13] = 8; // 8 sectors per cluster
        // With spc=8 we need ≥ 65525 * 8 = 524_200 data sectors. Bump
        // total_sectors so the volume meets the FAT32 cluster floor.
        s[32..36].copy_from_slice(&600_000u32.to_le_bytes());
        let g = parse_bpb(&s).unwrap();
        assert_eq!(g.bytes_per_cluster(), 4096);
    }

    #[test]
    fn bpb_derived_cluster_count_and_max_cluster() {
        let g = parse_bpb(&make_bpb()).unwrap();
        // total = 131072, data_start = 2050 → data_sectors = 129022.
        // sectors_per_cluster = 1, so cluster_count = 129022.
        assert_eq!(g.cluster_count(), 129022);
        // Highest valid cluster number is cluster_count + 1 = 129023.
        assert_eq!(g.max_cluster(), 129023);
    }

    // ---- parse_bpb validation failures ----

    #[test]
    fn bpb_bad_boot_signature_rejected() {
        let mut s = make_bpb();
        s[510] = 0; s[511] = 0;
        assert_eq!(parse_bpb(&s), Err(Fat32Error::BadBootSignature));
    }

    #[test]
    fn bpb_partial_boot_signature_rejected() {
        // 0x55 alone (without 0xAA) is also invalid.
        let mut s = make_bpb();
        s[511] = 0;
        assert_eq!(parse_bpb(&s), Err(Fat32Error::BadBootSignature));
    }

    #[test]
    fn bpb_non_fat32_fs_type_rejected() {
        let mut s = make_bpb();
        s[82..90].copy_from_slice(b"FAT16   ");
        assert_eq!(parse_bpb(&s), Err(Fat32Error::NotFat32));
    }

    #[test]
    fn bpb_invalid_bytes_per_sector_rejected() {
        let mut s = make_bpb();
        // 256 isn't 512 (and isn't FAT32-spec-legal at all).
        s[11] = 0x00; s[12] = 0x01;
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::InvalidBytesPerSector { value: 256 }),
        ));
    }

    #[test]
    fn bpb_non_512_byte_sectors_rejected() {
        // FAT32 spec allows 1024 / 2048 / 4096 as well, but the rest
        // of the Lockjaw stack hardcodes 512-byte sectors. Narrow the
        // parser to match.
        for value in [1024u16, 2048, 4096] {
            let mut s = make_bpb();
            s[11..13].copy_from_slice(&value.to_le_bytes());
            assert!(
                matches!(
                    parse_bpb(&s),
                    Err(Fat32Error::InvalidBytesPerSector { value: v }) if v == value,
                ),
                "expected InvalidBytesPerSector for {} bytes/sector",
                value,
            );
        }
    }

    #[test]
    fn bpb_zero_sectors_per_cluster_rejected() {
        let mut s = make_bpb();
        s[13] = 0;
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::InvalidSectorsPerCluster { value: 0 }),
        ));
    }

    #[test]
    fn bpb_non_power_of_two_sectors_per_cluster_rejected() {
        let mut s = make_bpb();
        s[13] = 3;
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::InvalidSectorsPerCluster { value: 3 }),
        ));
    }

    #[test]
    fn bpb_max_sectors_per_cluster_accepted() {
        // 128 is the spec maximum (a power of 2, ≤ 128). Larger values
        // (256+) can't even fit in u8 since the field is one byte.
        let mut s = make_bpb();
        s[13] = 128;
        // With spc=128 we need ≥ 65525 * 128 = 8_387_200 data sectors.
        // Bump total_sectors and sectors_per_fat to support that scale.
        // Larger FAT: cluster_count ≈ 65525 → FAT entries ≈ 65525 * 4 = 262_100 bytes ≈ 513 sectors.
        // Round up generously.
        s[36..40].copy_from_slice(&1024u32.to_le_bytes()); // sectors_per_fat
        // total = reserved(32) + fats(2*1024=2048) + data(65525*128) = 8_389_280
        s[32..36].copy_from_slice(&8_400_000u32.to_le_bytes());
        let g = parse_bpb(&s).unwrap();
        assert_eq!(g.sectors_per_cluster, 128);
    }

    #[test]
    fn bpb_zero_reserved_sectors_rejected() {
        let mut s = make_bpb();
        s[14] = 0; s[15] = 0;
        assert_eq!(parse_bpb(&s), Err(Fat32Error::ZeroReservedSectors));
    }

    #[test]
    fn bpb_zero_fats_rejected() {
        let mut s = make_bpb();
        s[16] = 0;
        assert_eq!(parse_bpb(&s), Err(Fat32Error::ZeroFats));
    }

    #[test]
    fn bpb_nonzero_root_entries_rejected() {
        // FAT16 had a fixed root dir; FAT32 must be zero here.
        let mut s = make_bpb();
        s[17] = 0xE0; s[18] = 0x00;
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::NonZeroRootEntries { value: 224 }),
        ));
    }

    #[test]
    fn bpb_nonzero_sectors_per_fat_16_rejected() {
        let mut s = make_bpb();
        s[22] = 0x40; s[23] = 0x00; // 64
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::NonZeroSectorsPerFat16 { value: 64 }),
        ));
    }

    #[test]
    fn bpb_zero_total_sectors_rejected() {
        let mut s = make_bpb();
        s[32..36].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_bpb(&s), Err(Fat32Error::ZeroTotalSectors));
    }

    #[test]
    fn bpb_zero_sectors_per_fat_rejected() {
        let mut s = make_bpb();
        s[36..40].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_bpb(&s), Err(Fat32Error::ZeroSectorsPerFat));
    }

    #[test]
    fn bpb_root_cluster_below_2_rejected() {
        let mut s = make_bpb();
        s[44..48].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::InvalidRootCluster { value: 1 }),
        ));
    }

    #[test]
    fn bpb_fat_region_exceeds_volume_rejected() {
        // sectors_per_fat = 100_000 with num_fats = 2 → FAT region = 200_000
        // sectors. Plus 32 reserved = 200_032 > 131_072 total. Without
        // the layout check, cluster_count() would underflow to a huge u32.
        let mut s = make_bpb();
        s[36..40].copy_from_slice(&100_000u32.to_le_bytes());
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::LayoutExceedsVolume { data_start: 200_032, total_sectors: 131_072 }),
        ));
    }

    #[test]
    fn bpb_fat_region_overflows_u32_rejected() {
        // sectors_per_fat near u32::MAX with num_fats = 2 would wrap u32
        // multiplication. Caught by u64 arithmetic + the LayoutExceedsVolume
        // check (the wrapped value would also exceed total_sectors).
        let mut s = make_bpb();
        s[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        let result = parse_bpb(&s);
        assert!(matches!(result, Err(Fat32Error::LayoutExceedsVolume { .. })));
        // Specifically: data_start should be the unwrapped u64 value,
        // not a u32-wrapped value (which is the bug we're guarding against).
        if let Err(Fat32Error::LayoutExceedsVolume { data_start, .. }) = result {
            assert!(data_start > u32::MAX as u64);
        }
    }

    #[test]
    fn bpb_fat_region_exactly_fills_volume_rejected_no_data() {
        // reserved + num_fats * sectors_per_fat == total_sectors → 0 data sectors.
        // total = 100, reserved = 2, num_fats = 2, sectors_per_fat = 49 → 100.
        let mut s = make_bpb();
        s[14..16].copy_from_slice(&2u16.to_le_bytes()); // reserved = 2
        s[16] = 2; // num_fats = 2
        s[32..36].copy_from_slice(&100u32.to_le_bytes()); // total = 100
        s[36..40].copy_from_slice(&49u32.to_le_bytes()); // sectors_per_fat = 49
        assert_eq!(parse_bpb(&s), Err(Fat32Error::NoDataClusters));
    }

    #[test]
    fn bpb_root_cluster_out_of_range_rejected() {
        // root_cluster set just past max_cluster.
        // total = 131_072, data_start = 32 + 2*1009 = 2050, data_sectors =
        // 129_022, cluster_count = 129_022 (sectors_per_cluster = 1),
        // max_cluster = 129_023. Setting root_cluster = 129_024 is out of range.
        let mut s = make_bpb();
        s[44..48].copy_from_slice(&129_024u32.to_le_bytes());
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::RootClusterOutOfRange { value: 129_024, max: 129_023 }),
        ));
    }

    #[test]
    fn bpb_root_cluster_at_max_accepted() {
        // Boundary: root_cluster == max_cluster is valid.
        let mut s = make_bpb();
        s[44..48].copy_from_slice(&129_023u32.to_le_bytes());
        let g = parse_bpb(&s).unwrap();
        assert_eq!(g.root_cluster, 129_023);
        assert_eq!(g.max_cluster(), 129_023);
    }

    #[test]
    fn bpb_minimum_fat32_cluster_count_accepted() {
        // Boundary: exactly FAT32_MIN_CLUSTERS (65525) clusters is the
        // smallest legal FAT32 volume per spec.
        // 65525 + 2 reserved = 65527 entries × 4 = 262_108 bytes; at
        // 512 bytes/sector that needs ≥ 512 sectors of FAT.
        // reserved=2, num_fats=2, sectors_per_fat=520 → data_start=1042.
        // total_sectors = 1042 + 65525 = 66_567; sectors_per_cluster=1.
        let mut s = make_bpb();
        s[14..16].copy_from_slice(&2u16.to_le_bytes());
        s[16] = 2;
        s[36..40].copy_from_slice(&520u32.to_le_bytes());
        s[32..36].copy_from_slice(&66_567u32.to_le_bytes());
        let g = parse_bpb(&s).unwrap();
        assert_eq!(g.cluster_count(), FAT32_MIN_CLUSTERS);
    }

    #[test]
    fn bpb_just_below_fat32_minimum_clusters_rejected() {
        // 65524 clusters: one below the FAT32 floor. Microsoft's spec
        // classifies this as FAT16, so a forged "FAT32   " type string
        // must not let it through. FAT sized for 65525+ entries.
        let mut s = make_bpb();
        s[14..16].copy_from_slice(&2u16.to_le_bytes());
        s[16] = 2;
        s[36..40].copy_from_slice(&520u32.to_le_bytes());
        s[32..36].copy_from_slice(&66_566u32.to_le_bytes()); // 65524 clusters
        assert_eq!(
            parse_bpb(&s),
            Err(Fat32Error::BelowFat32MinimumClusters {
                count: 65_524,
                minimum: FAT32_MIN_CLUSTERS,
            }),
        );
    }

    #[test]
    fn bpb_tiny_fat16_sized_volume_with_forged_type_rejected() {
        // The "trojan" case: a small volume that would naturally be
        // FAT16, with the fs_type field forged to claim FAT32. The
        // BelowFat32MinimumClusters check fires before the FAT-too-small
        // check (cluster_count is the FAT-type gate, FAT capacity is a
        // structural check that only matters for legitimately FAT32-sized
        // volumes).
        let mut s = make_bpb();
        s[14..16].copy_from_slice(&2u16.to_le_bytes());
        s[16] = 2;
        s[36..40].copy_from_slice(&50u32.to_le_bytes());
        s[32..36].copy_from_slice(&8_192u32.to_le_bytes()); // ~4 MiB volume
        assert!(matches!(
            parse_bpb(&s),
            Err(Fat32Error::BelowFat32MinimumClusters { .. }),
        ));
    }

    #[test]
    fn bpb_fat_too_small_for_cluster_count_rejected() {
        // The reviewer's case: a forged BPB with a huge data region
        // and a tiny FAT. cluster_count is comfortably above the FAT32
        // floor (so BelowFat32MinimumClusters doesn't fire), but the
        // FAT can't address every cluster.
        // Goal: 70_000 data clusters → need 70_002 entries × 4 = 280_008
        // bytes → ≥ 547 sectors of FAT. We deliberately set 100 sectors,
        // which holds 100 * 512 / 4 = 12_800 entries — far too few.
        let mut s = make_bpb();
        s[14..16].copy_from_slice(&2u16.to_le_bytes());
        s[16] = 2;
        s[36..40].copy_from_slice(&100u32.to_le_bytes());
        // total_sectors = reserved(2) + fats(2*100) + data(70_000) = 70_202.
        s[32..36].copy_from_slice(&70_202u32.to_le_bytes());
        assert_eq!(
            parse_bpb(&s),
            Err(Fat32Error::FatTooSmallForClusterCount {
                fat_entries: 12_800,
                required: 70_002,
            }),
        );
    }

    #[test]
    fn bpb_fat_exactly_large_enough_accepted() {
        // Boundary: FAT capacity == required_entries. cluster_count =
        // 65_534 needs 65_536 entries × 4 = 262_144 bytes = 512 sectors
        // of FAT exactly.
        let mut s = make_bpb();
        s[14..16].copy_from_slice(&2u16.to_le_bytes());
        s[16] = 2;
        s[36..40].copy_from_slice(&512u32.to_le_bytes());
        // total = reserved(2) + fats(2*512) + data(65_534) = 66_560.
        s[32..36].copy_from_slice(&66_560u32.to_le_bytes());
        let g = parse_bpb(&s).unwrap();
        assert_eq!(g.cluster_count(), 65_534);
    }

    // ---- cluster_to_sector ----

    #[test]
    fn cluster2_starts_at_data_region() {
        let g = parse_bpb(&make_bpb()).unwrap();
        assert_eq!(cluster_to_sector(2, &g), Some(g.data_start_sector));
    }

    #[test]
    fn cluster_arithmetic_with_multi_sector_clusters() {
        let mut s = make_bpb();
        s[13] = 8; // 8 sectors per cluster
        // Need ≥ 65525 * 8 = 524_200 data sectors for FAT32 minimum.
        s[32..36].copy_from_slice(&600_000u32.to_le_bytes());
        let g = parse_bpb(&s).unwrap();
        // cluster 2 starts at data_start; cluster 3 starts 8 sectors later.
        assert_eq!(cluster_to_sector(3, &g), Some(g.data_start_sector + 8));
        assert_eq!(cluster_to_sector(10, &g), Some(g.data_start_sector + 64));
    }

    #[test]
    fn cluster_below_2_returns_none() {
        let g = parse_bpb(&make_bpb()).unwrap();
        assert_eq!(cluster_to_sector(0, &g), None);
        assert_eq!(cluster_to_sector(1, &g), None);
    }

    #[test]
    fn cluster_above_max_returns_none() {
        let g = parse_bpb(&make_bpb()).unwrap();
        assert_eq!(cluster_to_sector(g.max_cluster() + 1, &g), None);
    }

    // ---- fat_entry_location ----

    #[test]
    fn fat_entry_location_cluster2_first_sector() {
        let g = parse_bpb(&make_bpb()).unwrap();
        // cluster 2, 4 bytes/entry → byte offset 8 → sector 0 of FAT, in-sector offset 8.
        assert_eq!(fat_entry_location(2, &g), Some((g.fat_start_sector, 8)));
    }

    #[test]
    fn fat_entry_location_crosses_sector_boundary() {
        let g = parse_bpb(&make_bpb()).unwrap();
        // Sector size 512, 4 bytes/entry → 128 entries per sector.
        // Cluster 128 → byte offset 512 → sector 1, in-sector 0.
        assert_eq!(fat_entry_location(128, &g), Some((g.fat_start_sector + 1, 0)));
        // Cluster 129 → byte offset 516 → sector 1, in-sector 4.
        assert_eq!(fat_entry_location(129, &g), Some((g.fat_start_sector + 1, 4)));
    }

    #[test]
    fn fat_entry_location_out_of_range_returns_none() {
        let g = parse_bpb(&make_bpb()).unwrap();
        assert_eq!(fat_entry_location(g.max_cluster() + 1, &g), None);
    }

    // ---- decode_fat_entry ----

    #[test]
    fn fat_entry_free() {
        let buf = [0u8; 16];
        assert_eq!(decode_fat_entry(&buf, 0), Some(FatEntry::Free));
    }

    #[test]
    fn fat_entry_used_decoded_to_next_cluster() {
        // Entry at cluster index 2 = 4 bytes at offset 8.
        // 0x0000_0042 = used, next cluster 0x42.
        let mut buf = [0u8; 16];
        buf[8..12].copy_from_slice(&0x0000_0042u32.to_le_bytes());
        assert_eq!(
            decode_fat_entry(&buf, 2),
            Some(FatEntry::Used { next: 0x42 }),
        );
    }

    #[test]
    fn fat_entry_top_4_bits_ignored() {
        // Spec says only low 28 bits are meaningful. Set top bits and
        // verify they're masked.
        let mut buf = [0u8; 16];
        buf[8..12].copy_from_slice(&0xF000_0042u32.to_le_bytes());
        assert_eq!(
            decode_fat_entry(&buf, 2),
            Some(FatEntry::Used { next: 0x42 }),
        );
    }

    #[test]
    fn fat_entry_end_of_chain() {
        // 0x0FFF_FFFF (and anything from 0x0FFF_FFF8 up) is EOC.
        let mut buf = [0u8; 8];
        buf[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        assert_eq!(decode_fat_entry(&buf, 1), Some(FatEntry::EndOfChain));
    }

    #[test]
    fn fat_entry_eoc_recommended_value() {
        // 0x0FFF_FFF8 is the spec-recommended EOC marker.
        let mut buf = [0u8; 8];
        buf[4..8].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
        assert_eq!(decode_fat_entry(&buf, 1), Some(FatEntry::EndOfChain));
    }

    #[test]
    fn fat_entry_bad_cluster() {
        let mut buf = [0u8; 8];
        buf[4..8].copy_from_slice(&0x0FFF_FFF7u32.to_le_bytes());
        assert_eq!(decode_fat_entry(&buf, 1), Some(FatEntry::Bad));
    }

    #[test]
    fn fat_entry_reserved_value_one() {
        // Cluster value 1 itself is reserved.
        let mut buf = [0u8; 8];
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(decode_fat_entry(&buf, 1), Some(FatEntry::Reserved));
    }

    #[test]
    fn fat_entry_reserved_high_range() {
        let mut buf = [0u8; 8];
        buf[4..8].copy_from_slice(&0x0FFF_FFF0u32.to_le_bytes());
        assert_eq!(decode_fat_entry(&buf, 1), Some(FatEntry::Reserved));
    }

    #[test]
    fn fat_entry_buffer_too_small_returns_none() {
        let buf = [0u8; 3];
        assert_eq!(decode_fat_entry(&buf, 0), None);
    }

    #[test]
    fn fat_entry_value_helper_matches_decode_fat_entry() {
        // decode_fat_entry_value should classify identically to
        // decode_fat_entry on the same raw u32.
        for raw in [0u32, 1, 2, 0xABCDEF, 0xFFFFFF7, 0xFFFFFFF, 0xF0000000] {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&raw.to_le_bytes());
            let from_bytes = decode_fat_entry(&buf, 0).unwrap();
            let from_value = decode_fat_entry_value(raw);
            assert_eq!(from_bytes, from_value, "raw=0x{:x}", raw);
        }
    }

    // ---- Directory entries ----

    /// Build a synthetic 32-byte dirent for a regular file.
    /// `name11` must already be the on-disk 8.3 form (11 bytes,
    /// space-padded). `cluster` is the first cluster (32-bit, split
    /// across the high/low fields). `size` is the file size in bytes.
    fn make_file_dirent(name11: &[u8; 11], cluster: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name11);
        e[11] = ATTR_ARCHIVE;
        let cluster_hi = (cluster >> 16) as u16;
        let cluster_lo = (cluster & 0xFFFF) as u16;
        e[20..22].copy_from_slice(&cluster_hi.to_le_bytes());
        e[26..28].copy_from_slice(&cluster_lo.to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    #[test]
    fn dirent_end_of_dir_when_first_byte_zero() {
        let e = [0u8; 32];
        assert_eq!(parse_dirent(&e), DirEntryStatus::EndOfDir);
    }

    #[test]
    fn dirent_deleted_when_first_byte_e5() {
        let mut e = [0u8; 32];
        e[0] = 0xE5;
        e[1] = b'A';
        e[11] = ATTR_ARCHIVE;
        assert_eq!(parse_dirent(&e), DirEntryStatus::Skip);
    }

    #[test]
    fn dirent_lfn_skipped() {
        let mut e = [0u8; 32];
        e[0] = 0x42; // LFN sequence number
        e[11] = ATTR_LFN_MASK; // 0x0F
        assert_eq!(parse_dirent(&e), DirEntryStatus::Skip);
    }

    #[test]
    fn dirent_volume_label_skipped() {
        let mut e = [0u8; 32];
        e[0] = b'L';
        e[11] = ATTR_VOLUME_ID;
        assert_eq!(parse_dirent(&e), DirEntryStatus::Skip);
    }

    #[test]
    fn dirent_regular_file_decoded() {
        let e = make_file_dirent(b"HELLO   TXT", 5, 17);
        match parse_dirent(&e) {
            DirEntryStatus::Entry(d) => {
                assert_eq!(&d.name, b"HELLO   TXT");
                assert_eq!(d.attr, ATTR_ARCHIVE);
                assert_eq!(d.first_cluster, 5);
                assert_eq!(d.size, 17);
                assert!(!d.is_directory());
            }
            other => panic!("expected Entry, got {:?}", other),
        }
    }

    #[test]
    fn dirent_directory_entry_recognized() {
        let mut e = make_file_dirent(b"SUBDIR     ", 7, 0);
        e[11] = ATTR_DIRECTORY;
        match parse_dirent(&e) {
            DirEntryStatus::Entry(d) => assert!(d.is_directory()),
            other => panic!("expected Entry, got {:?}", other),
        }
    }

    #[test]
    fn dirent_first_cluster_combines_hi_and_lo_fields() {
        let e = make_file_dirent(b"BIGFILE    ", 0x12345678, 0);
        match parse_dirent(&e) {
            DirEntryStatus::Entry(d) => assert_eq!(d.first_cluster, 0x12345678),
            other => panic!("expected Entry, got {:?}", other),
        }
    }

    #[test]
    fn dirent_first_byte_05_decoded_as_e5() {
        // Files whose real name starts with 0xE5 store 0x05 in the
        // first byte to avoid collision with the deleted marker.
        // 11-byte on-disk name: 0x05 + "BCDE" + 3 spaces + "TXT".
        let mut e = make_file_dirent(b"\x05BCDE   TXT", 2, 0);
        match parse_dirent(&e) {
            DirEntryStatus::Entry(d) => assert_eq!(d.name[0], 0xE5),
            other => panic!("expected Entry, got {:?}", other),
        }
        // Sanity: change byte 0 to a normal char and confirm no escape happens.
        e[0] = b'A';
        match parse_dirent(&e) {
            DirEntryStatus::Entry(d) => assert_eq!(d.name[0], b'A'),
            other => panic!("expected Entry, got {:?}", other),
        }
    }

    #[test]
    fn dirent_size_decoded() {
        let e = make_file_dirent(b"SIZED   TXT", 2, 0xCAFEBABE);
        match parse_dirent(&e) {
            DirEntryStatus::Entry(d) => assert_eq!(d.size, 0xCAFEBABE),
            other => panic!("expected Entry, got {:?}", other),
        }
    }

    // ---- Directory iteration ----

    /// Collect at most N dirents from `iter_dir`. Returns `(count, names)`.
    /// Avoids needing alloc — fixed-size array, asserts on overflow.
    fn collect_dirents<const N: usize>(buf: &[u8]) -> (usize, [[u8; 11]; N]) {
        let mut names = [[0u8; 11]; N];
        let mut count = 0;
        for e in iter_dir(buf) {
            assert!(count < N, "test bug: collect_dirents capacity {} exceeded", N);
            names[count] = e.name;
            count += 1;
        }
        (count, names)
    }

    #[test]
    fn dir_iter_yields_entries_then_stops_at_end_of_dir() {
        // Three slots: file, deleted, file, end-of-dir, garbage.
        let mut buf = [0u8; 32 * 5];
        buf[0..32].copy_from_slice(&make_file_dirent(b"FILE1   TXT", 2, 1));
        // Deleted slot
        buf[32] = 0xE5;
        buf[32 + 11] = ATTR_ARCHIVE;
        buf[64..96].copy_from_slice(&make_file_dirent(b"FILE2   TXT", 3, 2));
        // EndOfDir at offset 96 (byte 0 is 0)
        // Garbage past EOD shouldn't be seen.
        buf[128..160].copy_from_slice(&make_file_dirent(b"NEVER   TXT", 4, 3));

        let (count, names) = collect_dirents::<4>(&buf);
        assert_eq!(count, 2);
        assert_eq!(&names[0], b"FILE1   TXT");
        assert_eq!(&names[1], b"FILE2   TXT");
    }

    #[test]
    fn dir_iter_skips_lfn_and_volume_label() {
        let mut buf = [0u8; 32 * 4];
        // LFN entry
        buf[0] = 0x42;
        buf[11] = ATTR_LFN_MASK;
        // Volume label
        buf[32] = b'L';
        buf[32 + 11] = ATTR_VOLUME_ID;
        // Real file
        buf[64..96].copy_from_slice(&make_file_dirent(b"REAL    TXT", 2, 0));
        // EndOfDir at offset 96.
        let (count, names) = collect_dirents::<4>(&buf);
        assert_eq!(count, 1);
        assert_eq!(&names[0], b"REAL    TXT");
    }

    #[test]
    fn dir_iter_handles_partial_trailing_slot() {
        // Buffer ends mid-slot — iterator stops cleanly without panicking.
        let mut buf = [0u8; 50];
        buf[0..32].copy_from_slice(&make_file_dirent(b"ONLY    TXT", 2, 0));
        // Bytes 32..50 are a 18-byte fragment; not enough for another slot.
        let (count, _) = collect_dirents::<2>(&buf);
        assert_eq!(count, 1);
    }

    #[test]
    fn dir_iter_empty_buffer() {
        let buf = [0u8; 0];
        assert_eq!(iter_dir(&buf).count(), 0);
    }

    // ---- 8.3 path matching ----

    fn fake_entry(name11: &[u8; 11]) -> DirEntry {
        let mut name = [0u8; 11];
        name.copy_from_slice(name11);
        DirEntry { name, attr: 0, first_cluster: 0, size: 0 }
    }

    #[test]
    fn match_8_3_basic_lowercase_query() {
        let e = fake_entry(b"HELLO   TXT");
        assert!(match_8_3(b"hello.txt", &e));
    }

    #[test]
    fn match_8_3_uppercase_query() {
        let e = fake_entry(b"HELLO   TXT");
        assert!(match_8_3(b"HELLO.TXT", &e));
    }

    #[test]
    fn match_8_3_mixed_case_query() {
        let e = fake_entry(b"HELLO   TXT");
        assert!(match_8_3(b"HeLLo.TxT", &e));
    }

    #[test]
    fn match_8_3_no_extension() {
        let e = fake_entry(b"README     ");
        assert!(match_8_3(b"readme", &e));
    }

    #[test]
    fn match_8_3_short_name_padded_correctly() {
        // 5-char name padded with 3 spaces; no extension.
        let e = fake_entry(b"ABC        ");
        assert!(match_8_3(b"abc", &e));
    }

    #[test]
    fn match_8_3_extension_only_query_rejected() {
        // ".txt" has an empty name part. A malformed on-disk entry with
        // an all-spaces name and a real extension shouldn't be reachable
        // from a POSIX path lookup. Only "." and ".." are valid "empty
        // name" forms, and they're special-cased above.
        let e = fake_entry(b"        TXT");
        assert!(!match_8_3(b".txt", &e));
    }

    #[test]
    fn match_8_3_lone_dot_query_rejected_against_normal_entry() {
        // A bare "." should only match the on-disk "." dirent (which
        // has byte 0 = '.', byte 1 = ' '). It must not match a normal
        // entry whose name happens to start with a dot byte but isn't
        // the "." special directory.
        let e = fake_entry(b".OTHER     ");
        assert!(!match_8_3(b".", &e));
    }

    #[test]
    fn match_8_3_full_8_3_length() {
        // Maximum 8-char name + 3-char extension.
        let e = fake_entry(b"FILENAMEEXT");
        assert!(match_8_3(b"filename.ext", &e));
    }

    #[test]
    fn match_8_3_name_too_long_returns_false() {
        let e = fake_entry(b"OVERFLOWTXT");
        assert!(!match_8_3(b"overflowx.txt", &e));
    }

    #[test]
    fn match_8_3_extension_too_long_returns_false() {
        let e = fake_entry(b"FILE    TXT");
        assert!(!match_8_3(b"file.text", &e));
    }

    #[test]
    fn match_8_3_no_match_for_different_name() {
        let e = fake_entry(b"HELLO   TXT");
        assert!(!match_8_3(b"world.txt", &e));
    }

    #[test]
    fn match_8_3_dot_matches_dot_dirent() {
        // "." entry on disk is ".          " (dot + 10 spaces).
        let e = fake_entry(b".          ");
        assert!(match_8_3(b".", &e));
    }

    #[test]
    fn match_8_3_dotdot_matches_dotdot_dirent() {
        // ".." entry on disk is "..         " (two dots + 9 spaces).
        let e = fake_entry(b"..         ");
        assert!(match_8_3(b"..", &e));
    }
}
