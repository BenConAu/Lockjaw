/// Block device IPC protocol constants.
///
/// Follows the same pattern as display.rs: commands are u64 values
/// in msg[0], with up to 3 argument words in msg[1..3]. Responses
/// use the same 4-word layout.

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Query block device info.
/// Request:  msg = [CMD_GET_INFO, 0, 0, 0]
/// Response: msg = [capacity_sectors, sector_size, 0, 0]
pub const CMD_GET_INFO: u64 = 1;

/// Allocate a physically contiguous DMA buffer.
/// Request:  msg = [CMD_ALLOC_BUFFER, sector_count, 0, 0]
/// Response: msg = [status, pageset_handle, buffer_id, 0]
///   status: BLK_OK on success, BLK_ERR_* on failure.
///   pageset_handle: exported PageSet for the client to sys_map_pages.
///   buffer_id: server-assigned opaque ID for CMD_READ/CMD_WRITE.
///
/// Handle 0 is a valid handle table index, so status must be checked
/// separately — do not use pageset_handle == 0 as a failure sentinel.
///
/// The driver allocates physically contiguous pages via
/// sys_alloc_pages_contiguous and exports the handle to the client.
/// Contiguity is required because the virtqueue data descriptor uses
/// a single (phys_addr, len) pair — no scatter-gather in phase 1.
/// sector_count > 1 is only valid if the contiguous allocation
/// succeeds for ceil(sector_count * 512 / 4096) pages.
pub const CMD_ALLOC_BUFFER: u64 = 2;

/// Read sectors from disk into a previously allocated buffer.
/// Request:  msg = [CMD_READ, sector, count, buffer_id]
/// Response: msg = [status, 0, 0, 0]
///   buffer_id: the server-assigned ID from CMD_ALLOC_BUFFER.
///   status: 0 = ok, nonzero = error
pub const CMD_READ: u64 = 3;

/// Write sectors from a previously allocated buffer to disk.
/// Request:  msg = [CMD_WRITE, sector, count, buffer_id]
/// Response: msg = [status, 0, 0, 0]
///   buffer_id: the server-assigned ID from CMD_ALLOC_BUFFER.
///   status: 0 = ok, nonzero = error
pub const CMD_WRITE: u64 = 4;

/// Free a previously allocated buffer.
/// Request:  msg = [CMD_FREE_BUFFER, buffer_id, 0, 0]
/// Response: msg = [status, 0, 0, 0]
///   status: 0 = ok, BLK_ERR_INVALID = unknown buffer_id
pub const CMD_FREE_BUFFER: u64 = 5;

/// Standard sector size (bytes).
pub const SECTOR_SIZE: u64 = 512;

// ---------------------------------------------------------------------------
// Error codes (returned in response msg[0] for CMD_READ / CMD_WRITE)
// ---------------------------------------------------------------------------

pub const BLK_OK:              u64 = 0;
pub const BLK_ERR_IO:          u64 = 1;
pub const BLK_ERR_UNSUPPORTED: u64 = 2;
pub const BLK_ERR_INVALID:     u64 = 3; // bad parameters
pub const BLK_ERR_ALLOC:       u64 = 4; // allocation failed (OOM or table full)

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_constants_distinct() {
        let cmds = [CMD_GET_INFO, CMD_ALLOC_BUFFER, CMD_READ, CMD_WRITE, CMD_FREE_BUFFER];
        for i in 0..cmds.len() {
            for j in (i + 1)..cmds.len() {
                assert_ne!(cmds[i], cmds[j], "commands {} and {} collide", i, j);
            }
        }
    }

    #[test]
    fn error_constants_distinct() {
        let errs = [BLK_OK, BLK_ERR_IO, BLK_ERR_UNSUPPORTED, BLK_ERR_INVALID, BLK_ERR_ALLOC];
        for i in 0..errs.len() {
            for j in (i + 1)..errs.len() {
                assert_ne!(errs[i], errs[j]);
            }
        }
    }

    #[test]
    fn sector_size_is_512() {
        assert_eq!(SECTOR_SIZE, 512);
    }
}
