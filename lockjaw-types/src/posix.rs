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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(nr: u64, arg1: u64, arg2: u64) -> DispatchInputs {
        DispatchInputs { nr, arg1, arg2 }
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
}
