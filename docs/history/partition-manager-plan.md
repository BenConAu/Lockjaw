# Partition manager — design

## Why

Today `fat32-server` reads LBA 0 of whatever block server `init`
wires it to. On QEMU that's `test.img`, a bare FAT32 filesystem
(BPB at offset 0). On a real Pi SD card it's an MBR (partition
table at offset 0, FAT32 BPB at LBA 2048). The first Pi flash of
the M7-followup wiring hit this exact case: emmc2 selftest passed,
fat32 `BPB parse FAILED`.

The wrong fixes:

- **Parse MBR inside fat32-server** — bakes MBR knowledge into
  every filesystem server we ever write. ext4, NTFS, exFAT all
  duplicate the same code or share a helper. Either way, the
  layering is wrong.
- **Plumb `start_lba` through init** — puts disk-geometry concern
  into `init`. `init` is the most-load-bearing process; anything
  added there grows tendrils that are painful to migrate later.

The right answer: a **partition-manager** process between the raw
block server and each filesystem server. Reads the partition table,
exports per-partition `BlockEngine`-shaped endpoints, translates
`read(lba)` → `upstream.read(lba + start)`. Filesystem servers see
a bare block device starting at LBA 0.

## Non-goals (this milestone)

- GPT support — MBR only. GPT slots in as a second parser in
  `lockjaw-types::partition` later; same downstream interface.
- Hot-plug / dynamic mount — partition table parsed once at
  bootstrap.
- Writing partitions — read-only metadata; the engine still passes
  through `CMD_WRITE` so consumers can write within a partition.
- Multi-partition routing — MVP exposes ONE partition endpoint
  (the first FAT32 found). Extending to per-partition endpoints is
  additive (new CMD codes), not a redesign.
- Cross-device partition table (e.g., LVM, RAID) — not on the
  roadmap.

## Architecture

```
                  init
                    |
                    | (bootstrap: hands upstream + partition_srv_ep)
                    v
                                   sys_call
   block-server  <-----  partition-manager  <-----  fat32-server
   (emmc2 or       BlockClient   (translates       BlockClient
    virtio-blk)                  lba+start_lba)
```

- **`init`** allocates a `partition_srv_ep` endpoint. Spawns
  `partition-manager`, hands it the upstream `blk_srv_ep` (whichever
  the DTB probe selected — `emmc2_blk_srv_ep` on Pi,
  `blk_srv_ep` for virtio-blk on QEMU) plus its own
  `partition_srv_ep`. Hands `partition_srv_ep` (as a send-cap) to
  `fat32-server` in fat32's bootstrap reply, replacing the direct
  block-server reference.
