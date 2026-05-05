//! POSIX file-descriptor table for the personality server.
//!
//! A POSIX fd is just an integer index into a per-process table.
//! Each entry maps to either:
//!
//! - **Stdio**: routes writes straight to the kernel UART via
//!   `sys_debug_puts` (no FS server in the loop). Used for fds 0/1/2.
//!   For Phase F that's stdin/stdout/stderr; reads from stdin return EOF.
//! - **File**: forwards to the FS server using a server-assigned
//!   `server_handle`. Cursor lives server-side, so the entry stores
//!   only the handle plus open flags (no offset).
//!
//! Pure: no IPC, no allocation. The personality server holds one
//! `FdTable` per POSIX process and consults it from the dispatch loop.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const FD_STDIN: u32 = 0;
pub const FD_STDOUT: u32 = 1;
pub const FD_STDERR: u32 = 2;

/// Maximum file descriptors per process. Phase F's hard cap. Real
/// Linux defaults to ~1024 but we'll bump this when programs ask.
pub const MAX_FDS: usize = 32;

/// Lowest fd `alloc()` will hand out. Reserves 0/1/2 for stdio.
pub const FIRST_USER_FD: u32 = 3;

// ---------------------------------------------------------------------------
// FdEntry
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdKind {
    /// fd writes go to the kernel UART; reads return EOF.
    Stdio,
    /// fd is backed by an FS-server handle (`server_handle`).
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdEntry {
    pub kind: FdKind,
    /// The FS-server handle this fd refers to. Unused for `Stdio`
    /// entries; set to 0 there.
    pub server_handle: u32,
    /// Open flags (O_RDONLY etc.) carried verbatim from `openat`.
    /// Phase F doesn't act on most of these yet but stores them so
    /// future writes / append-mode etc. can consult the entry.
    pub flags: u32,
}

impl FdEntry {
    pub const fn stdio() -> Self {
        Self { kind: FdKind::Stdio, server_handle: 0, flags: 0 }
    }
    pub const fn file(server_handle: u32, flags: u32) -> Self {
        Self { kind: FdKind::File, server_handle, flags }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum FdError {
    /// Fd index is out of range or the slot is empty.
    BadFd,
    /// Table has no free slots.
    TooManyOpen,
    /// Caller tried to close a stdio fd. Phase F protects them so
    /// musl can keep using fd 1/2; later phases can lift this if a
    /// program needs to dup over stdio.
    StdioClosed,
}

// ---------------------------------------------------------------------------
// FdTable
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct FdTable {
    slots: [Option<FdEntry>; MAX_FDS],
}

impl FdTable {
    /// Empty table. fd 0/1/2 are unset — call `with_stdio()` to
    /// initialize them.
    pub const fn new() -> Self {
        Self { slots: [None; MAX_FDS] }
    }

    /// New table with fd 0/1/2 pre-populated as Stdio entries.
    pub const fn with_stdio() -> Self {
        let mut t = Self::new();
        t.slots[FD_STDIN as usize] = Some(FdEntry::stdio());
        t.slots[FD_STDOUT as usize] = Some(FdEntry::stdio());
        t.slots[FD_STDERR as usize] = Some(FdEntry::stdio());
        t
    }

    /// Allocate the lowest free fd >= [`FIRST_USER_FD`] and store
    /// `entry` there. Returns the fd, or `TooManyOpen` if the table
    /// is full.
    pub fn alloc(&mut self, entry: FdEntry) -> Result<u32, FdError> {
        for fd in (FIRST_USER_FD as usize)..MAX_FDS {
            if self.slots[fd].is_none() {
                self.slots[fd] = Some(entry);
                return Ok(fd as u32);
            }
        }
        Err(FdError::TooManyOpen)
    }

    pub fn lookup(&self, fd: u32) -> Option<&FdEntry> {
        self.slots.get(fd as usize).and_then(|s| s.as_ref())
    }

    /// Remove the entry for `fd`, returning it. Stdio fds (0/1/2)
    /// are protected — closing them returns `StdioClosed`.
    pub fn close(&mut self, fd: u32) -> Result<FdEntry, FdError> {
        if fd < FIRST_USER_FD {
            // 0/1/2 are reserved.
            return Err(FdError::StdioClosed);
        }
        let slot = self.slots.get_mut(fd as usize).ok_or(FdError::BadFd)?;
        slot.take().ok_or(FdError::BadFd)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_table_has_no_entries() {
        let t = FdTable::new();
        for fd in 0..MAX_FDS {
            assert!(t.lookup(fd as u32).is_none());
        }
    }

    #[test]
    fn with_stdio_populates_fds_0_1_2() {
        let t = FdTable::with_stdio();
        assert_eq!(t.lookup(FD_STDIN).map(|e| e.kind), Some(FdKind::Stdio));
        assert_eq!(t.lookup(FD_STDOUT).map(|e| e.kind), Some(FdKind::Stdio));
        assert_eq!(t.lookup(FD_STDERR).map(|e| e.kind), Some(FdKind::Stdio));
        assert!(t.lookup(3).is_none());
    }

    #[test]
    fn alloc_returns_first_user_fd() {
        let mut t = FdTable::with_stdio();
        let fd = t.alloc(FdEntry::file(42, 0)).unwrap();
        assert_eq!(fd, FIRST_USER_FD);
        assert_eq!(t.lookup(fd).unwrap().server_handle, 42);
    }

    #[test]
    fn alloc_returns_lowest_free_fd() {
        let mut t = FdTable::with_stdio();
        let a = t.alloc(FdEntry::file(1, 0)).unwrap();
        let b = t.alloc(FdEntry::file(2, 0)).unwrap();
        assert_eq!(a, 3);
        assert_eq!(b, 4);
        // Close a and re-alloc — should reuse fd 3.
        let _ = t.close(a).unwrap();
        let c = t.alloc(FdEntry::file(99, 0)).unwrap();
        assert_eq!(c, 3);
        assert_eq!(t.lookup(c).unwrap().server_handle, 99);
    }

    #[test]
    fn alloc_full_returns_too_many_open() {
        let mut t = FdTable::with_stdio();
        for i in 0..(MAX_FDS - FIRST_USER_FD as usize) {
            t.alloc(FdEntry::file(i as u32, 0)).unwrap();
        }
        assert_eq!(t.alloc(FdEntry::file(0, 0)), Err(FdError::TooManyOpen));
    }

    #[test]
    fn close_user_fd_removes_entry() {
        let mut t = FdTable::with_stdio();
        let fd = t.alloc(FdEntry::file(7, 0)).unwrap();
        let removed = t.close(fd).unwrap();
        assert_eq!(removed.server_handle, 7);
        assert!(t.lookup(fd).is_none());
    }

    #[test]
    fn close_unset_fd_returns_bad_fd() {
        let mut t = FdTable::with_stdio();
        assert_eq!(t.close(5), Err(FdError::BadFd));
    }

    #[test]
    fn close_out_of_range_returns_bad_fd() {
        let mut t = FdTable::with_stdio();
        assert_eq!(t.close(MAX_FDS as u32 + 10), Err(FdError::BadFd));
    }

    #[test]
    fn close_stdio_rejected() {
        let mut t = FdTable::with_stdio();
        assert_eq!(t.close(FD_STDIN), Err(FdError::StdioClosed));
        assert_eq!(t.close(FD_STDOUT), Err(FdError::StdioClosed));
        assert_eq!(t.close(FD_STDERR), Err(FdError::StdioClosed));
        // Stdio entries still present.
        assert!(t.lookup(FD_STDOUT).is_some());
    }

    #[test]
    fn flags_round_trip() {
        let mut t = FdTable::with_stdio();
        let fd = t.alloc(FdEntry::file(3, 0xCAFE)).unwrap();
        assert_eq!(t.lookup(fd).unwrap().flags, 0xCAFE);
    }

    #[test]
    fn two_fds_can_share_server_handle() {
        // Future dup() correctness: two fds may legitimately point
        // at the same server handle (same open file description).
        // The table doesn't reject this — the server-side cursor
        // sharing is governed by the server, not by this table.
        let mut t = FdTable::with_stdio();
        let a = t.alloc(FdEntry::file(42, 0)).unwrap();
        let b = t.alloc(FdEntry::file(42, 0)).unwrap();
        assert_ne!(a, b);
        assert_eq!(t.lookup(a).unwrap().server_handle, 42);
        assert_eq!(t.lookup(b).unwrap().server_handle, 42);
    }
}
