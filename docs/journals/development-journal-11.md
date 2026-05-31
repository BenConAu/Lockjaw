# Development Journal: The Coherence Envelope, the Re-Export Restructure, and the Reviewer Loop That Earned Its Keep

Written after the Phase 9–10 typed-MMIO substrate effort — ten commits
from `747c5fb` to `653c1f5`. The headline result: emmc2 — the last and
biggest driver, the one that exposed every silent bug shape the
substrate was meant to prevent — now drives all DMA through a
declarative coherence envelope, with zero `unsafe` in driver source and
two sanctioned raw syscalls (`sys_exit` for the panic handler,
`sys_debug_puts` for diagnostics). The driver regime is enforced by
construction: forbidden `sys_*` are no longer reachable from
`use lockjaw_userlib::*;`. The `check-driver-unsafe` xtask is the
backstop, not the gate.

It was also a session where the paired Codex + opus review loop went
from "habit" to "documented procedure." Four rounds on the
`check-driver-unsafe` xtask, each round genuinely catching a new
evasion class, ended with opus calling out the architectural move that
made the whole question different: don't make the wrong thing
detectable; make it unreachable.

## The five-axis problem the substrate was paying for

Before any of this, emmc2's ADMA2 read called `sys_dma_sync_for_cpu`
and `sys_dma_sync_for_device` by hand around every transfer. Five
things had to be right every time:

1. The right pageset handle.
2. The right offset within that pageset.
3. The right direction (clean before for `ToDevice`, invalidate after
   for `FromDevice`).
4. The right *number* of syncs — the `FromDevice` pre-clean (B2.2) was
   missed for months. Without it, CPU-dirty cache lines on the buffer
   would write back *over* the device's freshly DMA'd bytes after the
   transfer, producing stale zeros that looked like the kernel had
   succeeded and the controller had failed.
5. The right *order* relative to the device's actual completion — the
   post-IRQ `DAT_INHIBIT` drain (B4.1). The CPU's `sys_dma_sync_for_cpu`
   only orders cache operations; it does not arbitrate against the
   controller's still-in-flight AXI writes. Returning from the IRQ
   handler before those writes committed produced the *same* stale-zero
   symptom on the standalone selftest path (which didn't have the
   implicit drain that the long FAT32 chains accidentally gave it).

Five things, every transfer, all on the driver author. QEMU's coherent
bus hid four of the five from CI entirely. The bugs only surfaced on
the Pi 4B flash gate, and even there the symptoms looked like
controller flakiness, not coherence-ordering bugs.

This was Phase 9's brief: take the discipline away.

## The envelope: clean → kick → await → invalidate

The shape that emerged was a generic operation envelope in
`lockjaw-userlib::dma_transfer`:

```rust
pub fn run_dma_transfer<T, K, C, F>(
    regions: &[DmaRegion],
    completion: C,
    kick: F,
) -> Result<T, DmaTransferError<K, C::Error>>
where
    C: DmaCompletion,
    F: FnOnce() -> Result<T, K>,
```

The driver supplies what only the driver knows: a slice of `DmaRegion`s
(each tagged with `DmaDir::{ToDevice, FromDevice}`), a `DmaCompletion`
implementation that says "when is the device done", and a `kick`
closure that programs the controller and issues the operation. The
envelope owns the ordering — clean every region (both directions; the
`FromDevice` pre-clean *is* B2.2 in code), call `kick`, await the
completion, then invalidate every `FromDevice` region.

Three of the five axes mechanized cleanly. The fourth — *that* the
completion comes before the invalidate — became structural: the
envelope literally cannot run the post-invalidate before
`completion.await_complete()` returns, because that's the control flow.

The fifth — what regions exist, in what direction — is the one piece
the driver names. It can't be mechanized; only the driver knows what
its transfer touches. But that's the *minimum* information needed, and
the substrate makes it the *only* information needed.

## SyncCapable: making wrong-origin sync a compile error

