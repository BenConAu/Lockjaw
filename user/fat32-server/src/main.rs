#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use lockjaw_userlib::*;
use lockjaw_userlib::block::BlockClient;
use lockjaw_types::fat32::{
    cluster_to_sector, decode_fat_entry_value, fat_entry_location, iter_dir, match_8_3,
    parse_bpb, DirEntry, Fat32Geometry, FatEntry,
};
use lockjaw_types::fs::{
    dispatch, FsAction, FsRequest, FS_ERR_ALLOC, FS_ERR_INVALID, FS_ERR_IO,
    FS_ERR_IS_DIRECTORY, FS_ERR_NOT_DIRECTORY, FS_ERR_NOT_FOUND, FS_ERR_TOO_MANY_OPEN, FS_OK,
};

/// Terminate the server process. EL0 wfi-loops keep the thread in
/// `Running` state from the scheduler's POV — they don't block,
/// they spin a tick-period each iteration after the next IRQ wakes
/// the CPU. Use sys_exit so the scheduler removes us from rotation.
fn halt() -> ! {
    sys_exit();
}

// ---------------------------------------------------------------------------
// Mount: geometry + block-driver scratch buffers
// ---------------------------------------------------------------------------

/// All persistent per-mount state. Owns two block-driver-allocated
/// DMA buffers: one sized to the cluster (for data and dirent reads),
/// one sector-sized (for FAT-entry lookups). Both are mapped
/// server-side and read directly via raw pointers (no Rust borrow
/// across them, so the borrow checker can't flag the implicit
/// re-read aliasing — but the two buffers point at different
/// physical memory so there's no actual aliasing).
struct Mount {
    geom: Fat32Geometry,
    blk: BlockClient,
    cluster_scratch_va: u64,
    cluster_scratch_buffer_id: u32,
    fat_scratch_va: u64,
    fat_scratch_buffer_id: u32,
}

impl Mount {
    fn read_cluster(&self, cluster: u32) -> Result<(), u64> {
        let start_sector = cluster_to_sector(cluster, &self.geom).ok_or(FS_ERR_IO)?;
        self.blk
            .read(start_sector as u64, self.geom.sectors_per_cluster as u64,
                  self.cluster_scratch_buffer_id)
            .map_err(|_| FS_ERR_IO)
    }

    /// Bytes from the most recent [`Mount::read_cluster`] call.
    /// SAFETY of returned slice: caller must not call read_cluster again
    /// while holding the slice (the underlying buffer is reused).
    fn cluster_bytes(&self) -> &[u8] {
        let len = self.geom.bytes_per_cluster() as usize;
        // SAFETY: cluster_scratch_va was mapped at startup; len is the
        // size declared by the geometry; the block driver wrote len
        // bytes into it on the most recent read_cluster() call.
        unsafe { core::slice::from_raw_parts(self.cluster_scratch_va as *const u8, len) }
    }

