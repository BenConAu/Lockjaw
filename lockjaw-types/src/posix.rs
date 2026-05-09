//! POSIX personality server: pure decision logic.
//!
//! The personality server in `user/posix-server` is the Linux syscall ABI
//! shim for musl. Most of its run loop is a `match` on an incoming syscall
//! number, deciding how to reply. This module pulls that decision out as a
//! pure function so each syscall's reply policy can be covered by host
//! tests — every new POSIX syscall lands as one [`Action`] arm + one host
//! test, before any kernel-side wiring exists.
//!
//! The personality server's loop becomes:
//!
//! ```ignore
//! match dispatch(&DispatchInputs { nr, arg1, arg2, arg3 }) {
//!     Action::Reply { words } => sys_reply(words[0], ..),
//!     Action::EmitFromShared { fd, len, then_reply } => {
//!         // copy len bytes from server's mapping of the shared buffer
//!         // to the kernel UART, then sys_reply(then_reply, ..)
//!     }
//!     Action::Exit => break,
//!     Action::PosixInit => { /* bootstrap shared buffer + reply */ }
//!     Action::Unknown { nr } => { log + sys_reply(neg_errno(ENOSYS), ..) }
//! }
//! ```

use crate::addr::PAGE_SIZE;
use crate::elf::ElfInfo;

/// Sentinel syscall number for the shim's bootstrap handshake. No real
/// Linux syscall uses this value (it sits at the top of the u64 range).
/// The shim sends it as its very first call so the personality server
/// can hand back the shared buffer's PageSet and brk base.
pub const POSIX_INIT: u64 = 0xFFFF_FFFF_FFFF_FF00;

// ---------------------------------------------------------------------------
// Linux syscall numbers (aarch64, asm-generic/unistd.h).
// ---------------------------------------------------------------------------

pub const NR_IOCTL: u64 = 29;
pub const NR_OPENAT: u64 = 56;
pub const NR_CLOSE: u64 = 57;
pub const NR_READ: u64 = 63;
pub const NR_WRITE: u64 = 64;
pub const NR_WRITEV: u64 = 66;
pub const NR_EXIT_GROUP: u64 = 94;
pub const NR_SET_TID_ADDRESS: u64 = 96;

// ---------------------------------------------------------------------------
// Linux errno values referenced by current dispatch arms.
// ---------------------------------------------------------------------------

pub const EBADF: u64 = 9;
pub const ENOMEM: u64 = 12;
pub const EINVAL: u64 = 22;
pub const EMFILE: u64 = 24;
pub const ENOTTY: u64 = 25;
pub const ENOSYS: u64 = 38;
pub const EIO: u64 = 5;
pub const ENOENT: u64 = 2;
pub const ENOTDIR: u64 = 20;
pub const EISDIR: u64 = 21;
pub const EROFS: u64 = 30;

// ---------------------------------------------------------------------------
// Linux open(2) flags (subset relevant to Phase 1 read-only validation).
// ---------------------------------------------------------------------------

/// Access-mode mask covering O_RDONLY / O_WRONLY / O_RDWR.
pub const O_ACCMODE: u64 = 0o3;
pub const O_RDONLY: u64 = 0o0;
pub const O_WRONLY: u64 = 0o1;
pub const O_RDWR: u64 = 0o2;

pub const O_CREAT: u64 = 0o100;
pub const O_EXCL: u64 = 0o200;
pub const O_TRUNC: u64 = 0o1000;
pub const O_APPEND: u64 = 0o2000;

/// Bitmask of open flags that require write capability. Phase 1's
/// read-only filesystem rejects any open() with these set.
pub const WRITE_OPEN_FLAGS: u64 = O_CREAT | O_EXCL | O_TRUNC | O_APPEND;

/// Encode a positive errno as the negative-errno return value Linux's
/// syscall ABI uses (musl reads `r0 < 0` as `-errno`).
pub const fn neg_errno(e: u64) -> u64 {
    (-(e as i64)) as u64
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Inputs to one dispatch decision. `arg1`, `arg2`, `arg3` are the
/// next three message words after the syscall number; their meaning
/// depends on `nr` (e.g. for `write`, `arg1` is fd and `arg2` is byte
/// count in the shared buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DispatchInputs {
    pub nr: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
}

/// What the personality server should do in response to one syscall.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Send `words` as the four reply registers. Most syscalls only set
    /// `words[0]` (the return value); the others are zero today, but the
    /// full tuple is kept so multi-value returns don't need an ABI break.
    Reply { words: [u64; 4] },
    /// Emit `len` bytes from the server's mapping of the shared buffer
    /// to the kernel UART (logging fd 1 or 2), then send `then_reply` as
    /// the reply value (the Linux convention is "bytes written").
    EmitFromShared { fd: u64, len: u64, then_reply: u64 },
    /// Child called `exit_group`; break the dispatch loop.
    Exit,
    /// Bootstrap handshake. The server allocates and exports the shared
    /// buffer, then sends the layout reply itself.
    PosixInit,
    /// Unrecognized syscall number. The consumer logs `nr` for
    /// debuggability and replies with `-ENOSYS`. Carried as a distinct
    /// variant (rather than a plain `Reply`) so the diagnostic survives
    /// the pure-dispatch boundary.
    Unknown { nr: u64 },
    /// Linux `openat`. Path bytes live in the client's shared buffer
    /// for `path_len` bytes (musl's shim writes them there before
    /// calling). `dirfd` is currently ignored — Phase F treats every
    /// path as resolved against the FS server's root regardless of
    /// dirfd. `flags` is the Linux open(2) flags word.
    FileOpen { dirfd: u64, path_len: u64, flags: u64 },
    /// Linux `read(fd, _, len)`. The server reads up to `len` bytes
    /// from the FS handle backing `fd` into the *client's* shared
    /// buffer (one copy via the FS-server's per-handle buffer), then
    /// replies with the byte count.
    FileRead { fd: u64, len: u64 },
    /// Linux `close(fd)`. Server forwards to the FS server, drops
    /// the FD entry, replies 0 on success or -EBADF on bad fd.
    FileClose { fd: u64 },
}

