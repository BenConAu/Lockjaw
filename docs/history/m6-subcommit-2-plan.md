# M6 sub-commit 2: NormalNonCacheable MAIR slot + ADMA2 data path

## Open design problem (must resolve before coding)

The M6 plan (`docs/history/emmc2-block-storage-plan.md`, lines 549–559) says
"audit the MAIR table; add a Normal-Non-Cacheable entry if not present"
and map ADMA2 buffers with `NormalNonCacheable` via `sys_map_pages`.

This introduces a **mixed-attribute aliasing bug**, flagged by codex
during the M6 sub-commit 1 design review:

- Pages allocated via `sys_alloc_pages` are buddy-allocated RAM.
- Buddy-allocated RAM is already mapped in the kernel TTBR1 direct map
  as cacheable Normal (`KERNEL_VA_OFFSET + PA`).
- A `NormalNonCacheable` mapping in TTBR0 for the same PA creates two
  attribute regimes for the same physical page.
- ARMv8 architecturally permits but does not guarantee correct behavior
  across mismatched attributes. In practice: speculative cache fills via
  the cacheable mapping can return stale data after a DMA write; writes
  via the cacheable mapping may never reach RAM before a DMA read.

The plan picked NC mapping over cache maintenance (line 591–594) for
simplicity, but did not address how to make the NC mapping safe in the
presence of the direct map.

## Three options

### Option A — Cache maintenance (Linux's choice)
- Keep DMA buffers mapped cacheable Normal (no NC mapping ever).
- Add userspace helpers: `dc cvac` before host→device write,
  `dc civac` before/after device→host read.
- **Pros**: No kernel surface change beyond MAIR slot reservation.
  Works with existing PageSet model.
- **Cons**: Per-transfer maintenance is bug-prone. The Linux
  `dma_map_single` abstraction exists precisely because getting this
  right at every transfer is hard. Conflicts with the M6 plan's
  stated direction (NC mapping over cache maintenance).

### Option B — DMA pool (carve out from direct map)
- Reserve a DMA pool at kernel boot (proposed: 4 MiB = 1024 pages).
- Pool pages are excluded from kernel direct map → no aliasing
  possible.
- Add `sys_alloc_dma_pages(count)` syscall that allocates from this
  pool and returns `(PageSetHandle, phys_addr)`.
- `sys_map_pages` with `NormalNonCacheable` is only valid on
  DMA-pool PageSets; rejects others with `INVALID_PARAMETER`.
- **Pros**: No aliasing. Clean separation. Phys addr available to
  userspace exactly when needed (driver programming DMA registers).
- **Cons**: New syscall. Fixed pool size (pick generous, accept
  waste). Pool management code in kernel.

### Option C — On-demand unmap from direct map
- When `sys_map_pages` with `NormalNonCacheable` is called, kernel
  unmaps those PAs from TTBR1 direct map.
- When PageSet is freed/unmapped, remap them cacheable.
- Requires splitting direct map's 2 MiB blocks to L3 tables on first
  NC use; rejoin on last NC unmap.
- **Pros**: No fixed pool; any allocated page can be NC.
- **Cons**: Complex bookkeeping. TLB shootdown across CPUs. Bug
  surface in the split/rejoin code. The kernel direct map's "all RAM
  is always reachable" invariant goes away.

## Recommendation

**Option B**, for these reasons:

- Honors the plan's NC-mapping decision (line 591–594).
- Option A would require revisiting that plan-level decision and
  rebuilding the fix as cache maintenance everywhere.
- Option C's bookkeeping (split/rejoin, multi-CPU TLB shootdown,
  invariant break) doesn't pay for itself at Lockjaw's scale.
- M6's emmc2 use case needs ~4 KiB descriptor table + 4–32 KiB DMA
  buffer per transfer — fits trivially in a 4 MiB pool.
- A typed `SyscallError::DMA_POOL_EXHAUSTED` makes pool sizing a
  surfaceable failure mode rather than a silent corruption.

## Splitting sub-commit 2

The work is too big for a single atomic commit. Split into 2a and 2b:

### Sub-commit 2a: substrate (kernel + types + userlib)

No driver changes; existing PIO paths keep working. The MapMemory­
Attribute enum gains the `NormalNonCacheable` variant, the kernel
gains a DMA pool, but no user code maps anything NC yet.

### Sub-commit 2b: emmc2-driver ADMA2 path

Once 2a lands, emmc2-driver allocates from the DMA pool, builds an
ADMA2 descriptor, and reads LBA 0 via DMA instead of PIO. Verified on
Pi 4B with a wall-clock comparison vs the M4 PIO read.

