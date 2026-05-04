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
//! match dispatch(&DispatchInputs { nr, arg1, arg2 }) {
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

/// Sentinel syscall number for the shim's bootstrap handshake. No real
/// Linux syscall uses this value (it sits at the top of the u64 range).
/// The shim sends it as its very first call so the personality server
/// can hand back the shared buffer's PageSet and brk base.
pub const POSIX_INIT: u64 = 0xFFFF_FFFF_FFFF_FF00;

// ---------------------------------------------------------------------------
// Linux syscall numbers (aarch64, asm-generic/unistd.h).
// ---------------------------------------------------------------------------

pub const NR_IOCTL: u64 = 29;
pub const NR_WRITE: u64 = 64;
pub const NR_WRITEV: u64 = 66;
pub const NR_EXIT_GROUP: u64 = 94;
pub const NR_SET_TID_ADDRESS: u64 = 96;

// ---------------------------------------------------------------------------
// Linux errno values referenced by current dispatch arms.
// ---------------------------------------------------------------------------

pub const EBADF: u64 = 9;
pub const EINVAL: u64 = 22;
pub const ENOTTY: u64 = 25;
pub const ENOSYS: u64 = 38;

/// Encode a positive errno as the negative-errno return value Linux's
/// syscall ABI uses (musl reads `r0 < 0` as `-errno`).
pub const fn neg_errno(e: u64) -> u64 {
    (-(e as i64)) as u64
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Inputs to one dispatch decision. `arg1` and `arg2` are the next two
/// message words after the syscall number; their meaning depends on `nr`
/// (e.g. for `write`, `arg1` is fd and `arg2` is byte count in the
/// shared buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DispatchInputs {
    pub nr: u64,
    pub arg1: u64,
    pub arg2: u64,
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

        nr => Action::Unknown { nr },
    }
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

    fn inp(nr: u64, arg1: u64, arg2: u64) -> DispatchInputs {
        DispatchInputs { nr, arg1, arg2 }
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
}
