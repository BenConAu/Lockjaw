//! Filesystem-server (and future fs-mux) IPC protocol.
//!
//! The wire format is the same shape as [`crate::block`]: command in
//! `msg[0]`, up to three argument words in `msg[1..3]`, response uses
//! the same 4-word layout. A pure [`dispatch`] decision function
//! decodes a request into an [`FsAction`] so each command can be
//! covered by host tests before any server-side code exists.
//!
//! Phase 1 scope: open / read / close. The per-handle read buffer is
//! allocated by the server at `open()` and exported to the client.
//! Cursor lives server-side per handle. Subsequent phases add lseek /
//! stat / fstat.
//!
//! Path passing uses **inline bytes** in the message (max 16 bytes)
//! rather than a client-supplied PageSet handle, because the kernel's
//! `sys_export_handle` only exports server→client (caller→server
//! handle import would need a new syscall). Paths longer than 16
//! bytes will need either a long-path command or the new syscall in a
//! future phase. For the 8.3 short-name scope, "/HELLO.TXT" (10 bytes)
//! and similar fit comfortably.

// ---------------------------------------------------------------------------
// Commands (msg[0])
// ---------------------------------------------------------------------------

/// Open a path, allocate a per-handle read buffer, return a server
/// handle.
///
/// Request:  msg = [FS_OPEN, len_packed, path_lo, path_hi]
///   len_packed: low byte = path_len (1..=16),
///               next byte = buffer_pages (1..=8)
///   path_lo, path_hi: path bytes packed little-endian (16 bytes total)
///
/// Reply: msg = [status, handle, buffer_pageset_idx, buffer_size_bytes]
///   status: FS_OK on success, FS_ERR_* on failure.
///   handle: server-assigned u32 (zero-extended).
///   buffer_pageset_idx: handle index in the caller's table for the
///     read buffer.
///   buffer_size_bytes: total bytes the buffer can hold per read.
pub const FS_OPEN: u64 = 1;

/// Read up to `len` bytes from the open file at the server-side
/// cursor into the per-handle buffer; advances the cursor.
///
/// Request:  msg = [FS_READ, handle, len, 0]
/// Reply:    msg = [status, bytes_returned, 0, 0]
///   `bytes_returned` < `len` indicates EOF or buffer cap.
pub const FS_READ: u64 = 2;

/// Close an open handle. Frees the server-side handle slot and the
/// per-handle read buffer. The caller should also unmap+close their
/// own copy of the buffer's PageSet.
///
/// Request:  msg = [FS_CLOSE, handle, 0, 0]
/// Reply:    msg = [status, 0, 0, 0]
pub const FS_CLOSE: u64 = 3;

// ---------------------------------------------------------------------------
// Status / error codes (returned in reply msg[0])
// ---------------------------------------------------------------------------

pub const FS_OK: u64 = 0;
/// Path does not exist (or final component is missing).
pub const FS_ERR_NOT_FOUND: u64 = 1;
/// Bad parameters (unknown command, invalid handle, malformed path).
pub const FS_ERR_INVALID: u64 = 2;
/// Server's open-file table is full.
pub const FS_ERR_TOO_MANY_OPEN: u64 = 3;
/// Underlying block-device read failed.
pub const FS_ERR_IO: u64 = 4;
/// `path_len` was 0 or > FS_MAX_INLINE_PATH.
pub const FS_ERR_PATH_TOO_LONG: u64 = 5;
/// `buffer_pages` was 0 or > FS_MAX_BUFFER_PAGES.
pub const FS_ERR_INVALID_BUFFER_PAGES: u64 = 6;
/// Out of memory (couldn't allocate the per-handle read buffer).
pub const FS_ERR_ALLOC: u64 = 7;
/// Path was a directory but caller asked to open it as a file
/// (or vice-versa). Phase E only opens files.
pub const FS_ERR_IS_DIRECTORY: u64 = 8;
/// Path required a directory but landed on a non-directory entry.
/// Fired when an intermediate component of a multi-component path
/// is a file ("/file.txt/foo"), or when a trailing slash ("/file.txt/")
/// asserts that the final entry must be a directory and it isn't.
pub const FS_ERR_NOT_DIRECTORY: u64 = 9;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Maximum path length packed inline in an FS_OPEN request.
/// Limited by IPC message size: msg[2..=3] = 16 bytes.
pub const FS_MAX_INLINE_PATH: usize = 16;