There's an earlier problem the envelope had to handle: the kernel's
`sys_dma_sync_*` syscalls reject Buddy-origin pagesets (they're for
the coherent-bus QEMU virtio path, never touched by cache maintenance).
P9.10 had introduced origin types (`BuddyOrigin` / `DmaPoolOrigin`,
sealed under `DmaOrigin`) on `OwnedDmaMapping<O>` and `DmaBacking<O>`
to make this distinction at the allocation level. P9.11a closed the
sync side: `DmaRegion` has no public constructor, and the
`dma_region(offset, len, dir)` method is only `impl<O: SyncCapable>` —
where `SyncCapable` is a sealed sub-trait of `DmaOrigin` that only
`DmaPoolOrigin` implements.

So a driver that holds a Buddy-origin mapping (virtio-blk, ramfb,
fwcfg, virtqueue) can't even *construct* the value the envelope wants.
Handing one to a sync is a compile error at the `.dma_region(...)`
call site, not a runtime `INVALID_PARAMETER` from the kernel only
discovered on a hardware flash. The kernel's runtime check is still
there as defense-in-depth, but a driver that compiles cannot reach
through this path.

The first version had `OwnedDmaMapping<O: DmaOrigin = BuddyOrigin>` —
a default that minimized the migration ripple in the four Buddy users.
Ben overruled it: "explicit origin only plz." That conversation was
quick but principled — defaulting to the coherence-*incapable* origin
biases toward the wrong-on-hardware choice, exactly the
"coincidentally correct" pattern Tier 3 #13 warns about. Dropped the
default; added `<BuddyOrigin>` at the four sites; closed the gap.

## The two-envelope read

emmc2's ADMA2 single-block read has two regions that need coherence
work: the descriptor (CPU writes the ADMA2 entry; controller reads it)
and the buffer (controller writes the data; CPU reads it). Two
envelopes, nested:

- **Outer**, in `read()`: the whole buffer is a `FromDevice` region.
  Completion is `Immediate` because the per-block kick blocks. The
  `kick` runs the per-block loop, each iteration calling
  `adma2_single_block_read`.
- **Inner**, in `adma2_single_block_read`: the 8-byte descriptor is a
  `ToDevice` region. Completion is a new `SdhciDataCompletion` that
  wraps the IRQ-driven `CMD_COMPLETE` / `DATA_COMPLETE` wait *and* the
  B4.1 `DAT_INHIBIT` drain. The `kick` programs the controller (ADMA
  address, block size/count, combined `TRANSFER_MODE+COMMAND` write).

The buffer's outer clean covers B2.2; the descriptor's inner clean
covers the post-cacheable-DMA-migration C1 case; the buffer's outer
invalidate covers the post-`DATA_COMPLETE` read-back. Every cache-
coherence event in the read path is now the substrate's emission of
`dc cvac` (clean) or `dc civac` (clean-and-invalidate; the B2.1
replacement for the unpredictable-on-dirty-lines `dc ivac`) against
ranges the driver named once, in the type system.

