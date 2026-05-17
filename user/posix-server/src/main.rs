#![no_std]
#![no_main]

const LOCKJAW_SOURCE_HASH: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

#[used]
#[link_section = ".lockjaw_hash"]
static LOCKJAW_HASH_SECTION: u64 = LOCKJAW_SOURCE_HASH;

use lockjaw_userlib::*;
use lockjaw_userlib::elf::parse_elf;
use lockjaw_userlib::fs::{FsClient, FsError};
use lockjaw_types::addr::PAGE_SIZE;
use lockjaw_types::constants::{POSIX_MMAP_BASE, USER_STACK_BASE, USER_VA_END};
use lockjaw_types::fs::FS_MAX_INLINE_PATH;
use lockjaw_types::posix_fd::{FdEntry, FdKind, FdTable, FD_STDIN, MAX_FDS};
use lockjaw_userlib::elf_loader::{plan_elf_load, ElfLoadEntry};
use lockjaw_types::posix::{
    compute_va_layout, dispatch, neg_errno, write_linux_stack, Action, DispatchInputs,
    StackInputs, EBADF, EINVAL, EIO, EISDIR, EMFILE, ENOENT, ENOMEM, ENOSYS, ENOTDIR,
    STACK_LAYOUT_FIXED_BYTES,
};

/// Pre-built statically-linked musl hello binary.
/// Built with patched musl (see musl-lockjaw/).
static POSIX_HELLO: &[u8] = include_bytes!("../../posix-hello/hello");

/// Terminate the process. EL0 `wfi`-loops keep the thread in
/// `Running` state from the scheduler's POV — they don't block,
/// they spin a tick-period each iteration after the next IRQ wakes
/// the CPU. Use sys_exit so the scheduler removes us from rotation.
fn halt() -> ! {
    sys_exit();
}

// ---------------------------------------------------------------------------
// ELF loading — ported from user/init/src/main.rs:spawn_elf
// ---------------------------------------------------------------------------

/// Apply one [`ElfLoadEntry`] from a load plan: allocate a fresh page,
/// map it temporarily at `temp_va`, zero it, copy the file slice (if
/// any) into the right in-page offset, and append a `ProcessMapping`
/// for the child's address space at `map_array[mapping_idx]`.
///
/// All planning decisions (page count, file ranges, in-page offsets,
/// bounds checks) live in `lockjaw_types::elf_loader::plan_elf_load`.
/// This function is mechanical execution.
fn apply_elf_load_entry(
    entry: &ElfLoadEntry,
    elf_data: &[u8],
    map_array: *mut ProcessMapping,
    mapping_idx: usize,
    temp_va: u64,
) {
    let ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: seg alloc FAILED\n"); halt(); }
    };
    if !sys_map_pages(ps, temp_va, MapMemoryAttribute::Normal).is_ok() {
        puts("posix: seg map FAILED\n");
        halt();
    }
    unsafe { zero_page_at_va(temp_va); }

    let (src_start, src_end) = entry.src_file_range;
    if src_end > src_start {
        unsafe {
            core::ptr::copy_nonoverlapping(
                elf_data[src_start..src_end].as_ptr(),
                (temp_va + entry.in_page_offset as u64) as *mut u8,
                src_end - src_start,
            );
        }
    }

    unsafe {
        core::ptr::write(map_array.add(mapping_idx), ProcessMapping {
            virt_addr: entry.page_va,
            pageset_id: ps.0,
            page_index: 0,
            flags: if entry.executable { FLAG_EXECUTABLE } else { 0 },
        });
    }
}

/// Load ELF segments into freshly allocated pages. Returns mapping count.
/// `map_array_va` must point to a mapped page for ProcessMapping entries.
/// `temp_base_va` must have enough free VA for all segment pages.
///
/// All structural decisions live in `plan_elf_load`: page count for
/// each segment, file-range slicing, in-page offsets for unaligned
/// vaddrs, BSS-only pages, bounds and overflow checks. This function
/// just allocates a plan buffer, iterates the plan, and applies each
/// entry.
///
/// The plan buffer is sized for the binaries posix-server expects to
/// spawn (Phase 0: a tiny musl static "hello, lockjaw"). 64 entries =
/// ~2.5 KB on the stack — fits comfortably in posix-server's 8-page
/// stack. Larger binaries that overflow this cap surface as a clean
/// `TooManyEntries` error, not a stack overflow.
const POSIX_ELF_LOAD_BUF: usize = 64;

fn load_elf_segments(
    elf_data: &[u8],
    elf_info: &lockjaw_types::elf::ElfInfo,
    map_array_va: u64,
    temp_base_va: u64,
) -> usize {
    let mut planbuf = [ElfLoadEntry::EMPTY; POSIX_ELF_LOAD_BUF];
    let plan = match plan_elf_load(elf_info, elf_data.len(), &mut planbuf) {
        Ok(p) => p,
        Err(_) => { puts("posix: elf load plan FAILED\n"); halt(); }
    };

    let map_array = map_array_va as *mut ProcessMapping;
    for (i, entry) in plan.entries().iter().enumerate() {
        let temp_va = temp_base_va + (i as u64) * PAGE_SIZE;
        apply_elf_load_entry(entry, elf_data, map_array, i, temp_va);
    }
    plan.page_count()
}

