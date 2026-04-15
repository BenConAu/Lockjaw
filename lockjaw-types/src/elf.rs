/// Pure ELF64 parser for AArch64 executables.
/// No unsafe, no pointer casts — reads bytes directly.
/// Only handles ET_EXEC with PT_LOAD segments.

// ELF constants
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Maximum number of loadable segments.
pub const MAX_SEGMENTS: usize = 8;

/// A parsed loadable segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSegment {
    /// Virtual address where this segment is mapped.
    pub vaddr: u64,
    /// Offset into the ELF file where segment data begins.
    pub file_offset: u64,
    /// Size of segment data in the file (may be less than mem_size for BSS).
    pub file_size: u64,
    /// Size of the segment in memory (file_size + BSS zeroes).
    pub mem_size: u64,
    /// Whether this segment contains executable code.
    pub executable: bool,
    /// Whether this segment is writable.
    pub writable: bool,
}

/// Parsed ELF information.
#[derive(Debug)]
pub struct ElfInfo {
    /// Entry point virtual address.
    pub entry_point: u64,
    /// Loadable segments (PT_LOAD).
    pub segments: [LoadSegment; MAX_SEGMENTS],
    /// Number of valid entries in `segments`.
    pub segment_count: usize,
}

/// ELF parsing errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// File is too small to contain an ELF header.
    TooSmall,
    /// ELF magic number (0x7f ELF) not found.
    BadMagic,
    /// Not a 64-bit little-endian ELF.
    BadFormat,
    /// Not an executable (ET_EXEC).
    NotExec,
    /// Not targeting AArch64 (EM_AARCH64).
    NotAarch64,
    /// More than MAX_SEGMENTS loadable segments.
    TooManySegments,
}

/// Read a little-endian u16 from a byte slice at the given offset.
fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

/// Read a little-endian u32 from a byte slice at the given offset.
fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// Read a little-endian u64 from a byte slice at the given offset.
fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        data[off], data[off + 1], data[off + 2], data[off + 3],
        data[off + 4], data[off + 5], data[off + 6], data[off + 7],
    ])
}

/// Parse an ELF64 binary from a byte slice. No unsafe, no pointer casts.
pub fn parse_elf(data: &[u8]) -> Result<ElfInfo, ElfError> {
    // ELF64 header is 64 bytes
    if data.len() < 64 {
        return Err(ElfError::TooSmall);
    }

    // Check magic
    if data[0..4] != ELF_MAGIC {
        return Err(ElfError::BadMagic);
    }

    // Check class (64-bit) and endianness (little)
    if data[4] != ELFCLASS64 || data[5] != ELFDATA2LSB {
        return Err(ElfError::BadFormat);
    }

    // Check type and architecture
    let e_type = read_u16(data, 16);
    let e_machine = read_u16(data, 18);

    if e_type != ET_EXEC {
        return Err(ElfError::NotExec);
    }
    if e_machine != EM_AARCH64 {
        return Err(ElfError::NotAarch64);
    }

    let entry_point = read_u64(data, 24);
    let phoff = read_u64(data, 32) as usize;
    let phentsize = read_u16(data, 54) as usize;
    let phnum = read_u16(data, 56) as usize;

    let mut info = ElfInfo {
        entry_point,
        segments: [LoadSegment {
            vaddr: 0, file_offset: 0, file_size: 0, mem_size: 0,
            executable: false, writable: false,
        }; MAX_SEGMENTS],
        segment_count: 0,
    };

    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + phentsize > data.len() {
            break;
        }

        let p_type = read_u32(data, off);
        if p_type != PT_LOAD {
            continue;
        }

        if info.segment_count >= MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }

        let p_flags = read_u32(data, off + 4);
        let p_offset = read_u64(data, off + 8);
        let p_vaddr = read_u64(data, off + 16);
        let p_filesz = read_u64(data, off + 32);
        let p_memsz = read_u64(data, off + 40);

        info.segments[info.segment_count] = LoadSegment {
            vaddr: p_vaddr,
            file_offset: p_offset,
            file_size: p_filesz,
            mem_size: p_memsz,
            executable: (p_flags & PF_X) != 0,
            writable: (p_flags & PF_W) != 0,
        };
        info.segment_count += 1;
    }

    Ok(info)
}

