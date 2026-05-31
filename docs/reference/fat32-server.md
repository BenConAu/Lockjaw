# FAT32 Server

`user/fat32-server/` is the userspace filesystem service that
sits between the partition-manager (which gives it a block-engine
handle for a partition) and any client wanting to read files from
a FAT32 volume. Today the only client is posix-server (which
forwards POSIX `openat`/`read` to it).

This doc covers the wire protocol clients use, the cluster-chain
walk model, and the server's state machine. For the partition
manager that feeds it, see
[`partition-manager.md`](partition-manager.md); for the broader
POSIX flow that consumes it, see [`musl-port.md`](musl-port.md).

## The FS wire protocol

Three opcodes (`lockjaw-types/src/fs.rs:40-56`):

| Opcode | Value | Purpose |
|---|---|---|
| `FS_OPEN`  | 1 | Open a file by path; allocate a per-handle read buffer; return a server handle. |
| `FS_READ`  | 2 | Read up to N bytes from the open file into the per-handle buffer. |
| `FS_CLOSE` | 3 | Release the server handle + free the buffer. |

Reply word 0 is one of `FS_OK = 0` or `FS_ERR_*` (`:62-84`):

| Code | Value | Meaning |
|---|---|---|
| `FS_OK`                     | 0 | success |
| `FS_ERR_NOT_FOUND`          | 1 | path missing |
| `FS_ERR_INVALID`            | 2 | unknown command / bad handle / malformed path |
| `FS_ERR_TOO_MANY_OPEN`      | 3 | server table full |
| `FS_ERR_IO`                 | 4 | underlying block read failed |
| `FS_ERR_PATH_TOO_LONG`      | 5 | path_len == 0 or > FS_MAX_INLINE_PATH (16) |
| `FS_ERR_INVALID_BUFFER_PAGES` | 6 | buffer_pages == 0 or > FS_MAX_BUFFER_PAGES (8) |
| `FS_ERR_ALLOC`              | 7 | could not allocate per-handle buffer |
| `FS_ERR_IS_DIRECTORY`       | 8 | path is a directory but caller asked for a file |
| `FS_ERR_NOT_DIRECTORY`      | 9 | intermediate path component is not a directory |

### `FS_OPEN`

The path is **inline** in the 4-word message — there is no
PageSet involved on the request side (the comment at
`fs.rs:14-20` explains why: `sys_export_handle` only goes
server→client, so a client-supplied PageSet would need a new
syscall, and 16 inline bytes covers the 8.3 short-name scope).

```text
Request:  [FS_OPEN, len_packed, path_lo, path_hi]
  len_packed: low byte = path_len (1..=16),
              next byte = buffer_pages (1..=8),
              high 48 bits MUST be zero (reserved).
  path_lo, path_hi: 16 path bytes packed little-endian into two u64s.

Reply:    [status, handle, buffer_pageset_idx, buffer_size_bytes]
```

`buffer_pageset_idx` (word 2) is the handle index in the **caller's**
table for the per-handle read buffer — server allocates the
PageSet, maps it server-side, and calls `sys_export_handle` to
hand the client a handle for the same backing. The client is
responsible for `sys_map_pages` to map it into its own address
space and for `sys_close_handle` after `FS_CLOSE`.

Userspace helpers in `lockjaw-types/src/fs.rs`:

- `pack_open_header(path_len, buffer_pages) -> u64` (`:207`)
- `pack_path(path: &[u8]) -> (u64, u64)` (`:197`)
- `decode_open(req)` (`:176`) — server-side decode.

`FsRequest` (`:104`) is the **server-side decoder** struct for the
4-word message, not a wire DTO put into a PageSet.

### `FS_READ`

```text
Request:  [FS_READ, handle, len, 0]
Reply:    [status, bytes_returned, 0, 0]
```

The server reads from the file's current cursor into the
per-handle buffer, advances the cursor, and replies with the
byte count. `bytes_returned < len` means EOF reached or the
per-handle buffer cap is the smaller of the two.

### `FS_CLOSE`

```text
Request:  [FS_CLOSE, handle, 0, 0]
Reply:    [status, 0, 0, 0]
```

Frees the server-side handle slot + the per-handle buffer
PageSet. The caller is also expected to unmap and close their own
view of the buffer.

## The mount stack

```text
+-----------------------+   FS_OPEN/READ/CLOSE
|     posix-server      | ─────────────────────────► [fat32-server]
+-----------------------+
            │ FsClient (userlib wrapper)
            ▼
+-----------------------+   block-engine ops
|    fat32-server       | ─────────────────────────► [partition-manager]
+-----------------------+
            │ BlockEngine over a partition handle
            ▼
+-----------------------+   block-engine ops
|  partition-manager    | ─────────────────────────► [virtio-blk-driver
+-----------------------+                              or emmc2-driver]
            │ BlockEngine over the raw disk
            ▼
+-----------------------+
|  block-device driver  |
+-----------------------+
```

Each layer speaks BlockEngine to the one below it (see
`lockjaw-types/src/block.rs::CMD_*`) and speaks its own protocol
to the one above. fat32-server's "up" interface is the FS protocol
documented here.

## Mount sequence

At startup, fat32-server:

1. Receives its bootstrap message — **two** endpoint handles
   (`fat32-server/src/main.rs:442-447`): `fs_srv_ep` (this
   server's own endpoint, used for the dispatch loop's `sys_receive`)
   and `blk_srv_ep` (the upstream block-engine endpoint for the
   partition the server is mounting). init wires both at spawn
   time.