// ---------------------------------------------------------------------------
// Stack layout — Linux initial stack for musl _start
// ---------------------------------------------------------------------------

/// Phase 0 AT_RANDOM seed. Fixed; later phases should plumb real entropy.
/// "Lockjaw!POSIX000" — 16 bytes, deterministic so tests that read the
/// child's view of the seed see the same value every run.
const PHASE0_RANDOM_SEED: [u8; 16] = [
    0x4c, 0x6f, 0x63, 0x6b, 0x6a, 0x61, 0x77, 0x21, // "Lockjaw!"
    0x50, 0x4f, 0x53, 0x49, 0x58, 0x30, 0x30, 0x30, // "POSIX000"
];

/// Build the Linux initial stack the musl child reads from SP. The
/// pure layout writer lives in `lockjaw_types::posix::write_linux_stack`
/// (host-tested); this function is just the side-effect glue:
/// `stack_va` is the personality server's temp mapping of the 4-page
/// stack PageSet, and the layout goes in the top page (which the child
/// sees at `USER_STACK_BASE + 3 * PAGE_SIZE`).
fn write_stack_layout(stack_va: u64) {
    let layout_va = stack_va + 3 * PAGE_SIZE;
    let child_layout_va = USER_STACK_BASE + 3 * PAGE_SIZE;

    // SAFETY: stack_va points to a 4-page mapping the personality server
    // owns; the top page is reserved for the initial stack image.
    let buf = unsafe {
        core::slice::from_raw_parts_mut(layout_va as *mut u8, PAGE_SIZE as usize)
    };

    let argv0 = b"hello\0";
    if let Err(_) = write_linux_stack(buf, &StackInputs {
        argv0,
        random_seed: PHASE0_RANDOM_SEED,
        child_layout_va,
        page_size: PAGE_SIZE,
    }) {
        puts("posix: stack layout FAILED\n");
        halt();
    }
    // Sanity check the layout actually fits — surfaces a buffer-size
    // regression here rather than as a child segfault.
    debug_assert!((PAGE_SIZE as usize) >= STACK_LAYOUT_FIXED_BYTES + argv0.len());
}

// ---------------------------------------------------------------------------
// Per-FD file-resource tracking (kernel side of the FS handle)
// ---------------------------------------------------------------------------

/// Resources held server-side for one open POSIX fd. Mirrors the
/// FdTable slot but holds `lockjaw-userlib`-typed PageSetHandle that
/// can't live in the pure `lockjaw-types::posix_fd::FdEntry`. The
/// pair (FdTable, file_resources) is updated together — see
/// `open_fd` / `close_fd` helpers.
#[derive(Clone, Copy)]
struct FileResource {
    /// fat32-server's per-handle PageSet (in this process's table).
    buffer_pageset: PageSetHandle,
    /// Where this server has the buffer mapped.
    buffer_va: u64,
    /// Buffer size in bytes (≤ PAGE_SIZE for now).
    buffer_size: u32,
}

// ---------------------------------------------------------------------------
// mmap region tracking (Phase 2.2)
// ---------------------------------------------------------------------------

/// Maximum simultaneous mmap regions per process. Phase 2 cap; bump
/// when programs need more.
const MAX_MMAP_REGIONS: usize = 16;

/// One outstanding mmap region. Lookups are scoped by
/// `caller_token` — two clients can hold regions at the same VA
/// (each in its own address space), but a client can only operate
/// on entries it created. Mirrors the OpenTable shape in
/// fat32-server.
#[derive(Clone, Copy)]
struct MmapEntry {
    /// Identifies which posix client owns this entry. munmap and
    /// mprotect refuse cross-caller lookups.
    caller_token: u64,
    base_va: u64,
    len_bytes: u64,
    /// Server-side handle for the PageSet. Closed on munmap (the
    /// export bumped refcount so the client's handle survives until
    /// it's closed).
    server_pageset: PageSetHandle,
}

/// Per-process mmap region table. Insertion is O(N) — fine for
/// MAX_MMAP_REGIONS = 16.
struct MmapTable {
    slots: [Option<MmapEntry>; MAX_MMAP_REGIONS],
}

impl MmapTable {
    const fn new() -> Self {
        Self { slots: [None; MAX_MMAP_REGIONS] }
    }

    /// True if at least one slot is free. Used to pre-check insert
    /// capacity BEFORE doing any state-mutating IPC (PageSet alloc,
    /// handle export). Single-threaded dispatch means
    /// `has_room()` at decision time is equivalent to
    /// "next insert will succeed".
    fn has_room(&self) -> bool {
        self.slots.iter().any(|s| s.is_none())
    }