    /// Look up the FAT entry for `cluster` and decode it.
    fn fat_next(&self, cluster: u32) -> Result<FatEntry, u64> {
        let (sector, in_sector_offset) =
            fat_entry_location(cluster, &self.geom).ok_or(FS_ERR_IO)?;
        self.blk
            .read(sector as u64, 1, self.fat_scratch_buffer_id)
            .map_err(|_| FS_ERR_IO)?;
        // SAFETY: block driver just wrote 512 bytes into the FAT scratch.
        let sector_bytes = unsafe {
            core::slice::from_raw_parts(self.fat_scratch_va as *const u8, 512)
        };
        let off = in_sector_offset as usize;
        let raw = u32::from_le_bytes([
            sector_bytes[off], sector_bytes[off + 1],
            sector_bytes[off + 2], sector_bytes[off + 3],
        ]);
        Ok(decode_fat_entry_value(raw))
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Walk a path of `/`-separated components and return the directory
/// entry for the final component.
///
/// Path semantics (POSIX-flavored):
///
/// - Empty components from leading, internal, or trailing slashes are
///   collapsed (`/DIR//FILE.TXT` ≡ `/DIR/FILE.TXT`).
/// - A trailing `/` asserts that the final entry must be a directory.
///   `/DIR/` returns the dir entry; `/FILE.TXT/` rejects with
///   `FS_ERR_NOT_DIRECTORY`. Without a trailing slash the entry's
///   type isn't enforced here (the caller decides — e.g.
///   `handle_open` rejects directories with `FS_ERR_IS_DIRECTORY`
///   because Phase E only opens files).
/// - An intermediate component that resolves to a non-directory
///   (`/FILE.TXT/foo`) rejects with `FS_ERR_NOT_DIRECTORY`.
/// - `""`, `"/"`, `"//"` etc. (no real components) return
///   `FS_ERR_NOT_FOUND` — the root has no `DirEntry` representation.
fn resolve_path(mount: &Mount, path: &[u8]) -> Result<DirEntry, u64> {
    // The trailing slash carries semantic information that the
    // empty-component filter below would otherwise drop. Capture it
    // before the filter loop and enforce after resolution.
    let must_be_directory = path.last() == Some(&b'/');

    let mut cluster = mount.geom.root_cluster;
    let mut last: Option<DirEntry> = None;

    for component in path.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
        // If we already resolved a previous component, we have more
        // path to walk — that means the previous entry must be a
        // directory we descend into now.
        if let Some(prev) = last {
            if !prev.is_directory() {
                return Err(FS_ERR_NOT_DIRECTORY);
            }
            cluster = prev.first_cluster;
            if cluster < 2 {
                return Err(FS_ERR_NOT_FOUND);
            }
        }
        last = Some(lookup_in_dir(mount, cluster, component)?);
    }

    let entry = last.ok_or(FS_ERR_NOT_FOUND)?;
    if must_be_directory && !entry.is_directory() {
        return Err(FS_ERR_NOT_DIRECTORY);
    }
    Ok(entry)
}

/// Walk a directory's cluster chain looking for a dirent matching
/// `name` (8.3 case-insensitive). Returns NotFound if exhausted.
fn lookup_in_dir(mount: &Mount, dir_first_cluster: u32, name: &[u8]) -> Result<DirEntry, u64> {
    let mut cluster = dir_first_cluster;
    loop {
        mount.read_cluster(cluster)?;
        // Scope the dir_data borrow so it ends before fat_next clobbers
        // the cluster scratch (different buffer, but be explicit).
        let found = {
            let dir_data = mount.cluster_bytes();
            iter_dir(dir_data).find(|e| match_8_3(name, e))
        };
        if let Some(entry) = found {
            return Ok(entry);
        }
        match mount.fat_next(cluster)? {
            FatEntry::Used { next } => cluster = next,
            FatEntry::EndOfChain => return Err(FS_ERR_NOT_FOUND),
            // Free / Bad / Reserved cluster mid-chain: corrupt FS.
            _ => return Err(FS_ERR_IO),
        }
    }
}

// ---------------------------------------------------------------------------
// Open-file table
// ---------------------------------------------------------------------------

const MAX_OPEN_FILES: usize = 8;

#[derive(Clone, Copy)]
struct OpenFile {
    /// Caller-token isolation: only the client that opened this
    /// handle can read or close it. Same pattern as
    /// `lockjaw_userlib::block::BufferTracker`. A nonzero token
    /// means a kernel-assigned identity from an exported endpoint
    /// (token 0 is reserved for "no caller" / direct kernel calls
    /// and never matches a real handle).
    caller_token: u64,
    start_cluster: u32,
    size: u32,
    cursor: u32,
    buffer_ps: PageSetHandle,
    buffer_va: u64,
    buffer_size_bytes: u32,
    buffer_pages: u8,
}

struct OpenTable {
    slots: [Option<OpenFile>; MAX_OPEN_FILES],
}

impl OpenTable {
    const fn new() -> Self {
        const NONE: Option<OpenFile> = None;
        Self { slots: [NONE; MAX_OPEN_FILES] }
    }

    /// Insert into the first free slot. Returns the handle (slot
    /// index + 1, so 0 is never a valid handle).
    fn insert(&mut self, of: OpenFile) -> Option<u32> {
        for (i, s) in self.slots.iter_mut().enumerate() {
            if s.is_none() {
                *s = Some(of);
                return Some(i as u32 + 1);
            }
        }
        None
    }

    /// Lookup scoped to `caller_token`. Returns None if the slot is
    /// empty, the handle is out of range, OR the slot belongs to a
    /// different caller — preventing cross-client handle access.
    fn get(&self, handle: u32, caller_token: u64) -> Option<&OpenFile> {
        let idx = (handle as usize).checked_sub(1)?;
        let slot = self.slots.get(idx).and_then(|s| s.as_ref())?;
        if slot.caller_token != caller_token {
            return None;
        }
        Some(slot)
    }