- **`partition-manager`** at startup:
  1. Builds a `BlockClient` against the upstream `blk_srv_ep`.
  2. `upstream.get_info()` → `buffer_attribute` (forwarded to its
     own clients verbatim — pass-through), `sector_size`,
     `capacity_sectors`. **Reject upstream with `sector_size !=
     512`**: the entire stack (block protocol constant,
     `fat32::parse_bpb`, both block drivers) hardcodes 512. Don't
     silently lie in `info()` — log and halt so the assumption
     break is visible at startup, not as data corruption later.
  3. Allocates a 1-sector buffer via `upstream.alloc_buffer(1)`,
     maps NC/Normal per the upstream's `buffer_attribute`, reads
     LBA 0 via `upstream.read(0, 1, buf)`.
  4. Calls `lockjaw_types::partition::parse_disk(&sector_zero,
     upstream_capacity_sectors)`:
     - Requires `0xAA55` at bytes 510-511 (both MBR and FAT BPB
       carry it). Absence → `Err(Unrecognised)`.
     - **Strong FAT discriminator**: bytes 82-90 == `"FAT32   "`
       (the same filesystem-type string `fat32::parse_bpb` uses
       at `lockjaw-types/src/fat32.rs:140-142`). A naïve
       jump-bytes (`0xEB ?? 0x90`) check would misclassify MBR
       boot sectors whose code starts with a jump instruction —
       the "FAT32   " string is what makes the test robust. On
       match → `Ok(BareFat { sector_count: upstream_capacity })`.
     - Else: parse the four 16-byte MBR entries at byte 446.
       Each non-empty entry (`partition_type != 0`) is bounds-
       checked: both `start_lba` and `sector_count` are widened
       to `u64` before addition (the sum of two `u32`s never
       overflows `u64`, but a naïve `u32` add could wrap silently
       past the capacity check), then `start + count >
       upstream_capacity_sectors` → `Err(Unrecognised)`. A typed
       entry with `sector_count == 0` is also `Err(Unrecognised)`
       — don't half-advertise a malformed table. If all four
       entries have `partition_type == 0` → `Err(NoPartitions)`.
       Otherwise → `Ok(Mbr { partitions })`.
  5. Selects the partition to serve (MVP: first FAT32-typed entry
     in MBR mode; the synthesized whole-disk partition in BareFat
     mode).
  6. Tears down the bootstrap buffer fully, in this order
     (matches `user/fat32-server/src/main.rs:472`):
     1. `sys_unmap_pages(pageset, va)` — drops the VA mapping.
     2. `VMEM.free(va, 1)` — releases the VA reservation.
     3. `upstream.free_buffer(buffer_id)` — drops upstream
        server-side ref.
     4. `sys_close_handle(pageset)` — drops the local handle-
        table entry.
     Skipping any of these leaks a per-bootstrap resource
     (mapping, VA range, upstream slot, or handle slot). The full
     four-step dance is explicit because each resource is owned
     by a different layer.
  7. Builds a `PartitionBlockEngine` (see below) and enters
     `run_block_server(&mut engine, partition_srv_ep)`.
- **`fat32-server`** is **unchanged**. It receives an endpoint, does
  `get_info()`, allocates buffers, reads/writes sectors. From its
  perspective the disk starts at LBA 0 and has exactly the partition's
  sector count.

## API: `lockjaw-types::partition`

Pure logic; allocation-free; host-tested.

```rust
pub const MBR_SIGNATURE: u16 = 0xAA55;
pub const MAX_MBR_PARTITIONS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbrPartition {
    pub partition_type: u8,
    pub start_lba: u32,    // MBR encodes 32-bit LBAs
    pub sector_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskLayout {
    /// LBA 0 is a FAT BPB — the whole disk is one FAT volume
    /// starting at 0. `sector_count` is the upstream's reported
    /// capacity, threaded in by the caller.
    BareFat { sector_count: u64 },
    /// LBA 0 is an MBR with 0xAA55 signature. `partitions[i]` is
    /// the i-th 16-byte entry; `partition_type == 0` means slot
    /// is empty (skip).
    Mbr { partitions: [MbrPartition; MAX_MBR_PARTITIONS] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionError {
    /// 0xAA55 missing, OR present but not bare-FAT and at least one
    /// non-empty MBR entry is malformed: `sector_count == 0` (typed
    /// but empty geometry), or `start_lba + sector_count` (u64-widened)
    /// exceeds the upstream capacity. Refuse to half-advertise a
    /// malformed table.
    Unrecognised,
    /// 0xAA55 present, all four MBR entries have partition_type=0.
    NoPartitions,
}

/// Parse LBA 0. `sector_zero` must be exactly 512 bytes.
/// `upstream_capacity_sectors` is stored in the `BareFat` variant
/// AND used to bounds-check each non-empty MBR entry (any
/// `start_lba + sector_count` exceeding capacity → `Unrecognised`).
pub fn parse_disk(
    sector_zero: &[u8; 512],
    upstream_capacity_sectors: u64,
) -> Result<DiskLayout, PartitionError>;

/// MBR partition-type byte recognisers.
pub fn is_fat32(partition_type: u8) -> bool {
    // 0x0B = FAT32 CHS, 0x0C = FAT32 LBA. We accept both; modern
    // SDs use 0x0C.
    matches!(partition_type, 0x0B | 0x0C)
}
```