/// Pure decision: classify one received syscall message into an
/// [`Action`]. No side effects.
///
/// The personality server's loop is a single `match` over the result.
pub fn dispatch(inp: &DispatchInputs) -> Action {
    match inp.nr {
        POSIX_INIT => Action::PosixInit,

        NR_WRITE | NR_WRITEV => {
            // arg1 = fd, arg2 = byte count in the shared buffer.
            if inp.arg1 != 1 && inp.arg1 != 2 {
                return Action::Reply { words: [neg_errno(EBADF), 0, 0, 0] };
            }
            // The shim writes to the shared page first then calls the
            // server, so anything past PAGE_SIZE is a protocol bug. The
            // earlier draft panicked here; codifying as EINVAL is
            // testable and friendlier to malformed clients.
            if inp.arg2 > PAGE_SIZE {
                return Action::Reply { words: [neg_errno(EINVAL), 0, 0, 0] };
            }
            Action::EmitFromShared { fd: inp.arg1, len: inp.arg2, then_reply: inp.arg2 }
        }

        NR_EXIT_GROUP => Action::Exit,

        NR_SET_TID_ADDRESS => Action::Reply { words: [1, 0, 0, 0] },

        NR_IOCTL => Action::Reply { words: [neg_errno(ENOTTY), 0, 0, 0] },

        NR_OPENAT => {
            // arg1 = dirfd, arg2 = path_len (in shared buffer), arg3 = flags.
            // Path-length cap matches the shared buffer size (PAGE_SIZE).
            // Empty path is a protocol violation from the shim — reject
            // here rather than letting the FS server return NotFound.
            if inp.arg2 == 0 || inp.arg2 > PAGE_SIZE {
                return Action::Reply { words: [neg_errno(EINVAL), 0, 0, 0] };
            }
            // Phase 1 is read-only. Reject access modes other than
            // O_RDONLY and any write-touching flag. Writing programs
            // should see EROFS up-front rather than getting an fd
            // they can't actually write to.
            if inp.arg3 & O_ACCMODE != O_RDONLY {
                return Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] };
            }
            if inp.arg3 & WRITE_OPEN_FLAGS != 0 {
                return Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] };
            }
            Action::FileOpen { dirfd: inp.arg1, path_len: inp.arg2, flags: inp.arg3 }
        }

        NR_READ => {
            // arg1 = fd, arg2 = byte count to read into the shared buffer.
            // len cap matches the shared buffer size.
            if inp.arg2 > PAGE_SIZE {
                return Action::Reply { words: [neg_errno(EINVAL), 0, 0, 0] };
            }
            Action::FileRead { fd: inp.arg1, len: inp.arg2 }
        }

        NR_CLOSE => Action::FileClose { fd: inp.arg1 },

        nr => Action::Unknown { nr },
    }
}

// ---------------------------------------------------------------------------
// Dynamic VA layout (where to put the shared buffer + brk after the ELF)
// ---------------------------------------------------------------------------

/// Resolved VA layout for a POSIX child: where the ELF image ends, where
/// the personality server's shared buffer goes, where the heap (brk)
/// starts, and where mmap regions get carved from. All addresses are
/// page-aligned.
#[derive(Debug, PartialEq, Eq)]
pub struct PosixVaLayout {
    /// First page-aligned VA strictly after every loaded segment. Doubles
    /// as the lower bound for placing dynamic regions.
    pub elf_end_aligned: u64,
    /// VA at which the child sees the personality server's shared buffer
    /// (one page above `elf_end_aligned`, leaving a guard page).
    pub child_shared_va: u64,
    /// First VA musl can grow brk into (one page above the shared buffer).
    pub brk_base: u64,
    /// Base VA of the mmap region. Anonymous private mmap allocations
    /// from the posix-server's bump allocator carve upward from here.
    /// Must satisfy `mmap_base > user_stack_base + stack_pages*PAGE_SIZE
    /// + PAGE_SIZE` (above stack + one guard page).
    pub mmap_base: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// `seg.vaddr + seg.mem_size` overflows u64 for some segment. The
    /// ELF parser shouldn't produce this, but the layout function owns
    /// the policy of refusing rather than wrapping silently.
    SegmentEndOverflow { seg_idx: usize },
    /// Page-aligning the largest segment end overflows u64.
    AlignOverflow,
    /// `brk_base` would overlap (or sit above) the user stack. The
    /// personality server has no place to put the heap if this fires.
    OverflowsStack { brk_base: u64, user_stack_base: u64 },
    /// `mmap_base` does not sit strictly above the user stack + one
    /// guard page. Phase 2.0 invariant: a fixed POSIX_MMAP_BASE is
    /// safe only because brk is confined below USER_STACK_BASE
    /// (existing OverflowsStack check) AND mmap is above the stack
    /// top (this check). If a future ABI change moves the stack up,
    /// POSIX_MMAP_BASE must move with it; this error catches the
    /// regression at boot rather than at use.
    MmapBelowStack {
        mmap_base: u64,
        user_stack_base: u64,
        stack_pages: u32,
    },
}