/// Find a named section in an ELF64 binary and read a u64 from it.
/// Used to extract the .lockjaw_hash section for build hash verification.
/// Returns None if the section is not found or the ELF is malformed.
pub fn find_section_u64(data: &[u8], name: &str) -> Option<u64> {
    if data.len() < 64 || data[0..4] != ELF_MAGIC {
        return None;
    }

    let shoff = read_u64(data, 40) as usize;     // e_shoff
    let shentsize = read_u16(data, 58) as usize;  // e_shentsize
    let shnum = read_u16(data, 60) as usize;       // e_shnum
    let shstrndx = read_u16(data, 62) as usize;    // e_shstrndx

    if shoff == 0 || shnum == 0 || shentsize < 64 {
        return None;
    }

    // Find the section string table
    let strtab_off = shoff + shstrndx * shentsize;
    if strtab_off + shentsize > data.len() {
        return None;
    }
    let strtab_addr = read_u64(data, strtab_off + 24) as usize; // sh_offset
    let strtab_size = read_u64(data, strtab_off + 32) as usize; // sh_size

    if strtab_addr + strtab_size > data.len() {
        return None;
    }

    // Search each section header for the named section
    for i in 0..shnum {
        let sh_off = shoff + i * shentsize;
        if sh_off + shentsize > data.len() {
            break;
        }
        let name_idx = read_u32(data, sh_off) as usize; // sh_name
        let sec_offset = read_u64(data, sh_off + 24) as usize; // sh_offset
        let sec_size = read_u64(data, sh_off + 32) as usize; // sh_size

        // Compare section name from string table
        if strtab_addr + name_idx < data.len() {
            let name_start = strtab_addr + name_idx;
            let name_end = data[name_start..].iter().position(|&b| b == 0)
                .map(|p| name_start + p)
                .unwrap_or(data.len());
            let sec_name = &data[name_start..name_end];
            if sec_name == name.as_bytes() && sec_size >= 8 && sec_offset + 8 <= data.len() {
                return Some(read_u64(data, sec_offset));
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::vec;
    use std::vec::Vec;

    /// Build a minimal valid AArch64 ELF64 with the given segments.
    fn build_test_elf(entry: u64, segments: &[(u64, u64, u64, u64, u32)]) -> Vec<u8> {

        let phentsize: u16 = 56; // standard ELF64 program header size
        let phoff: u64 = 64;     // program headers start right after ELF header
        let phnum = segments.len() as u16;
        let total_size = 64 + (phnum as usize) * (phentsize as usize) + 4096;

        let mut elf = vec![0u8; total_size];

        // ELF header
        elf[0..4].copy_from_slice(&ELF_MAGIC);
        elf[4] = ELFCLASS64;              // 64-bit
        elf[5] = ELFDATA2LSB;             // little-endian
        elf[6] = 1;                        // ELF version
        elf[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        elf[18..20].copy_from_slice(&EM_AARCH64.to_le_bytes());
        elf[24..32].copy_from_slice(&entry.to_le_bytes());
        elf[32..40].copy_from_slice(&phoff.to_le_bytes());
        elf[54..56].copy_from_slice(&phentsize.to_le_bytes());
        elf[56..58].copy_from_slice(&phnum.to_le_bytes());

        // Program headers
        for (i, &(vaddr, offset, filesz, memsz, flags)) in segments.iter().enumerate() {
            let base = 64 + i * (phentsize as usize);
            // p_type = PT_LOAD
            elf[base..base+4].copy_from_slice(&PT_LOAD.to_le_bytes());
            // p_flags
            elf[base+4..base+8].copy_from_slice(&flags.to_le_bytes());
            // p_offset
            elf[base+8..base+16].copy_from_slice(&offset.to_le_bytes());
            // p_vaddr
            elf[base+16..base+24].copy_from_slice(&vaddr.to_le_bytes());
            // p_filesz
            elf[base+32..base+40].copy_from_slice(&filesz.to_le_bytes());
            // p_memsz
            elf[base+40..base+48].copy_from_slice(&memsz.to_le_bytes());
        }

        elf
    }

    #[test]
    fn valid_elf_with_two_segments() {
        let elf = build_test_elf(0x400000, &[
            (0x400000, 0x1000, 0x500, 0x500, PF_X),      // code: executable
            (0x401000, 0x2000, 0x100, 0x200, PF_W),       // data: writable, BSS
        ]);
        let info = parse_elf(&elf).unwrap();
        assert_eq!(info.entry_point, 0x400000);
        assert_eq!(info.segment_count, 2);
        assert_eq!(info.segments[0].vaddr, 0x400000);
        assert!(info.segments[0].executable);
        assert!(!info.segments[0].writable);
        assert_eq!(info.segments[1].vaddr, 0x401000);
        assert!(info.segments[1].writable);
        assert!(!info.segments[1].executable);
    }

    #[test]
    fn bss_segment_mem_larger_than_file() {
        let elf = build_test_elf(0x400000, &[
            (0x400000, 0x1000, 0x100, 0x1000, PF_W), // file=256, mem=4096 (BSS)
        ]);
        let info = parse_elf(&elf).unwrap();
        assert_eq!(info.segments[0].file_size, 0x100);
        assert_eq!(info.segments[0].mem_size, 0x1000);
    }

    #[test]
    fn too_small_file() {
        assert_eq!(parse_elf(&[0; 32]).unwrap_err(), ElfError::TooSmall);
    }

    #[test]
    fn bad_magic() {
        let mut elf = build_test_elf(0, &[]);
        elf[0] = 0x00; // corrupt magic
        assert_eq!(parse_elf(&elf).unwrap_err(), ElfError::BadMagic);
    }

    #[test]
    fn wrong_format_32bit() {
        let mut elf = build_test_elf(0, &[]);
        elf[4] = 1; // ELFCLASS32 instead of ELFCLASS64
        assert_eq!(parse_elf(&elf).unwrap_err(), ElfError::BadFormat);
    }

    #[test]
    fn wrong_architecture() {
        let mut elf = build_test_elf(0, &[]);
        elf[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        assert_eq!(parse_elf(&elf).unwrap_err(), ElfError::NotAarch64);
    }

    #[test]
    fn zero_segments() {
        let elf = build_test_elf(0x400000, &[]);
        let info = parse_elf(&elf).unwrap();
        assert_eq!(info.entry_point, 0x400000);
        assert_eq!(info.segment_count, 0);
    }
}