## DMA-pool isolation principle (revised after codex review)

The pool isn't just "excluded from direct map" — it's **structurally
isolated from every kernel path that would map a page cacheable**.
Codex flagged four such paths today:

- `src/mm/page_alloc.rs:141`: zero-initialises pages via the direct
  map (`paddr + KERNEL_VA_OFFSET`).
- `src/process.rs:361`: process scratch pages mapped via direct map.
- `src/syscall/handler.rs:137`: donated kernel objects initialised
  via raw paddr (direct-map writes).
- `lockjaw-types/src/kvm.rs:59`: donated PageSet data pages re-mapped
  cacheable Normal in KVM.

A "DMA pool excluded from direct map" plan that doesn't also cordon
off these paths just turns silent corruption (alias) into kernel
faults (write to unmapped paddr) the moment a DMA-pool PageSet hits
any of them.

Three rules govern DMA-pool pages from this commit forward:

1. **Allocation source is the pool, not buddy.** `sys_alloc_dma_pages`
   does not call into the buddy allocator. Free returns to the pool.
2. **Origin travels with the PageSet, not just the handle.** The
   origin tag lives in the page-resident PageSet *header* and on
   `PageSetEntry`, so the free path (which operates on `header_kva`
   alone after handle removal — see `src/cap/pageset_table.rs:303`
   and `:418`) knows which pool to return pages to.
3. **DMA-pool PageSets are rejected by every kernel path that would
   touch them cacheable.** `sys_map_pages` only allows `NormalNon­
   Cacheable`. `create_process` rejects them. The KVM remap in
   `lockjaw-types/src/kvm.rs:59` rejects them. The donated-kernel-
   object path in `src/syscall/handler.rs:137` rejects them. The
   page zero-init in `src/mm/page_alloc.rs:141` is bypassed for
   pool pages — pool init zeroes once at boot under the identity
   map, before the MMU is enabled and before the direct map exists.

Rule 3 is the load-bearing one. Without it, a caller can hand a
DMA-pool PageSet to `create_process` (or any other cacheable-mapping
path) and reintroduce the alias. The substrate must reject — typed
errors at every entry point — not document.

## Sub-commit 2a — detailed steps

### Step 1: Kernel — reserve DMA pool, separate from buddy

- `src/main.rs` (or wherever boot mem-init lives): reserve
  `DMA_POOL_PHYS_BASE..+DMA_POOL_SIZE` *before* buddy registers free
  ranges. Pool pages never enter buddy.
- Pool sized at 4 MiB initial (1024 pages). Pinned constant in
  `lockjaw-types/src/dma_pool.rs` so future bumps surface in the
  test pin.
- Boot-time zero-init of the pool happens under the identity map
  before MMU enable, so it doesn't touch the direct map. After MMU
  enable, the kernel never writes pool pages cacheably.
- New module `lockjaw-types/src/dma_pool.rs` (pure): free-list
  representation + `alloc(count) -> Option<PhysAddr>` /
  `free(phys, count)`. Host-tested. Mirrors the buddy split pattern.
- New module `src/cap/dma_pool.rs`: kernel-side wrapper that holds
  the singleton allocator state and grabs a lock. Same shape as
  existing `src/cap/pageset_table.rs`.

### Step 2: Kernel — exclude pool from direct map

- `src/arch/aarch64/mmu.rs`: when building the direct map, skip the
  pool's range. If the pool falls inside a 2 MiB block descriptor,
  split that block into an L3 table at boot (one-time cost; we have
  L3-split logic for the user-side already).
- TLBI is a no-op here — MMU isn't yet enabled when the direct map
  is built.

### Step 3: PageSet origin tagging — in shared metadata, not just HandleKind

- `lockjaw-types/src/pageset_table.rs`: extend `PageSetEntry` with
  `origin: PageSetOrigin` (variants: `Buddy`, `DmaPool`).
- Same file: extend `PageSetHeader` with `origin` so the free path
  (`src/cap/pageset_table.rs:303` / `:418`) can read it after handle
  removal.
- `HandleKind::PageSet` does NOT need its own origin field — the
  header carries it and the handle's `kva` already points at the
  header. This keeps origin in one place.