The slot table also had to change. The pre-P9.11b `DmaBuf` was
`{ handle: u64, pa: u64, sector_count: u64 }` — Copy, raw handle, easy
to pass around. Codex flagged that an early attempt at making
`DmaBacking` itself `Copy` was an ownership lie (the backing owns a
closable pageset handle; `Copy` lets you accidentally hold two owners
and close one's handle while the other still uses it). So `DmaBuf`
became `{ backing: DmaBacking<DmaPoolOrigin>, sector_count }`,
move-only, stored as `[Option<DmaBuf>; N]`, with `find_buf` returning a
`&DmaBuf` borrow and `free_buffer` calling `slot.take()` before close.
The driver-internal restructure was bigger than the envelope itself,
but it's what made the per-buffer DmaRegion minting actually safe.

Pi flashed clean: `[BLOCKDEV] /dev/sd0 ready ... selftest read OK`
(the load-bearing one — the standalone selftest is the path the B4.1
drain exists to make work), `[FAT32-TEST] read 17 bytes: hello from
fat32`, `posix-hello: hello from fat32`, `[NEON-CANARY] PASS`. P9.12
added `#![deny(unsafe_code)]` to the emmc2 crate root — there was
nothing left to deny, but the compile-error guarantee landed.

## Construction, not detection: opus's pivot

Phase 10's plan called for a `check-driver-unsafe` xtask that would
enforce three rules across every driver crate: `#![deny(unsafe_code)]`
at the root, zero `#[allow(unsafe_code)]` attributes anywhere, no
direct raw-syscall calls outside the `{sys_exit, sys_debug_puts}`
allowlist. The first version was textual: a comment- and string-
literal-aware blanker followed by a substring scan for
`sys_<ident>(`. The four real drivers passed; three injected
violations in uart-driver failed end-to-end with the right findings.
Solid.

Codex's review came back with two High findings about the syscall
check. The `sys_<ident>(` matcher was too syntax-narrow: it didn't
handle `sys_alloc_pages::<T>(...)` (turbofish) or `sys_alloc_pages\n(...)`
(line-broken paren). And the deny check matched the substring
`deny(unsafe_code)` anywhere in the file, which would mis-classify an
outer `#[deny(unsafe_code)]` on a helper item as crate-level
enforcement.

Rewrote with `syn`. Crate-level deny became `attr.style ==
AttrStyle::Inner && parse_nested_meta(...)`. Allow became a
`syn::visit::Visit` impl walking every attribute in the AST,
tolerating spaced/list-form. Syscall scan became `visit_item_use` (walk
the use-tree leaves) + `visit_path` (any segment named `syscall` whose
leaf isn't allowlisted). All the syntax-evasion cases Codex listed
closed. 33 unit tests, green path on all five real drivers, smoke
tests on uart with the new injections all flagged correctly.

Round 3: Codex came back with a different fix-now. Aliased imports
evaded the scan — `use lockjaw_userlib::syscall as sc; sc::sys_x()`.
The use-statement names `syscall`, but the call site has segments
`[sc, sys_x]`. And even an AST scan can't follow aliases without name
resolution, which `syn` doesn't do. Opus's review of the same commit
went further: the `sys_*` wrappers in `lockjaw-userlib` are *safe*
`pub fn` (the inner `unsafe` is the `asm!`), so `deny(unsafe_code)`
doesn't backstop the syscall rule the way it does the unsafe rule.
The xtask check is the sole gate. A textual or AST scan with this
weakness is "discipline wearing a construction label."

Then opus said the thing that reframed the whole problem:

> "Don't detect forbidden syscalls in driver source at all — make the
> names unreachable."

The fix wasn't in the xtask. It was in `lockjaw-userlib`'s root
re-export. The crate had `pub use syscall::*` at the crate root,
meaning `use lockjaw_userlib::*;` in any driver brought the full
syscall surface into scope. Trim that to `pub use syscall::{sys_exit,
sys_debug_puts};` and the forbidden names are no longer in scope to
alias from. A driver that wants `sys_alloc_pages` has to write the
explicit `lockjaw_userlib::syscall::sys_alloc_pages` path — and the
xtask's job becomes "does driver source mention `syscall::` at all?",
which is trivial to detect *because the path is the import statement*.
The alias `use ...syscall as sc;` *still names `syscall` in the use
line*, even though the call site uses `sc`. Caught at the import.

The restructure rippled across 11 non-driver crates that used the root
glob for syscalls — init, the servers, the tests. Each gained one
`use lockjaw_userlib::syscall::*;` line; the three crates with
specific syscall imports (hello, neon-canary, sleep-test) split their
use blocks. Driver crates were untouched. Both reviewers approved the
restructured re-export as the right move, then asked me to trim the
non-sys items (`BootInfo`, `SchedTelemetry`, `IRQ_FLAG_EDGE`,
`park_forever`) out of the root re-export too — they had zero driver
consumers, and surfacing names "just because the wildcard used to
surface them" undoes the point of selective re-export.

## Four rounds, each catching something new

The `check-driver-unsafe` xtask took four review rounds end to end:

- **Round 1** (textual): codex blocked on enforcement strength; opus
  blocked on attribute-syntax brittleness.
- **Round 2** (syn for deny/allow): both approved the attribute checks;
  codex flagged turbofish + line-break + alias evasions in the syscall
  scan; opus listed five missed shapes (brace import, brace-with-
  rename, brace-with-self, spaced path, UFCS qself) and pointed out
  that brace-imports are the dominant idiom in every existing driver.
- **Round 3** (syn for syscall path + the re-export restructure): all
  five round-2 shapes closed by `visit_item_use` + `visit_path`; the
  module-alias `use ...syscall as sc;` case closed because the use
  walker sees the original name; codex flagged macro opacity (`syn`
  doesn't parse inside `macro_rules!` rule bodies); opus independently
  flagged the same macro evasion *and* a raw-ident evasion
  (`r#syscall` resolves to `syscall` but `Ident::to_string()` returns
  `"r#syscall"` so the string compare failed).
- **Round 4** (`visit_macro` token-stream scan + `ident_str`
  raw-ident normalization): codex returned no correctness findings.

Each round was load-bearing. Not polishing. Three would have been
ideal; four was justified by genuine new evasion classes surfacing
each time. The journal-9 lesson — that pair-review catches things the
single-reviewer pass misses — held even harder on Phase 10 C than on
prior phases, because the failure mode (a false-negative in an
enforcement tool) is exactly the kind of correctness problem one
reviewer reading the same code in the same way wouldn't notice.

## Where opus and codex differ

Across this whole effort the two reviewers consistently complemented
each other rather than duplicating each other. Codex tends to focus on
syntax-class enumeration: "here are five variants of the same idiom
your scanner doesn't handle." Opus tends to reframe: "the question
isn't 'how do I make my scanner robust' — it's 'why is my scanner the
gate at all'." Codex's macro-opacity finding and opus's macro-opacity
finding were the same item phrased identically; opus's raw-ident
finding and codex's separate take on the same diff didn't overlap.
Phase 9.11a's biggest catch was codex (the `Copy` derive on
`DmaBacking` is an ownership lie); Phase 10's biggest catch was opus
(make the names unreachable). I expect this pattern to repeat —
they're not redundant.

The workflow that crystallized:

1. Stage the diff. **Don't self-review.**
2. Launch codex (resumable thread — same `thread_id` across rounds) +
   opus (Agent tool, model=opus, run_in_background, reads
   `docs/reviewer-mode.md` at the top of its prompt). Both run on the
   same staged diff.
3. Read both verdicts before commit. Fix-now findings go before
   commit; tech-debt goes in `docs/tech-debt.md`.
4. Iterate. Codex remembers prior rounds via the resumable thread.
   Opus is fresh each round; the prompt re-briefs it on what changed
   since the last round.
5. Three rounds is normal. Four is justified when each round is new
   evasions, not polish. Five-plus means the design needs a step back.

The doc that captures all of this is
`docs/paired-review-workflow.md` — companion to the reviewer-side
`docs/reviewer-mode.md` and `docs/reviewer-mode-codex.md` files. The
orchestrator reads the workflow doc; the reviewers read their
respective mode docs. I don't read either reviewer's mode doc —
that's the load-bearing division of labor Ben asked for: "the review
is staged by you and done by other agents, you don't review it
yourself in the typical flow."

## What landed

Ten commits, all reviewed (paired Codex + opus), all `make test` green,
two Pi-flash gates passed (P9.11b for the emmc2 conversion;
restructure + Phase 10 C bundled for the re-export and the xtask):

- `P9.10` DMA allocation → typed mappings (the substrate-first half:
  `OwnedDmaMapping`, `BorrowedDmaMapping`, `DmaBacking`, the origin
  markers).
- `P9.11a` generic coherence envelope (`run_dma_transfer`,
  `DmaCompletion`, `DmaRegion`, `DmaDir` with discriminants from 1,
  `SyncCapable` gating).
- `dma: require explicit DMA origin` (drop the `BuddyOrigin` default).
- `P9.11b` emmc2 ADMA2 conversion (`SdhciDataCompletion`, two-envelope
  read, move-only `DmaBacking<DmaPoolOrigin>` slot table,
  `sys_dma_sync_*` demoted to `pub(crate)`).
- `P9.12` `#![deny(unsafe_code)]` on emmc2.
- `lockjaw-userlib: drop the root sys_* glob` (construction half —
  forbidden syscalls unreachable from `use lockjaw_userlib::*;`).
- `Phase 10 C: check-driver-unsafe xtask` (syn AST + `visit_macro`
  token-stream scan + `r#`-ident normalization + the `syscall::sys_*`
  path detector; wired into `make build`).
- `Phase 10 D: regspec / wirespec format reference docs`.
- `Phase 10 E: driver-substrate book chapter + CLAUDE.md + tech-debt`
  (the chapter at `docs/book-of-lockjaw/04-driver-substrate.md` frames
  DMA coherence by construction as the worked example).
- `docs: paired-review-workflow.md` (the orchestrator-side review-loop
  guide).

The user-mode driver regime as it stands today: zero `unsafe` in
driver source, only `sys_exit` and `sys_debug_puts` reachable, DMA
coherence owned by the envelope, the regime enforced at the import
boundary by construction (re-export trim) and backstopped by
`cargo xtask check-driver-unsafe` in `make build`. The next
architectural move — the `SdhciCommandInit<S>` no-bypass operation
layer that gates raw DMA PAs and command issuance behind a type-state
layer (the rubric R3 remainder) — is tracked in `docs/tech-debt.md`
against the second SDHCI consumer.

## What this also means

The journal-9 / journal-10 pattern — more design, less greenfield
code, more cross-cutting changes — held all the way through this
session too. The Phase 10 C iteration in particular was an exercise
in "the right fix is one layer up." Round 1's textual scan was
trying to enforce the regime as discipline. Round 2's syn-based scan
was trying to enforce it as parser-coverage. Round 3's re-export
restructure moved the enforcement to the place where it *can't* be
evaded by syntax: the import boundary. The xtask still runs, but
it's a backstop for the explicit-path escape, not the gate. The gate
is `rustc` failing on an unresolved name. That's the same payoff as
the kernel's push→pull conversions: a class of compliance bug moves
from "the author had to remember it" to "the type system enforces
it" — except here "the type system" is really "the public-API
surface."

The other lesson: paired review is worth its cycles when the failure
mode is a false-negative in an enforcement tool. Three rounds was
the median for the substantive commits; four was needed for Phase 10 C
because each round genuinely exposed a new evasion. The cost is real
(reviewer cycles, prompt drafting, fix-then-restage). The benefit
shows up exactly when single-reviewer review wouldn't.

The session also produced its own meta-artifact: the
`paired-review-workflow.md` doc, written so a future Claude Code
session can pick the loop up cold. Reviewer-mode docs already existed
for the reviewers; the orchestrator-side doc was missing. Writing it
took twenty minutes; the next time the same loop is needed, the cost
to spin it up should be near zero.

## Where things stand

Phase 9 + Phase 10 of the master plan
(`/Users/Ben/.claude/plans/nifty-rolling-naur.md` and its descendant
plan files) are complete. The typed-MMIO substrate effort that started
back at Phase 1 of that plan — typed register accessors, then typed
wire DTOs, then driver-side `claim_typed`, then the per-family
helpers, then the coherence envelope, then the regime lockdown — is
done end to end. The next typed-MMIO concern is the `SdhciCommandInit`
layer when it forces; the next driver concern is whatever the second
SDHCI consumer (eMMC1 on Pi 4B, or a future SoC's eMMC chip) needs.

What this also enables, but isn't in scope yet:

- **Pi 4B display driver** — ramfb is QEMU-only; a real Pi display
  driver would go through the same regime now that the substrate is
  there to support it without new framework code.
- **The `SdhciCommandInit<S>` operation layer** — when the second SDHCI
  consumer lands. Tracked.
- **Architecture hardening on the kernel side** — the typed-MMIO work
  was the userspace-substrate slice of the broader push→pull /
  make-illegal-states-unrepresentable agenda. The kernel side
  (handle-revocation, KVA migration, kernel-image relink) was journal
  9's and journal 10's story; the substrate side is this one. Both
  continue.

Test counts: lockjaw-types host suite at ~750 (the `dma_coherence`
host tests added three to the prior 747). xtask gained 40 unit tests
for `check-driver-unsafe` covering every evasion class the four
reviewer rounds surfaced. QEMU integration: 87/87 on both GICv3 and
GICv2. Pi 4B: two clean flashes — the emmc2 conversion (P9.11b) and
the restructure + Phase 10 C bundle.