/// Maximum read-buffer size (in pages) for one open handle.
/// Sized at the maximum supported FAT32 cluster (32 KiB = 8 pages).
pub const FS_MAX_BUFFER_PAGES: u8 = 8;

// ---------------------------------------------------------------------------
// Dispatch decision (pure)
// ---------------------------------------------------------------------------

/// One received IPC message, in raw 4-word form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsRequest {
    pub cmd: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
}

/// What the server should do in response to one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsAction {
    /// Open the file at `path[..path_len]`, allocate a per-handle
    /// buffer of `buffer_pages` pages, reply with handle + buffer.
    Open { path: [u8; FS_MAX_INLINE_PATH], path_len: u8, buffer_pages: u8 },
    /// Read up to `len` bytes from `handle` into its buffer, reply
    /// with bytes_returned.
    Read { handle: u32, len: u32 },
    /// Close `handle` and free its buffer.
    Close { handle: u32 },
    /// Send `words` as the reply (typically a status code + zeros).
    /// Used for malformed requests and unknown commands.
    Reply { words: [u64; 4] },
}

/// Pure decision: classify one FS request into an [`FsAction`].
///
/// Reserved bits (bits above the spec'd width of each field, plus
/// any unused argument word) must be zero. This module owns the
/// request-shape contract; silent truncation of a malformed 64-bit
/// `handle` to 32 bits (which would happen with `as u32`) could
/// turn a bogus client message into a valid op on the wrong handle.
pub fn dispatch(req: &FsRequest) -> FsAction {
    match req.cmd {
        FS_OPEN => {
            // FS_OPEN's arg1 layout: byte 0 = path_len, byte 1 =
            // buffer_pages, bits 16..=63 reserved. Reserved bits must
            // be zero so future schema additions can claim them
            // without ambiguity.
            if req.arg1 >> 16 != 0 {
                return FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] };
            }
            decode_open(req)
        }
        FS_READ => {
            // handle and len are 32-bit fields; anything in the high
            // 32 bits is a protocol violation.
            if req.arg1 > u32::MAX as u64 {
                return FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] };
            }
            if req.arg2 > u32::MAX as u64 {
                return FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] };
            }
            if req.arg3 != 0 {
                return FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] };
            }
            FsAction::Read {
                handle: req.arg1 as u32,
                len: req.arg2 as u32,
            }
        }
        FS_CLOSE => {
            if req.arg1 > u32::MAX as u64 {
                return FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] };
            }
            if req.arg2 != 0 || req.arg3 != 0 {
                return FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] };
            }
            FsAction::Close { handle: req.arg1 as u32 }
        }
        _ => FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
    }
}

fn decode_open(req: &FsRequest) -> FsAction {
    let path_len_raw = (req.arg1 & 0xFF) as u8;
    let buffer_pages = ((req.arg1 >> 8) & 0xFF) as u8;

    if path_len_raw == 0 || path_len_raw as usize > FS_MAX_INLINE_PATH {
        return FsAction::Reply { words: [FS_ERR_PATH_TOO_LONG, 0, 0, 0] };
    }
    if buffer_pages == 0 || buffer_pages > FS_MAX_BUFFER_PAGES {
        return FsAction::Reply { words: [FS_ERR_INVALID_BUFFER_PAGES, 0, 0, 0] };
    }

    let mut path = [0u8; FS_MAX_INLINE_PATH];
    path[..8].copy_from_slice(&req.arg2.to_le_bytes());
    path[8..16].copy_from_slice(&req.arg3.to_le_bytes());

    FsAction::Open { path, path_len: path_len_raw, buffer_pages }
}