2. Reads sector 0 (the BPB) via the block-engine.
3. Calls `lockjaw_types::fat32::parse_bpb(sector0)` at
   `lockjaw-types/src/fat32.rs:136` — pure function that returns a
   `Fat32Geometry` (sectors-per-cluster, FAT start, data region
   start, root cluster) or `Fat32Error`.
4. Validates `cluster_count >= FAT32_MIN_CLUSTERS = 65525`
   (`:79`) — rejects forged "FAT32" signatures on a FAT16-sized
   volume.
5. Enters the dispatch loop for FS_OPEN / FS_READ / FS_CLOSE.

The geometry is read once and held for the volume's lifetime.

## Cluster-chain walk

`user/fat32-server/src/main.rs:71::fat_next(cluster) -> Result<FatEntry, u64>`
is the workhorse: given the current cluster number, read the
corresponding FAT entry to get the next cluster (or end-of-chain
/ bad-sector sentinel). `FatEntry` distinguishes those cases.

Reading a file is then a per-cluster loop:
- Translate cluster -> sector via `fat32::cluster_to_sector(cluster, geom)`
  at `lockjaw-types/src/fat32.rs:278`.
- `read_cluster(cluster)` (`fat32-server/src/main.rs:51`) issues
  the block-engine CMD_READ for the cluster's worth of sectors.
- Copy bytes from `cluster_bytes()` (`:62`) into the per-handle
  buffer at the current offset.
- Walk `fat_next` to advance.

Directory traversal uses the same walk: `lookup_in_dir(mount, dir_first_cluster, name)`
(`:145`) walks the directory's cluster chain looking for matching
8.3 entries; `resolve_path` (`:111`) chains
component-by-component down a path.

## Server state

The server holds one `OpenTable` for the volume (`:191`). Each
slot is an `OpenFile` (path, first_cluster, current_cluster,
cursor, per-handle buffer's PageSet+VA, the caller's IPC token).

Per-handle ownership is checked on every op:

```rust
fn get(&self, handle: u32, caller_token: u64) -> Option<&OpenFile>;     // :216
fn get_mut(&mut self, handle, caller_token) -> Option<&mut OpenFile>;   // :225
fn remove(&mut self, handle, caller_token) -> Option<OpenFile>;          // :234
```

A handle issued to one client cannot be operated on by another —
the `caller_token` check (compared against
`sys_query_caller_token` for the current IPC) makes cross-caller
handle reuse return `FS_ERR_INVALID`. See
[`../architecture/02-handle-identity-tokens.md`](../architecture/02-handle-identity-tokens.md)
for the identity model.

## Per-handle buffer model

FAT32 reads are big — one cluster is 4 KiB to 32 KiB typically.
The server doesn't have a single shared buffer that all clients
read into; each `FS_OPEN` allocates a per-handle PageSet of size
`buffer_pages * 4 KiB`, **maps it server-side** for the cluster
copy step, and exports it to the client via `sys_export_handle`
so the client can map it on its own side. The buffer is freed on
`FS_CLOSE` (server-side) — the client is responsible for closing
its own PageSet handle separately (`sys_close_handle`).

The client-side wrapper `FsClient` in `lockjaw-userlib`
(`user/lockjaw-userlib/src/fs.rs:54`) does NOT hide this — its
`open()` returns:

```rust
pub struct OpenedFile {
    pub handle:      u32,             // server-assigned, for read/close
    pub pageset:     PageSetHandle,   // in client's table
    pub buffer_size: u32,             // bytes per read
}
```

(at `lockjaw-userlib/src/fs.rs:41`). The client must call
`sys_map_pages(opened.pageset, ...)` itself to map the buffer and
`sys_close_handle(opened.pageset)` after `FsClient::close()` to
release the client-side reference. The wrapper docstring at
`fs.rs:36-39` is explicit about this client obligation.

`buffer_pages` is negotiated at OPEN time. Bounds:
`1..=FS_MAX_BUFFER_PAGES = 8` (`fs.rs:96`). Out-of-range -> `FS_ERR_INVALID_BUFFER_PAGES`.

## Where it lives

| File | Role |
|---|---|
| `user/fat32-server/src/main.rs` | The server itself: dispatch, mount, cluster walk, handle table. |
| `lockjaw-types/src/fat32.rs` | Pure FAT32 model: `parse_bpb`, `cluster_to_sector`, `Fat32Geometry`, `Fat32Error`. Host-tested. |
| `lockjaw-types/src/fs.rs` | FS wire protocol: opcodes, status codes, FsRequest struct, FsAction decision enum. |
| `user/lockjaw-userlib/src/fs.rs` | Client-side `FsClient` wrapper. |
| `user/posix-server/src/main.rs` | The current FS client (translates POSIX open/read into FS ops). |

## What's NOT done

- **Write paths.** No `FS_WRITE`. The protocol has space for it
  but the server is read-only today.
- **Directory listing.** `lookup_in_dir` exists for path
  resolution but there is no `FS_GETDENTS` analogue.
- **Long file names.** Inline path cap is 16 bytes including any
  separators. Anything longer returns `FS_ERR_PATH_TOO_LONG`.
- **Multi-volume.** The server mounts one volume per process; if
  init wires two FAT32 partitions, it spawns two fat32-server
  instances.
- **Concurrent access on one handle.** Per-handle state isn't
  locked — a single client serializes its own ops. Two clients on
  the same volume have separate handles and separate buffers,
  so cross-client interference is impossible.

Each is a scope cut, not a design issue; the protocol leaves room
for the read-only/write/getdents extensions when needed.
