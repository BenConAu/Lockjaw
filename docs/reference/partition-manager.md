# Partition Manager

`user/partition-manager/` sits between a raw block-device driver
(virtio-blk or emmc2) and the filesystem service for the **one**
partition it selects. It parses sector 0, picks one partition
(the lowest-index FAT32 entry from an MBR, or the whole disk for
a bare FAT volume), and exposes a BlockEngine that translates
partition-relative sectors to disk-relative sectors for the
selected slot.

For *why* the partition layer is a separate process at all (rather
than logic inside the block driver or the filesystem), see
[`../architecture/03-non-virtualizable-hardware.md`](../architecture/03-non-virtualizable-hardware.md).
For the BlockEngine protocol it speaks, see
`lockjaw-types/src/block.rs::CMD_*`. For the FS server that's the
current consumer, see [`fat32-server.md`](fat32-server.md).

## Where it sits

```text
[fat32-server]                     <-- BlockEngine consumer
       |
       v   sector reads relative to the selected partition
[partition-manager]                <-- this doc
       |
       v   sector reads relative to disk start
[virtio-blk-driver | emmc2-driver] <-- raw block-device producer
```

Both edges speak the same BlockEngine protocol. The manager is a
thin sector-offset translator plus the LBA-0 classifier. **It
serves exactly one partition per instance** — see "Single-partition
selection" below.

## Sector-0 classifier

`lockjaw_types::partition::parse_disk` at
`lockjaw-types/src/partition.rs:71`:

```rust
pub fn parse_disk(
    sector_zero: &[u8; 512],
    upstream_capacity_sectors: u64,
) -> Result<DiskLayout, PartitionError>;
```

The `upstream_capacity_sectors` arg is what enables the bounds
check that distinguishes a valid MBR from a malformed one — every
non-empty MBR entry must satisfy `start_lba + sector_count <=
upstream_capacity_sectors` (widened to u64 to catch u32 wrap).

`DiskLayout` (`partition.rs:33`):

```rust
pub enum DiskLayout {
    /// LBA 0 is a FAT32 BPB — whole disk is one FAT volume starting at LBA 0.
    BareFat { sector_count: u64 },
    /// LBA 0 is an MBR with a valid 0xAA55 signature.
    Mbr { partitions: [MbrPartition; MAX_MBR_PARTITIONS] },
}
```

`MAX_MBR_PARTITIONS = 4` (`:10`); `MBR_SIGNATURE = 0xAA55` (`:9`).
The BareFat discriminator is the 8-byte "FAT32   " string at
offset 82, NOT a jump-byte heuristic (that would misclassify MBR
boot sectors whose boot code starts with a short jump).

`PartitionError` (`partition.rs:45`):

| Variant | Meaning |
|---|---|
| `Unrecognised` | 0xAA55 missing, OR present-but-not-bare-FAT32 AND at least one non-empty MBR entry is malformed (`sector_count == 0` or `start_lba + sector_count > upstream_capacity`). |
| `NoPartitions` | 0xAA55 present, all four MBR entries have `partition_type == 0`. |

`is_fat32(partition_type)` at `:61` returns true for 0x0B (CHS) and
0x0C (LBA). Real SD cards use 0x0C.

The classifier is pure and host-tested.

## Single-partition selection

`partition-manager/src/main.rs:238-269` picks `(start_lba,
sector_count)` from the classified layout:

- `DiskLayout::BareFat { sector_count }` -> `(0, sector_count)` — whole disk.
- `DiskLayout::Mbr { partitions }` -> walks `partitions.iter()` and
  returns the **first** slot whose `is_fat32(partition_type)` is
  true. "Lowest slot index wins" — real SD cards put the boot
  partition at slot 0.
- No FAT32 partition found, or `parse_disk` error -> halt.

This selection happens **once at startup**. The
`PartitionBlockEngine` then holds the chosen partition's
`(start_lba, sector_count)` for its lifetime. A second filesystem
on a second partition is not served by this instance — that would
need a second partition-manager spawn (init wires one per FS
client today, which is one).

## `PartitionBlockEngine`

`user/partition-manager/src/main.rs:79`:

```rust
struct PartitionBlockEngine {
    upstream:             BlockClient,
    start_lba:            u64,
    sector_count:         u64,
    upstream_buffer_attr: MapMemoryAttribute,
    buf_map:              [Option<BufMapEntry>; MAX_BUFFERS],
}
```

