#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use lockjaw_userlib::*;
use lockjaw_userlib::syscall::*;
use lockjaw_userlib::block::{
    BlockEngine, BlockInfo, BlockError, BlockClient, AllocatedBuffer, run_block_server,
};
use lockjaw_types::partition::{parse_disk, is_fat32, DiskLayout};
use lockjaw_types::vmem::MapMemoryAttribute;

// Matches upstream emmc2 and virtio-blk MAX_DMA_BUFFERS.
const MAX_BUFFERS: usize = 8;

// ---------------------------------------------------------------------------
// BootstrapBuf — normal-path teardown guard
//
// `halt()` calls `sys_exit()` with no unwind, so `Drop` is never invoked on
// error paths. This struct's value is collecting the 4-step teardown in one
// place so the normal path cannot omit a step. On halt paths, resources leak
// as accepted process-death leaks (same as all other drivers).
// ---------------------------------------------------------------------------

struct BootstrapBuf {
    pageset:   PageSetHandle,
    buffer_id: u32,
    va:        u64,  // 0 until sys_map_pages succeeds
}

impl BootstrapBuf {
    fn new(alloc: AllocatedBuffer) -> Self {
        Self { pageset: alloc.pageset, buffer_id: alloc.buffer_id, va: 0 }
    }

    fn set_va(&mut self, va: u64) { self.va = va; }

    /// Full 4-step teardown. Must be called on the normal (non-halt) path.
    /// Order matches docs/history/partition-manager-plan.md §6 and fat32-server:
    ///   sys_unmap → VMEM.free_unmapped → upstream.free_buffer → sys_close_handle
    ///
    /// Type-level VA-leak-on-unmap-failure invariant: VMEM accepts
    /// the freed VA only via `VaUnmapped` proof; on unmap failure the
    /// VA stays leaked (safer than aliasing on reuse).
    fn teardown(self, upstream: &mut BlockClient) {
        if self.va != 0 {
            if let Ok(p) = unmap_pages_tracked(self.pageset, self.va, 1) {
                VMEM.free_unmapped(p);
            }
        }
        upstream.free_buffer(self.buffer_id).ok();
        sys_close_handle(self.pageset);
    }
}

// ---------------------------------------------------------------------------
// BufMapEntry
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct BufMapEntry {
    // Handle in this process's table — what run_block_server hands back to us
    // as `ps` in read/write/free_buffer calls.
    pageset_local: PageSetHandle,
    // Buffer ID in the upstream server's namespace — what we pass to
    // upstream.read / upstream.write / upstream.free_buffer.
    upstream_buffer_id: u32,
}

// ---------------------------------------------------------------------------
// PartitionBlockEngine
// ---------------------------------------------------------------------------

struct PartitionBlockEngine {
    upstream:             BlockClient,
    start_lba:            u64,
    sector_count:         u64,
    upstream_buffer_attr: MapMemoryAttribute,
    buf_map:              [Option<BufMapEntry>; MAX_BUFFERS],
}

impl PartitionBlockEngine {
    fn find_slot(&self, ps: PageSetHandle) -> Option<usize> {
        self.buf_map.iter().position(|e| {
            e.map(|e| e.pageset_local.0 == ps.0).unwrap_or(false)
        })
    }
}

impl BlockEngine for PartitionBlockEngine {
    fn info(&self) -> BlockInfo {
        BlockInfo {
            capacity_sectors: self.sector_count,
            sector_size:      512,
            buffer_attribute: self.upstream_buffer_attr,
        }
    }

    fn alloc_buffer(&mut self, sector_count: u64) -> Result<PageSetHandle, BlockError> {
        // Reserve local slot FIRST: if upstream alloc came first and the local
        // table were full, the upstream allocation would be unrecoverable.
        let slot_idx = self.buf_map.iter().position(|e| e.is_none())
            .ok_or(BlockError::AllocFailed)?;

        let alloc = self.upstream.alloc_buffer(sector_count)
            .map_err(|_| BlockError::AllocFailed)?;

        self.buf_map[slot_idx] = Some(BufMapEntry {
            pageset_local:      alloc.pageset,
            upstream_buffer_id: alloc.buffer_id,
        });
        Ok(alloc.pageset)
    }

    fn read(&mut self, sector: u64, count: u64, buffer: PageSetHandle)
        -> Result<(), BlockError>
    {
        // Bounds check — the partition boundary is a security boundary.
        let end = sector.checked_add(count).ok_or(BlockError::InvalidParameter)?;
        if end > self.sector_count {
            return Err(BlockError::InvalidParameter);
        }

        let idx = self.find_slot(buffer).ok_or(BlockError::InvalidBuffer)?;
        let entry = self.buf_map[idx].unwrap();

        // Defense-in-depth: startup validation proved start+count <= capacity,
        // so an in-bounds sector cannot wrap here; checked_add catches engine bugs.
        let upstream_sector = self.start_lba.checked_add(sector)
            .ok_or(BlockError::InvalidParameter)?;

        self.upstream.read(upstream_sector, count, entry.upstream_buffer_id)
            .map_err(|e| e)
    }