### Host tests (`#[cfg(test)]`)

- `parse_disk_bare_fat_recognised_by_fat32_string` — synthetic
  sector with `"FAT32   "` at offset 82 and `0xAA55` at 510-511
  returns `BareFat { sector_count }` with the passed capacity.
- `parse_disk_mbr_boot_code_with_eb_xx_90_not_classified_as_fat`
  — **THE regression test** (per codex): synthetic MBR with
  realistic `0xEB ?? 0x90` boot-code bytes at offset 0 but NO
  `"FAT32   "` string and valid partition entries → must return
  `Mbr`, not `BareFat`. A jump-bytes-only discriminator would
  fail this test; the FAT32 string check passes it.
- `parse_disk_mbr_signature_only_no_entries` — `0xAA55` at the
  right offset, all entries type-0, no FAT32 string →
  `NoPartitions`.
- `parse_disk_mbr_single_fat32_lba_partition` — one type-0x0C
  entry at slot 0 → `Mbr` with start_lba=2048, count=N, type=0x0C.
- `parse_disk_mbr_four_partitions_finds_all` — all four slots
  populated; all parsed; entries with type 0 between non-empties
  preserved (caller decides what to do).
- `parse_disk_mbr_partition_end_equals_capacity_accepted` —
  off-by-one boundary: `start_lba + sector_count == capacity` must
  pass (the `>` check, not `>=`).