/// Compute where to place the shared buffer, brk, and mmap base for a
/// POSIX child.
///
/// Layout (in ascending VA):
///
/// ```text
/// [ELF segments] elf_end_aligned [guard] child_shared_va [shared] brk_base
///   [...] user_stack_base [stack: stack_pages] [guard] mmap_base [...]
/// ```
///
/// Pure: no I/O, no syscalls. The personality server uses the result to
/// allocate and map the shared page, seed musl's brk pointer, and pass
/// `mmap_base` to the shim via POSIX_INIT.
pub fn compute_va_layout(
    info: &ElfInfo,
    user_stack_base: u64,
    stack_pages: u32,
    mmap_base: u64,
) -> Result<PosixVaLayout, LayoutError> {
    let mut elf_end: u64 = 0;
    for i in 0..info.segment_count {
        let seg = &info.segments[i];
        let seg_end = seg
            .vaddr
            .checked_add(seg.mem_size)
            .ok_or(LayoutError::SegmentEndOverflow { seg_idx: i })?;
        if seg_end > elf_end {
            elf_end = seg_end;
        }
    }
    let elf_end_aligned = elf_end
        .checked_add(PAGE_SIZE - 1)
        .ok_or(LayoutError::AlignOverflow)?
        & !(PAGE_SIZE - 1);

    let child_shared_va = elf_end_aligned
        .checked_add(PAGE_SIZE)
        .ok_or(LayoutError::AlignOverflow)?;
    let brk_base = child_shared_va
        .checked_add(PAGE_SIZE)
        .ok_or(LayoutError::AlignOverflow)?;

    if brk_base >= user_stack_base {
        return Err(LayoutError::OverflowsStack { brk_base, user_stack_base });
    }

    // mmap_base must sit strictly above the stack top + one guard page.
    // The kernel maps stack_pages at user_stack_base, occupying VAs
    // [user_stack_base, user_stack_base + stack_pages*PAGE_SIZE). One
    // page above that for a guard, then mmap_base.
    let stack_top = user_stack_base
        .checked_add((stack_pages as u64) * PAGE_SIZE)
        .ok_or(LayoutError::AlignOverflow)?;
    let stack_top_plus_guard = stack_top
        .checked_add(PAGE_SIZE)
        .ok_or(LayoutError::AlignOverflow)?;
    if mmap_base < stack_top_plus_guard {
        return Err(LayoutError::MmapBelowStack {
            mmap_base,
            user_stack_base,
            stack_pages,
        });
    }

    Ok(PosixVaLayout {
        elf_end_aligned,
        child_shared_va,
        brk_base,
        mmap_base,
    })
}

// ---------------------------------------------------------------------------
// Linux initial stack layout (musl _start ABI)
// ---------------------------------------------------------------------------

// auxv entry types musl actually reads at startup.
pub const AT_NULL: u64 = 0;
pub const AT_PAGESZ: u64 = 6;
pub const AT_RANDOM: u64 = 25;

// Field offsets in the stack layout. Each u64 is 8 bytes. The auxv
// section is three (type, val) pairs — 48 bytes — followed by the
// 16-byte AT_RANDOM seed and the argv0 string.
const OFF_ARGC: usize = 0;
const OFF_ARGV0_PTR: usize = 8;
const OFF_ARGV_TERM: usize = 16;
const OFF_ENVP_TERM: usize = 24;
const OFF_AUXV: usize = 32;
const AUXV_BYTES: usize = 3 * 16; // PAGESZ, RANDOM, NULL — each 16 bytes
const OFF_RANDOM: usize = OFF_AUXV + AUXV_BYTES; // 80
const OFF_ARGV0_STR: usize = OFF_RANDOM + 16;     // 96

/// Minimum bytes required to hold the fixed part of the layout (argc,
/// argv pointer, argv/envp terminators, three auxv entries, AT_RANDOM
/// seed). Add `argv0.len()` for the variable-length argv0 string.
pub const STACK_LAYOUT_FIXED_BYTES: usize = OFF_ARGV0_STR;