    /// Insert an entry. Returns Err(()) if the table is full.
    /// Callers that have already acquired external resources
    /// (PageSet alloc, exported handle) should pre-check via
    /// `has_room()` so this branch is unreachable on the success
    /// path.
    fn insert(&mut self, e: MmapEntry) -> Result<(), ()> {
        for slot in self.slots.iter_mut() {
            if slot.is_none() {
                *slot = Some(e);
                return Ok(());
            }
        }
        Err(())
    }

    /// Find the entry matching `(caller_token, base_va)` and remove
    /// it. Returns the entry; None if no caller-scoped match.
    fn take_by_base_va(&mut self, caller_token: u64, base_va: u64) -> Option<MmapEntry> {
        for slot in self.slots.iter_mut() {
            if let Some(e) = slot.as_ref() {
                if e.caller_token == caller_token && e.base_va == base_va {
                    return slot.take();
                }
            }
        }
        None
    }

    /// Find an entry by `(caller_token, base_va)` without removing.
    fn find_by_base_va(&self, caller_token: u64, base_va: u64) -> Option<&MmapEntry> {
        for slot in self.slots.iter() {
            if let Some(e) = slot.as_ref() {
                if e.caller_token == caller_token && e.base_va == base_va {
                    return Some(e);
                }
            }
        }
        None
    }
}

/// Bump-only VA allocator for the mmap region. Phase 2 doesn't
/// reuse freed VA — every mmap claims the next contiguous chunk.
/// 1 GiB of user VA between POSIX_MMAP_BASE and USER_VA_END leaves
/// plenty of room before the bump runs into the limit.
struct MmapVaAllocator {
    next: u64,
    limit: u64,
}

impl MmapVaAllocator {
    const fn new(base: u64, limit: u64) -> Self {
        Self { next: base, limit }
    }

    /// Reserve `pages` PAGE_SIZE pages. Returns the base VA, or
    /// None if the region would exceed `limit`.
    fn alloc(&mut self, pages: u64) -> Option<u64> {
        let bytes = pages.checked_mul(PAGE_SIZE)?;
        let new_next = self.next.checked_add(bytes)?;
        if new_next > self.limit {
            return None;
        }
        let base = self.next;
        self.next = new_next;
        Some(base)
    }
}

// ---------------------------------------------------------------------------
// Deferred remote-close queue
// ---------------------------------------------------------------------------

/// Max in-flight remote handles waiting on retry from a failed
/// rollback `fs.close()`. Bounded so a runaway transport failure
/// halts the personality server (loudly) rather than leaking
/// indefinitely.
const MAX_DEFERRED_CLOSES: usize = 4;

/// Push a server handle onto the retry queue. Returns Err if the
/// queue is already full (signalling the dispatch loop to halt —
/// continuing would silently leak fat32-server slots).
fn defer_close(
    deferred: &mut [Option<u32>; MAX_DEFERRED_CLOSES],
    handle: u32,
) -> Result<(), ()> {
    for slot in deferred.iter_mut() {
        if slot.is_none() {
            *slot = Some(handle);
            return Ok(());
        }
    }
    Err(())
}

/// Try once to close every queued handle. Successful closes leave
/// the slot empty for reuse; failures are left in place for the
/// next iteration. Called at the top of the dispatch loop.
fn drain_deferred_closes(
    fs: &FsClient,
    deferred: &mut [Option<u32>; MAX_DEFERRED_CLOSES],
) {
    for slot in deferred.iter_mut() {
        if let Some(h) = *slot {
            if fs.close(h).is_ok() {
                *slot = None;
            }
        }
    }
}

/// Wrap `fs.close()` so a transport failure schedules a retry
/// instead of leaking the remote slot. Local resources (pageset
/// handle, VA) are independent of the remote close — the caller
/// frees those itself before invoking this helper.
fn close_remote_or_defer(
    fs: &FsClient,
    deferred: &mut [Option<u32>; MAX_DEFERRED_CLOSES],
    server_handle: u32,
) {
    if fs.close(server_handle).is_ok() {
        return;
    }
    if defer_close(deferred, server_handle).is_err() {
        puts("posix: deferred-close queue full; halting\n");
        halt();
    }
}

/// Map FsError → POSIX errno.
fn fs_error_to_errno(e: FsError) -> u64 {
    match e {
        FsError::NotFound => neg_errno(ENOENT),
        FsError::IsDirectory => neg_errno(EISDIR),
        FsError::NotDirectory => neg_errno(ENOTDIR),
        FsError::TooManyOpen => neg_errno(EMFILE),
        FsError::AllocFailed => neg_errno(ENOMEM),
        FsError::Io => neg_errno(EIO),
        FsError::PathTooLong
            | FsError::Invalid
            | FsError::InvalidBufferPages
            | FsError::Unknown
            | FsError::IpcFailed => neg_errno(EINVAL),
    }
}