- `parse_disk_mbr_one_good_entry_plus_out_of_range_rejected` —
  one valid + one past-disk entry → whole table rejected as
  `Unrecognised` (don't-half-advertise under multi-entry).
- `parse_disk_mbr_hole_between_entries_preserved` — empty slot
  between non-empties → preserved as `partition_type == 0`.
- `parse_disk_mbr_typed_entry_with_zero_sectors_rejected` — typed
  entry with `sector_count == 0` → `Unrecognised`.
- `parse_disk_mbr_all_empty_with_garbage_in_unused_fields_returns_no_partitions`
  — emptiness is keyed strictly off the type byte; garbage
  start_lba/sector_count in cleared slots must not trigger
  validation.
- `parse_disk_mbr_partition_extends_past_disk_rejected` —
  start_lba + sector_count > upstream_capacity → `Unrecognised`.
- `parse_disk_mbr_partition_arithmetic_overflow_rejected` —
  `start_lba = u32::MAX`, `sector_count = 1`: a naïve u32 add
  would wrap to 0 and pass the capacity check, but the u64-widened
  sum (4_294_967_296) correctly exceeds capacity → `Unrecognised`.
- `parse_disk_random_data_returns_unrecognised` — neither FAT32
  string nor 0xAA55.
- `parse_disk_no_aa55_signature_unrecognised` — has `"FAT32   "`
  string but missing 0xAA55 → `Unrecognised` (don't trust a
  half-shaped header).
- `is_fat32_recognises_0b_and_0c` — type byte recogniser.

(All synthetic 512-byte arrays in tests; no fixtures.)

## API: `user/partition-manager`

```rust
struct PartitionBlockEngine {
    upstream: BlockClient,
    start_lba: u64,
    sector_count: u64,
    upstream_buffer_attr: MapMemoryAttribute,
    // Map: when alloc_buffer returns PageSetHandle to client, we got
    // it from upstream with buffer_id Y. Future read/write/free
    // need to use Y for upstream calls.
    buf_map: [Option<BufMapEntry>; MAX_BUFFERS],
}

struct BufMapEntry {
    /// PageSet handle as seen in *this* process's handle table.
    pageset_local: u64,
    /// Buffer ID assigned by the upstream server.
    upstream_buffer_id: u32,
}
```

`MAX_BUFFERS` matches upstream — emmc2's `MAX_DMA_BUFFERS=8` and
virtio-blk's equivalent. 8 is enough for one filesystem consumer
with a couple of scratch buffers.

`impl BlockEngine for PartitionBlockEngine`:

- `info()` → `BlockInfo { capacity_sectors: self.sector_count,
  sector_size: 512, buffer_attribute: self.upstream_buffer_attr }`.
- `alloc_buffer(n)`:
  1. **Reserve the local slot FIRST**: scan `buf_map` for an
     empty entry. None free → return `Err(AllocFailed)` without
     touching the upstream. This ordering matters: if upstream
     alloc came first and then the local slot was full, the
     upstream allocation would leak (no path to find or free it
     again).
  2. With a local slot reserved, call `self.upstream.alloc_buffer(n)`.
     On `Err` → release the slot reservation, return
     `Err(AllocFailed)`.
  3. Stash the returned `(pageset, buffer_id)` in the reserved
     slot, return `pageset`.
- `read(sector, count, ps)`:
  - Bounds-check via `checked_add`: `sector.checked_add(count)
    .ok_or(InvalidParameter)? <= self.sector_count` → otherwise
    `Err(InvalidParameter)`. The partition boundary is the
    security boundary; without our check the upstream would
    happily read off the partition into the next one, leaking
    data. Same overflow discipline `emmc2-driver` uses at
    `user/emmc2-driver/src/main.rs:1246`.
  - Translate via `checked_add` again:
    `self.start_lba.checked_add(sector)`. The startup partition
    validation already proved `start_lba + sector_count <=
    upstream_capacity`, so an in-bounds sector cannot push
    `start_lba + sector` past the disk — the second checked_add
    is defense-in-depth against an engine bug.
  - Look up `buffer_id` from `ps.0` in `buf_map`. Call
    `self.upstream.read(self.start_lba + sector, count,
    buffer_id)`.
- `write(...)` mirrors `read`.
- `free_buffer(ps)`: look up `buffer_id`, call
  `self.upstream.free_buffer(buffer_id)` AND
  `sys_close_handle(ps)` (drops the local handle), clear the
  slot. `run_block_server` invokes this on both normal client
  free AND export-failure paths
  (`user/lockjaw-userlib/src/block.rs:165`), so the close must
  not be conditional.

The two-level buffer indirection (this engine's `buf_map` +
`run_block_server`'s `BufferTracker`) is intentional: each layer
namespaces buffer IDs for its own clients. The outer
`BufferTracker` translates fat32-server's `buffer_id` to
PageSetHandle. This engine's `buf_map` translates PageSetHandle
back to upstream's `buffer_id`. Each is bounded by `MAX_BUFFERS`;
slot exhaustion returns `AllocFailed`.

## init wiring change

`init` already allocates `emmc2_blk_srv_ep` and `blk_srv_ep`,
selects between them via the DTB probe (`has_compatible_hash`).
After this change:

- `init` allocates a new `partition_srv_ep`.
- `init` allocates a new `partmgr_boot_ep` for partition-manager's
  bootstrap.
- `init` spawns `partition-manager` (new ELF) after the chosen
  block server is up.
- `init` exports `(upstream_blk_srv_ep, partition_srv_ep)` to
  partition-manager in its bootstrap reply.
- `init` hands `partition_srv_ep` (instead of
  `active_blk_srv_ep`) to `fat32-server` in fat32's bootstrap
  reply.

**Strict bootstrap ordering invariant** (load-bearing per codex —
same shape as the existing emmc2-before-fat32 reorder at
`user/init/src/main.rs:633`):

1. Raw block server (`emmc2-driver` or `virtio-blk-driver`)
   bootstraps and reaches `run_block_server` (its receive loop).
2. `init` spawns `partition-manager`, bootstraps it with
   `(upstream_blk_srv_ep, partition_srv_ep)`, **waits for
   partition-manager's bootstrap reply confirming it has read
   LBA 0 and entered its own `run_block_server`**. Only then is
   `partition_srv_ep` ready to serve.
3. `init` spawns `fat32-server` with `partition_srv_ep`.
   fat32-server's first `get_info()` `sys_call` now reaches a
   partition-manager already in receive state.

"Endpoint exported" is not enough: the target must already be in
its receive loop before the downstream client does its first
`sys_call`. Skipping the explicit wait re-creates the same
deadlock one layer higher.

DTB probe stays in `init` because it picks WHICH block server
exists; partition-manager doesn't care which upstream it talks
to, only that it talks to one. The probe is hardware-presence
discovery; partition-manager is software-layout interpretation.
Different concerns, different homes.

## QEMU bare-FAT path

`test.img` is a bare FAT32 filesystem starting at offset 0.
partition-manager reads LBA 0, sees a BPB jump, returns
`BareFat { sector_count }`, builds a `PartitionBlockEngine` with
`start_lba=0`. Every `read(s, c, b)` becomes
`upstream.read(0 + s, c, b)` — semantically equivalent to today's
direct wiring. QEMU integration test (95/95) must stay green.

## QEMU partitioned test (new)

To exercise the actual MBR-parsing path in QEMU, add a second
test image: `partitioned.img` = 1 MB MBR + 64 MB FAT32 partition
at LBA 2048. Existing `test-img` Makefile recipe extended to
build both. New integration assertion: `fat32-test` reads
`/HELLO.TXT` through the partition-manager → virtio-blk chain
with the same expected contents.

This is the host-equivalent of the Pi case and lets us catch
MBR-layout regressions without flashing.

## What this enables next

- **emmc2 resume**: with partition-manager handling LBA
  translation, the stashed CMD17/CMD18 dual-path work re-applies
  cleanly. `fat32-server` → `partition-manager` →
  `emmc2-driver` (CMD17 single-block path) reads the BPB at the
  partition's start LBA and continues.
- **GPT** is the next parser added to `lockjaw-types::partition`.
  partition-manager grows a check ("MBR signature + protective MBR
  partition type 0xEE? → GPT path"), same downstream interface.
- **Per-partition endpoints**: when we have a second filesystem
  type (or want to mount a swap partition, etc.), add
  `CMD_GET_PARTITION_COUNT` / `CMD_OPEN_PARTITION(n)` to expose
  multi-partition routing. The current code structure (single
  `PartitionBlockEngine` per active partition) extends to N
  engines per partition-manager.

## Risks

1. **Buffer attribute pass-through.** partition-manager forwards
   the upstream's `buffer_attribute` verbatim to its clients. If
   upstream is `NormalNonCacheable` (emmc2), fat32-server gets
   NormalNonCacheable. Correct: buffers physically come from
   upstream's pool, so the attribute must match. The contract
   added in M7-followup is what makes this work.
2. **Buffer lifetime across reboots/restarts.** If
   partition-manager dies, the upstream still holds buffers
   allocated by partition-manager's `BufMapEntry`s. No automatic
   recovery in MVP — partition-manager death is unrecoverable
   (the same is true of fat32-server today). Acceptable for now;
   a future "block-stack supervisor" can restart and re-discover.
3. **MBR with no FAT32 entry on Pi.** A user reformats their SD
   to ext4 — partition-manager returns "no FAT32 partition
   found", refuses to serve. fat32-server times out on its first
   `get_info()` IPC. Acceptable; the message in
   partition-manager's startup log makes the cause clear.
4. **PartitionBlockEngine.read out-of-range.** A caller asks for
   sectors past the partition end. Our bounds check catches it
   → `InvalidParameter`. Without our check, the upstream would
   read off the partition into the next one, leaking data. This
   is the security boundary the partition layer enforces.
5. **`buf_map` slot exhaustion.** `MAX_BUFFERS=8` (matches
   upstream caps). If fat32-server somehow allocates more than 8
   buffers, `alloc_buffer` returns `AllocFailed` — and crucially
   does so BEFORE allocating from upstream (slot-reserve-first
   ordering in `alloc_buffer`), so the failure path never leaks
   upstream resources. Current fat32-server uses 3 (BPB + cluster
   scratch + FAT scratch); plenty of headroom.

## File layout

```
docs/history/partition-manager-plan.md                      (this doc)
lockjaw-types/src/partition.rs                      (~150 LOC + tests)
lockjaw-types/src/lib.rs                            (mod partition;)
user/partition-manager/Cargo.toml
user/partition-manager/src/main.rs                  (~250 LOC)
user/init/src/main.rs                               (~+30 LOC)
user/lockjaw-userlib/src/lib.rs                     (re-export partition?)
Cargo.toml                                          (workspace member)
Makefile                                            (build target +
                                                     partitioned.img recipe)
tests/qemu_integration.sh                           (partitioned-path
                                                     assertion)
```

## Test plan summary

- `cargo test -p lockjaw-types --target aarch64-apple-darwin` —
  new `partition` module's host tests pass.
- `make test-qemu-gicv3` — 95/95 (existing bare-FAT path) +
  the new partitioned-disk integration line.
- Pi 4B flash — fat32-test reads `/HELLO.TXT` from the FAT32
  partition on the boot SD, end-to-end through
  fat32 → partition-manager → emmc2.

## Code volume estimate

- `lockjaw-types::partition`: ~120 LOC parser + range validation
  + zero-sector rejection + FAT32-type helper + ~190 LOC tests
  (15 cases incl. the EB-XX-90-not-BareFat regression, overflow,
  exact-end boundary, mixed valid+invalid, hole-in-table,
  zero-sector typed, garbage-in-empty-slots).
- `user/partition-manager`: ~280 LOC (bootstrap + dispatch loop
  via `run_block_server` + `PartitionBlockEngine` impl + handle-
  close paths + sector_size validation + logs).
- `init` modification: ~30 LOC (endpoint alloc + spawn + reply
  wiring).
- Makefile / qemu test: ~40 LOC.

Total ~560 LOC (codex re-estimate after blocking issues: 520-620),
of which ~210 are host-tested pure logic and ~280 are mechanical
IPC.

## Open questions for codex

1. **Single endpoint vs N endpoints in MVP.** I lean single
   ("the FAT32 partition") because it's the actual need today and
   the extension to N is additive. Is there a future cost I'm
   missing?
2. **`BareFat` synthesis.** Is "treat bare FAT as a single
   whole-disk partition" the right semantic, or should
   partition-manager refuse to wrap a non-partitioned disk and
   require init to bypass it on QEMU? My instinct says the
   uniform-interface argument wins: every block-server consumer
   talks to a partition-manager, no special cases in init.
3. **Buffer-id translation layer.** Acceptable to have two
   `MAX_BUFFERS=8` slot tables (one in `BufferTracker`, one in
   `PartitionBlockEngine.buf_map`), or worth refactoring
   `BlockEngine` so engines can introspect their own
   `BufferTracker`? I lean leave-it-alone for MVP.
4. **Bounds checking semantics.** PartitionBlockEngine rejects
   out-of-range reads with `InvalidParameter`. Should it instead
   pass through and let the upstream return whatever it returns?
   I argue the partition boundary is the security boundary
   (preventing inter-partition reads); blunt rejection is
   correct.
5. **Anything that should be in lockjaw-types but isn't here?**
   E.g., is `PartitionBlockEngine`'s structure host-testable
   without an actual upstream? I think no — the IPC is the
   interesting part — but if you see a way to lift more logic
   out, flag it.

## Out-of-scope follow-ups

- GPT parsing (add `parse_gpt` next to `parse_disk` in
  `lockjaw-types::partition`; partition-manager dispatches on
  first-sector content).
- Multi-partition simultaneous mount (add `CMD_OPEN_PARTITION` to
  partition-manager's IPC).
- Block-stack supervisor for restart-on-crash recovery.
- Cross-device assembly (LVM, RAID).
- Write boundary enforcement: today we let writes go to any
  in-bounds sector; future work could mark partitions read-only
  via a flag at partition-manager construction.
