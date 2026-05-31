# Post-C1 Fix Plan — emmc2 data-visibility + user-mode NEON + defensive cleanups

## Status

**Draft for review.** Consolidates findings from the post-C1
combined investigation (other-claude reviewer + 12 sonnet
subagents). Two real correctness bugs identified plus a long
tail of latent / defensive issues, all unified into one phased
plan because the work is interleaved by acceptance gates.

This plan picks up where the cacheable-DMA migration paused:
C1 landed on `main` (commit `771301e`), then the NEON-disable
kernel fix landed (commit `e52450c`) and unblocked Pi boot past
the buddy panic. C1's principled-fix Pi gate (selftest reads
`0xAA55` on a diagnostic-clean kernel) is NOT met — `sys_dma_sync_for_cpu`
on its own is not providing data visibility. The investigation
explains why and what to do.

Phase 9 of the typed-MMIO substrate (emmc2 typed conversion)
remains paused per `nifty-rolling-naur.md`; this plan must
complete (or at minimum B1 + B2) before Phase 9 resumes.

## Context

### What we have so far

- `771301e` C1: cacheable-DMA migration atomic flip + emmc2 conversion.
  - DmaPool maps Cacheable Normal everywhere; sync syscalls
    (`sys_dma_sync_for_cpu`, `sys_dma_sync_for_device`) live;
    rejection matrix flipped; emmc2 polling-shape converted.
- `e52450c` Kernel-NEON fix: kernel built with
  `aarch64-unknown-none-softfloat` so the compiler can't emit
  SIMD in kernel code. `BuddyAllocator::alloc`'s autovectorised
  loop no longer exists; the latent NEON-not-saved-on-IRQ
  exception path can't corrupt anything.

### What the investigation found

Two real correctness bugs, in order of relevance to the
unfinished C1 Pi gate:

1. **emmc2 `0x0`-not-`0xAA55` symptom** has a concrete
   protocol-violation root cause. The selftest pre-zeros the
   destination buffer via `ptr::write_bytes`, leaving dirty
   CPU cache lines for a buffer that's about to be DMA-written
   by the SDHCI controller. Per ARM DDI 0487 §D7.4.2, `dc ivac`
   on a dirty cache line is *unpredictable* on Cortex-A72: the
   dirty zeros may evict over the DMA-written DRAM and the CPU
   reads stale zeros. Linux's `__dcache_inval_poc_nosync` uses
   `dc civac` (clean-and-invalidate) on partial / boundary
   lines specifically to avoid this; Lockjaw's
   `cache::invalidate_range` (`src/arch/aarch64/cache.rs:118-130`)
   uses `dc ivac` unconditionally.

2. **User-mode NEON state is silently dropped across every
   context switch.** `src/sched/context.rs`'s `SavedContext`
   carries `x19-x30` only (96 bytes); NO `v` registers and
   NO `FPCR` / `FPSR` are preserved. All 15 user binaries
   built today are compiled with the default
   `aarch64-unknown-none` target (NEON-on). The kernel-NEON
   fix made the kernel stop using NEON, which removed the
   *kernel* exposure (an IRQ no longer clobbers user NEON
   through SAVE_REGS/RESTORE_REGS), but the *context-switch*
   exposure remains: when the scheduler swaps user thread A
   for user thread B, B inherits ALL 32 of A's `v` registers
   plus FP control regs. AAPCS64 caller/callee-saved discipline
   does not narrow this — preemption is asynchronous, so a
   timer IRQ can land anywhere with any of the 32 `v` registers
   live. The fix in B1.1 saves all 32. This does NOT produce
   the emmc2 `0x0` symptom (the byte path from SDHCI DMA →
   selftest signature check is provably NEON-free) but it is
   a real bug with arbitrary blast radius across all 15 user
   processes.

Plus a long tail of additional issues the investigation
surfaced: an Arasan-controller adjacent-16-bit-write errata
(missing shadow-register workaround), a defensive
`DAT_INHIBIT` poll after `TRANSFER_COMPLETE`, a stale
`NORMAL_INT_STATUS` clear at function entry, `MAX_BINDINGS=96`
too small for Pi 4B INTIDs (153, 158), `SDHCI_SIGNAL_ENABLE`
never written, a stepping-dependent 1-GiB DMA ceiling check
for B0-stepping Pi 4B boards, and one architecturally-wrong
comment in `cache.rs`.

### Why "we should do all of this at once" — and why not