    fn get_mut(&mut self, handle: u32, caller_token: u64) -> Option<&mut OpenFile> {
        let idx = (handle as usize).checked_sub(1)?;
        let slot = self.slots.get_mut(idx).and_then(|s| s.as_mut())?;
        if slot.caller_token != caller_token {
            return None;
        }
        Some(slot)
    }

    fn remove(&mut self, handle: u32, caller_token: u64) -> Option<OpenFile> {
        let idx = (handle as usize).checked_sub(1)?;
        let slot_ref = self.slots.get_mut(idx)?;
        match slot_ref {
            Some(of) if of.caller_token == caller_token => slot_ref.take(),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers (one per FsAction variant that produces side effects)
// ---------------------------------------------------------------------------

fn handle_open(
    mount: &Mount,
    table: &mut OpenTable,
    caller_token: u64,
    path: &[u8],
    buffer_pages: u8,
) {
    let entry = match resolve_path(mount, path) {
        Ok(e) => e,
        Err(e) => { sys_reply(e, 0, 0, 0); return; }
    };
    if entry.is_directory() {
        sys_reply(FS_ERR_IS_DIRECTORY, 0, 0, 0);
        return;
    }

    // Allocate the per-handle read buffer.
    let buffer_ps = match sys_alloc_pages(buffer_pages as u64) {
        Ok(ps) => ps,
        Err(_) => { sys_reply(FS_ERR_ALLOC, 0, 0, 0); return; }
    };
    let buffer_va = match VMEM.alloc(buffer_pages as usize) {
        Some(va) => va,
        None => {
            let _ = sys_close_handle(buffer_ps);
            sys_reply(FS_ERR_ALLOC, 0, 0, 0);
            return;
        }
    };
    if !sys_map_pages(buffer_ps, buffer_va, MapMemoryAttribute::Normal).is_ok() {
        let _ = sys_close_handle(buffer_ps);
        // No mapping was ever established — alloc-but-never-mapped path.
        VMEM.free_unused_allocation(buffer_va, buffer_pages as usize);
        sys_reply(FS_ERR_ALLOC, 0, 0, 0);
        return;
    }

    let buffer_size_bytes = (buffer_pages as u32) * (PAGE_SIZE as u32);

    let of = OpenFile {
        caller_token,
        start_cluster: entry.first_cluster,
        size: entry.size,
        cursor: 0,
        buffer_ps,
        buffer_va,
        buffer_size_bytes,
        buffer_pages,
    };

    let handle = match table.insert(of) {
        Some(h) => h,
        None => {
            // Mapping was established; tear it down through the
            // proof-token path. VA leaks on unmap failure.
            if let Ok(p) = unmap_pages_tracked(buffer_ps, buffer_va, buffer_pages as usize) {
                VMEM.free_unmapped(p);
            }
            let _ = sys_close_handle(buffer_ps);
            sys_reply(FS_ERR_TOO_MANY_OPEN, 0, 0, 0);
            return;
        }
    };

    // Export the buffer PageSet to the calling client so it can map.
    let exported = match sys_export_handle(buffer_ps) {
        Ok(idx) => idx,
        Err(_) => {
            // Roll back the table insert. We just inserted under
            // `caller_token`, so removing under the same token succeeds.
            let removed = table.remove(handle, caller_token).unwrap();
            if let Ok(p) = unmap_pages_tracked(removed.buffer_ps, removed.buffer_va, removed.buffer_pages as usize) {
                VMEM.free_unmapped(p);
            }
            let _ = sys_close_handle(removed.buffer_ps);
            sys_reply(FS_ERR_ALLOC, 0, 0, 0);
            return;
        }
    };

    sys_reply(FS_OK, handle as u64, exported, buffer_size_bytes as u64);
}

fn handle_read(
    mount: &Mount,
    table: &mut OpenTable,
    caller_token: u64,
    handle: u32,
    len: u32,
) {
    let of = match table.get(handle, caller_token) {
        Some(of) => *of,
        None => { sys_reply(FS_ERR_INVALID, 0, 0, 0); return; }
    };

    let remaining = of.size.saturating_sub(of.cursor);
    let to_read = len.min(of.buffer_size_bytes).min(remaining);
    if to_read == 0 {
        // EOF (or zero-length request). Reply OK with 0 bytes.
        sys_reply(FS_OK, 0, 0, 0);
        return;
    }

    // Walk FAT chain to the cluster containing `cursor`.
    let bytes_per_cluster = mount.geom.bytes_per_cluster();
    let mut cluster = of.start_cluster;
    let mut clusters_to_skip = of.cursor / bytes_per_cluster;
    while clusters_to_skip > 0 {
        match mount.fat_next(cluster) {
            Ok(FatEntry::Used { next }) => cluster = next,
            Ok(FatEntry::EndOfChain) => {
                // cursor past EOF — shouldn't happen if size is honest.
                sys_reply(FS_ERR_IO, 0, 0, 0);
                return;
            }
            _ => { sys_reply(FS_ERR_IO, 0, 0, 0); return; }
        }
        clusters_to_skip -= 1;
    }

    let mut in_cluster_offset = of.cursor % bytes_per_cluster;
    let mut bytes_left = to_read;
    let mut dest_offset: u32 = 0;
    let dest = of.buffer_va as *mut u8;

    loop {
        if mount.read_cluster(cluster).is_err() {
            sys_reply(FS_ERR_IO, 0, 0, 0);
            return;
        }
        let cluster_bytes = mount.cluster_bytes();
        let in_this_cluster = (bytes_per_cluster - in_cluster_offset).min(bytes_left);
        // SAFETY: dest is the per-handle buffer (mapped server-side)
        // with `buffer_size_bytes` valid bytes; dest_offset+in_this_cluster
        // is bounded by buffer_size_bytes by construction (to_read cap).
        unsafe {
            core::ptr::copy_nonoverlapping(
                cluster_bytes.as_ptr().add(in_cluster_offset as usize),
                dest.add(dest_offset as usize),
                in_this_cluster as usize,
            );
        }
        dest_offset += in_this_cluster;
        bytes_left -= in_this_cluster;
        if bytes_left == 0 {
            break;
        }
        // Walk to next cluster.
        match mount.fat_next(cluster) {
            Ok(FatEntry::Used { next }) => cluster = next,
            // Ran out mid-read despite to_read budgeting? Treat as EOF.
            Ok(FatEntry::EndOfChain) => break,
            _ => { sys_reply(FS_ERR_IO, 0, 0, 0); return; }
        }
        in_cluster_offset = 0;
    }

    // Advance cursor and reply with bytes actually copied.
    let bytes_returned = dest_offset;
    if let Some(of_mut) = table.get_mut(handle, caller_token) {
        of_mut.cursor = of_mut.cursor.saturating_add(bytes_returned);
    }
    sys_reply(FS_OK, bytes_returned as u64, 0, 0);
}

fn handle_close(table: &mut OpenTable, caller_token: u64, handle: u32) {
    let of = match table.remove(handle, caller_token) {
        Some(of) => of,
        None => { sys_reply(FS_ERR_INVALID, 0, 0, 0); return; }
    };
    // Proof-token teardown: VA returns to VMEM only on successful unmap.
    if let Ok(p) = unmap_pages_tracked(of.buffer_ps, of.buffer_va, of.buffer_pages as usize) {
        VMEM.free_unmapped(p);
    }
    let _ = sys_close_handle(of.buffer_ps);
    sys_reply(FS_OK, 0, 0, 0);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("fat32: starting\n");

    // Allocate one Reply object for both bootstrap and outbound block IPCs.
    let reply = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("fat32: create reply FAILED\n"); halt(); }
    };

    // Bootstrap: receive fs_srv_ep (this server's own endpoint) and
    // blk_srv_ep (the block driver's server endpoint) from init.
    let bootstrap = match sys_call_ret4(bootstrap_endpoint(), reply, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("fat32: bootstrap FAILED\n"); halt(); }
    };
    let fs_srv_ep = EndpointHandle(bootstrap[0]);
    let blk_srv_ep = EndpointHandle(bootstrap[1]);
    puts("fat32: bootstrapped\n");