`buf_map` is **single-partition** local-handle → upstream-buffer-id
translation, not multi-partition routing. When the FS server
calls `alloc_buffer`, the manager calls `upstream.alloc_buffer`
to get an `AllocatedBuffer { pageset, buffer_id }`, records the
mapping, and returns the local pageset handle. Subsequent
`read`/`write` operations on that handle look up the upstream
`buffer_id` via `find_slot(buffer)` (`:88`).

## Operations

`PartitionBlockEngine` impls `BlockEngine` (`:95`):

| Method | Line | Behavior |
|---|---|---|
| `info(&self)` | `:96` | Returns `BlockInfo { capacity_sectors: self.sector_count, sector_size: 512, buffer_attribute: self.upstream_buffer_attr }`. Sector size is a hardcoded constant — startup halts at `:206-209` if the upstream's `info.sector_size != 512`. |
| `alloc_buffer(&mut self, sector_count) -> PageSetHandle` | `:104` | Reserves a local slot FIRST (so a full table is caught before allocating upstream), then forwards to upstream `alloc_buffer`, records the mapping, returns the local pageset. |
| `read(&mut self, sector, count, buffer)` | `:120` | **Bounds check** (`:123-127`, comment: "the partition boundary is a security boundary"): `sector.checked_add(count) > self.sector_count` -> `InvalidParameter`. Then translates `upstream_sector = self.start_lba + sector` (also `checked_add` for defense-in-depth) and forwards. |
| `write(&mut self, sector, count, buffer)` | `:141` | Same bounds check + translation as `read`. |
| `free_buffer(&mut self, buffer)` | `:159` | Removes from slot table, frees upstream ref, closes local handle. |

The bounds check in `read`/`write` is the load-bearing security
boundary — a FS server cannot read past its assigned partition
even if it tries.

## Bootstrap

At `_start` (`:179`), partition-manager:

1. Allocates a Reply object.
2. `sys_call_ret4` to its bootstrap endpoint; init replies with
   **two endpoints** (`:192-198`):
   - `partition_srv_ep` — partition-manager's own server endpoint
     for the dispatch loop.
   - `upstream_blk_srv_ep` — the block driver's server endpoint.
3. `upstream.get_info()` to learn the disk's capacity and sector
   size. Halts if `sector_size != 512` (`:206-209`).
4. Allocates a 1-sector bootstrap buffer + a temporary VA, maps,
   and reads LBA 0 (`:212-233`).
5. `parse_disk(sector_zero, info.capacity_sectors)` and the
   single-partition selection (`:236-269`).
6. Tears down the bootstrap buffer (4-step:
   `sys_unmap_pages` -> `VMEM.free` -> `upstream.free_buffer` -> `sys_close_handle`).
7. Constructs `PartitionBlockEngine { ... }` and calls
   `run_block_server(&mut engine, partition_srv_ep)` (`:284`).

The init-side wiring (`user/init/src/main.rs`) creates the
`partition_srv_ep` once and hands the same endpoint to the
filesystem server during its startup — there is no fanout.

## What's not implemented

- **GPT.** Only MBR + bare FAT32 are parsed today. GPT would land
  as a `DiskLayout::Gpt` variant.
- **Multi-partition serving from one instance.** The current
  selection picks one partition at startup; a multi-FS system
  would spawn multiple partition-manager instances (one per
  consumed partition).
- **Dynamic resize / runtime repartitioning.** Boundaries are
  set at boot from sector 0.
- **Writes to disk metadata.** Data writes go through fat32-server
  but no path writes new MBR entries.

## Where it lives

| File | Role |
|---|---|
| `user/partition-manager/src/main.rs` | The server itself. |
| `lockjaw-types/src/partition.rs` | Pure `parse_disk`, `MbrPartition`, `DiskLayout`, `PartitionError`, `is_fat32`. Host-tested. |
| `lockjaw-types/src/block.rs` | BlockEngine `CMD_*` constants this server speaks both up and down. |
| `user/lockjaw-userlib/src/block.rs` | `run_block_server` + `BlockEngine` trait + `BlockClient` + `AllocatedBuffer`. |

## Status

Partition-manager landed in 2026-05 (commits `4b7432d` pure
parser + tests, `784125d` server, `32fc785` init wiring). The
QEMU integration test (`tests/qemu_integration.sh`) and the Pi 4B
flash both exercise the full stack: block driver -> partition-manager
-> fat32-server -> posix-server -> musl client. See
[`../history/partition-manager-plan.md`](../history/partition-manager-plan.md)
for the original design rationale.