**Header-layout migration (codex follow-up):** the current
`PageSetHeader` is *exactly 16 bytes*, asserted in
`lockjaw-types/src/pageset_table.rs:723` and referenced in pinned
comments throughout the file ("byte offset 16", "starting at byte
offset 16", etc. — `:26-27`, `:57`, `:63`, `:721`). Adding `origin`
breaks the 16-byte invariant. The migration is mechanical because
the runtime code already reads via `size_of::<PageSetHeader>()`
(see `:70`, `:216`, `:241`, `:325`), but the comments and the size
assertion all need updating in lock-step. This sub-commit treats
header-layout migration as a first-class step, not an incidental
edit:

- Pick the new exact size (likely 24 bytes after adding a 1-byte
  enum + padding to 8-byte alignment for the trailing u64 array).
  Pin the new size in the existing assertion.
- Update *all* comments that name "16 bytes" or "byte offset 16" in
  `pageset_table.rs` (lines 26, 27, 57, 63, 721, plus any others
  the audit catches).
- Update `docs/history/posix-phase2-mmap-plan.md:65` and
  `docs/development-journal-9.md:103` if they pin the same number.
- Re-run the size-pin tests — `header_metadata_size_is_16_bytes`
  becomes `header_metadata_size_is_24_bytes` (or whatever the new
  pin is).
- Stack-budget verification (`cargo xtask check-stack`) to confirm
  the 8-byte growth doesn't push any caller past its budget.

### Step 4: Kernel — `sys_alloc_dma_pages` syscall

- `src/syscall/handler.rs`: new `SYS_ALLOC_DMA_PAGES` dispatch.
- Allocates `count` pages from the DMA pool (not buddy).
- Builds PageSet with `origin = DmaPool` tag in header + entry.
- **Returns the handle in x1, single scalar.** Caller uses the
  existing `sys_query_pageset_phys(handle, 0)` to get the phys
  address of the first page (per codex: phys exposure already exists
  via that syscall, no new ABI needed).
- Returns `SyscallError::DMA_POOL_EXHAUSTED` (new typed error) when
  the pool can't satisfy the request.

### Step 5: Kernel — reject DMA-pool PageSets in cacheable paths

The plan-critical step. Each path that today maps a page cacheable
must check origin and reject DmaPool with `INVALID_PARAMETER`
(or a typed `WRONG_ORIGIN` if we want a distinct shape):

- `src/syscall/handler.rs` `sys_map_pages`: if origin is DmaPool
  and `attr != NormalNonCacheable`, reject. Conversely, if attr is
  `NormalNonCacheable` and origin is Buddy, reject.
- `lockjaw-types/src/kvm.rs:59` (donated PageSet KVM remap): if
  origin is DmaPool, refuse. The donate-as-kernel-object flow can
  only consume Buddy-origin PageSets.
- `src/syscall/handler.rs:137` (donated kernel-object init via
  paddr): same rejection point as the KVM remap, just at the
  syscall-handler level.
- `create_process` path that consumes mappings via
  `lockjaw-types/src/vmem.rs:363`: any DmaPool-origin PageSet in
  the mapping list is rejected before the cacheable-build step.
- `src/mm/page_alloc.rs:141` (page zero-init via direct map): never
  called for DmaPool pages because the alloc path is the pool, not
  buddy. No code change needed here, but a debug assert in pool's
  free path that `paddr` falls in the pool range catches mistakes.

Each rejection point gets a unit/integration test that proves the
rejection fires.

### Step 6: NormalNonCacheable MAIR slot

- `lockjaw-types/src/page_table.rs`: current code has
  `MAIR_DEVICE = 0` (slot 0) and `MAIR_NORMAL = 1` (slot 1), per
  the existing constants and the `MAIR_EL1_VALUE` encoding
  `(0x00 << 0) | (0xFF << 8)`. Add `MAIR_NORMAL_NC = 2` (slot 2).
- Encoding: `0x44` for slot 2 = Normal Memory, Outer + Inner
  Non-cacheable (ARM ARM Table B2-2 / D7-D6: 0b0100_0100). Update
  `MAIR_EL1_VALUE` to `(0x00 << 0) | (0xFF << 8) | (0x44 << 16)`.
- Update the `MAIR_EL1_VALUE` exact-value test (`page_table.rs:772`)
  to the new constant.
- `src/arch/aarch64/mmu.rs`: confirm the new value is what gets
  written to `MAIR_EL1` at MMU init. Likely a no-op recompile since
  the const is sourced from lockjaw-types.

### Step 7: MapMemoryAttribute::NormalNonCacheable variant

- `lockjaw-types/src/vmem.rs`: add `NormalNonCacheable = 2` variant.
- `to_pte_attrs()`: returns `(MAIR_NORMAL_NC, SH_OUTER)`. Outer
  shareable for cross-master coherence with the SDHCI DMA engine.
- `from_raw`: switches to `Option<Self>`. `0`/`1`/`2` are valid;
  anything else returns `None`. The kernel handler maps `None` to
  `SyscallError::INVALID_PARAMETER`. (`Result` would also work; per
  codex, `Option` is lighter and the only failure mode is "unknown
  discriminant", so a sentinel `None` carries enough information.)
- This is the validation tightening that sub-commit 1 deferred to
  here per the plan's Q4 resolution.

### Step 8: lockjaw-userlib — `sys_alloc_dma_pages` wrapper

- `user/lockjaw-userlib/src/syscall.rs`:
  `pub fn sys_alloc_dma_pages(count: u64) -> Result<PageSetHandle, SyscallError>`.
  Single return. Caller calls `sys_query_pageset_phys(handle, 0)` to
  get the phys address.
- Re-export from `lib.rs`.

### Step 9: Tests + docs

- Host tests:
  - `MapMemoryAttribute::from_raw(2) == Some(NormalNonCacheable)`.
  - `from_raw(3..=u64::MAX) == None`.
  - `to_pte_attrs(NormalNonCacheable) == (MAIR_NORMAL_NC, SH_OUTER)`.
  - `MAIR_EL1_VALUE` exact-value pin updated.
  - DMA pool allocator: alloc + free round-trip; exhaustion returns
    `None`; double-free panics in tests.
- Integration assertions (QEMU):
  - `sys_alloc_dma_pages(1)` succeeds.
  - Returned PageSet's first-page phys (via `sys_query_pageset_phys`)
    falls within the reserved DMA pool range.
  - `sys_map_pages` with `NormalNonCacheable` on a Buddy PageSet
    returns INVALID_PARAMETER.
  - `sys_map_pages` with `Normal` on a DmaPool PageSet returns
    INVALID_PARAMETER.
  - Donating a DmaPool PageSet to a kernel object path returns
    INVALID_PARAMETER (covers the `kvm.rs:59` and `handler.rs:137`
    rejection points).
  - `create_process` with a DmaPool PageSet in the mapping list
    returns INVALID_PARAMETER.
- `docs/reference/syscalls.md`: SYS_ALLOC_DMA_PAGES row added with note about
  using `sys_query_pageset_phys` for the address; sys_map_pages row
  updated for the third attr variant + the cacheable-vs-pool origin
  matrix.

### Sub-commit 2a verification

- `cargo test -p lockjaw-types` green; new tests visible in count.
- `make build` green.
- `make test-qemu-gicv3` 98+/98+ (existing 98 + new DMA-pool assertions).
- `cargo xtask check-stack` etc. green per usual.
- Pi 4B boot: existing M5 emmc2 path still passes (no driver changes
  yet; just verifies the kernel didn't regress).

## Sub-commit 2b — emmc2-driver ADMA2 path

(Outline; detail finalised when 2a lands.)

- Allocate descriptor table: `sys_alloc_dma_pages(1)` → 4 KiB page,
  one ADMA2 descriptor (16 bytes) at the start.
- Allocate read buffer: `sys_alloc_dma_pages(1)` → 4 KiB page,
  enough for 1 block (512 B) + slack.
- Map both NC via `sys_map_pages(.., NormalNonCacheable)`.
- Build ADMA2 descriptor: `(buffer_phys, 512, ACT=2 [transfer],
  END=1, INT=0, VALID=1)`. Layout per SDHCI §1.13 ADMA2 32-bit.
- Program SDHCI:
  - `HOST_CONTROL_1`: `DMA_SEL = 0b10` (ADMA2 32-bit), bits[4:3].
  - `SDHCI_ADMA_ADDRESS` (0x058): descriptor table phys addr. This
    is a *different* register from `SDHCI_SYSADDR` (0x000), which is
    the SDMA source/destination address (for non-ADMA SDMA mode) and
    aliases `SDHCI_ARGUMENT2` for Auto-CMD23 (M5). Both constants
    coexist in `lockjaw-types/src/sdhci.rs`; this commit adds 0x058
    alongside.
  - `BLOCK_SIZE`, `BLOCK_COUNT` as usual.
  - `TRANSFER_MODE`: `READ | DMA` (no PIO, no AUTO_CMD23 for single
    block).
- Issue CMD17 with DATA_PRESENT.
- Wait for `TRANSFER_COMPLETE` (DMA fires asynchronously, no
  per-block BUF_RD_READY draining needed).
- Verify MBR signature in the buffer (same check as M4).
- Log wall-clock comparison vs the M4 PIO read for proof DMA is
  actually being used.

Success line:
`[EMMC2:ADMA] LBA0 read via ADMA2-32, descriptors=1, t=...us`

## Risks (sub-commit 2a)

1. **Pool exhaustion in practice**. 4 MiB / (driver buffer + descriptor
   table) gives many concurrent transfers' worth at Lockjaw's current
   driver count. Bumpable if a real workload pushes it.
2. **PageSet origin field size**. `HandleKind::PageSet` already has a
   discriminant; adding a one-byte origin enum should fit existing
   layout assertions. If it overflows, that's a separate substrate
   conversation.
3. **Direct-map exclusion code**. The MMU init code currently assumes
   "all RAM is in the direct map." Adding a hole requires careful
   handling of the L1/L2 block descriptors that would have covered
   the pool. Worst case: split to L3 tables for the affected 2 MiB
   regions and clear the pool's L3 entries.
4. **TLB invalidation at boot is a no-op** (MMU not yet enabled when
   the direct map is built), so no shootdown concern in 2a.

## Risks (sub-commit 2b — for context)

1. **BCM2711-specific ADMA2 quirks**. Linux's
   `drivers/mmc/host/sdhci-iproc.c` lists Pi-specific quirks. None
   should bite for a basic 32-bit ADMA2 read at 25 MHz, but worth a
   read before the bring-up commit.
2. **DMA descriptor alignment**. ADMA2 descriptors must be
   8-byte-aligned; descriptor table base must be 4-byte-aligned. A
   page-aligned base + 16-byte descriptor satisfies both.
3. **64-bit ADMA2**. Pi 4B advertises 64-bit ADMA2 in CAPABILITIES
   bit 28, but 32-bit suffices for our PAs (RAM under 4 GiB on Pi 4B
   physical address space). Stay 32-bit.

## Out of scope (deferred follow-ups)

- ADMA2 multi-block (uses descriptor chains, not just one descriptor).
- ADMA2 write path (waits for M7's BlockEngine).
- Per-PageSet refcounted close path for DMA pool (current design:
  pool free on PageSet close, like buddy).
- Larger pool / dynamic resize (current: fixed 4 MiB at boot).
- 64-bit ADMA2 / ADMA3 (only matters when PAs cross 4 GiB).
- Cache-maintenance fallback (Option A) — held in reserve if the
  pool approach turns out to be insufficient on real hardware.

## Revision history

**v1 (initial draft) — codex flagged 5 issues:**
1. MAIR slot ordering reversed in plan vs code (Device=0, Normal=1).
2. Pool exclusion from direct map insufficient — kernel has 4+ paths
   that map pages cacheable; all need to reject DmaPool.
3. DMA-pool misuse loophole — nothing prevents create_process /
   donate-as-kernel-object from using DmaPool pages cacheably.
4. PageSetOrigin should be in shared metadata (header + entry), not
   `HandleKind::PageSet` only — free path operates on header_kva
   after handle removal.
5. `sys_alloc_dma_pages` should be single-return; phys addr
   available via existing `sys_query_pageset_phys`.

**v3 (this version) — addresses v2's remaining medium:**
- Step 3 now treats the `PageSetHeader` 16→24 byte layout migration
  as a first-class step. Lists the comments / pinned tests / docs
  that need to update in lock-step, plus a stack-budget check.

**v2 — addresses all 5 of v1's findings:**
- Step 6 corrects MAIR slot encoding and value.
- New "DMA-pool isolation principle" section makes Rule 3
  (rejection at every cacheable-mapping path) the load-bearing
  invariant, with each rejection point listed and tested.
- Step 5 enumerates the four kernel paths that must reject DmaPool
  and adds an integration test per rejection.
- Step 3 puts origin in `BackedHeader` + `PageSetEntry`, not
  `HandleKind`.
- Step 4/8 changes `sys_alloc_dma_pages` to single-return
  (`PageSetHandle`); userspace queries phys via the existing
  `sys_query_pageset_phys`.
- ADMA register section (sub-commit 2b) now distinguishes
  `SDHCI_ADMA_ADDRESS` (0x058) from `SDHCI_SYSADDR` (0x000) so
  there's no register conflict.

## Codex sign-off request

Plan v2 above. Specific questions:

1. Does the new "DMA-pool isolation principle" section close the
   four kernel paths you flagged, or are there more cacheable-
   mapping sites I missed?
2. Is `Option<Self>` the right return for `from_raw`, or do you
   prefer `Result` for the named error?
3. Is putting origin in both `PageSetEntry` AND `BackedHeader`
   right, or should it live only in the header (with the entry
   carrying a reference)?

If the plan is sound, please say so explicitly so I can start
coding. If you spot another high-severity issue, flag it and I'll
revise again.
