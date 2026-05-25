# Cacheable DMA Migration Plan

## Status

**Draft for review.** No code touched. The m7-irq-experiment branch
established that with NC DMA mappings on BCM2711 emmc2, IRQ-driven
data-path reads return stale zeroes from the buffer immediately
after `DATA_COMPLETE` because the SDHCI controller's outbound AXI
writes have not yet committed to DRAM when the interrupt fires.
This plan is the principled fix.

This is a substrate-level migration plan, not a driver patch. It
will touch `lockjaw-types::vmem` (attributes), `lockjaw-types::
dma_pool` (origin contract), `src/cap/dma_pool.rs` (kernel pool
ownership), `src/arch/aarch64/mmu.rs` (kernel direct-map
inclusion), the `sys_map_pages` rejection matrix, and add two new
syscalls. It then ripples through every user driver that touches a
DMA buffer (virtio-blk, ramfb, emmc2).

## Context

### What NC bought us, and what it didn't

The M6 substrate (commit `10a01e8`, May 15 2026, "M6 sub-commit 2a
step 2: DMA pool + NormalNonCacheable substrate") made DMA pages
Normal Non-Cacheable everywhere they're mapped:

- `lockjaw-types::dma_pool::DmaPool` carves a 2 MiB region at the
  top of RAM (one L2 block on AArch64).
- Pool pages are excluded from the kernel TTBR1 direct map (so
  speculative CPU caching of those PAs is impossible).
- Pool pages are excluded from the buddy allocator's free list.
- `sys_map_pages` enforces a rejection matrix: only DmaPool-origin
  PageSets can be mapped `NormalNonCacheable`; Buddy-origin
  PageSets cannot. Conversely, `NormalCacheable` mappings reject
  DmaPool origin.
- Per-process invariants reject DmaPool-origin PageSets for
  stack/scratch/user mappings in `create_process` and for
  donate-as-kernel-object in `create_kernel_object_kvm`.

The architectural property this delivers is **mixed-attribute
aliasing is impossible by construction**. The ARMv8 architecture
specifies that two mappings of the same PA with mismatched
cacheability is UB; M6 made that UB unreachable from any code
path. This was codex-reviewed five passes.

What NC mappings do **not** deliver:

- Any guarantee that a device's DMA writes are visible to the CPU
  at the instant some controller status bit flips.
- Any CPU-side operation that drains another bus master's pending
  writes through the SoC interconnect.
- Immunity from controller completion-semantics quirks.

### The emmc2 bug

BCM2711 emmc2 (Arasan SDHCI v3) asserts
`NORMAL_INT_STATUS.TRANSFER_COMPLETE` (= what the SDHCI core spec
calls "data transfer complete") at the point the controller's data
state machine finishes, **before** its outbound AXI writes have
committed to DRAM. With NC mappings, a CPU read of the buffer
immediately after seeing `TRANSFER_COMPLETE` can race the tail of
those writes and observe stale data — uniformly zero for a
zero-initialised buffer.

The race was masked in the polling shape (M4-M7) by the diagnostic
hex-dump that ran between the polling-loop exit and the signature
check: 32 NC buffer loads plus 32 UART MMIO writes plus the
formatting overhead provided enough wall-clock time for the
controller's AXI traffic to drain. Removing the dump in dbbd35c
exposed the race; the IRQ-driven shape exposed it again.

### What the reference implementations do

Linux SDHCI (`drivers/mmc/host/sdhci.c`) handles `SDHCI_INT_DATA_END`
in `sdhci_data_irq` → `sdhci_finish_data` → `__sdhci_finish_data_common`
→ `sdhci_adma_table_post` → **`dma_sync_sg_for_cpu(dev, sg, sg_len,
DMA_FROM_DEVICE)`**. On ARM64 with non-coherent DMA, this expands
to a `dc ivac` loop bracketed by `dsb sy`. The cache invalidation
participates in the bus protocol as an outer-shareable operation:
the cache controller forces pending writes from other coherent
masters to commit before the invalidation completes. The drain is
a structural side-effect of the cache invalidate, not a separate
primitive.

U-Boot SDHCI (`drivers/mmc/sdhci.c`, `sdhci_transfer_data`) polls
`SDHCI_INT_STATUS` for `SDHCI_INT_DATA_END`, with `udelay(10)`
between iterations to avoid hammering the controller register. The
`udelay` is **polling-cadence pacing**, not the completion
mechanism. After the loop exits on `DATA_END`, U-Boot calls
`dma_unmap_single(host->start_addr, ..., mmc_get_dma_dir(data))`
which internally invokes `invalidate_dcache_range` — the same
`dc ivac` loop Linux uses.

**Both references use cacheable DMA buffers and cache invalidation
on the read-completion path.** Neither uses a delay as the
completion mechanism; neither relies on a controller-validated
"AXI committed" status bit (no such bit exists on this silicon —
the Arasan IP datasheet is NDA, and no public BCM2711
documentation describes one). The controller-side completion
contract genuinely tops out at `TRANSFER_COMPLETE`.

### Why Lockjaw is the outlier

Lockjaw with NC + nothing: broken.
Lockjaw with NC + dummy MMIO read + dsb sy: broken (the dummy read
serialises at the controller's MMIO interface, which does not
serialise the controller's outbound AXI writes to DRAM).
Lockjaw with NC + delay: would be unique to us — neither reference
does this.
Lockjaw with cacheable + invalidate: matches both references.

The construction-safety win from M6 came from "single attribute
everywhere for DMA pages", not from "the attribute is specifically
NC". Switching the chosen attribute from NC to Cacheable preserves
the alias-safety invariant if all the matrix rules and the kernel
direct-map decision are re-stated consistently.

## Sequencing against Phase 9

**Phase 9 is paused at P9.4 until this migration (C0-C3) lands on
main.** Two reasons:

1. **Phase 9's verification model presumes a trustworthy Pi-flash
   gate.** P9.5 through P9.12 each require the hard gates
   `[FAT32-TEST] read 17 bytes: hello from fat32` AND
   `posix-hello: hello from fat32` to fire on Pi. With the
   NC-mapping data-visibility bug in place, every Phase 9
   driver-conversion commit either fails the Pi gate outright or
   only passes because of incidental MMIO/UART traffic that
   accidentally drains the SDHCI AXI buffer. The bisect property
   Phase 9 was designed around — "a regression at any P9.x
   localizes cleanly because all prior gates fired" — is not real
   today; it is dressed up by diagnostic-print side-effects.

2. **P9.4 has not actually validated end-to-end on Pi.** The
   working-tree artifacts tell the story: `kernel8.p9.4-no-diags.img`
   produced `[EMMC2:BLK] selftest MBR signature=0x0 BAD` on Pi;
   `kernel8.p9.4-third-flash-with-diags.img` passed only because
   the diagnostic prints provided the inadvertent drain. So the
   P9.4 hard gates as defined in the Phase 9 plan ("both hard gates
   fire") have never been met on a diagnostic-clean kernel.
   Continuing forward from P9.4 on top of that builds on a
   foundation we don't trust.

The order is:

1. **Pause Phase 9 at the current state.** P9.0-P9.3 codegen-prep
   commits remain landed on main (they were Pi-flash-N/A; no Pi
   regression risk). P9.4 driver work stays stashed/branched.
2. **Land the cacheable DMA migration C0-C3 on main.** Substrate-
   level change; codex-reviewed; independent of Phase 9 scope.
3. **Re-flash P9.4 with the cacheable substrate in place.** The
   hard gates fire on a diagnostic-clean kernel. This is the
   legitimate P9.4 acceptance.
4. **Resume Phase 9 P9.5-P9.12.** Each commit Pi-flashes against
   the now-trustworthy gate; the bisect property is real.

The m7-irq-experiment branch's findings (GIC ITARGETSR fix,
BoundIrq helper, level IRQ mask/unmask, sys_unmask_irq owner check)
are independent of this migration and feed back into a separate
mainline IRQ-conversion commit any time after C1 lands (emmc2 is
working end-to-end on the polling shape with sync calls at that
point) — they do not block
or get blocked by the cacheable migration.

## Strategic intent

- **Preserve the single-attribute invariant.** DMA pages must
  remain unreachable through any mapping with a different
  attribute. The four rejection paths from M6 sub-commit 2a step 2
  are not deleted — they are re-stated for the new attribute.
- **Match a reference implementation.** Both Linux and U-Boot use
  cacheable buffers + cache invalidation. The new Lockjaw
  contract matches both.
- **No driver-side cache instructions.** `dc ivac` is a privileged
  primitive that doesn't exist at EL0 on standard Lockjaw boots.
  Cache invalidation is exposed to user drivers via a syscall
  surface, not via inline asm. This keeps drivers `#![deny(unsafe_code)]`
  -clean and centralizes the cache-maintenance protocol in one
  audited kernel module.
- **The new syscalls are device-handoff primitives, not
  ad-hoc.** They mirror Linux's `dma_sync_for_cpu` and
  `dma_sync_for_device` API shape: synchronous, range-based,
  direction-tagged.
- **Pi flash is the acceptance gate.** QEMU virt does not exercise
  the emmc2 path; the bug only reproduces on real hardware.

## Architecture

### What changes

#### `lockjaw-types::vmem::MapMemoryAttribute`

Today:
- `Device` — used for MMIO.
- `Normal` — used for cacheable user/kernel mappings.
- `NormalNonCacheable` — used for DMA buffers; the rejection
  matrix ensures only DmaPool-origin PageSets can take this
  attribute.

After:
- `Device` — unchanged.
- `Normal` — unchanged. Used for general cacheable user/kernel
  mappings. This is what DmaPool-origin PageSets will use post-
  migration.
- `NormalNonCacheable` — **retained as an attribute variant but
  removed from the DmaPool rejection matrix**. NC stays available
  for future non-DMA use cases (no caller today). DmaPool-origin
  PageSets may NOT be mapped NC under any circumstance post-
  migration. If a future use case appears that needs NC for DMA,
  it gets its own separately-carved pool with its own
  Buddy/direct-map exclusion contract — not an opt-in on the
  existing pool.

Rejection matrix post-migration (no mixed states possible):
- DmaPool-origin + `Normal` → ALLOWED (the only option for DmaPool).
- DmaPool-origin + `NormalNonCacheable` → REJECTED (`INVALID_PARAMETER`).
- DmaPool-origin + `Device` → REJECTED.
- Buddy-origin + `Normal` → ALLOWED.
- Buddy-origin + `Device` → ALLOWED (existing MMIO path).
- Buddy-origin + `NormalNonCacheable` → REJECTED (existing rule).

Single attribute everywhere for DmaPool: Cacheable. No opt-in,
no escape hatch. The alias-safety guarantee from M6 sub-commit 2a
step 2 is preserved structurally — codex review's first concern
addressed.

#### `lockjaw-types::dma_pool`

Pool layout (carve, alignment, capacity) is unchanged. The change
is in the **kernel side's** treatment of the pool's PAs.

Today the kernel TTBR1 direct map explicitly **excludes** the
DmaPool's L2 block (via `mmu::exclude_dma_pool_from_direct_map`).
The exclusion exists because if the pool were both in the kernel's
cacheable direct map and in a user process's NC mapping, the
architectural mixed-attribute UB would fire on speculative CPU
loads.

Post-migration:
- DmaPool pages are mapped **Cacheable** in user processes (the
  new default).
- DmaPool pages are also mapped **Cacheable** in the kernel TTBR1
  direct map. The L2 exclusion is removed.
- The single-attribute invariant is preserved: every mapping of
  every DmaPool PA is Cacheable Inner+Outer Write-Back.
- Speculative CPU caching of pool pages via the direct map is now
  intended and harmless — the cache-invalidate syscall flushes
  stale lines before the consumer reads.

Per the rejection matrix above, no DmaPool PageSet may take an NC
mapping. The pool is uniformly Cacheable in kernel direct map and
in every user mapping. Per-PageSet attribute opt-in is explicitly
not part of this migration; a future NC-for-DMA use case would
get a separate pool (with its own direct-map exclusion contract,
mirroring today's DmaPool design).

#### Kernel-side cache primitives — `src/arch/aarch64/cache.rs`

New module. Two safe functions exposed:

```rust
/// Invalidate the data cache for the given KVA range, then dsb sy.
/// Used after a device's DMA write completes to make the data
/// visible to subsequent CPU loads. The KVA must be in the kernel
/// direct map (KVM-pool addresses or RAM direct-map addresses);
/// the byte range need not be cache-line aligned but the operation
/// will round to cache line boundaries internally.
///
/// # Safety
/// Caller must ensure no in-flight CPU write exists to the range,
/// because invalidation drops any dirty lines. For the device-read
/// completion path this is always true (the CPU has not written
/// the buffer).
pub unsafe fn invalidate_range(kva_start: KernelVa, byte_len: usize);

/// Clean (write back) the data cache for the given KVA range, then
/// dsb sy. Used before a device's DMA read starts to flush
/// pending CPU writes to memory so the device sees them.
pub fn clean_range(kva_start: KernelVa, byte_len: usize);
```

`invalidate_range` is `unsafe` because dropping dirty lines is
destructive if the CPU has writes pending; `clean_range` is safe
because it preserves data either way. Both are sequenced by the
final `dsb sy` so the caller does not need to issue one
afterwards.

Implementation: cache line size read from `CTR_EL0.DminLine` —
**new kernel code in C0** (`grep CTR_EL0` returns zero hits in
the current tree; this is introduced, not consolidated). Stored
as a startup-initialized static `usize` for the cache primitive
to consume. Then loop `dc ivac, x0` / `dc cvac, x0` per line,
then `dsb sy`. Every asm line commented per CLAUDE.md.

#### New syscalls

```rust
/// sys_dma_sync_for_cpu(pageset, offset, len)
/// After a device's DMA write completes, make the data visible to
/// the calling user driver. Invalidates the cache lines covering
/// [offset, offset+len) within the PageSet. PageSet must be
/// DmaPool origin; len must be > 0; offset+len must not exceed
/// the PageSet's byte size.
pub const SYS_DMA_SYNC_FOR_CPU: u64 = 30;

/// sys_dma_sync_for_device(pageset, offset, len)
/// Before a device's DMA read starts, ensure pending CPU writes
/// to the buffer are visible to the device. Cleans (writes back)
/// the cache lines covering [offset, offset+len). Same validation
/// rules as for_cpu.
pub const SYS_DMA_SYNC_FOR_DEVICE: u64 = 31;
```

Both syscalls map the PageSet via the kernel direct map (which now
covers DmaPool pages cacheably), invoke the corresponding
`cache::*_range` primitive, and return `SyscallError::OK` on
success.

#### `lockjaw-userlib`

New module `lockjaw_userlib::dma_sync` exposing:

```rust
pub fn sync_for_cpu(pageset: PageSetHandle, offset: u64, len: u64) -> Result<(), SyscallError>;
pub fn sync_for_device(pageset: PageSetHandle, offset: u64, len: u64) -> Result<(), SyscallError>;
```

`lockjaw_userlib::dma_sync` is **new substrate this migration
introduces**. There is no pre-existing `lockjaw_userlib::dma`
module or `OwnedDmaMapping` type in the as-landed tree (codex
review of an earlier draft caught the false reference) — CLAUDE.md
mentions them as policy aspiration, not implemented substrate.
The typed-wrapper layer (an `OwnedDmaMapping` or equivalent that
fuses allocation with sync calls at the access boundary) is a
separate substrate item, deferred to wherever the typed-MMIO
extraction roadmap places it. This plan ships only the raw syscall
wrappers; drivers consume them directly.

#### Driver-side usage

```rust
// Engine.read flow (read direction, polling shape on main):
//
//   issue_command(CMD17)
//   poll NORMAL_INT_STATUS for CMD_COMPLETE  (existing)
//   W1C clear CMD_COMPLETE                    (existing)
//   poll NORMAL_INT_STATUS for DATA_COMPLETE  (existing)
//   W1C clear DATA_COMPLETE                   (existing)
//   sync_for_cpu(buf, 0, 512)                 (NEW — replaces the
//                                              incidental AXI drain
//                                              that the M7-era 32-byte
//                                              dump used to provide;
//                                              also replaces the
//                                              experimental
//                                              sdhci_read32(PRESENT_STATE)
//                                              + dsb sy attempt on the
//                                              m7-irq-experiment branch
//                                              that didn't work)
//   return Ok                                 (caller can now safely
//                                              read buf)
```

For write direction:
```rust
// before issuing CMD24/25:
//   sync_for_device(buf, 0, len)              (NEW — flush any
//                                              pending CPU writes
//                                              so the controller
//                                              sees them)
//   issue_command(CMD24)
//   poll NORMAL_INT_STATUS for DATA_COMPLETE  (existing)
```

For the descriptor table (CPU writes, device reads): symmetric to
the buffer write case — `sync_for_device` after each descriptor
write before kicking the controller. Today we use a `dsb sy`
after the volatile write of the descriptor; with cacheable
mappings that's no longer sufficient (the line may still be in
the L1 cache and not visible to the device), so the
`sync_for_device` call replaces it.

### What stays the same

- `sys_alloc_dma_pages` syscall surface — unchanged.
- DmaPool size, alignment, carve location — unchanged.
- `PageSetOrigin::DmaPool` tag — unchanged.
- The four rejection-matrix code paths from M6 step 2a — still
  present, restated for the new attribute (no opt-in).
- Single-attribute invariant — preserved: **Cacheable everywhere
  for DmaPool, no exceptions**. The `NormalNonCacheable` variant
  still exists in `MapMemoryAttribute` for potential non-DMA use
  but is REJECTED for DmaPool-origin PageSets.

### What goes away

- `mmu::exclude_dma_pool_from_direct_map` — deleted. The pool
  participates in the direct map normally.
- The "you may NOT cacheably map a DmaPool page" branch in
  `sys_map_pages` is inverted to "you MUST cacheably map a
  DmaPool page; NC is rejected for DmaPool". The matrix shape is
  preserved (single allowed attribute per origin), only the
  allowed attribute changes.

## Phased migration

Each phase ends Pi-flashable and codex-reviewable. Order chosen so
the early phases land safely without changing any driver
behaviour; the late phases convert drivers one at a time.

### Phase C0 — API surface as `NOT_SUPPORTED` stubs

Zero functional change. Purely additive surface. The C0
boundary is reviewable as "no driver behaviour change; new
syscalls reserved but disabled."

- Add `lockjaw-types/src/arch/aarch64/cache.rs` (host-testable
  cache-line-math helpers; `dc ivac` / `dc cvac` lives in the
  kernel side).
- Add `src/arch/aarch64/cache.rs` (kernel-side primitive with the
  asm). Compiled but unreferenced from any dispatch path yet.
- Wire `SYS_DMA_SYNC_FOR_CPU` and `SYS_DMA_SYNC_FOR_DEVICE` into
  `lockjaw-types::syscall` numbering + the kernel dispatch table,
  **handlers return `NOT_SUPPORTED` unconditionally**. Do not
  attempt to look up the PageSet or operate on memory — there is
  no direct-map KVA for DmaPool at this point (the exclusion is
  still in place). Returning NOT_SUPPORTED with no side-effect
  side-steps codex review's second concern: the implementation
  target genuinely does not exist yet, so the syscall does
  nothing.
- Add userlib wrappers (`lockjaw_userlib::dma_sync`) that proxy
  the new syscalls and surface the `NOT_SUPPORTED` error to the
  caller verbatim. Drivers don't call them yet.
- Acceptance: `make test` 99/99. Kernel image growth bounded
  (rough budget: 1 KB).

### Phase C1 — Atomic flip + emmc2 conversion

This is the structural commit. Atomic. Six things land
together because the safety + bisect-utility properties both
require it:

1. `sys_map_pages`: DmaPool-origin + `Normal` becomes the only
   allowed attribute for DmaPool. DmaPool-origin +
   `NormalNonCacheable` is REJECTED (per the post-migration
   rejection matrix in Architecture). Buddy-origin rules
   unchanged.
2. `mmu.rs`: `exclude_dma_pool_from_direct_map` is deleted; the
   DmaPool participates in the kernel TTBR1 direct map as
   Cacheable Inner+Outer WB.
3. **Boot-time `dc ivac` over the entire DmaPool KVA range,
   immediately after step 2's direct-map inclusion lands and
   before any `sys_alloc_dma_pages` call is reachable.** Until
   step 2, the pool was excluded from every kernel and user
   mapping; any firmware-era cache lines for those PAs were
   harmless because no KVA could observe them. After step 2,
   those PAs become reachable via the kernel direct map and via
   every user-process Cacheable mapping — firmware-era cache
   lines become live aliases the first time the pool is consumed.
   The dc-ivac sweep drops them so the first consumer reads
   fresh DRAM. Safe to invalidate (not clean): no kernel or
   driver code has issued writes to the pool yet, so there is no
   CPU-side data to lose. Cost: one-time per boot, < 1 ms for
   2 MiB. Folding this in at C1 forecloses a non-deterministic
   bug class (first-allocation reads pre-kernel firmware state)
   at the commit that introduces the reachability — rather than
   waiting for Pi flash to surface a symptom that depends on
   which firmware paths happened to touch which pool PAs on this
   particular boot.
4. `sys_dma_sync_for_cpu` / `sys_dma_sync_for_device` handlers
   stop returning `NOT_SUPPORTED` and start doing the real
   `dc ivac` / `dc cvac` work via the now-existing direct-map
   KVA for the PageSet's PAs.
5. All call sites of `sys_alloc_dma_pages` + `sys_map_pages` that
   previously passed `NormalNonCacheable` are updated to pass
   `Normal`. Without the update they would fail `sys_map_pages`
   immediately because NC for DmaPool is now rejected.
6. The emmc2-driver `adma2_single_block_read` path is converted
   (polling shape — main's current code, no IRQ binding;
   `bound_irq` and the IRQ-driven path live only on
   m7-irq-experiment and merge later per Sequencing-against-
   Phase-9 above):
   - After the DATA_COMPLETE poll loop exits and the bit is
     W1C-cleared, insert
     `dma_sync::sync_for_cpu(buf, 0, 512)` before returning Ok.
     This is the principled replacement for the AXI-drain side
     effect that the M7 diagnostic dump was incidentally
     providing.
   - Replace the post-descriptor-write `dsb sy` with
     `dma_sync::sync_for_device(desc, 0, 8)` before kicking the
     controller — the cacheable descriptor line must be flushed
     so the controller sees it. (`dsb sy` is no longer
     sufficient post-migration because the descriptor write may
     land in CPU cache rather than memory.)
   - Remove the diagnostic 32-byte post-read dump (its drain
     side-effect is no longer needed and would only mask
     regressions in the sync primitive).

**Why all six together**: a half-state where steps 1-5 land
without step 6 leaves Pi flash observably broken — the emmc2
data path reads from a Cacheable mapping without invalidation
and continues to see stale data, producing the same MBR=0 BAD
symptom we have today. A half-state where steps 1-2,4-6 land
without step 3 leaves a non-deterministic bug class live:
firmware-era cache lines for pool PAs become reachable through
the new direct-map mapping and may corrupt the first allocation
in a way that depends on which firmware paths touched which
PAs on this boot. Combined with the substrate-only half-states
(steps 1+2 without 4, or 4 without 1+2), the only commit
boundary that produces an observable correctness improvement
and forecloses the firmware-cache bug class is "all six
together." The Phase 9 process rule ("temporary off-regime
states cost the same scrutiny as permanent ones") applies.

**Acceptance:**
- `make test` 99/99 (QEMU's coherent DMA means QEMU passing is
  necessary-not-sufficient for the cache-maintenance contract).
- **Pi flash hard gates**: `[FAT32-TEST] read 17 bytes: hello
  from fat32` AND `posix-hello: hello from fat32`, on a
  diagnostic-clean kernel (no 32-byte dump). This is the
  principled-fix gate from the prior plan section, raised here
  as the Pi flash gate for C1.
- **Paired Codex+Claude review on the STAGED diff before
  commit**, given the commit's structural size and the fact that
  it lands the substrate AND the first consumer simultaneously.

### Phase C2 — Convert other DMA-using drivers (if any)

- Survey: enumerate all callers of `sys_alloc_dma_pages` after
  C1 lands. As of this plan's writing, the only such caller is
  emmc2 (covered by C1). ramfb's framebuffer is currently
  Buddy-origin Cacheable, not DmaPool, so it is not in scope for
  this phase.
- virtio-blk explicitly **not** in scope: its virtqueue and
  request pages are Buddy-origin Cacheable today
  (`user/virtio-blk-driver/src/main.rs` ~lines 345-348 and
  383-386, per codex). It runs only on QEMU virt where DMA is
  coherent; no cache-invalidate is required and adding sync calls
  would be a no-op. Its non-DmaPool DMA setup is pre-existing
  tech debt independent of this migration (see Tech debt
  carry-forward below).
- If C2's survey finds no drivers beyond emmc2, this phase is
  vestigial and collapses to a documentation note in C3.
- Acceptance: 99/99; Pi flash for emmc2 still green.

### Phase C3 — Retire / archive

- Delete `mmu::exclude_dma_pool_from_direct_map` if not already
  deleted in C1.
- Document the post-migration shape in `docs/book-of-lockjaw/`
  (the DMA chapter, if it exists; otherwise add a short section
  to the architecture chapter).
- Update `docs/yagni-parking-lot.md` to remove any items now
  obsolete; add `docs/tech-debt.md` entries for deferred pieces.

### Tech debt carry-forward (not in scope)

- **virtio-blk uses Buddy-origin Cacheable for DMA without sync
  calls.** Safe on QEMU virt (coherent DMA model). Would be
  unsafe on real hardware with a non-coherent DMA bus. Out of
  scope for this migration because virtio-blk never runs on real
  hardware in Lockjaw's current device set. Tracked in
  `docs/tech-debt.md` post-C3 with a note that any future
  hardware running virtio-blk-driver would need either DmaPool
  conversion or a new sync API that accepts Buddy-origin
  cacheable PageSets.
- Per-PageSet or per-pool NC opt-in for DMA. Not implemented;
  noted in Out of Scope below.

## Verification model

- **Tier 1 (every commit): `make test` 99/99** on GICv3 + GICv2.
  Catches bootstrap regressions. QEMU DMA is coherent so the
  cache-invalidate path itself is exercised but its correctness is
  not stressed by QEMU.
- **Tier 2 (every commit that touches a Pi-exercised driver): Pi
  flash.** Hard gates: `[FAT32-TEST] read 17 bytes: hello from
  fat32` AND `posix-hello: hello from fat32` for emmc2. Equivalent
  gates for any other driver as they migrate.
- **Counter-test (C1 only): Pi flash with the diagnostic 32-byte
  dump REMOVED in the same commit.** This is the principled-fix
  gate. If the selftest passes without the dump, the cache
  invalidate is doing the drain work that the dump used to
  incidentally do. If it fails, the cache invalidate is not
  sufficient and we have a deeper bug (which would be very
  surprising given both Linux and U-Boot rely on the same
  primitive).

## Risks

1. **Cache line alignment.** `dc ivac` / `dc cvac` operate at
   cache line granularity (64 bytes on Cortex-A72). Buffers that
   share a cache line with non-DMA data could be silently affected
   by invalidate. Mitigation: DMA pool allocations are
   page-granular (4 KiB), inherently cache-line aligned at both
   start and end. Smaller-than-line writes inside the buffer (e.g.
   a single u32 write to a descriptor table) are also safe
   because the entire enclosing line is wholly within the
   page. This is what the user-driver layer guarantees: a DMA
   PageSet contains nothing but DMA data.

2. **Direct map inclusion.** Removing
   `exclude_dma_pool_from_direct_map` means the kernel can now
   speculatively cache DMA pool lines via the direct map.
   Cacheable + cache invalidation is the contract that makes this
   safe. The risk is real if a driver forgets to call
   `sync_for_cpu` before reading — they will see stale data. The
   mitigation is twofold: codex review on every driver migration
   commit, and the deferred typed-wrapper substrate
   (post-migration) that fuses the sync call with the access
   pattern at the type system level. Until that wrapper lands,
   driver source carries explicit `sync_for_cpu` / `sync_for_device`
   calls at every device handoff, and the codex-reviewer
   checklist for any DMA-using commit includes "every
   device-completion path is followed by `sync_for_cpu` before
   buffer access."

3. **Performance regression on the descriptor-write path.** Today
   `ptr::write_volatile(desc) + dsb sy` is a single store to NC
   memory. Post-migration it becomes `write to cacheable line +
   sync_for_device` (one syscall, one `dc cvac`, one `dsb sy`).
   The syscall adds 100s of cycles per descriptor. For a single
   ADMA descriptor per CMD17 this is negligible; for multi-block
   transfers (P9 future work) we should batch the sync over the
   whole descriptor table rather than per-entry.

4. **QEMU does not exercise the bug class.** QEMU virt DMA is
   coherent (the virtio-blk model shares cache with the CPU at
   the simulator level). A regression in the cache-invalidate
   primitive would not be caught by `make test`. Pi flash is the
   only real verification for the cache-maintenance contract.
   Mitigation: codex review on the kernel-side primitive; Pi
   flash at C1 boundary.

5. **Boot-loader / firmware state.** The cache state at kernel
   entry may include lines for DMA pool PAs from VC4-firmware
   activity. Whatever those lines contain is not authoritative —
   the firmware's view, not the kernel's. The defensive primitive
   is **`dc ivac`** (invalidate, drop the lines so subsequent
   reads go to DRAM); not `dc cvac` (clean / write-back, which
   would commit stale firmware data to DRAM and let a driver
   read it back as if it were fresh — the wrong primitive,
   thanks reviewer pass 2). Boot-time invalidate of the entire
   pool's KVA range is safe because no kernel or driver code has
   issued writes to the pool yet at that point — there is no CPU-
   side data to lose. Cost: one-time per boot, < 1 ms for 2 MiB.

## Out of scope

- IOMMU integration. Lockjaw does not expose an IOMMU; DMA is
  direct-PA. This migration does not change that.
- Non-ARM architectures. The plan is ARM64-specific.
- Generalising the sync primitives to MMIO. MMIO mappings stay
  `Device` and don't need cache maintenance.
- Per-PageSet or per-pool NC opt-in for DMA. Removed from this
  migration entirely (originally listed; codex review pass 1
  flagged it as incompatible with single-attribute alias safety
  given the kernel-direct-map inclusion). If a future driver
  needs NC for DMA, it gets its own pool, modeled on the original
  DmaPool design with its own direct-map exclusion.
- Migrating virtio-blk to the new sync API. It runs only on QEMU
  virt (coherent DMA), uses Buddy-origin Cacheable pages, and
  doesn't need cache invalidate. Pre-existing tech debt — see
  Tech debt carry-forward in Phased migration.
- Performance optimisation of the descriptor write path beyond
  "batch per kick". The post-migration cost is acceptable for the
  Phase 9 driver set.
- Re-evaluating M6 step 2a's broader invariants. The single-
  attribute invariant stands; this migration changes the chosen
  attribute, not the rule.

## What this enables

- emmc2 IRQ-driven data path works end-to-end on Pi without the
  diagnostic 32-byte dump, matching the established Linux pattern.
- The m7-irq-experiment branch's findings (GIC ITARGETSR, BoundIrq
  helper, level IRQ mask/unmask, owner-check) feed back into a
  mainline IRQ-conversion commit once C1 lands (emmc2-driver is
  working end-to-end on Pi at that point — polling shape with
  sync calls).
- The driver-construction story acquires its missing primitive:
  drivers no longer need to reason about AXI drain semantics —
  they call `sync_for_cpu` after device handoff and `sync_for_device`
  before, with the rest handled by the substrate.
- Future DMA-using drivers (Ethernet, USB, second SD) inherit the
  reference-aligned contract by construction.

## Open questions for review

1. ~~Should the legacy `NormalNonCacheable` attribute survive at
   all~~ — **Resolved by codex pass 1**: NC stays as an attribute
   variant for potential future non-DMA use, but is removed from
   the DmaPool rejection matrix entirely. DmaPool is Cacheable-
   only. No opt-in. Future NC-for-DMA needs its own pool.
2. Should `sys_dma_sync_for_cpu` accept any KernelVa range (kernel
   primitive), or only DmaPool-origin PageSet handles (validated
   surface)? Recommendation: PageSet handle only, for the same
   reason `sys_map_pages` accepts only handles — every cache
   operation is gated by validation that the caller owns the
   range.
3. Is the `unsafe` on `cache::invalidate_range` correct, given the
   "no concurrent writes" precondition? Or should the precondition
   be enforced by a typestate at the PageSet level? Recommendation:
   `unsafe` for the kernel-side primitive (fine for an audited
   single-call-site), no typestate for now — revisit if the
   invariant becomes hard to maintain.
4. ~~Should the boot-time pool-flush (Risk #5) be belt-and-braces
   defensive, or only added if Pi flash shows an observable
   problem at boot?~~ — **Resolved by reviewer pass 5**
   (other-claude): folded into C1 as step 3 ("Boot-time `dc ivac`
   over the entire DmaPool KVA range, immediately after the
   direct-map inclusion lands and before any
   `sys_alloc_dma_pages` call is reachable"). Discipline rule:
   "decide based on whether Pi flash surfaces the symptom" is the
   wrong framing for a non-deterministic correctness bug class —
   it accepts the class as "we'll see" instead of foreclosing it
   structurally at the commit that introduces the reachability.
   Risk #5 text retained for the underlying explanation; the
   action is now in the C1 step list.
5. ~~C2 atomicity~~ — **Resolved by codex pass 1, then again by
   pass 3**: C1 is the atomic structural commit (sync syscalls
   go live, direct-map inclusion lands, rejection matrix
   flipped, every existing `NormalNonCacheable` call site
   converted, **and the emmc2-driver polling-shape conversion
   lands in the same commit** per pass 3). C0 is purely API
   stubs returning `NOT_SUPPORTED` (no cache ops on memory the
   kernel cannot reach). C2 is the vestigial "other DMA
   drivers if any" phase (today: empty). C3 is retire/archive.
6. ~~Plan references substrate that does not exist~~ — **Resolved
   by reviewer pass 2** (other-claude): the `lockjaw_userlib::dma`
   / `OwnedDmaMapping` references were aspirational from CLAUDE.md,
   not implemented substrate; rewritten as "new substrate this
   migration introduces" (raw `dma_sync` only; typed wrapper
   deferred). The `CTR_EL0.DminLine` "already done" reference was
   also wrong (zero hits in tree); rewritten as new kernel code
   in C0.
7. ~~Risk #5 wrong primitive~~ — **Resolved by reviewer pass 2**:
   boot-time pool flush is `dc ivac` (invalidate), not `dc cvac`
   (clean). Cleaning would commit stale firmware data to DRAM;
   invalidating drops the lines so subsequent reads go to fresh
   DRAM. Risk #5 text updated.
8. ~~C1 / C2 contradiction about where the first emmc2 sync call
   lands~~ — **Resolved by codex pass 3**: C1 now atomically
   includes the emmc2-driver conversion (sync calls + diagnostic
   dump removal) along with the substrate flip. The substrate-
   only intermediate state is not a useful bisect point because
   Pi flash would still show MBR=0 BAD (cacheable mapping without
   invalidate is observably the same as NC-without-drain), so
   the only commit boundary that produces a correctness
   improvement is "substrate + first consumer together." Phases
   renumbered: C2 is now "other DMA drivers if any" (currently
   vestigial); C3 is retire/archive.
9. ~~C1 driver snippet referenced `bound_irq.unmask()` which
   doesn't exist on main~~ — **Resolved by reviewer pass 3**
   (other-claude): C1's emmc2 conversion targets the **polling
   shape** (main's actual code), not the IRQ shape (which lives
   only on m7-irq-experiment). The snippet now reads as
   poll-CMD_COMPLETE → W1C → poll-DATA_COMPLETE → W1C →
   `dma_sync::sync_for_cpu(buf, 0, 512)` → return. The IRQ-driven
   shape arrives in the separate post-C1 m7-irq merge, which
   integrates `sync_for_cpu` into the new IRQ-completion path
   then.
10. ~~Three stale "after C2" references after the C1+emmc2 fold-
    in~~ — **Resolved by reviewer pass 4** (other-claude):
    Sequencing-against-Phase-9, What-this-enables, and Open Q
    #9's resolution all updated to "after C1 lands" (where
    emmc2 actually goes end-to-end). Open Q #5's resolution
    text tightened: C2 is the vestigial "other DMA drivers if
    any" phase, not a driver-side conversion. Phase 9 plan
    file's IRQ-merge timing also updated from "after C3" to
    "after C1". The m7-irq mainline merge is no longer gated
    on an empty phase.