    let blk = BlockClient::new(blk_srv_ep, reply);

    // Query the block engine's required buffer attribute. Both
    // virtio-blk and emmc2 now return Normal (cacheable): virtio-blk
    // uses Buddy-origin pages (always cacheable; coherent DMA on
    // QEMU); emmc2 uses DmaPool-origin pages (Cacheable post C1 of
    // the cacheable-DMA migration; coherence maintained via the
    // sync syscalls at engine.read / engine.write boundaries).
    // Mapping with the wrong attribute fails at sys_map_pages.
    let blk_info = match blk.get_info() {
        Ok(i) => i,
        Err(_) => { puts("fat32: blk get_info FAILED\n"); halt(); }
    };
    let blk_attr = blk_info.buffer_attribute;

    // ---- Mount: read sector 0, parse BPB, allocate scratch buffers. ----
    let bpb_buf = match blk.alloc_buffer(1) {
        Ok(b) => b,
        Err(_) => { puts("fat32: blk alloc_buffer (BPB) FAILED\n"); halt(); }
    };
    let bpb_va = VMEM.alloc(1).expect("VA exhausted for BPB buffer");
    if !sys_map_pages(bpb_buf.pageset, bpb_va, blk_attr).is_ok() {
        puts("fat32: BPB buffer map FAILED\n"); halt();
    }
    if blk.read(0, 1, bpb_buf.buffer_id).is_err() {
        puts("fat32: read sector 0 FAILED\n"); halt();
    }
    // SAFETY: block driver just wrote 512 bytes into bpb_va.
    let sector0: &[u8; 512] = unsafe { &*(bpb_va as *const [u8; 512]) };
    let geom = match parse_bpb(sector0) {
        Ok(g) => g,
        Err(_) => { puts("fat32: BPB parse FAILED\n"); halt(); }
    };
    // Done with the BPB buffer. Free it back to the block driver's pool.
    // Proof-token teardown: VA returns to VMEM only on successful unmap.
    if let Ok(p) = unmap_pages_tracked(bpb_buf.pageset, bpb_va, 1) {
        VMEM.free_unmapped(p);
    }
    let _ = blk.free_buffer(bpb_buf.buffer_id);
    let _ = sys_close_handle(bpb_buf.pageset);

