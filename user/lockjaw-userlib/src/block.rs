/// Block device server loop, engine trait, and client wrapper.
///
/// Hardware block drivers implement `BlockEngine`. The reusable
/// `run_block_server()` handles IPC dispatch, buffer tracking, and
/// handle export. Clients use `BlockClient` for typed access.
///
/// Same architecture as display.rs: engine methods are synchronous.
/// For I/O, the engine internally submits + waits for completion
/// (e.g., virtqueue submit + IRQ wait). The server loop does not
/// multiplex IRQ notifications — that is the engine's responsibility.

pub use lockjaw_types::block::*;
use crate::syscall::*;
use crate::handle::{EndpointHandle, ReplyHandle, PageSetHandle};
use lockjaw_types::vmem::MapMemoryAttribute;

// ---------------------------------------------------------------------------
// BlockError
// ---------------------------------------------------------------------------

/// Errors returned by the block engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// I/O error from the device.
    IoError,
    /// Operation not supported by the device.
    Unsupported,
    /// Invalid parameters (out of range, bad alignment, etc).
    InvalidParameter,
    /// Buffer allocation failed (out of memory).
    AllocFailed,
    /// The buffer handle is not recognized.
    InvalidBuffer,
    /// IPC failure (client-side only).
    IpcFailed,
}

// ---------------------------------------------------------------------------
// BlockInfo
// ---------------------------------------------------------------------------

/// Block device info returned by CMD_GET_INFO.
#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    /// Total capacity in sectors.
    pub capacity_sectors: u64,
    /// Sector size in bytes (typically 512).
    pub sector_size: u64,
    /// MapMemoryAttribute the client MUST use when calling
    /// `sys_map_pages` on a buffer the engine allocates. emmc2's
    /// DMA-pool buffers are NC-only (kernel-enforced); virtio-blk's
    /// `sys_alloc_pages_contiguous` pages are cacheable Normal.
    /// Mapping with the wrong attribute fails at the kernel boundary
    /// — this contract makes the right choice queryable.
    pub buffer_attribute: MapMemoryAttribute,
}

// ---------------------------------------------------------------------------
// BufferTracker
// ---------------------------------------------------------------------------

const MAX_BUFFERS: usize = 8;

/// Maps server-assigned buffer_ids to engine-space PageSetHandles,
/// scoped by caller token for cross-client isolation.
///
/// Limitation: slots are only freed by explicit CMD_FREE_BUFFER.
/// If a client exits without freeing its buffers, the slots leak
/// permanently. Proper cleanup requires kernel death notifications
/// tied to caller-token lifecycle (future work).
struct BufferTracker {
    slots: [Option<BufferSlot>; MAX_BUFFERS],
    next_id: u32,
}

#[derive(Clone, Copy)]
struct BufferSlot {
    buffer_id: u32,
    caller_token: u64,
    engine_handle: PageSetHandle,
}

impl BufferTracker {
    const fn new() -> Self {
        Self { slots: [None; MAX_BUFFERS], next_id: 1 }
    }

    /// Assign a unique buffer_id and track the engine handle + caller.
    /// Returns the buffer_id, or None if full.
    fn track(&mut self, caller_token: u64, engine_handle: PageSetHandle) -> Option<u32> {
        for slot in self.slots.iter_mut() {
            if slot.is_none() {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                *slot = Some(BufferSlot { buffer_id: id, caller_token, engine_handle });
                return Some(id);
            }
        }
        None
    }

    /// Look up an engine handle by buffer_id, scoped to caller_token.
    /// Returns None if the buffer doesn't exist or belongs to a different caller.
    fn translate(&self, buffer_id: u32, caller_token: u64) -> Option<PageSetHandle> {
        for slot in self.slots.iter() {
            if let Some(s) = slot {
                if s.buffer_id == buffer_id && s.caller_token == caller_token {
                    return Some(s.engine_handle);
                }
            }
        }
        None
    }