// ---------------------------------------------------------------------------
// Phase 1 file-syscall handlers
// ---------------------------------------------------------------------------

fn handle_file_open(
    fs: &FsClient,
    fd_table: &mut FdTable,
    file_resources: &mut [Option<FileResource>; MAX_FDS],
    deferred: &mut [Option<u32>; MAX_DEFERRED_CLOSES],
    server_shared_va: u64,
    path_len: u64,
    flags: u64,
) {
    if server_shared_va == 0 {
        // openat before POSIX_INIT — protocol violation.
        sys_reply(neg_errno(EINVAL), 0, 0, 0);
        return;
    }
    if path_len == 0 || path_len > FS_MAX_INLINE_PATH as u64 {
        // Phase F caps paths at FS_MAX_INLINE_PATH (16 bytes); longer
        // paths need the future capability-passing extension.
        sys_reply(neg_errno(EINVAL), 0, 0, 0);
        return;
    }
    // SAFETY: server_shared_va is the personality server's mapping
    // of the per-client shared buffer; the shim wrote `path_len`
    // bytes there before calling. path_len ≤ FS_MAX_INLINE_PATH ≤
    // PAGE_SIZE, so the slice is in-bounds.
    let path: &[u8] = unsafe {
        core::slice::from_raw_parts(server_shared_va as *const u8, path_len as usize)
    };

    let opened = match fs.open(path, 1) {
        Ok(o) => o,
        Err(e) => { sys_reply(fs_error_to_errno(e), 0, 0, 0); return; }
    };

    // Past this point, every failure path must release the remote
    // FS handle via close_remote_or_defer (which schedules a retry
    // if fs.close fails) so we don't leak fat32-server open-file
    // slots when local setup fails.

    // Map the per-handle buffer locally so we can read from it on
    // FileRead (then copy into the client's shared buffer).
    let buffer_va = match VMEM.alloc(1) {
        Some(va) => va,
        None => {
            let _ = sys_close_handle(opened.pageset);
            close_remote_or_defer(fs, deferred, opened.handle);
            sys_reply(neg_errno(ENOMEM), 0, 0, 0);
            return;
        }
    };
    if !sys_map_pages(opened.pageset, buffer_va, MapMemoryAttribute::Normal).is_ok() {
        let _ = sys_close_handle(opened.pageset);
        VMEM.free(buffer_va, 1);
        close_remote_or_defer(fs, deferred, opened.handle);
        sys_reply(neg_errno(ENOMEM), 0, 0, 0);
        return;
    }

    let fd = match fd_table.alloc(FdEntry::file(opened.handle, flags as u32)) {
        Ok(fd) => fd,
        Err(_) => {
            let _ = sys_unmap_pages(opened.pageset, buffer_va);
            let _ = sys_close_handle(opened.pageset);
            VMEM.free(buffer_va, 1);
            close_remote_or_defer(fs, deferred, opened.handle);
            sys_reply(neg_errno(EMFILE), 0, 0, 0);
            return;
        }
    };
    file_resources[fd as usize] = Some(FileResource {
        buffer_pageset: opened.pageset,
        buffer_va,
        buffer_size: opened.buffer_size,
    });
    sys_reply(fd as u64, 0, 0, 0);
}

fn handle_file_read(
    fs: &FsClient,
    fd_table: &FdTable,
    file_resources: &[Option<FileResource>; MAX_FDS],
    server_shared_va: u64,
    fd: u64,
    len: u64,
) {
    if server_shared_va == 0 {
        // read before POSIX_INIT — there's no shared buffer to copy
        // bytes back to. Reject with EINVAL rather than dereferencing
        // VA 0 in the copy_nonoverlapping below.
        sys_reply(neg_errno(EINVAL), 0, 0, 0);
        return;
    }
    let entry = match fd_table.lookup(fd as u32) {
        Some(e) => *e,
        None => { sys_reply(neg_errno(EBADF), 0, 0, 0); return; }
    };
    match entry.kind {
        FdKind::Stdio => {
            // Phase F: stdin returns EOF; reading from stdout/stderr
            // is EBADF. (Linux returns EBADF for read on a write-only
            // fd, which stdout/stderr effectively are here.)
            let r = if fd == FD_STDIN as u64 { 0 } else { neg_errno(EBADF) };
            sys_reply(r, 0, 0, 0);
        }
        FdKind::File => {
            let resource = match &file_resources[fd as usize] {
                Some(r) => *r,
                None => { sys_reply(neg_errno(EBADF), 0, 0, 0); return; }
            };
            let cap = (len as u32).min(resource.buffer_size).min(PAGE_SIZE as u32);
            let bytes_returned = match fs.read(entry.server_handle, cap) {
                Ok(n) => n,
                Err(e) => { sys_reply(fs_error_to_errno(e), 0, 0, 0); return; }
            };
            // Copy from per-handle buffer to client's shared buffer
            // so the shim can deliver the data to the user buf.
            // SAFETY: both source and dest are mapped pages this
            // process owns; bytes_returned ≤ buffer_size ≤ PAGE_SIZE.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    resource.buffer_va as *const u8,
                    server_shared_va as *mut u8,
                    bytes_returned as usize,
                );
            }
            sys_reply(bytes_returned as u64, 0, 0, 0);
        }
    }
}