    // Allocate the per-mount cluster scratch (sized to the actual cluster).
    let cluster_sectors = geom.sectors_per_cluster as u64;
    let cluster_buf = match blk.alloc_buffer(cluster_sectors) {
        Ok(b) => b,
        Err(_) => { puts("fat32: blk alloc_buffer (cluster) FAILED\n"); halt(); }
    };
    let cluster_pages = ((cluster_sectors * 512 + PAGE_SIZE - 1) / PAGE_SIZE) as usize;
    let cluster_va = VMEM.alloc(cluster_pages).expect("VA exhausted for cluster scratch");
    if !sys_map_pages(cluster_buf.pageset, cluster_va, blk_attr).is_ok() {
        puts("fat32: cluster scratch map FAILED\n"); halt();
    }

    // FAT scratch is always one sector.
    let fat_buf = match blk.alloc_buffer(1) {
        Ok(b) => b,
        Err(_) => { puts("fat32: blk alloc_buffer (fat) FAILED\n"); halt(); }
    };
    let fat_va = VMEM.alloc(1).expect("VA exhausted for FAT scratch");
    if !sys_map_pages(fat_buf.pageset, fat_va, blk_attr).is_ok() {
        puts("fat32: FAT scratch map FAILED\n"); halt();
    }

    let mount = Mount {
        geom,
        blk,
        cluster_scratch_va: cluster_va,
        cluster_scratch_buffer_id: cluster_buf.buffer_id,
        fat_scratch_va: fat_va,
        fat_scratch_buffer_id: fat_buf.buffer_id,
    };

    puts("fat32: mounted, cluster_size=");
    put_decimal(geom.bytes_per_cluster() as u64);
    puts(" bytes, root_cluster=");
    put_decimal(geom.root_cluster as u64);
    puts(", clusters=");
    put_decimal(geom.cluster_count() as u64);
    puts("\n");

    // ---- IPC dispatch loop. ----
    let mut table = OpenTable::new();
    loop {
        let msg = match sys_receive_ret4(fs_srv_ep) {
            Ok(m) => m,
            Err(_) => { puts("fat32: receive FAILED\n"); halt(); }
        };
        // Caller-token isolation: handles are scoped to the client
        // that opened them. Same pattern as block-server's BufferTracker.
        let caller_token = sys_query_caller_token();
        let action = dispatch(&FsRequest {
            cmd: msg[0],
            arg1: msg[1],
            arg2: msg[2],
            arg3: msg[3],
        });
        match action {
            FsAction::Open { path, path_len, buffer_pages } => {
                handle_open(
                    &mount, &mut table, caller_token,
                    &path[..path_len as usize], buffer_pages,
                );
            }
            FsAction::Read { handle, len } => {
                handle_read(&mount, &mut table, caller_token, handle, len);
            }
            FsAction::Close { handle } => {
                handle_close(&mut table, caller_token, handle);
            }
            FsAction::Reply { words } => {
                sys_reply(words[0], words[1], words[2], words[3]);
            }
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("fat32: PANIC\n");
    halt();
}