These items are NOT one commit. The dirty-cache-line bug (#1)
and the NEON gap (#2) are independent root causes that need
their own commits, their own review passes, and their own
Pi-flash validation. The errata-class issues either fix the
same symptom as #1 OR don't matter until #1 is fixed; either
way they're sequenced after. The defensive cleanups don't
block anything and should land last.

The plan below stages this carefully so each commit's
acceptance gate is unambiguous and Pi flash time is spent on
work that actually disambiguates hypotheses.

## Strategic intent

- **Close the silent-corruption class FIRST.** User-mode NEON
  save/restore (B1 — was B3) lands ahead of every other fix
  in this plan. Reason: it's a class of silently-corrupting
  bugs across 15 user processes; every subsequent
  investigation runs on shaky ground while it's open. If a
  later emmc2 fix doesn't behave as expected, "could user-mode
  NEON corruption be contributing?" becomes a confounding
  variable that must be ruled out before any other hypothesis
  can be trusted. Eliminating the class first removes the
  confounder before it's needed.

- **Confirm the emmc2 hypothesis with an unambiguous
  experiment, on a stable foundation.** Once B1 (NEON) is in,
  do the B2.0 disambiguating flash: delete the pre-DMA
  zero-fill AND add `sys_dma_sync_for_device` on the buffer.
  Both changes together rule out two distinct sources of
  pre-DMA dirty lines (the deleted pre-zero AND any residue
  from prior DmaPool tenancy or bootstrap zeroing). If `0xAA55`
  appears, the driver-side protocol fix (B2.2) is load-bearing
  and lands; B2.1 (kernel primitive hardening) goes alongside
  as defensive. If `0x0` persists, the bug isn't cache-related
  at all and we escalate to Arasan errata (B3). Doing the
  experiment on a NEON-corruption-free kernel means the
  signal is clean.

- **Split kernel and driver responsibilities honestly.** The
  driver owns the pre-DMA protocol step (clean buffer before
  handing to device); the kernel owns making its post-DMA
  invalidate primitive safe-by-default for callers that get
  the contract right. Both layers land changes. B2.2 (driver)
  is what actually closes the symptom — post-DMA `dc civac`
  alone cannot recover from dirty pre-DMA lines (the writeback
  step overwrites the device's bytes). B2.1 (kernel) is
  defensive hardening so a future driver that forgets the
  protocol step gets safer-than-current behaviour instead of
  unpredictable.

- **Defensive cleanups grouped and landed last.** They don't
  block C1's Pi gate; bundling them out front would dilute
  bisect signal on the actually-failing path.

- **Pi flash is the only meaningful gate for B2.** QEMU virt's
  DMA model is coherent — `dc ivac` on dirty lines doesn't
  manifest the bug in QEMU. `make test` 100/100 stays as a
  necessary-not-sufficient bar at every commit. B1's canary
  also exercises a regression that QEMU's `-smp 1` may not
  reproduce — Pi flash is the load-bearing test for it too.

## Phased plan

### Phase B1 — User-mode NEON save/restore

Lands first because it closes a silent-corruption class
affecting all 15 user processes. Every subsequent
investigation in this plan benefits from running on a
NEON-corruption-free baseline. Two sub-commits: kernel
change + regression-guard canary.

#### B1.1 — Extend `SavedContext` + `context_switch`

Change shape (`src/sched/context.rs` + `lockjaw-types/src/thread.rs`):

- `SavedContext` grows by 528 bytes:
  - `vregs: [u128; 32]` — `v0`-`v31` full 128 bits each.
  - `fpcr: u64`, `fpsr: u64` — registers are architecturally
    64-bit, though only the low 32 bits have defined fields
    (Linux uses `u32` for the same reason; `u64` is the
    conservative choice that matches `mrs`/`msr` semantics).
- `SavedContext` itself carries `#[repr(C, align(16))]`. The
  `stp q.., q..` instructions in the new ASM require the
  effective address to be 16-byte aligned (the q-form takes
  an alignment fault on misaligned access regardless of
  `SCTLR_EL1.A`). The TCB allocation that backs `SavedContext`
  must also start on a 16-byte boundary — verify in TCB
  allocator if not already guaranteed.
- `context_switch` ASM adds, before the GPR/stack switch:
  - `stp q0, q1, [x0, #NEON_OFF + 0*32]` … `stp q30, q31,
    [x0, #NEON_OFF + 15*32]` — 16 paired stores covering
    all 32 V registers.
  - `mrs xN, FPCR` + `mrs xM, FPSR` + `stp xN, xM, [x0, #FPCR_OFF]`.
- After the stack switch (loading the incoming TCB's saved
  context), mirror the loads (16 `ldp q.., q..` + `ldp xN, xM`
  + `msr FPCR, xN` + `msr FPSR, xM`).
- Compile-time assertions on the struct layout matching the
  ASM offsets (same pattern as `ExceptionContext`), PLUS
  `const _: () = assert!(core::mem::align_of::<SavedContext>() >= 16);`
  so the alignment invariant the q-form depends on is
  enforced at compile time.

Why ALL 32 V registers (not just `v8-v15`): this scheduler
boundary is NOT a function-call boundary in the AAPCS64
sense. Preemption is asynchronous — a 10 ms timer IRQ can
land at *any* user-mode instruction with *any* SIMD register
live. Walk the path:

1. User code at EL0 has arbitrary state in `v0-v31` at some
   random PC.
2. Timer IRQ fires; vector table runs `SAVE_REGS` (GPRs +
   sys regs only).
3. Kernel handler runs in soft-float kernel mode
   (post-`e52450c`); user's `v0-v31` are preserved in hardware
   because the kernel literally never reads or writes them.
4. Scheduler decides to switch. Kernel calls `context_switch`
   from inside the IRQ handler — there is NO user-side call
   site that spilled caller-saved NEON. AAPCS64 caller-saved
   discipline does not apply: the caller (user code) did not
   call anything, it was interrupted.
5. If `context_switch` saved only `v8-v15`, `v0-v7` and
   `v16-v31` of the outgoing thread would be left live in
   hardware for the incoming thread to clobber.
6. When the outgoing thread is later resumed, it reads (say)
   `v0` and sees residue from whatever thread ran in between.

Linux saves all 32 V registers (architecturally; the lazy/
deferred mechanism in `fpsimd.c` is a perf optimization layered
on top, not an ABI subset). The Lockjaw equivalent at v1 must
also save all 32 unconditionally. Cost: 16 × `stp q..` on save
+ 16 × `ldp q..` on restore = ~512 bytes per TCB, single-digit
extra cycles per switch. Negligible; the value is
architectural correctness.

Initial TCB setup zeros the NEON state (or uses architectural-
default FPCR/FPSR if it matters — Rust's `core::arch::aarch64`
expects FPCR's default rounding mode).

Why not save NEON in `SAVE_REGS` instead: the kernel handler
doesn't touch NEON (post-`e52450c` soft-float kernel target),
so NEON state preserves through any exception that returns to
the same thread. Only context-switch boundaries swap threads;
that's where the save/restore belongs. Same design as Linux.

Acceptance:
- `make test` 100/100.
- B1.2 (canary test) passes on Pi.
- **Kernel-binary-is-NEON-free check**: the "kernel handler
  doesn't touch NEON, so state preserves through any
  exception" claim rests on the binary actually being free of
  NEON instructions. Post-`e52450c` the compiler's soft-float
  target prevents emission, but inline asm could in principle
  reintroduce it. Pre-commit smoke check:
  `rust-objdump -d target/aarch64-unknown-none-softfloat/release/lockjaw |
  grep -E ' v[0-9]+\.| q[0-9]+,| d[0-9]+,' | head` must return
  empty. Wire this into `xtask` or `make build` if it isn't
  already; the existing `objdump | grep` check from the
  `e52450c` commit message is the same pattern, expanded to
  cover q-form and d-form references not just `v` lanes.

#### B1.2 — NEON canary user-mode test

A test that deliberately tries to provoke the corruption:

- New user-mode binary `user/neon-canary/`:
  - Spawns 2+ threads (or relies on init-spawned siblings).
  - Each thread runs a tight loop:
    - Loads ALL 32 V registers (`v0-v31`) with a known
      thread-unique pattern (e.g. thread-id-tagged values per
      register).
    - Yields (or busy-loops to invite preemption — timer IRQ
      will preempt anywhere inside the loop body).
    - Reads `v0-v31` back.
    - Asserts every register's pattern is unchanged.
    - If broken, logs the thread ID + register index +
      actual vs expected values.
  - Loading ALL 32 V regs (not just `v8-v15`) is essential:
    a canary that only checks `v8-v15` would pass even with
    the wrong design (only saving the callee-saved subset).
    Loading the full architectural file is what proves the
    architectural fix.
- Integrated into the QEMU integration test as an
  always-pass test that becomes always-fail if `B1.1` is
  ever reverted.

Acceptance:
- The canary, committed but applied to an immediate-pre-B1.1
  build (`git stash` B1.1's kernel change, build with the
  canary, flash, observe failure), reliably fails on Pi —
  this proves the canary actually exercises the gap rather
  than passing vacuously. Restore B1.1 and re-flash to
  confirm passes. The witness commit is "main + canary -
  B1.1", not an arbitrary historical commit like `e52450c`.
- The canary, running on B1.1, reliably passes on Pi.
- `make test` 100/100 with the canary in place.

### Phase B2 — emmc2 data-visibility root cause

The actual unblocking work for C1's Pi gate. Runs on top of
B1 so the disambiguating flash signal is clean (no NEON
corruption to second-guess). Three sub-commits: one
hypothesis-confirming flash (does not ship), one principled
kernel-side fix, one driver-side hygiene change.

#### B2.0 — Disambiguating flash (no commit)

Hypothesis test only. **Do not commit.** Build with two
local changes and re-flash to confirm the root cause before
designing the fix:

- `user/emmc2-driver/src/main.rs:1045`: delete the
  `ptr::write_bytes(selftest_va as *mut u8, 0, 512)` line
  (the pre-DMA buffer-zero in the selftest).
- *Same site*: insert `sys_dma_sync_for_device(selftest_buf,
  0, 512)` immediately before the DMA dispatch. This explicitly
  cleans any dirty cache lines from any source — the deleted
  pre-zero, OR residue from a prior tenant of the DmaPool page,
  OR kernel-side zeroing during initial bootstrap that was
  never flushed to PoC. Without this second change, a `0x0`
  result is ambiguous: it could mean "dirty-line theory
  insufficient" OR "alloc path leaves lines dirty from prior
  owner." With the sync, lines are provably clean at DMA-start.
- Rebuild and Pi-flash.

Three outcomes determine which sub-commit lands next:

- **Selftest reads `0xAA55`** → driver-side `sync_for_device`
  closes the bug. B2.2 (drivers explicitly clean buffers before
  DMA-from-device) is the load-bearing fix. B2.1 (kernel
  `invalidate_range` upgraded to `dc civac` everywhere) is
  defensive hardening against future drivers that forget the
  protocol step — land it too, but its role is "primitive
  safe-by-default for the next driver," not "fix for this
  symptom."
- **Selftest reads `0x0`** → really is something else (driver
  hygiene was sufficient but didn't fix it). Escalate to Phase
  B3 (Arasan errata) for the actual cause.
- **Selftest reads partial / mixed** → multiple causes
  superimposed; both B2 and B3 are required.

This step doesn't ship code; it informs Phase B2's commit
choice in under one Pi-flash cycle.

#### B2.1 — Kernel primitive: `dc civac` everywhere + comment correction (defensive hardening)

The principled primitive upgrade. **Not load-bearing for the
emmc2 `0xAA55` symptom — B2.2 is.** B2.1's job is to convert
`dc ivac`'s UNPREDICTABLE behaviour on dirty cache lines (per
ARM DDI 0487 §D7.4.2) into `dc civac`'s deterministic
behaviour. A future driver that forgets B2.2's pre-DMA-clean
protocol still gets data corruption — the deterministic
writeback overwrites the device's bytes — but the corruption
is reproducible and debuggable instead of unpredictable. B2.1
does NOT eliminate the data-loss class on its own; that
requires the driver to maintain the pre-DMA-clean invariant,
which is B2.2's contract.

`src/arch/aarch64/cache.rs:118-130`'s `invalidate_range`
currently issues `dc ivac` over every line in the range, which
is unpredictable on dirty lines per ARM DDI 0487 §D7.4.2.
A half-fix — only upgrading *boundary* lines to `dc civac`
— still leaves interior aligned lines vulnerable to dirty
state. And the upgrade alone *cannot* close the C1 symptom:
if `civac` runs post-DMA on lines that were dirty pre-DMA,
the clean phase writes the stale CPU data back to DRAM,
overwriting the device's bytes. Post-DMA civac on dirty
lines is just as bad as ivac. The ONLY way the symptom
closes is for the lines to be clean BEFORE DMA — which is
B2.2's responsibility.

Two architectural choices for this primitive:

- **A. Make the primitive safe by construction.** Use
  `dc civac` (clean-and-invalidate) on EVERY line in the
  range, interior and boundary. On clean lines the writeback
  is a no-op; on dirty lines it writes back before
  invalidating, eliminating the unpredictable eviction. The
  primitive then has the property "if you call this on a
  range whose lines were clean before DMA, you get the
  device's bytes back regardless of any pre-DMA history."
  Cost: writeback of clean lines is single-digit cycles per
  line; for 512-byte buffers (8 lines) the cost is negligible.

- **B. Document the contract and require caller cooperation.**
  Keep `dc ivac` (Linux's choice in `__dcache_inval_poc_nosync`).
  Document that callers must have cleaned dirty lines BEFORE
  calling `sys_dma_sync_for_cpu`, typically via a prior
  `sys_dma_sync_for_device`. Linux's API requires this via
  the `dma_map_single` / `dma_unmap_single` contract.

This plan chooses **A**. Reasons:

1. Lockjaw's `sys_dma_sync_for_cpu` is a syscall — every
   call site is a security boundary, and the caller is
   user-mode code that the kernel can't trust to have
   maintained any particular invariant. Safe-by-construction
   primitives are the appropriate kernel-side choice.
2. The performance difference (cycle counts on a 512-byte
   buffer) is in the noise compared to the syscall overhead.
3. Boundary lines need `dc civac` anyway (for partial-line
   ranges); applying it uniformly removes a special case
   from the implementation.

Change shape:

- `src/arch/aarch64/cache.rs::invalidate_range`: replace
  the `dc ivac, x0` instruction in the per-line loop with
  `dc civac, x0`. One-token change.
- Update the function's doc comment to explicitly state the
  new invariant: "safe regardless of caller cache state,
  PROVIDED the buffer was clean of pre-DMA dirty lines before
  the device wrote it (driver responsibility, see
  `sys_dma_sync_for_device`)."
- Rewrite the pre-existing comment at `cache.rs:118-130` (the
  one this commit is touching anyway) that claims `dc ivac`'s
  outer-shareable property drains pending writes from other
  coherent masters. Per sub-agent A2.5 this is wrong: `dc ivac`
  operates on the PE's own cache, doesn't drain remote
  masters' AXI write buffers. The actual drain is the SDHCI
  controller's `TRANSFER_COMPLETE` handshake. The new comment
  states this explicitly — device DMA write drain is the
  device's responsibility, signaled by its completion interrupt
  / status, NOT by this CPU-cache primitive. (This is the
  cache.rs:124 comment fix previously slated as B4.5; folded
  in here because rewriting the function and leaving its own
  comment architecturally wrong would waste a Codex pass.)
- Update `src/arch/aarch64/cache.rs::clean_range` doc comment
  (no behaviour change) to note that the two primitives
  are now duals: `invalidate_range` is "clean+invalidate"
  in implementation, "invalidate-for-CPU-consumer" in
  semantics; `clean_range` is "clean-only" (no invalidate)
  for the device-going-to-read direction.

Acceptance:
- `make test` 100/100.
- Pi flash: no regression in B2.2's gate. **B2.1 alone is
  not expected to meet the `0xAA55` gate** — see the rationale
  paragraph above. The Pi-flash expectation here is "no worse
  than pre-B2.1 with the symptom still controlled by B2.2's
  driver-side clean."

#### B2.2 — Driver-side pre-DMA `sync_for_device` on destination buffer (load-bearing)

**This is the commit that actually closes the C1 Pi gate.**
The kernel primitive at B2.1 cannot close the symptom by
itself (see B2.1's rationale paragraph). The driver must
explicitly hand ownership of the destination buffer to the
device before initiating a read-direction DMA — cleaning any
dirty cache lines so that post-DMA invalidate-for-CPU does
not write stale data back over the device's freshly-deposited
bytes.

This matches Linux's `dma_map_single(DMA_FROM_DEVICE)`
contract surface.

Change shape:

- In `user/emmc2-driver/src/main.rs::Emmc2BlockEngine::read`
  (the place that loops over `adma2_single_block_read` for
  per-sector reads): add `sys_dma_sync_for_device(buffer, 0,
  count*512)` immediately after `find_buf` and before the
  per-sector loop. This cleans any prior dirty cache lines
  (e.g., from the selftest's pre-zero, or from a previous
  read's partial overwrite) before the DMA starts writing.
- Same treatment in the selftest path (`main.rs:1045` site
  region) so the selftest no longer relies on having
  pre-zeroed-buffer dirty lines being correctly handled by
  the kernel post-hoc.

Land alongside B2.1 as a separate commit so the responsibility
split is visible in history: B2.1 makes the kernel primitive
safe-by-default *for future drivers that forget the protocol
step*; B2.2 wires the protocol contract at the driver layer
*for the symptom in front of us*.

Acceptance:
- `make test` 100/100.
- Pi flash: selftest reads `0xAA55` (the C1 principled-fix
  gate finally met). B2.2 is the load-bearing commit for this
  acceptance gate; B2.1 must be present (already landed) but
  is not what fixes the symptom.

### Phase B3 — Arasan controller errata (conditional)

Only land if B2 doesn't fully fix the symptom. The Arasan
SDHCI IP has documented errata that Linux's `sdhci-iproc.c`
works around but Lockjaw does not. If B2.0's disambiguating
flash returned `0x0` (not `0xAA55`), these are the next
candidates.

#### B3.1 — Shadow-register two-32-bit-write workaround

Arasan errata: two 16-bit writes to adjacent registers within
2 SD-clock cycles can lose the first write. Linux's
`sdhci_iproc_writew` shadows `BLOCK_SIZE` + `BLOCK_COUNT` and
`TRANSFER_MODE` + `COMMAND` into single 32-bit writes.

Lockjaw currently issues three separate 16-bit writes at
`user/emmc2-driver/src/main.rs:1163-1165`:

```rust
sdhci_write16(mmio_va, SDHCI_BLOCK_SIZE, 512);
sdhci_write16(mmio_va, SDHCI_BLOCK_COUNT, 1);
sdhci_write16(mmio_va, SDHCI_TRANSFER_MODE, SDHCI_TRNS_READ | SDHCI_TRNS_DMA);
```

Change to:

```rust
// BLOCK_SIZE (0x004) + BLOCK_COUNT (0x006) as one 32-bit write.
sdhci_write32(mmio_va, SDHCI_BLOCK_SIZE, (1 << 16) | 512);
// TRANSFER_MODE (0x00C) + COMMAND (0x00E) — see B3.1.b below.
```

The COMMAND write is currently issued separately later in the
sequence (after `SDHCI_ARGUMENT`); fold that into a paired
32-bit write with `TRANSFER_MODE`:

```rust
sdhci_write32(mmio_va, SDHCI_TRANSFER_MODE,
    ((cmd17 as u32) << 16) | (SDHCI_TRNS_READ | SDHCI_TRNS_DMA) as u32);
```

Same pattern needed in `adma2_transfer` (the dead-code
multi-block path) for consistency — convert both at the same
time so re-enabling the path later doesn't reintroduce the
errata exposure. (Mild tension with CLAUDE.md's "delete dead
code" rule: `adma2_transfer` is intentionally kept per the
Out-of-scope entry on CMD18 + Auto-CMD23 re-enablement; the
"errata-fix dead code in place" call follows from that
deferral, not from an independent decision to carry the dead
path.)

Acceptance:
- `make test` 100/100.
- Pi flash: selftest reads `0xAA55` consistently across
  10+ boots.

#### B3.2 — 4-clock-cycle inter-write delay at ≤400 kHz

Arasan errata (separate from B3.1): at ID-phase clock
frequencies (≤400 kHz), writes to the controller need a
4-SD-clock-cycle gap to be reliably accepted. Linux's
`sdhci_iproc.c` inserts `udelay((4*1_000_000 + clk - 1) / clk)`
after every 32-bit MMIO write at low clock speed.

Only relevant during ID phase (CMD0/8/41/2/3/7 enumeration);
once the clock is bumped to 25/50/100 MHz the cycles are sub-
microsecond and the natural MMIO overhead covers it.

Change shape:
- Add a `delay_low_clock(controller_clk_hz)` userlib helper.
- Wrap MMIO writes during ID phase with it.
- Or simpler: pass a `phase: enum { Id, Data }` to the helper
  and have it no-op during Data phase.

Acceptance:
- `make test` 100/100.
- Pi flash: ID phase consistent across cold-boot from
  power-off (some Pi 4B units historically had cold-boot ID
  flakes that this might also address).

### Phase B4 — Defensive cleanups

Latent / minor / hygiene work. None blocks the Pi gate;
batched for a separate commit to keep the bisect signal on
B1-B3 unambiguous.

#### B4.1 — `DAT_INHIBIT` poll after `TRANSFER_COMPLETE`

`PRESENT_STATE.DAT_INHIBIT` is the controller's own signal
that its data path is genuinely idle (writes drained). Even
post-B1, polling for it after `TRANSFER_COMPLETE` provides a
second ordering point against any AXI-drain timing edge case.

Change shape:
- `adma2_single_block_read` after the `DATA_COMPLETE` break:
  add `poll_until_clear_32(mmio_va, SDHCI_PRESENT_STATE, SDHCI_DAT_INHIBIT, Nanos::from_millis(10))`.
- If it doesn't clear within 10 ms, surface as
  `Emmc2Error::DatInhibitStuck` (new variant).

Acceptance:
- `make test` 100/100.
- Pi flash: no regression to B1 / B2 gates.

#### B4.2 — Pre-CMD `NORMAL_INT_STATUS` write-all clear

U-Boot writes `0xFFFF` to `NORMAL_INT_STATUS` before issuing
`COMMAND` to guard against stale `CMD_COMPLETE` /
`DATA_COMPLETE` bits from a prior operation. Lockjaw only
w1c-clears specific bits it observed.

Change shape:
- In `adma2_single_block_read`, before the `SDHCI_COMMAND`
  write, add `sdhci_write16(mmio_va, SDHCI_NORMAL_INT_STATUS, 0xFFFF)`.
- Same in `adma2_transfer` for consistency.

Acceptance:
- `make test` 100/100.
- Pi flash: stable across 10+ boots.

#### B4.3 — `MAX_BINDINGS = 96` → `256`

`src/arch/aarch64/irq_bind.rs:7`'s 96-entry IRQ binding
table is too small for Pi 4B SPIs: UART = INTID 153, emmc2 =
INTID 158. Both currently bind through the matrix-rejected
path (silent fail). Pi 4B never exercised this yet because
no IRQ-driven driver runs on Pi today, but a Pi-side
IRQ-conversion of emmc2 (after this plan completes) would
need the bump.

Change shape:
- `const MAX_BINDINGS: usize = 256;`
- Verify table size doesn't blow stack / static budget (it's
  `Option<u64>` × 256 = 4 KiB, fine for BSS).

Acceptance:
- `make test` 100/100.

#### B4.4 — `SDHCI_SIGNAL_ENABLE = 0` after reset

Latent: prevents phantom IRQ delivery if IRQ mode is later
enabled but `SIGNAL_ENABLE` hasn't been written. Currently
the field is whatever value the firmware / reset left
behind.

Change shape:
- In emmc2-driver bootstrap (after `soft_reset_all`,
  `main.rs:529-534`), explicitly write
  `SDHCI_SIGNAL_ENABLE = 0` to mask all interrupts.
- Leave the M3+ comment that this will be flipped on
  per-bit when individual IRQs are bound.

Acceptance:
- `make test` 100/100. No Pi gate impact (polling shape
  doesn't use SIGNAL_ENABLE).

_Note._ The previously-listed B4.5 (`cache.rs:124` comment
fix) is folded into B2.1, where the same function body is
already being rewritten — no point burning a Codex pass on a
comment-only commit that lives in the function B2.1 touches.

_Note._ The previously-listed B4.6 (`Adma2Descriptor` builder
consolidation) is deferred to Phase 9's P9.9 (ADMA2 descriptor
→ generated wire DTO). It is a Phase-9-shaped change; bundling
it into a defensive-cleanup commit set whose theme is "don't
break the bisect signal" makes the bisect harder, not easier
— if B4 later turns out to introduce a regression, the bisect
lands on a commit that touched both register-I/O patterns AND
descriptor construction.

_Note._ The previously-listed Phase B5 (B0-stepping 1-GiB DMA
ceiling check) is moved to `docs/tracking/yagni-parking-lot.md`. Per
CLAUDE.md ("track deferred ideas in yagni-parking-lot.md"):
the user's Pi is C0+, no B0 board is on hand, the check is
speculative future work, and adding stepping detection + new
error variant + docs entry now would be off-theme for a plan
whose acceptance gate is "C1's Pi-flash gate finally met."

(B4 sections that REMAIN: B4.1, B4.2, B4.3, B4.4 — each
lands as its OWN commit per the bisect-granularity argument
in the suggested commit shape below.)

## Verification model

Two-tier per commit, same as the cacheable-DMA plan:

- **Tier 1 — `make test`**: 100/100 on GICv3 + GICv2 at every
  commit. Catches regression in the bootstrap/probe/claim
  path that fires on QEMU virt. QEMU has coherent DMA, so
  the cache-coherence bugs B2 addresses are NOT exercised by
  QEMU — `make test` passing is necessary-not-sufficient.

- **Tier 2 — Pi flash**: every B1, B2, B3 commit must boot on
  Pi 4B and pass the relevant gate. B4 commits should also
  be Pi-flashed but with the lower bar of "no regression."

  Specific hard gates:
  - After B1.2 (NEON canary): canary passes ≥10 consecutive
    boots. The canary, witnessed against "main + canary -
    B1.1" (B1.1 stashed), reliably fails — proves the canary
    is exercising the gap rather than vacuously passing.
  - After B2.2: `[FAT32-TEST] read 17 bytes: hello from
    fat32` AND `posix-hello: hello from fat32` on a
    diagnostic-clean kernel. B2.2 is the load-bearing commit
    for this gate.

## Risks

1. **B1.1 changes the ABI of every TCB.** Risk: stale
   process state in long-lived TCBs (none today; every TCB
   is created post-B1.1) or in any TCB serialised to disk
   (none today). No mitigation needed beyond the compile-
   time layout assertion. The 528-byte SavedContext growth
   is paid once per TCB at allocation and once per context
   switch in cycles; quantify in commit message but expect
   sub-microsecond impact.

2. **B1.2 canary may surface OTHER NEON bugs that aren't
   the context-switch gap.** If the canary fails post-B1.1
   too, investigate further before declaring B1 done.
   Possible secondary bug: a kernel codepath that the
   soft-float target somehow lets emit NEON (verify via
   `objdump | grep -E "v[0-9]+\."` after build).

3. **B2.0 may not give a clean signal.** If applying
   `sys_dma_sync_for_device` AND deleting the pre-zero
   still produces `0x0` sometimes and `0xAA55` other times,
   multiple causes are superimposed. Mitigation: land B2.1
   + B2.2 anyway (they're the principled fixes at both
   layers), then run B3 (Arasan) in parallel.

4. **B3 may be unnecessary if B2 fully fixes it.** Wasted
   work if B2 is sufficient. Mitigation: B3 is conditional
   on B2.0's outcome — only triggered if `sys_dma_sync_for_device`
   + delete-pre-zero still returns `0x0`.

5. **B2.1's clean-everywhere primitive imposes a writeback
   cost on every `sys_dma_sync_for_cpu` call.** For our
   current driver (one 512-byte read per selftest) this is
   negligible. For future high-bandwidth devices (e.g.
   network RX with large rings) the writeback of clean lines
   is wasted work. If this surfaces as a hot path, the
   counter-design is "tag DmaPool allocations with a
   was-dirty flag and only call civac when set." Tech debt;
   not in this plan's scope.

## Out of scope

- Phase 9 (emmc2 typed-MMIO conversion) — this plan must
  complete before Phase 9 resumes; the Phase 9 commits are
  separate.
- IRQ-driven completion for emmc2 — the `m7-irq-experiment`
  branch's GIC fixes + BoundIrq helper are independent of
  this plan; they feed back to mainline as a separate
  commit AFTER B1 (when the polling shape is validated end-
  to-end). The order matters: confirm polling-shape data
  visibility first, then convert to IRQ-driven.
- Cache-line-aware `OwnedDmaMapping` typed wrapper — the
  fuse of sync calls with the access pattern. Tech debt;
  not in this plan's scope.
- Multi-block CMD18 + Auto-CMD23 re-enablement (dead-code
  in `adma2_transfer` today). Same architectural concerns
  as CMD17 once those are settled.

## What this unblocks

- **Silent NEON-corruption class eliminated** (after B1):
  the canary protects against silent regressions; new drivers
  or libraries that use NEON can rely on context-switch
  preserving full architectural SIMD state. Every subsequent
  Pi-flash investigation in this plan runs on a clean
  baseline where "could user-mode NEON corruption be
  contributing?" is no longer a confounding hypothesis.
- **C1's Pi-flash gate finally met** (after B2):
  the cacheable-DMA migration is empirically validated on
  Pi; the chain from `engine.read` → cache-sync → consumer
  read works without diagnostic-print drain.
- **Phase 9 resumes** (`docs/history/cacheable-dma-migration-plan.md`
  + `nifty-rolling-naur.md`): P9.4 driver work picks up
  from the post-C1 baseline; subsequent P9.5-P9.12 commits
  Pi-validate cleanly.
- **m7-irq-experiment merges to main** as a follow-up
  IRQ-conversion commit: the GIC ICFGR/ITARGETSR fix,
  BoundIrq helper, sys_unmask_irq with caller-token check.
- **Future Pi-side IRQ binding works** (after B4.3): UART
  and emmc2 INTIDs above 96 can be bound without silent
  failure.

## Suggested commit shape

In order:

1. **`kernel: save/restore v0-v31 + FPCR + FPSR in context
   switch` (B1.1)** — user-mode NEON gap closed; all 32 V
   registers preserved at the scheduler boundary.
2. **`tests: NEON canary user-mode binary` (B1.2)** —
   regression guard. Witness-commit failure proves the canary
   exercises the gap; passes on B1.1.
3. B2.0 — non-committed disambiguating flash (delete pre-zero
   + add `sys_dma_sync_for_device`); results inform whether
   Phase B3 is needed.
4. **`kernel: cache::invalidate_range uses dc civac
   everywhere + fix dc ivac comment` (B2.1)** — kernel
   primitive made safe-by-default. Defensive hardening for
   future drivers; not load-bearing for the symptom. Folds in
   the old B4.5 comment fix because the same function body is
   being rewritten.
5. **`emmc2-driver: sync_for_device on buffer before CMD17`
   (B2.2)** — driver-side protocol contract. **Load-bearing
   for the C1 Pi-flash gate.**
6. Pi-flash gate met → confirm `0xAA55` before proceeding.
7. (If needed, per B2.0 outcome) **`emmc2-driver: Arasan
   shadow-register workaround for adjacent 16-bit writes`
   (B3.1)** — errata fix.
8. (If needed) **`emmc2-driver: 4-clock-cycle delay at
   ≤400 kHz ID phase` (B3.2)** — second errata fix.
9. **`emmc2-driver: DAT_INHIBIT poll after TRANSFER_COMPLETE`
   (B4.1)** — its own commit. Touches `adma2_single_block_read`
   completion path. If a regression lands here it's isolable
   to the poll.
10. **`emmc2-driver: pre-CMD NORMAL_INT_STATUS write-all
    clear` (B4.2)** — its own commit. Touches `adma2_single_block_read`
    command-issue path. Bundling with B4.1 would put two
    completion-vs-issue-side changes on one bisect point.
11. **`kernel: MAX_BINDINGS 96 → 256` (B4.3)** — Pi 4B IRQ
    table. Single-constant change; independent.
12. **`emmc2-driver: SIGNAL_ENABLE = 0 after reset` (B4.4)** —
    its own commit. Touches bootstrap; if a regression lands
    here it's isolable to the reset path.

(Old B4.5 folded into B2.1; old B4.6 deferred to Phase 9's
P9.9; old B5.1 moved to `docs/tracking/yagni-parking-lot.md`.)

Each Pi-flash-relevant commit gets a labelled image preserved
in the working tree per the existing pattern.

## Open questions for review

1. **Should B2.2 (driver-side pre-DMA sync) be in the typed
   wrapper instead of the driver?** Eventually yes (the
   `OwnedDmaMapping` typed surface should fuse sync calls
   with access patterns), but for this plan the driver call
   site is sufficient. The wrapper migration is its own
   future work.

2. **Should B1.1 save only `v8-v15` (AAPCS64-callee-saved
   subset)?** No — preemption is asynchronous, not a function
   call. The full v0-v31 + FPCR + FPSR set must be preserved.
   See B1.1's rationale paragraph for the path-walk.

3. **Should B4.3 (MAX_BINDINGS) be in this plan or wait for
   the m7-irq-experiment merge?** Land here. It's a constant
   bump with no behavior change today; doing it independently
   shortens the m7-irq merge's diff.

4. **Should the NEON canary (B1.2) live in `user/` (as a
   separately-spawned binary) or be folded into an existing
   test process?** Separate binary — keeps the canary's
   spawn semantics explicit and lets the user disable it
   independently if it becomes noisy.

5. **Pi flash budget for this plan**:
   - B1.1 (NEON save/restore) — 1 flash.
   - B1.2 (canary) — 2 flashes (witness against "main +
     canary - B1.1", then with B1.1 restored).
   - B2.0 (disambiguating, non-committed) — 1 flash.
   - B2.1, B2.2 (kernel + driver) — 2 flashes.
   - B4.1, B4.2, B4.3, B4.4 — 4 sanity flashes (one each,
     per the bisect-granularity decision).
   - B3.1, B3.2 (conditional on B2.0 outcome) — 2 flashes
     if needed.
   - Upper bound: 12 Pi flashes if every commit gets its own
     flash and B3 is needed. Lower bound: ~9 flashes if B3
     is skipped and B4 sanity flashes are batched (e.g.
     flash once after B4.1-B4.4 land together). Acceptable
     given the unblocking value.
