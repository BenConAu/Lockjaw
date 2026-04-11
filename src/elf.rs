/// Minimal ELF64 parser for AArch64 executables.
/// Only handles ET_EXEC with PT_LOAD segments.

/// ELF64 header (first 64 bytes of the file).
#[repr(C)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// ELF64 program header (one per segment).
#[repr(C)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

// ELF constants
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// A parsed loadable segment.
#[derive(Clone, Copy)]
pub struct LoadSegment {
    pub vaddr: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub mem_size: u64,
    pub executable: bool,
    pub writable: bool,
}

/// Parsed ELF information.
pub struct ElfInfo {
    pub entry_point: u64,
    pub segments: [LoadSegment; 8],
    pub segment_count: usize,
}

#[derive(Debug)]
pub enum ElfError {
    BadMagic,
    NotExec,
    NotAarch64,
    TooManySegments,
}

/// Parse an ELF64 binary from a byte slice.
pub fn parse_elf(data: &[u8]) -> Result<ElfInfo, ElfError> {
    if data.len() < core::mem::size_of::<Elf64Header>() {
        return Err(ElfError::BadMagic);
    }

    let header = unsafe { &*(data.as_ptr() as *const Elf64Header) };

    // Check magic
    if header.e_ident[0..4] != ELF_MAGIC {
        return Err(ElfError::BadMagic);
    }
    if header.e_ident[4] != ELFCLASS64 || header.e_ident[5] != ELFDATA2LSB {
        return Err(ElfError::BadMagic);
    }

    // Check type and architecture
    if header.e_type != ET_EXEC {
        return Err(ElfError::NotExec);
    }
    if header.e_machine != EM_AARCH64 {
        return Err(ElfError::NotAarch64);
    }

    // Parse program headers
    let mut info = ElfInfo {
        entry_point: header.e_entry,
        segments: [LoadSegment {
            vaddr: 0, file_offset: 0, file_size: 0, mem_size: 0,
            executable: false, writable: false,
        }; 8],
        segment_count: 0,
    };

    let phdr_start = header.e_phoff as usize;
    let phdr_size = header.e_phentsize as usize;

    for i in 0..header.e_phnum as usize {
        let offset = phdr_start + i * phdr_size;
        if offset + phdr_size > data.len() {
            break;
        }

        let phdr = unsafe { &*(data[offset..].as_ptr() as *const Elf64Phdr) };

        if phdr.p_type != PT_LOAD {
            continue;
        }

        if info.segment_count >= 8 {
            return Err(ElfError::TooManySegments);
        }

        info.segments[info.segment_count] = LoadSegment {
            vaddr: phdr.p_vaddr,
            file_offset: phdr.p_offset,
            file_size: phdr.p_filesz,
            mem_size: phdr.p_memsz,
            executable: (phdr.p_flags & PF_X) != 0,
            writable: (phdr.p_flags & PF_W) != 0,
        };
        info.segment_count += 1;
    }

    Ok(info)
}