    fn write(&mut self, sector: u64, count: u64, buffer: PageSetHandle)
        -> Result<(), BlockError>
    {
        let end = sector.checked_add(count).ok_or(BlockError::InvalidParameter)?;
        if end > self.sector_count {
            return Err(BlockError::InvalidParameter);
        }

        let idx = self.find_slot(buffer).ok_or(BlockError::InvalidBuffer)?;
        let entry = self.buf_map[idx].unwrap();

        let upstream_sector = self.start_lba.checked_add(sector)
            .ok_or(BlockError::InvalidParameter)?;

        self.upstream.write(upstream_sector, count, entry.upstream_buffer_id)
            .map_err(|e| e)
    }

    fn free_buffer(&mut self, buffer: PageSetHandle) {
        let idx = match self.find_slot(buffer) {
            Some(i) => i,
            None => return,  // already freed or never allocated — silently ignore
        };
        let entry = self.buf_map[idx].take().unwrap();

        // Free upstream ref first; then close our local handle.
        // If upstream.free_buffer errs, we still close the handle — our
        // local entry is now a zombie and there's no recovery path.
        self.upstream.free_buffer(entry.upstream_buffer_id).ok();
        sys_close_handle(entry.pageset_local);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("partmgr: starting\n");

    let reply_obj = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("partmgr: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: single sys_call to init to receive our endpoint handles.
    // Ordering guarantee: init sequences block-server bootstrap → partmgr
    // bootstrap → fat32 bootstrap. fat32's first get_info() call blocks in
    // kernel IPC until partmgr enters run_block_server. No explicit ready-
    // signal is needed or achievable on a single thread (see #129 design note).
    let bootstrap = match sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("partmgr: bootstrap FAILED\n"); halt(); }
    };
    let partition_srv_ep    = EndpointHandle(bootstrap[0]);
    let upstream_blk_srv_ep = EndpointHandle(bootstrap[1]);
    puts("partmgr: bootstrapped\n");

    let mut upstream = BlockClient::new(upstream_blk_srv_ep, reply_obj);
    let info = match upstream.get_info() {
        Ok(i) => i,
        Err(_) => { puts("partmgr: get_info FAILED\n"); halt(); }
    };

    if info.sector_size != 512 {
        puts("partmgr: upstream sector_size != 512, halting\n");
        halt();
    }

    // Allocate a 1-sector bootstrap buffer to read LBA 0.
    let alloc = match upstream.alloc_buffer(1) {
        Ok(a) => a,
        Err(_) => { puts("partmgr: alloc bootstrap buffer FAILED\n"); halt(); }
    };
    let mut bpb_buf = BootstrapBuf::new(alloc);

    let bpb_va = match VMEM.alloc(1) {
        Some(va) => va,
        None => { puts("partmgr: VMEM alloc FAILED\n"); halt(); }
    };

    if !sys_map_pages(alloc.pageset, bpb_va, info.buffer_attribute).is_ok() {
        puts("partmgr: map bpb FAILED\n");
        halt();
    }
    // Set va AFTER successful map so teardown() skips unmap if map never succeeded.
    bpb_buf.set_va(bpb_va);

    if upstream.read(0, 1, alloc.buffer_id).is_err() {
        puts("partmgr: read LBA 0 FAILED\n");
        halt();
    }

    let sector_zero: &[u8; 512] = unsafe { &*(bpb_va as *const [u8; 512]) };
    let layout = parse_disk(sector_zero, info.capacity_sectors);

    let (start_lba, sector_count) = match layout {
        Ok(DiskLayout::BareFat { sector_count }) => {
            // Bare FAT32 filesystem at LBA 0 — whole disk is one volume.
            puts("partmgr: bare FAT32 disk\n");
            (0u64, sector_count)
        }
        Ok(DiskLayout::Mbr { partitions }) => {
            // MBR disk: select the first FAT32-typed partition.
            // Lowest slot index wins — real SD cards put the boot partition at slot 0.
            let mut found = None;
            for p in partitions.iter() {
                if is_fat32(p.partition_type) {
                    found = Some((p.start_lba as u64, p.sector_count as u64));
                    break;
                }
            }
            match found {
                Some(t) => {
                    puts("partmgr: MBR FAT32 partition found\n");
                    t
                }
                None => {
                    puts("partmgr: MBR has no FAT32 partition, halting\n");
                    halt();
                }
            }
        }
        Err(_) => {
            puts("partmgr: parse_disk failed, halting\n");
            halt();
        }
    };

    // 4-step teardown: sys_unmap → VMEM.free → upstream.free_buffer → sys_close_handle.
    bpb_buf.teardown(&mut upstream);

    puts("partmgr: serving\n");

    let mut engine = PartitionBlockEngine {
        upstream,
        start_lba,
        sector_count,
        upstream_buffer_attr: info.buffer_attribute,
        buf_map: [None; MAX_BUFFERS],
    };

    run_block_server(&mut engine, partition_srv_ep);
}

fn halt() -> ! {
    sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("partmgr: PANIC\n");
    halt();
}