/// Inputs to [`write_linux_stack`].
pub struct StackInputs<'a> {
    /// argv[0] bytes, including the trailing null. musl's `_start`
    /// reads strings via the argv pointers and stops at the first
    /// `\0`; we require the caller to include it explicitly so a
    /// missing terminator can't slip past as a silent off-by-one.
    pub argv0: &'a [u8],
    /// 16 bytes of entropy exposed via AT_RANDOM. musl seeds its TLS
    /// canary and stack guard from here. Phase 0 uses a fixed seed;
    /// later phases should pass real entropy.
    pub random_seed: [u8; 16],
    /// VA the *child* sees this page mapped at. argv[0] and the
    /// AT_RANDOM auxv pointer are absolute addresses into this page,
    /// so they must reflect the child's view, not whatever temp VA
    /// the personality server used to write the layout.
    pub child_layout_va: u64,
    /// Value to publish in the AT_PAGESZ auxv entry.
    pub page_size: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StackError {
    /// `buf` is too small to hold the fixed layout plus `argv0`.
    /// `required` is `STACK_LAYOUT_FIXED_BYTES + argv0.len()`.
    BufferTooSmall { required: usize, given: usize },
    /// `argv0` is empty or its last byte isn't `\0`. Every other
    /// terminator in the layout (argv, envp, auxv) is written by this
    /// function, but argv0's trailing null is part of the caller's
    /// string and easy to forget.
    Argv0NotNullTerminated,
}

/// Write the Linux initial-stack layout musl's `_start` reads from SP
/// into `buf`. Pure: no I/O, no syscalls. Caller maps a stack page,
/// passes a `&mut [u8]` view of it, and copies the page to its final
/// location.
///
/// Layout (byte offsets, all u64 fields little-endian):
///
/// ```text
/// +0  argc = 1
/// +8  argv[0] pointer            -> child_layout_va + 96
/// +16 argv terminator (0)
/// +24 envp terminator (0)
/// +32 auxv[0].a_type = AT_PAGESZ
/// +40 auxv[0].a_val  = page_size
/// +48 auxv[1].a_type = AT_RANDOM
/// +56 auxv[1].a_val  -> child_layout_va + 80
/// +64 auxv[2].a_type = AT_NULL
/// +72 auxv[2].a_val  = 0
/// +80 random_seed[0..16]
/// +96 argv0 bytes (including the trailing \0)
/// ```
pub fn write_linux_stack(buf: &mut [u8], inp: &StackInputs) -> Result<(), StackError> {
    if inp.argv0.is_empty() || *inp.argv0.last().unwrap() != 0 {
        return Err(StackError::Argv0NotNullTerminated);
    }
    let required = STACK_LAYOUT_FIXED_BYTES + inp.argv0.len();
    if buf.len() < required {
        return Err(StackError::BufferTooSmall { required, given: buf.len() });
    }

    let argv0_ptr = inp.child_layout_va + OFF_ARGV0_STR as u64;
    let random_ptr = inp.child_layout_va + OFF_RANDOM as u64;

    write_u64(buf, OFF_ARGC, 1);
    write_u64(buf, OFF_ARGV0_PTR, argv0_ptr);
    write_u64(buf, OFF_ARGV_TERM, 0);
    write_u64(buf, OFF_ENVP_TERM, 0);
    write_u64(buf, OFF_AUXV, AT_PAGESZ);
    write_u64(buf, OFF_AUXV + 8, inp.page_size);
    write_u64(buf, OFF_AUXV + 16, AT_RANDOM);
    write_u64(buf, OFF_AUXV + 24, random_ptr);
    write_u64(buf, OFF_AUXV + 32, AT_NULL);
    write_u64(buf, OFF_AUXV + 40, 0);

    buf[OFF_RANDOM..OFF_RANDOM + 16].copy_from_slice(&inp.random_seed);
    buf[OFF_ARGV0_STR..OFF_ARGV0_STR + inp.argv0.len()].copy_from_slice(inp.argv0);

    Ok(())
}

fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::{LoadSegment, MAX_SEGMENTS};

    fn inp(nr: u64, arg1: u64, arg2: u64) -> DispatchInputs {
        DispatchInputs { nr, arg1, arg2, arg3: 0 }
    }

    fn inp4(nr: u64, arg1: u64, arg2: u64, arg3: u64) -> DispatchInputs {
        DispatchInputs { nr, arg1, arg2, arg3 }
    }

    fn read_u64(buf: &[u8], offset: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buf[offset..offset + 8]);
        u64::from_le_bytes(bytes)
    }

    fn make_inputs(argv0: &[u8]) -> StackInputs<'_> {
        StackInputs {
            argv0,
            random_seed: *b"random_seed_1234",
            child_layout_va: 0x1000_0000,
            page_size: 4096,
        }
    }

    /// Build an `ElfInfo` directly from a list of (vaddr, mem_size)
    /// pairs. file_offset/file_size aren't read by compute_va_layout
    /// so they're left at zero.
    fn make_info(segs: &[(u64, u64)]) -> ElfInfo {
        let mut segments = [LoadSegment {
            vaddr: 0,
            file_offset: 0,
            file_size: 0,
            mem_size: 0,
            executable: false,
            writable: false,
        }; MAX_SEGMENTS];
        for (i, (vaddr, mem_size)) in segs.iter().enumerate() {
            segments[i] = LoadSegment {
                vaddr: *vaddr,
                file_offset: 0,
                file_size: 0,
                mem_size: *mem_size,
                executable: false,
                writable: false,
            };
        }
        ElfInfo {
            entry_point: 0,
            segments,
            segment_count: segs.len(),
        }
    }

    // Use a stack base well above any test ELF.
    const TEST_STACK_BASE: u64 = 0x0000_0040_0000_0000;
    // Tests use a 4-page stack and an mmap base safely above stack + guard.
    const TEST_STACK_PAGES: u32 = 4;
    const TEST_MMAP_BASE: u64 = 0x0000_0050_0000_0000;

    #[test]
    fn write_fd1_emits_from_shared() {
        assert_eq!(
            dispatch(&inp(NR_WRITE, 1, 13)),
            Action::EmitFromShared { fd: 1, len: 13, then_reply: 13 },
        );
    }

    #[test]
    fn write_fd2_emits_from_shared() {
        assert_eq!(
            dispatch(&inp(NR_WRITE, 2, 64)),
            Action::EmitFromShared { fd: 2, len: 64, then_reply: 64 },
        );
    }

    #[test]
    fn write_fd0_is_ebadf() {
        // stdin isn't writable — same EBADF path as any other unknown fd.
        assert_eq!(
            dispatch(&inp(NR_WRITE, 0, 8)),
            Action::Reply { words: [neg_errno(EBADF), 0, 0, 0] },
        );
    }

    #[test]
    fn write_fd3_is_ebadf() {
        assert_eq!(
            dispatch(&inp(NR_WRITE, 3, 8)),
            Action::Reply { words: [neg_errno(EBADF), 0, 0, 0] },
        );
    }

    #[test]
    fn write_zero_length_on_valid_fd_still_emits() {
        // Linux semantics: write(fd, _, 0) returns 0 without error.
        assert_eq!(
            dispatch(&inp(NR_WRITE, 1, 0)),
            Action::EmitFromShared { fd: 1, len: 0, then_reply: 0 },
        );
    }

    #[test]
    fn write_at_page_size_boundary_ok() {
        assert_eq!(
            dispatch(&inp(NR_WRITE, 1, PAGE_SIZE)),
            Action::EmitFromShared { fd: 1, len: PAGE_SIZE, then_reply: PAGE_SIZE },
        );
    }

    #[test]
    fn write_past_page_size_is_einval() {
        // Codifies the policy: shim bug returns EINVAL, never panics here.
        assert_eq!(
            dispatch(&inp(NR_WRITE, 1, PAGE_SIZE + 1)),
            Action::Reply { words: [neg_errno(EINVAL), 0, 0, 0] },
        );
    }

    #[test]
    fn writev_same_shape_as_write() {
        assert_eq!(
            dispatch(&inp(NR_WRITEV, 1, 7)),
            Action::EmitFromShared { fd: 1, len: 7, then_reply: 7 },
        );
    }

    #[test]
    fn writev_bad_fd_is_ebadf() {
        assert_eq!(
            dispatch(&inp(NR_WRITEV, 99, 4)),
            Action::Reply { words: [neg_errno(EBADF), 0, 0, 0] },
        );
    }

    #[test]
    fn exit_group_returns_exit() {
        assert_eq!(dispatch(&inp(NR_EXIT_GROUP, 0, 0)), Action::Exit);
    }

    #[test]
    fn set_tid_address_returns_one() {
        assert_eq!(
            dispatch(&inp(NR_SET_TID_ADDRESS, 0, 0)),
            Action::Reply { words: [1, 0, 0, 0] },
        );
    }

    #[test]
    fn ioctl_returns_enotty() {
        assert_eq!(
            dispatch(&inp(NR_IOCTL, 0, 0)),
            Action::Reply { words: [neg_errno(ENOTTY), 0, 0, 0] },
        );
    }

    #[test]
    fn unknown_syscall_returns_unknown_variant() {
        // Distinct from Reply so consumer can log nr for diagnostics.
        assert_eq!(dispatch(&inp(999, 0, 0)), Action::Unknown { nr: 999 });
    }

    #[test]
    fn unknown_syscall_preserves_nr() {
        // Different unknown nrs surface distinctly so the log shows the
        // actual offending number.
        assert_eq!(dispatch(&inp(0xDEAD, 0, 0)), Action::Unknown { nr: 0xDEAD });
    }

    #[test]
    fn posix_init_sentinel_returns_posix_init() {
        assert_eq!(dispatch(&inp(POSIX_INIT, 0, 0)), Action::PosixInit);
    }

    #[test]
    fn neg_errno_two_complements_correctly() {
        // EBADF=9 should encode as -9 in u64 two's-complement.
        assert_eq!(neg_errno(EBADF), (-9i64) as u64);
        assert_eq!(neg_errno(ENOSYS), (-38i64) as u64);
        assert_eq!(neg_errno(EINVAL), (-22i64) as u64);
    }

    // ---- Phase 1 file syscalls (openat / read / close) ----

    #[test]
    fn openat_decoded_with_dirfd_pathlen_flags() {
        let action = dispatch(&inp4(NR_OPENAT, /*dirfd=*/0xFFFFFFFFFFFFFF9C, /*path_len=*/10, /*flags=*/0));
        assert_eq!(
            action,
            Action::FileOpen { dirfd: 0xFFFFFFFFFFFFFF9C, path_len: 10, flags: 0 },
        );
    }

    #[test]
    fn openat_zero_path_len_is_einval() {
        // Empty path is a protocol violation from the shim — caught here
        // rather than passed to the FS server.
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 0, 0)),
            Action::Reply { words: [neg_errno(EINVAL), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_path_too_long_is_einval() {
        // path_len > PAGE_SIZE can't fit in the shim's shared buffer.
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, PAGE_SIZE + 1, 0)),
            Action::Reply { words: [neg_errno(EINVAL), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_flags_plumbed_through() {
        // High bits (above O_ACCMODE / WRITE_OPEN_FLAGS) are harmless
        // and pass through. We use a bit value that doesn't collide
        // with any of the rejected flags.
        let harmless = 1u64 << 20; // O_CLOEXEC = 0o2000000 — same kind of bit
        let action = dispatch(&inp4(NR_OPENAT, 0, 5, harmless));
        assert_eq!(
            action,
            Action::FileOpen { dirfd: 0, path_len: 5, flags: harmless },
        );
    }

    #[test]
    fn openat_o_wronly_is_erofs() {
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 5, O_WRONLY)),
            Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_o_rdwr_is_erofs() {
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 5, O_RDWR)),
            Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_o_creat_is_erofs() {
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 5, O_CREAT)),
            Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_o_trunc_is_erofs() {
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 5, O_TRUNC)),
            Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_o_append_is_erofs() {
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 5, O_APPEND)),
            Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_o_excl_is_erofs() {
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 5, O_EXCL)),
            Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_rdonly_with_creat_still_erofs() {
        // O_RDONLY by itself is fine; combined with O_CREAT it isn't.
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 5, O_RDONLY | O_CREAT)),
            Action::Reply { words: [neg_errno(EROFS), 0, 0, 0] },
        );
    }

    #[test]
    fn openat_pure_rdonly_zero_flags_accepted() {
        // O_RDONLY == 0 is the most common shape from fopen("r").
        assert_eq!(
            dispatch(&inp4(NR_OPENAT, 0, 10, O_RDONLY)),
            Action::FileOpen { dirfd: 0, path_len: 10, flags: O_RDONLY },
        );
    }

    #[test]
    fn read_decoded_with_fd_and_len() {
        assert_eq!(
            dispatch(&inp(NR_READ, 3, 1024)),
            Action::FileRead { fd: 3, len: 1024 },
        );
    }

    #[test]
    fn read_zero_length_decoded() {
        // read(_, 0) is legal; returns 0 bytes.
        assert_eq!(
            dispatch(&inp(NR_READ, 3, 0)),
            Action::FileRead { fd: 3, len: 0 },
        );
    }

    #[test]
    fn read_at_page_size_boundary_ok() {
        assert_eq!(
            dispatch(&inp(NR_READ, 3, PAGE_SIZE)),
            Action::FileRead { fd: 3, len: PAGE_SIZE },
        );
    }

    #[test]
    fn read_past_page_size_is_einval() {
        // Same shared-buffer cap as write.
        assert_eq!(
            dispatch(&inp(NR_READ, 3, PAGE_SIZE + 1)),
            Action::Reply { words: [neg_errno(EINVAL), 0, 0, 0] },
        );
    }

    #[test]
    fn close_decoded_with_fd() {
        assert_eq!(
            dispatch(&inp(NR_CLOSE, 5, 0)),
            Action::FileClose { fd: 5 },
        );
    }

    #[test]
    fn close_doesnt_validate_fd_at_dispatch() {
        // The dispatch layer doesn't know which fds are open — it just
        // emits the action and the runtime decides. fd 0 (stdin) is a
        // valid close request shape; the runtime side rejects it.
        assert_eq!(
            dispatch(&inp(NR_CLOSE, 0, 0)),
            Action::FileClose { fd: 0 },
        );
    }

    // ---- write_linux_stack ----

    #[test]
    fn stack_argc_at_offset_zero() {
        let mut buf = [0xAAu8; 256];
        write_linux_stack(&mut buf, &make_inputs(b"hello\0")).unwrap();
        assert_eq!(read_u64(&buf, 0), 1);
    }

    #[test]
    fn stack_argv0_pointer_targets_in_page_string() {
        let mut buf = [0u8; 256];
        let inputs = make_inputs(b"hello\0");
        write_linux_stack(&mut buf, &inputs).unwrap();
        // argv[0] pointer must point at where the string lives in the
        // child's view (child_layout_va + 96).
        assert_eq!(read_u64(&buf, 8), inputs.child_layout_va + 96);
    }

    #[test]
    fn stack_argv_terminator_present() {
        let mut buf = [0xAAu8; 256];
        write_linux_stack(&mut buf, &make_inputs(b"x\0")).unwrap();
        assert_eq!(read_u64(&buf, 16), 0);
    }

    #[test]
    fn stack_envp_terminator_present() {
        let mut buf = [0xAAu8; 256];
        write_linux_stack(&mut buf, &make_inputs(b"x\0")).unwrap();
        assert_eq!(read_u64(&buf, 24), 0);
    }

    #[test]
    fn stack_at_pagesz_carries_input_value() {
        let mut buf = [0u8; 256];
        let inputs = StackInputs {
            argv0: b"hello\0",
            random_seed: [0; 16],
            child_layout_va: 0x1000_0000,
            page_size: 16384, // unusual value to prove it's plumbed through
        };
        write_linux_stack(&mut buf, &inputs).unwrap();
        assert_eq!(read_u64(&buf, 32), AT_PAGESZ);
        assert_eq!(read_u64(&buf, 40), 16384);
    }

    #[test]
    fn stack_at_random_pointer_targets_seed() {
        let mut buf = [0u8; 256];
        let inputs = make_inputs(b"x\0");
        write_linux_stack(&mut buf, &inputs).unwrap();
        assert_eq!(read_u64(&buf, 48), AT_RANDOM);
        // Pointer must reference the seed bytes at +80 in the child view.
        assert_eq!(read_u64(&buf, 56), inputs.child_layout_va + 80);
    }

    #[test]
    fn stack_at_null_terminator_present() {
        let mut buf = [0xAAu8; 256];
        write_linux_stack(&mut buf, &make_inputs(b"x\0")).unwrap();
        assert_eq!(read_u64(&buf, 64), AT_NULL);
        assert_eq!(read_u64(&buf, 72), 0);
    }

    #[test]
    fn stack_random_seed_copied_verbatim() {
        let mut buf = [0u8; 256];
        let seed = *b"0123456789abcdef";
        write_linux_stack(&mut buf, &StackInputs {
            argv0: b"x\0",
            random_seed: seed,
            child_layout_va: 0x1000_0000,
            page_size: 4096,
        }).unwrap();
        assert_eq!(&buf[80..96], &seed);
    }

    #[test]
    fn stack_argv0_string_copied_verbatim_with_null() {
        let mut buf = [0u8; 256];
        write_linux_stack(&mut buf, &make_inputs(b"hello\0")).unwrap();
        assert_eq!(&buf[96..96 + 6], b"hello\0");
    }

    #[test]
    fn stack_long_argv0_extends_required_size() {
        // 32-byte argv0 — required = 96 + 32 = 128.
        let argv0 = b"this-is-a-longer-program-name\x00\x00\x00";
        let mut buf = [0u8; 128];
        write_linux_stack(&mut buf, &make_inputs(argv0)).unwrap();
        assert_eq!(&buf[96..96 + argv0.len()], argv0);
    }

    #[test]
    fn stack_buffer_too_small_returns_error() {
        let mut buf = [0u8; 100]; // need 96 + 6 = 102
        assert_eq!(
            write_linux_stack(&mut buf, &make_inputs(b"hello\0")),
            Err(StackError::BufferTooSmall { required: 102, given: 100 }),
        );
    }

    #[test]
    fn stack_buffer_exactly_required_size_ok() {
        let mut buf = [0u8; 102];
        assert!(write_linux_stack(&mut buf, &make_inputs(b"hello\0")).is_ok());
    }

    #[test]
    fn stack_argv0_missing_null_terminator_rejected() {
        let mut buf = [0u8; 256];
        assert_eq!(
            write_linux_stack(&mut buf, &make_inputs(b"hello")),
            Err(StackError::Argv0NotNullTerminated),
        );
    }

    #[test]
    fn stack_argv0_empty_rejected() {
        let mut buf = [0u8; 256];
        assert_eq!(
            write_linux_stack(&mut buf, &make_inputs(b"")),
            Err(StackError::Argv0NotNullTerminated),
        );
    }

    // ---- compute_va_layout ----

    #[test]
    fn layout_typical_elf_text_data_bss() {
        // text at 0x40_0000 (1 page), data at 0x41_0000 (1 page),
        // bss extending data's mem_size to 2 pages.
        let info = make_info(&[
            (0x40_0000, 0x1000),
            (0x41_0000, 0x2000),
        ]);
        let l = compute_va_layout(&info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE).unwrap();
        // Largest seg_end is 0x41_0000 + 0x2000 = 0x412000 (already page aligned).
        assert_eq!(l.elf_end_aligned, 0x41_2000);
        assert_eq!(l.child_shared_va, 0x41_3000);
        assert_eq!(l.brk_base, 0x41_4000);
    }

    #[test]
    fn layout_unaligned_segment_end_rounds_up() {
        // Segment ends at 0x40_1234 → aligned to 0x40_2000.
        let info = make_info(&[(0x40_0000, 0x1234)]);
        let l = compute_va_layout(&info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE).unwrap();
        assert_eq!(l.elf_end_aligned, 0x40_2000);
        assert_eq!(l.child_shared_va, 0x40_3000);
        assert_eq!(l.brk_base, 0x40_4000);
    }

    #[test]
    fn layout_results_are_page_aligned() {
        let info = make_info(&[(0x40_0001, 0x789)]);
        let l = compute_va_layout(&info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE).unwrap();
        let mask = PAGE_SIZE - 1;
        assert_eq!(l.elf_end_aligned & mask, 0);
        assert_eq!(l.child_shared_va & mask, 0);
        assert_eq!(l.brk_base & mask, 0);
    }

    #[test]
    fn layout_empty_segments_starts_at_origin() {
        // No segments — elf_end_aligned is 0; layout still places the
        // shared buffer and brk in the lowest two non-null pages.
        let info = make_info(&[]);
        let l = compute_va_layout(&info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE).unwrap();
        assert_eq!(l.elf_end_aligned, 0);
        assert_eq!(l.child_shared_va, PAGE_SIZE);
        assert_eq!(l.brk_base, 2 * PAGE_SIZE);
    }

    #[test]
    fn layout_picks_max_over_segments_not_last() {
        // Out-of-order segments — max isn't the final entry.
        let info = make_info(&[
            (0x42_0000, 0x1000),  // ends at 0x42_1000 — the highest
            (0x40_0000, 0x1000),  // ends at 0x40_1000
            (0x41_0000, 0x1000),  // ends at 0x41_1000
        ]);
        let l = compute_va_layout(&info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE).unwrap();
        assert_eq!(l.elf_end_aligned, 0x42_1000);
    }

    #[test]
    fn layout_brk_overlapping_stack_is_error() {
        // ELF ends just below the stack base; brk_base would step over.
        let stack_base = 0x40_3000;
        let info = make_info(&[(0x40_0000, 0x2000)]); // ends 0x40_2000
        // elf_end_aligned = 0x40_2000, child = 0x40_3000, brk = 0x40_4000.
        // brk (0x40_4000) >= stack (0x40_3000) → error.
        assert_eq!(
            compute_va_layout(&info, stack_base, TEST_STACK_PAGES, TEST_MMAP_BASE),
            Err(LayoutError::OverflowsStack {
                brk_base: 0x40_4000,
                user_stack_base: stack_base,
            }),
        );
    }

    #[test]
    fn layout_brk_equal_to_stack_is_error() {
        // brk_base == user_stack_base is also a conflict — the heap
        // would start at the very first page of the stack region.
        let stack_base = 0x40_4000;
        let info = make_info(&[(0x40_0000, 0x2000)]); // brk_base = 0x40_4000
        assert!(matches!(
            compute_va_layout(&info, stack_base, TEST_STACK_PAGES, TEST_MMAP_BASE),
            Err(LayoutError::OverflowsStack { .. }),
        ));
    }

    #[test]
    fn layout_segment_end_overflow_is_error() {
        // vaddr + mem_size overflows u64.
        let info = make_info(&[(u64::MAX - 100, 200)]);
        assert_eq!(
            compute_va_layout(&info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE),
            Err(LayoutError::SegmentEndOverflow { seg_idx: 0 }),
        );
    }

    #[test]
    fn layout_align_overflow_is_error() {
        // seg_end = u64::MAX exactly — seg_end + (PAGE_SIZE - 1) overflows.
        // (Synthetic case; real ELFs would never get here, but the
        // bounds policy catches it cleanly.)
        let info = make_info(&[(u64::MAX & !(PAGE_SIZE - 1), PAGE_SIZE - 1)]);
        // seg_end = u64::MAX, +PAGE_SIZE-1 overflows.
        assert_eq!(
            compute_va_layout(&info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE),
            Err(LayoutError::AlignOverflow),
        );
    }

    // ---- mmap_base layout invariant (Phase 2.0) ----

    #[test]
    fn layout_returns_mmap_base() {
        let info = make_info(&[(0x40_0000, 0x1000)]);
        let l = compute_va_layout(
            &info, TEST_STACK_BASE, TEST_STACK_PAGES, TEST_MMAP_BASE,
        ).unwrap();
        assert_eq!(l.mmap_base, TEST_MMAP_BASE);
    }

    #[test]
    fn layout_real_posix_constants() {
        // The actual POSIX_MMAP_BASE = 16 MiB sits well above
        // USER_STACK_BASE = 8 MiB + 4-page stack + guard. Sanity check.
        use crate::constants::{POSIX_MMAP_BASE, USER_STACK_BASE};
        let info = make_info(&[(0x40_0000, 0x1000)]);
        let l = compute_va_layout(
            &info, USER_STACK_BASE, 4, POSIX_MMAP_BASE,
        ).unwrap();
        assert_eq!(l.mmap_base, POSIX_MMAP_BASE);
    }

    #[test]
    fn layout_mmap_base_below_stack_top_is_error() {
        // mmap_base lands inside the stack region — must reject.
        let info = make_info(&[(0x40_0000, 0x1000)]);
        let stack_pages = 4u32;
        // Stack occupies [TEST_STACK_BASE, TEST_STACK_BASE + 4*PAGE_SIZE).
        // mmap_base equal to the first stack page would be an overlap.
        let bad_mmap = TEST_STACK_BASE + 2 * PAGE_SIZE;
        assert_eq!(
            compute_va_layout(&info, TEST_STACK_BASE, stack_pages, bad_mmap),
            Err(LayoutError::MmapBelowStack {
                mmap_base: bad_mmap,
                user_stack_base: TEST_STACK_BASE,
                stack_pages,
            }),
        );
    }

    #[test]
    fn layout_mmap_base_at_stack_top_no_guard_is_error() {
        // mmap_base = stack_top exactly (no guard page) — must reject.
        let info = make_info(&[(0x40_0000, 0x1000)]);
        let stack_pages = 4u32;
        let bad_mmap = TEST_STACK_BASE + (stack_pages as u64) * PAGE_SIZE;
        assert!(matches!(
            compute_va_layout(&info, TEST_STACK_BASE, stack_pages, bad_mmap),
            Err(LayoutError::MmapBelowStack { .. }),
        ));
    }

    #[test]
    fn layout_mmap_base_one_guard_page_above_stack_ok() {
        // mmap_base = stack_top + PAGE_SIZE — minimum legal placement.
        let info = make_info(&[(0x40_0000, 0x1000)]);
        let stack_pages = 4u32;
        let ok_mmap = TEST_STACK_BASE + (stack_pages as u64) * PAGE_SIZE + PAGE_SIZE;
        let l = compute_va_layout(
            &info, TEST_STACK_BASE, stack_pages, ok_mmap,
        ).unwrap();
        assert_eq!(l.mmap_base, ok_mmap);
    }
}