fn handle_file_close(
    fs: &FsClient,
    fd_table: &mut FdTable,
    file_resources: &mut [Option<FileResource>; MAX_FDS],
    fd: u64,
) {
    let entry = match fd_table.lookup(fd as u32) {
        Some(e) => *e,
        None => { sys_reply(neg_errno(EBADF), 0, 0, 0); return; }
    };
    match entry.kind {
        FdKind::Stdio => {
            // Phase F protects stdio from being closed; musl wouldn't
            // normally close fd 1/2 itself, but a buggy program might.
            sys_reply(neg_errno(EBADF), 0, 0, 0);
        }
        FdKind::File => {
            // Close the remote handle FIRST so a transport failure
            // doesn't leak the fat32-server open-file slot. If the
            // remote close fails, the local fd stays live and the
            // caller sees the error — a retry can drive the close
            // through later. Otherwise our small server-side
            // open-file table would silently fill up.
            if let Err(e) = fs.close(entry.server_handle) {
                sys_reply(fs_error_to_errno(e), 0, 0, 0);
                return;
            }
            // Remote handle is gone; only now drop our side.
            // close() can only fail with StdioClosed (handled above)
            // or BadFd (we just looked it up). Either is unreachable.
            let _ = fd_table.close(fd as u32);
            if let Some(r) = file_resources[fd as usize].take() {
                let _ = sys_unmap_pages(r.buffer_pageset, r.buffer_va);
                let _ = sys_close_handle(r.buffer_pageset);
                VMEM.free(r.buffer_va, 1);
            }
            sys_reply(0, 0, 0, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2.2 mmap handlers
// ---------------------------------------------------------------------------

/// Allocate a PageSet of N pages, claim a base_va from the bump
/// allocator, export the handle to the caller, and record the
/// `(caller_token, base_va, len_bytes, server_pageset)` entry.
///
/// Reply: `(0, base_va, exported_handle, pages)` on success;
/// `(-errno, 0, 0, 0)` on any failure. The mmap_table.has_room()
/// pre-check ensures that nothing after sys_export_handle can fail —
/// the export is the point of no return because there's no way to
/// rescind a handle already in the caller's handle table without
/// the FS_MMAP_ROLLBACK protocol (Phase 2.3). Pre-checking keeps
/// today's runtime correct without that protocol.
fn handle_file_mmap(
    mmap_table: &mut MmapTable,
    va_alloc: &mut MmapVaAllocator,
    len_bytes: u64,
) {
    // dispatch already validated len > 0 and len <= MAX_MMAP_BYTES.
    let pages = (len_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
    let caller_token = sys_query_caller_token();

    // 1. Pre-check that mmap_table has room. If not, fail fast with
    //    no IPC side effects. Single-threaded dispatch means a
    //    has_room() == true here implies the insert at step 5 will
    //    succeed.
    if !mmap_table.has_room() {
        sys_reply(neg_errno(ENOMEM), 0, 0, 0);
        return;
    }

    // 2. Allocate the data PageSet.
    let server_ps = match sys_alloc_pages(pages) {
        Ok(ps) => ps,
        Err(_) => { sys_reply(neg_errno(ENOMEM), 0, 0, 0); return; }
    };

    // 3. Reserve a base VA. On failure, rewind by freeing the PageSet
    //    we just allocated.
    let base_va = match va_alloc.alloc(pages) {
        Some(va) => va,
        None => {
            let _ = sys_close_handle(server_ps);
            sys_reply(neg_errno(ENOMEM), 0, 0, 0);
            return;
        }
    };

    // 4. Export the handle to the caller. sys_export_handle bumps
    //    the PageSet refcount and inserts a handle in the caller's
    //    table. After this point we cannot rescind the export
    //    without an explicit rollback protocol — so step 5 must
    //    not fail (guaranteed by the has_room precheck above).
    let exported_handle = match sys_export_handle(server_ps) {
        Ok(h) => h,
        Err(_) => {
            // Pre-export failure path: close server PageSet, no
            // caller-side cleanup needed because the export never
            // happened. VA bump is intentionally NOT rewound — Phase
            // 2 is bump-only by design.
            let _ = sys_close_handle(server_ps);
            sys_reply(neg_errno(ENOMEM), 0, 0, 0);
            return;
        }
    };

    // 5. Record the entry. Cannot fail: pre-checked has_room above,
    //    and dispatch is single-threaded so no other action can
    //    consume the slot between steps 1 and 5.
    let entry = MmapEntry {
        caller_token,
        base_va,
        len_bytes,
        server_pageset: server_ps,
    };
    mmap_table.insert(entry).expect("pre-checked has_room at step 1");

    sys_reply(0, base_va, exported_handle, pages);
}

/// Look up `(caller_token, base_va)` in the mmap_table. Phase 2
/// supports only exact whole-region unmap: the caller's `len_bytes`
/// MUST match the original allocation. Mismatch returns EINVAL;
/// missing entry returns EINVAL too (no separate ENOENT — Linux
/// uses EINVAL for "not a known mapping"). Cross-caller lookups
/// return EINVAL because the table scopes by caller_token.
///
/// On match: take the entry, close the server-side PageSet handle,
/// reply 0.
fn handle_file_munmap(
    mmap_table: &mut MmapTable,
    base_va: u64,
    len_bytes: u64,
) {
    let caller_token = sys_query_caller_token();
    // First peek to verify len matches. Take only on full match so a
    // wrong-len call doesn't accidentally drop the entry.
    let matches = match mmap_table.find_by_base_va(caller_token, base_va) {
        Some(e) => e.len_bytes == len_bytes,
        None => false,
    };
    if !matches {
        sys_reply(neg_errno(EINVAL), 0, 0, 0);
        return;
    }
    // Confirmed match — take and close.
    let entry = mmap_table
        .take_by_base_va(caller_token, base_va)
        .expect("just-confirmed entry");
    let _ = sys_close_handle(entry.server_pageset);
    sys_reply(0, 0, 0, 0);
}

/// Shim-side rollback for the failure path of mmap. The shim sends
/// `NR_MMAP_ROLLBACK` if its local `sys_map_pages` failed AFTER a
/// successful FS_MMAP reply. Looks up `(caller_token, base_va)` and
/// tears down the entry — close the server-side PageSet handle and
/// remove from the table. Reply 0 on success, EINVAL if the entry
/// is unknown (caller bug or duplicate rollback).
///
/// This is the missing half of the Phase 2.2 export-then-insert
/// handshake: the export bumped refcount on the caller's table,
/// but a failed local map means the caller never actually used
/// the region. Without rollback, the server's PageSet handle and
/// the caller's exported handle would both leak.
fn handle_file_mmap_rollback(
    mmap_table: &mut MmapTable,
    base_va: u64,
) {
    let caller_token = sys_query_caller_token();
    if let Some(entry) = mmap_table.take_by_base_va(caller_token, base_va) {
        let _ = sys_close_handle(entry.server_pageset);
        sys_reply(0, 0, 0, 0);
    } else {
        sys_reply(neg_errno(EINVAL), 0, 0, 0);
    }
}

/// mprotect on an mmap region: must match (caller_token, base_va,
/// len_bytes) exactly. Phase 2 has no kernel API to actually change
/// protection, so we only accept calls that "set" the existing RW
/// protection (already validated by dispatch). Reply 0 on match,
/// EINVAL on mismatch.
///
/// The narrow rule keeps the no-op stub truthful: mprotect succeeds
/// only for known mmap regions owned by this caller. brk, ELF,
/// stack, arbitrary VAs, and other clients' mmap regions all
/// return EINVAL even with the right prot.
fn handle_file_mprotect(
    mmap_table: &MmapTable,
    base_va: u64,
    len_bytes: u64,
) {
    let caller_token = sys_query_caller_token();
    let matches = matches!(
        mmap_table.find_by_base_va(caller_token, base_va),
        Some(e) if e.len_bytes == len_bytes,
    );
    if matches {
        sys_reply(0, 0, 0, 0);
    } else {
        sys_reply(neg_errno(EINVAL), 0, 0, 0);
    }
}

// ---------------------------------------------------------------------------
// Side effects for dispatch actions
// ---------------------------------------------------------------------------

/// Apply an [`Action::EmitFromShared`]: copy `len` bytes out of the server's
/// mapping of the shared buffer into one atomic sys_debug_puts, then reply.
/// Splitting the read pointer / length / reply value out of the action lets
/// `dispatch` decide policy (which fds are valid, length cap) without
/// pulling syscalls into lockjaw-types.
///
/// Rejects writes that arrive before POSIX_INIT (server_shared_va == 0)
/// — without the guard, the unsafe slice below would read from VA 0
/// and the kernel would fault. Per the comment on dispatch's write
/// arm, this is a protocol violation; reporting EINVAL is friendlier
/// than crashing the personality server.
fn apply_emit_from_shared(server_shared_va: u64, len: u64, then_reply: u64) {
    if server_shared_va == 0 {
        sys_reply(neg_errno(EINVAL), 0, 0, 0);
        return;
    }
    // SAFETY: server_shared_va is the personality server's mapping of
    // the shared buffer; the child wrote `len` bytes there before the
    // IPC and is blocked waiting on our reply, so the buffer is stable
    // for the duration of this read.
    let data = unsafe {
        core::slice::from_raw_parts(server_shared_va as *const u8, len as usize)
    };
    sys_debug_puts(data);
    sys_reply(then_reply, 0, 0, 0);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("posix-server: starting\n");

    // --- Bootstrap with init (no handles needed, just complete the handshake) ---
    let reply = match sys_alloc_pages(1).and_then(sys_create_reply) {
        Ok(h) => h,
        Err(_) => { puts("posix: reply alloc FAILED\n"); halt(); }
    };
    let bootstrap = match sys_call_ret4(bootstrap_endpoint(), reply, 0, 0, 0, 0) {
        Ok(r) => r,
        Err(_) => { puts("posix: bootstrap call FAILED\n"); halt(); }
    };
    let fs_ep = EndpointHandle(bootstrap[0]);
    let fs = FsClient::new(fs_ep, reply);
    puts("[BOOTSTRAP] posix-server\n");

    // --- Parse embedded POSIX binary ---
    let elf_info = match parse_elf(POSIX_HELLO) {
        Ok(info) => info,
        Err(_) => { puts("posix: ELF parse FAILED\n"); halt(); }
    };

    // --- Compute dynamic VAs from ELF layout ---
    // Pure decision (and overflow / stack-overlap / mmap-below-stack
    // checks) live in lockjaw_types::posix::compute_va_layout
    // (host-tested). The 4 below matches the stack_pages allocation a
    // few lines down.
    let layout = match compute_va_layout(&elf_info, USER_STACK_BASE, 4, POSIX_MMAP_BASE) {
        Ok(l) => l,
        Err(_) => {
            puts("posix: ELF layout overlaps stack/mmap or overflows\n");
            halt();
        }
    };
    let child_shared_va = layout.child_shared_va;
    let brk_base = layout.brk_base;
    let mmap_base = layout.mmap_base;

    // --- Allocate working pages for ELF loading ---
    let map_array_va = VMEM.alloc(1).expect("VA exhausted");
    let temp_base_va = VMEM.alloc(128).expect("VA exhausted");
    let scratch_ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: scratch alloc FAILED\n"); halt(); }
    };

    let map_array_ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: map array alloc FAILED\n"); halt(); }
    };
    if !sys_map_pages(map_array_ps, map_array_va, MapMemoryAttribute::Normal).is_ok() {
        puts("posix: map array FAILED\n");
        halt();
    }

    // --- Load ELF segments ---
    let mapping_count = load_elf_segments(
        POSIX_HELLO, &elf_info, map_array_va, temp_base_va,
    );

    // --- Build stack with Linux initial layout ---
    let stack_pages: u64 = 4;
    let stack_ps = match sys_alloc_pages(stack_pages) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: stack alloc FAILED\n"); halt(); }
    };
    // Map stack temporarily to write the argc/argv/auxv layout
    let temp_stack_va = VMEM.alloc(stack_pages as usize).expect("VA exhausted");
    if !sys_map_pages(stack_ps, temp_stack_va, MapMemoryAttribute::Normal).is_ok() {
        puts("posix: stack map FAILED\n");
        halt();
    }
    for p in 0..stack_pages {
        unsafe { zero_page_at_va(temp_stack_va + p * PAGE_SIZE); }
    }
    write_stack_layout(temp_stack_va);

    // --- Create syscall endpoint (child gets this at handle 0) ---
    let syscall_ep_ps = match sys_alloc_pages(1) {
        Ok(ps) => ps,
        Err(_) => { puts("posix: ep alloc FAILED\n"); halt(); }
    };
    let syscall_ep = match sys_create_endpoint(syscall_ep_ps) {
        Ok(ep) => ep,
        Err(_) => { puts("posix: ep create FAILED\n"); halt(); }
    };

    // --- Spawn POSIX child ---
    let mut name_buf = [0u8; 16];
    name_buf[..11].copy_from_slice(b"posix-hello");

    puts("posix-server: spawning posix-hello...\n");
    let result = sys_create_process(
        map_array_va,
        mapping_count as u64,
        elf_info.entry_point,
        stack_ps,
        scratch_ps,
        syscall_ep.raw(),
        name_buf.as_ptr() as u64,
    );
    if !result.is_ok() {
        puts("posix-server: spawn FAILED\n");
        halt();
    }
    puts("posix-server: posix-hello spawned OK\n");

    // --- Syscall dispatch loop ---
    let mut server_shared_va: u64 = 0;
    let mut fd_table = FdTable::with_stdio();
    let mut file_resources: [Option<FileResource>; MAX_FDS] = [None; MAX_FDS];
    let mut deferred_closes: [Option<u32>; MAX_DEFERRED_CLOSES] = [None; MAX_DEFERRED_CLOSES];
    let mut mmap_table = MmapTable::new();
    // Bump-only VA allocator from POSIX_MMAP_BASE to USER_VA_END.
    // Phase 2 doesn't reuse freed VA — every mmap claims the next
    // contiguous chunk. Out of ~1 GiB of available range that's
    // plenty for the malloc workloads Phase 2 targets.
    let mut mmap_va_alloc = MmapVaAllocator::new(POSIX_MMAP_BASE, USER_VA_END);

    loop {
        // Retry any FS handles whose close failed in a previous open
        // rollback. Local resources for these were already freed; only
        // the remote slot is still alive.
        drain_deferred_closes(&fs, &mut deferred_closes);

        let msg = match sys_receive_ret4(syscall_ep) {
            Ok(m) => m,
            Err(_) => { puts("posix: receive FAILED\n"); halt(); }
        };

        // Pure decision lives in lockjaw_types::posix::dispatch (host-tested).
        // This loop is mechanical execution of the returned Action.
        let action = dispatch(&DispatchInputs {
            nr: msg[0],
            arg1: msg[1],
            arg2: msg[2],
            arg3: msg[3],
        });

        match action {
            Action::PosixInit => {
                // First call from child — set up shared buffer.
                // Allocate page, map in our VA space (server_shared_va).
                let shared_ps = match sys_alloc_pages(1) {
                    Ok(ps) => ps,
                    Err(_) => { puts("posix: shared alloc FAILED\n"); halt(); }
                };
                server_shared_va = VMEM.alloc(1).expect("VA exhausted");
                if !sys_map_pages(shared_ps, server_shared_va, MapMemoryAttribute::Normal).is_ok() {
                    puts("posix: shared map FAILED\n");
                    halt();
                }
                // Export the PageSet into the blocked child's handle table.
                // sys_export_handle implicitly targets the caller from the
                // last sys_receive (via current_reply_paddr).
                let child_idx = match sys_export_handle(shared_ps) {
                    Ok(idx) => idx,
                    Err(_) => { puts("posix: export shared FAILED\n"); halt(); }
                };
                // Reply: [child's PageSet handle, child VA, brk base, mmap base]
                // mmap_base in word 3 is new in Phase 2.0 — the shim
                // stashes it for use by mmap() once Phase 2.3 wires it
                // up. Until then it's a free pass-through with no
                // user-visible effect.
                sys_reply(child_idx, child_shared_va, brk_base, mmap_base);
                puts("posix-server: POSIX_INIT OK\n");
            }

            Action::EmitFromShared { fd: _, len, then_reply } => {
                apply_emit_from_shared(server_shared_va, len, then_reply);
            }

            Action::Exit => {
                puts("posix-server: child exit\n");
                break;
            }

            Action::Reply { words } => {
                sys_reply(words[0], words[1], words[2], words[3]);
            }

            Action::Unknown { nr } => {
                puts("posix: unknown nr=");
                put_hex(nr);
                puts(" -> ENOSYS\n");
                sys_reply(neg_errno(ENOSYS), 0, 0, 0);
            }

            Action::FileOpen { dirfd: _, path_len, flags } => {
                handle_file_open(
                    &fs, &mut fd_table, &mut file_resources, &mut deferred_closes,
                    server_shared_va, path_len, flags,
                );
            }
            Action::FileRead { fd, len } => {
                handle_file_read(
                    &fs, &fd_table, &file_resources,
                    server_shared_va, fd, len,
                );
            }
            Action::FileClose { fd } => {
                handle_file_close(&fs, &mut fd_table, &mut file_resources, fd);
            }

            // Phase 2.1 stubs. The dispatch arms above (in lockjaw_types)
            // reject malformed shapes with the right errno; what
            // reaches these match arms has already passed validation.
            // Phase 2.3 adds the shim-side caller; until then, no
            // existing client exercises these arms.
            Action::FileMmap { len_bytes, prot: _, flags: _ } => {
                handle_file_mmap(&mut mmap_table, &mut mmap_va_alloc, len_bytes);
            }
            Action::FileMunmap { base_va, len_bytes } => {
                handle_file_munmap(&mut mmap_table, base_va, len_bytes);
            }
            Action::FileMprotect { base_va, len_bytes, prot: _ } => {
                handle_file_mprotect(&mmap_table, base_va, len_bytes);
            }
            Action::FileMadvise { base_va: _, len_bytes: _, advice: _ } => {
                // Hints aren't load-bearing — reply 0 unconditionally
                // regardless of region state.
                sys_reply(0, 0, 0, 0);
            }
            Action::FileMmapRollback { base_va } => {
                handle_file_mmap_rollback(&mut mmap_table, base_va);
            }
        }
    }

    puts("posix-server: done\n");
    sys_exit();
}

// put_hex is imported from lockjaw_userlib (atomic emit).

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    puts("posix-server: PANIC\n");
    halt();
}
