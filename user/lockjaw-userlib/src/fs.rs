//! Typed client for the fat32-server (and any future FS server).
//!
//! Hides IPC packing behind methods. Each method is one synchronous
//! `sys_call`. The wire protocol lives in `lockjaw_types::fs`; this
//! module just translates calls/replies to typed Rust signatures.

pub use lockjaw_types::fs::*;
use crate::syscall::*;
use crate::handle::{EndpointHandle, PageSetHandle, ReplyHandle};

// ---------------------------------------------------------------------------
// FsError — typed view of FS_ERR_* status codes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Invalid,
    TooManyOpen,
    Io,
    PathTooLong,
    InvalidBufferPages,
    AllocFailed,
    IsDirectory,
    NotDirectory,
    /// Server returned a status code we don't recognise.
    Unknown,
    /// IPC failed (transport-level).
    IpcFailed,
}

// ---------------------------------------------------------------------------
// OpenedFile — result of FsClient::open
// ---------------------------------------------------------------------------

/// Returned from [`FsClient::open`]. The client is responsible for
/// `sys_map_pages(pageset, ...)` to access the read buffer, and for
/// `sys_close_handle(pageset)` after `close()` to release its
/// reference (the server holds a separate reference).
#[derive(Clone, Copy, Debug)]
pub struct OpenedFile {
    /// Server-assigned handle for read/close.
    pub handle: u32,
    /// PageSet for the per-handle read buffer (in the client's table).
    pub pageset: PageSetHandle,
    /// Total bytes the buffer can hold per `read()`.
    pub buffer_size: u32,
}

// ---------------------------------------------------------------------------
// FsClient
// ---------------------------------------------------------------------------

pub struct FsClient {
    endpoint: EndpointHandle,
    reply: ReplyHandle,
}

impl FsClient {
    pub const fn new(endpoint: EndpointHandle, reply: ReplyHandle) -> Self {
        Self { endpoint, reply }
    }

    /// Open `path`, allocate a per-handle buffer of `buffer_pages`
    /// pages on the server side, and return the resulting handle and
    /// buffer PageSet.
    pub fn open(&self, path: &[u8], buffer_pages: u8) -> Result<OpenedFile, FsError> {
        if path.is_empty() || path.len() > FS_MAX_INLINE_PATH {
            return Err(FsError::PathTooLong);
        }
        if buffer_pages == 0 || buffer_pages > FS_MAX_BUFFER_PAGES {
            return Err(FsError::InvalidBufferPages);
        }
        let header = pack_open_header(path.len() as u8, buffer_pages);
        let (lo, hi) = pack_path(path);
        let msg = self.call(FS_OPEN, header, lo, hi)?;
        Self::decode_status(msg[0])?;
        Ok(OpenedFile {
            handle: msg[1] as u32,
            pageset: PageSetHandle(msg[2]),
            buffer_size: msg[3] as u32,
        })
    }

    /// Read up to `len` bytes from `handle` into its server-allocated
    /// buffer, advancing the cursor. Returns the number of bytes
    /// written into the buffer (may be less than `len` at EOF).
    pub fn read(&self, handle: u32, len: u32) -> Result<u32, FsError> {
        let msg = self.call(FS_READ, handle as u64, len as u64, 0)?;
        Self::decode_status(msg[0])?;
        Ok(msg[1] as u32)
    }

    /// Close `handle`. The server frees its side of the per-handle
    /// buffer; the client must still close its own PageSet handle.
    pub fn close(&self, handle: u32) -> Result<(), FsError> {
        let msg = self.call(FS_CLOSE, handle as u64, 0, 0)?;
        Self::decode_status(msg[0])
    }

    fn call(&self, cmd: u64, a1: u64, a2: u64, a3: u64) -> Result<[u64; 4], FsError> {
        sys_call_ret4(self.endpoint, self.reply, cmd, a1, a2, a3)
            .map_err(|_| FsError::IpcFailed)
    }

    fn decode_status(status: u64) -> Result<(), FsError> {
        match status {
            FS_OK => Ok(()),
            FS_ERR_NOT_FOUND => Err(FsError::NotFound),
            FS_ERR_INVALID => Err(FsError::Invalid),
            FS_ERR_TOO_MANY_OPEN => Err(FsError::TooManyOpen),
            FS_ERR_IO => Err(FsError::Io),
            FS_ERR_PATH_TOO_LONG => Err(FsError::PathTooLong),
            FS_ERR_INVALID_BUFFER_PAGES => Err(FsError::InvalidBufferPages),
            FS_ERR_ALLOC => Err(FsError::AllocFailed),
            FS_ERR_IS_DIRECTORY => Err(FsError::IsDirectory),
            FS_ERR_NOT_DIRECTORY => Err(FsError::NotDirectory),
            _ => Err(FsError::Unknown),
        }
    }
}