    /// Remove a buffer by id, scoped to caller_token.
    /// Returns the engine handle for freeing, or None if not owned.
    fn remove(&mut self, buffer_id: u32, caller_token: u64) -> Option<PageSetHandle> {
        for slot in self.slots.iter_mut() {
            if let Some(s) = *slot {
                if s.buffer_id == buffer_id && s.caller_token == caller_token {
                    let handle = s.engine_handle;
                    *slot = None;
                    return Some(handle);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// BlockEngine trait
// ---------------------------------------------------------------------------

/// Block engine trait — implemented by hardware-specific drivers.
///
/// Engine methods are synchronous. For I/O operations (read/write),
/// the engine internally submits the request and waits for completion
/// (e.g., virtqueue submit + IRQ wait). The server loop does not
/// multiplex IRQ notifications.
///
/// Engine methods receive engine-space PageSetHandles (translated by
/// BufferTracker). The engine does not call sys_export_handle — the
/// server loop handles the export chain.
pub trait BlockEngine {
    /// Query device info (capacity, sector size).
    fn info(&self) -> BlockInfo;

    /// Allocate a DMA-capable buffer for `sector_count` sectors.
    /// Returns a PageSetHandle the server will export to the client.
    /// The engine must track the buffer internally for later read/write.
    fn alloc_buffer(&mut self, sector_count: u64)
        -> Result<PageSetHandle, BlockError>;

    /// Read `count` sectors starting at `sector` into `buffer`.
    /// Synchronous: submits I/O and blocks until completion.
    fn read(&mut self, sector: u64, count: u64, buffer: PageSetHandle)
        -> Result<(), BlockError>;

    /// Write `count` sectors starting at `sector` from `buffer`.
    /// Synchronous: submits I/O and blocks until completion.
    fn write(&mut self, sector: u64, count: u64, buffer: PageSetHandle)
        -> Result<(), BlockError>;

    /// Free a previously allocated buffer. Called on export failure.
    fn free_buffer(&mut self, buffer: PageSetHandle);
}

// ---------------------------------------------------------------------------
// run_block_server
// ---------------------------------------------------------------------------

/// Run the block device IPC server loop. Receives on `server_ep`,
/// decodes block commands, dispatches to the engine, handles buffer
/// export. Never returns.
pub fn run_block_server(
    engine: &mut impl BlockEngine,
    server_ep: EndpointHandle,
) -> ! {
    let mut buffers = BufferTracker::new();

    loop {
        let msg = match sys_receive_ret4(server_ep) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Query the kernel-assigned caller token for this message.
        // Scopes all buffer operations to the calling client.
        let caller = sys_query_caller_token();
        let cmd = msg[0];

        if cmd == CMD_GET_INFO {
            let info = engine.info();
            sys_reply(info.capacity_sectors, info.sector_size,
                info.buffer_attribute as u64, 0);

        } else if cmd == CMD_ALLOC_BUFFER {
            let sector_count = msg[1];

            let ps = match engine.alloc_buffer(sector_count) {
                Ok(h) => h,
                Err(_) => {
                    sys_reply(BLK_ERR_ALLOC, 0, 0, 0);
                    continue;
                }
            };

            // Track with caller token for cross-client isolation.
            let buffer_id = match buffers.track(caller, ps) {
                Some(id) => id,
                None => {
                    engine.free_buffer(ps);
                    sys_reply(BLK_ERR_ALLOC, 0, 0, 0);
                    continue;
                }
            };

            // Export PageSet into the caller's handle table.
            let client_ps_idx = match sys_export_handle(ps) {
                Ok(idx) => idx,
                Err(_) => {
                    buffers.remove(buffer_id, caller);
                    engine.free_buffer(ps);
                    sys_reply(BLK_ERR_ALLOC, 0, 0, 0);
                    continue;
                }
            };

            // Client gets: [status, pageset_handle, buffer_id, 0]
            sys_reply(BLK_OK, client_ps_idx, buffer_id as u64, 0);

        } else if cmd == CMD_READ {
            let sector = msg[1];
            let count = msg[2];
            let buffer_id = msg[3] as u32;

            let engine_buf = match buffers.translate(buffer_id, caller) {
                Some(h) => h,
                None => {
                    sys_reply(BLK_ERR_INVALID, 0, 0, 0);
                    continue;
                }
            };

            let status = match engine.read(sector, count, engine_buf) {
                Ok(()) => BLK_OK,
                Err(BlockError::IoError) => BLK_ERR_IO,
                Err(BlockError::Unsupported) => BLK_ERR_UNSUPPORTED,
                Err(_) => BLK_ERR_INVALID,
            };
            sys_reply(status, 0, 0, 0);

        } else if cmd == CMD_WRITE {
            let sector = msg[1];
            let count = msg[2];
            let buffer_id = msg[3] as u32;

            let engine_buf = match buffers.translate(buffer_id, caller) {
                Some(h) => h,
                None => {
                    sys_reply(BLK_ERR_INVALID, 0, 0, 0);
                    continue;
                }
            };

            let status = match engine.write(sector, count, engine_buf) {
                Ok(()) => BLK_OK,
                Err(BlockError::IoError) => BLK_ERR_IO,
                Err(BlockError::Unsupported) => BLK_ERR_UNSUPPORTED,
                Err(_) => BLK_ERR_INVALID,
            };
            sys_reply(status, 0, 0, 0);

        } else if cmd == CMD_FREE_BUFFER {
            let buffer_id = msg[1] as u32;

            let status = match buffers.remove(buffer_id, caller) {
                Some(ps) => {
                    engine.free_buffer(ps);
                    BLK_OK
                }
                None => BLK_ERR_INVALID,
            };
            sys_reply(status, 0, 0, 0);

        } else {
            sys_reply(BLK_ERR_UNSUPPORTED, 0, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// BlockClient
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// AllocatedBuffer (client-side)
// ---------------------------------------------------------------------------

/// Result of CMD_ALLOC_BUFFER — holds both the PageSetHandle (for
/// sys_map_pages) and the server-assigned buffer_id (for read/write).
#[derive(Clone, Copy, Debug)]
pub struct AllocatedBuffer {
    /// PageSet handle in the client's handle table (for mapping).
    pub pageset: PageSetHandle,
    /// Server-assigned buffer ID (for CMD_READ / CMD_WRITE / CMD_FREE_BUFFER).
    pub buffer_id: u32,
}

// ---------------------------------------------------------------------------
// BlockClient
// ---------------------------------------------------------------------------

/// Client-side block device wrapper. Hides IPC message packing behind
/// typed methods. Each method does one synchronous IPC call.
pub struct BlockClient {
    endpoint: EndpointHandle,
    reply: ReplyHandle,
}

impl BlockClient {
    pub fn new(endpoint: EndpointHandle, reply: ReplyHandle) -> Self {
        Self { endpoint, reply }
    }

    /// Query block device info (capacity, sector size, buffer
    /// attribute). The attribute MUST be used when calling
    /// `sys_map_pages` on buffers allocated via this client.
    pub fn get_info(&self) -> Result<BlockInfo, BlockError> {
        let msg = self.call(CMD_GET_INFO, 0, 0, 0)?;
        let buffer_attribute = MapMemoryAttribute::from_raw(msg[2])
            .ok_or(BlockError::InvalidParameter)?;
        Ok(BlockInfo {
            capacity_sectors: msg[0],
            sector_size: msg[1],
            buffer_attribute,
        })
    }

    /// Allocate a DMA buffer from the driver. Returns both the
    /// PageSetHandle (for sys_map_pages) and the server-assigned
    /// buffer_id (for read/write/free).
    pub fn alloc_buffer(&self, sector_count: u64) -> Result<AllocatedBuffer, BlockError> {
        let msg = self.call(CMD_ALLOC_BUFFER, sector_count, 0, 0)?;
        Self::decode_status(msg[0])?;
        Ok(AllocatedBuffer {
            pageset: PageSetHandle(msg[1]),
            buffer_id: msg[2] as u32,
        })
    }

    /// Read sectors into a previously allocated buffer.
    pub fn read(&self, sector: u64, count: u64, buffer_id: u32) -> Result<(), BlockError> {
        let msg = self.call(CMD_READ, sector, count, buffer_id as u64)?;
        Self::decode_status(msg[0])
    }

    /// Write sectors from a previously allocated buffer.
    pub fn write(&self, sector: u64, count: u64, buffer_id: u32) -> Result<(), BlockError> {
        let msg = self.call(CMD_WRITE, sector, count, buffer_id as u64)?;
        Self::decode_status(msg[0])
    }

    /// Free a previously allocated buffer.
    pub fn free_buffer(&self, buffer_id: u32) -> Result<(), BlockError> {
        let msg = self.call(CMD_FREE_BUFFER, buffer_id as u64, 0, 0)?;
        Self::decode_status(msg[0])
    }

    fn decode_status(status: u64) -> Result<(), BlockError> {
        match status {
            BLK_OK => Ok(()),
            BLK_ERR_IO => Err(BlockError::IoError),
            BLK_ERR_UNSUPPORTED => Err(BlockError::Unsupported),
            BLK_ERR_INVALID => Err(BlockError::InvalidParameter),
            BLK_ERR_ALLOC => Err(BlockError::AllocFailed),
            _ => Err(BlockError::InvalidParameter),
        }
    }

    fn call(&self, cmd: u64, a1: u64, a2: u64, a3: u64) -> Result<[u64; 4], BlockError> {
        sys_call_ret4(self.endpoint, self.reply, cmd, a1, a2, a3)
            .map_err(|_| BlockError::IpcFailed)
    }
}