/// Pack a path into the (path_lo, path_hi) words used by FS_OPEN.
/// Returns the two words as `(arg2, arg3)`. Caller is responsible
/// for ensuring `path.len() <= FS_MAX_INLINE_PATH`.
pub fn pack_path(path: &[u8]) -> (u64, u64) {
    let mut buf = [0u8; FS_MAX_INLINE_PATH];
    let n = path.len().min(FS_MAX_INLINE_PATH);
    buf[..n].copy_from_slice(&path[..n]);
    let lo = u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
    let hi = u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]);
    (lo, hi)
}

/// Pack the (path_len, buffer_pages) header for FS_OPEN's `arg1`.
pub fn pack_open_header(path_len: u8, buffer_pages: u8) -> u64 {
    (buffer_pages as u64) << 8 | (path_len as u64)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cmd: u64, a1: u64, a2: u64, a3: u64) -> FsRequest {
        FsRequest { cmd, arg1: a1, arg2: a2, arg3: a3 }
    }

    fn open_req(path: &[u8], buffer_pages: u8) -> FsRequest {
        let (lo, hi) = pack_path(path);
        let header = pack_open_header(path.len() as u8, buffer_pages);
        req(FS_OPEN, header, lo, hi)
    }

    // ---- protocol constants ----

    #[test]
    fn command_constants_distinct() {
        let cmds = [FS_OPEN, FS_READ, FS_CLOSE];
        for i in 0..cmds.len() {
            for j in (i + 1)..cmds.len() {
                assert_ne!(cmds[i], cmds[j], "commands {} and {} collide", i, j);
            }
        }
    }

    #[test]
    fn error_constants_distinct() {
        let errs = [
            FS_OK,
            FS_ERR_NOT_FOUND,
            FS_ERR_INVALID,
            FS_ERR_TOO_MANY_OPEN,
            FS_ERR_IO,
            FS_ERR_PATH_TOO_LONG,
            FS_ERR_INVALID_BUFFER_PAGES,
            FS_ERR_ALLOC,
            FS_ERR_IS_DIRECTORY,
            FS_ERR_NOT_DIRECTORY,
        ];
        for i in 0..errs.len() {
            for j in (i + 1)..errs.len() {
                assert_ne!(errs[i], errs[j]);
            }
        }
    }

    #[test]
    fn pack_path_round_trips() {
        let src = b"/HELLO.TXT";
        let (lo, hi) = pack_path(src);
        let mut got = [0u8; FS_MAX_INLINE_PATH];
        got[..8].copy_from_slice(&lo.to_le_bytes());
        got[8..].copy_from_slice(&hi.to_le_bytes());
        assert_eq!(&got[..src.len()], src);
        // Trailing bytes are zero.
        for i in src.len()..FS_MAX_INLINE_PATH {
            assert_eq!(got[i], 0);
        }
    }

    #[test]
    fn pack_path_full_16_bytes() {
        let src = b"0123456789ABCDEF"; // exactly 16 bytes
        let (lo, hi) = pack_path(src);
        let mut got = [0u8; FS_MAX_INLINE_PATH];
        got[..8].copy_from_slice(&lo.to_le_bytes());
        got[8..].copy_from_slice(&hi.to_le_bytes());
        assert_eq!(&got, src);
    }

    // ---- dispatch decision ----

    #[test]
    fn open_decoded_with_path_and_buffer_pages() {
        let r = open_req(b"/HELLO.TXT", 1);
        match dispatch(&r) {
            FsAction::Open { path, path_len, buffer_pages } => {
                assert_eq!(path_len, 10);
                assert_eq!(buffer_pages, 1);
                assert_eq!(&path[..10], b"/HELLO.TXT");
            }
            other => panic!("expected Open, got {:?}", other),
        }
    }

    #[test]
    fn open_with_max_buffer_pages_accepted() {
        let r = open_req(b"/X", FS_MAX_BUFFER_PAGES);
        match dispatch(&r) {
            FsAction::Open { buffer_pages, .. } => assert_eq!(buffer_pages, FS_MAX_BUFFER_PAGES),
            other => panic!("expected Open, got {:?}", other),
        }
    }

    #[test]
    fn open_with_zero_path_len_rejected() {
        let r = req(FS_OPEN, pack_open_header(0, 1), 0, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_PATH_TOO_LONG, 0, 0, 0] },
        );
    }

    #[test]
    fn open_with_path_too_long_rejected() {
        // path_len = 17 (one more than FS_MAX_INLINE_PATH).
        let r = req(FS_OPEN, pack_open_header(17, 1), 0, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_PATH_TOO_LONG, 0, 0, 0] },
        );
    }

    #[test]
    fn open_with_zero_buffer_pages_rejected() {
        let r = open_req(b"/X", 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID_BUFFER_PAGES, 0, 0, 0] },
        );
    }

    #[test]
    fn open_with_too_many_buffer_pages_rejected() {
        let r = open_req(b"/X", FS_MAX_BUFFER_PAGES + 1);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID_BUFFER_PAGES, 0, 0, 0] },
        );
    }

    #[test]
    fn open_path_at_inline_boundary_accepted() {
        // path_len == FS_MAX_INLINE_PATH (16) is the maximum legal value.
        let r = open_req(b"0123456789ABCDEF", 1);
        match dispatch(&r) {
            FsAction::Open { path, path_len, .. } => {
                assert_eq!(path_len as usize, FS_MAX_INLINE_PATH);
                assert_eq!(&path, b"0123456789ABCDEF");
            }
            other => panic!("expected Open, got {:?}", other),
        }
    }

    #[test]
    fn read_decoded() {
        assert_eq!(
            dispatch(&req(FS_READ, 5, 1024, 0)),
            FsAction::Read { handle: 5, len: 1024 },
        );
    }

    #[test]
    fn close_decoded() {
        assert_eq!(
            dispatch(&req(FS_CLOSE, 7, 0, 0)),
            FsAction::Close { handle: 7 },
        );
    }

    #[test]
    fn unknown_command_returns_invalid() {
        assert_eq!(
            dispatch(&req(999, 0, 0, 0)),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
    }

    #[test]
    fn read_handle_high_bits_rejected() {
        // High bits in handle indicate a malformed message — silent
        // truncation could turn a bogus value into a valid op on the
        // wrong handle. The protocol contract is "reserved bits must
        // be zero".
        let r = req(FS_READ, 0xDEADBEEF_00000005, 100, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
    }

    #[test]
    fn read_len_high_bits_rejected() {
        let r = req(FS_READ, 5, 0x1_0000_0000, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
    }

    #[test]
    fn read_with_nonzero_arg3_rejected() {
        // arg3 is reserved; clients must zero it.
        let r = req(FS_READ, 5, 100, 1);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
    }

    #[test]
    fn read_at_u32_max_handle_accepted() {
        // Boundary: u32::MAX is the largest legal handle value.
        let r = req(FS_READ, u32::MAX as u64, 100, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Read { handle: u32::MAX, len: 100 },
        );
    }

    #[test]
    fn close_handle_high_bits_rejected() {
        let r = req(FS_CLOSE, 0xDEADBEEF_00000005, 0, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
    }

    #[test]
    fn close_with_nonzero_reserved_args_rejected() {
        // arg2 set
        let r = req(FS_CLOSE, 5, 1, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
        // arg3 set
        let r = req(FS_CLOSE, 5, 0, 1);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
    }

    #[test]
    fn open_with_high_bits_in_header_rejected() {
        // bits 16..=63 of arg1 are reserved.
        let mut header = pack_open_header(10, 1);
        header |= 1 << 16;
        let r = req(FS_OPEN, header, 0, 0);
        assert_eq!(
            dispatch(&r),
            FsAction::Reply { words: [FS_ERR_INVALID, 0, 0, 0] },
        );
    }

    #[test]
    fn open_path_zero_padded_in_unused_bytes() {
        // Short path: bytes after path_len must be zero in the decoded path.
        let r = open_req(b"X", 1);
        match dispatch(&r) {
            FsAction::Open { path, path_len, .. } => {
                assert_eq!(path_len, 1);
                assert_eq!(path[0], b'X');
                for i in 1..FS_MAX_INLINE_PATH {
                    assert_eq!(path[i], 0, "byte {} should be zero, was {:#x}", i, path[i]);
                }
            }
            other => panic!("expected Open, got {:?}", other),
        }
    }
}
