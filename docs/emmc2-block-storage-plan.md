# Pi 4B emmc2 Block Storage + CPRMAN Clock Tree

## Context

Lockjaw boots end-to-end on a real Pi 4B today (journal-10 has the
log) but has no working storage there. The QEMU integration uses a
virtio-blk device + FAT32 server; on Pi the same FAT32 server has
nothing to talk to. The Pi's microSD slot is wired to the BCM2711
emmc2 SDHCI controller at MMIO `0xFE340000`, but emmc2 needs a
provisioned CPRMAN clock — and Lockjaw has no clock infrastructure
at all today (no `ClockId` type, no clock controller driver, no DTB
`clocks` property resolution).

This plan brings up:
1. The clock tree prerequisite (M0a–M0c): DTB clocks-reference
   resolution, the BCM2711 CPRMAN driver scoped to the storage path,
   and a clock-capability handle.
2. The emmc2 block driver itself (M1–M6): MMIO + capabilities,
   ID-mode enumeration, PIO read/write, ADMA2 DMA path.
3. The block-device userspace server (M7) so the existing
   fat32-server can swap virtio-blk → emmc2 transparently.

The full milestone breakdown lives at
`/Users/Ben/Downloads/emmc2-block-storage-plan.md`. This file
maps each milestone to existing Lockjaw infrastructure, calls
out the gaps that need new code, and pins down the plan's open
design questions.

## Out of scope

- The legacy `sdhci-iproc` controller at `0xFE300000` (Pi 4B
  wireless).
- eMMC commands (CMD1, MMC switch). emmc2 on a Pi 4B drives the
  microSD slot only.
- UHS modes, tuning, DDR50, SDR104. Cap at SD High-Speed
  (50 MHz).
- SDSC (pre-2 GB byte-addressed) cards.
- Clocks other than CM_EMMC2. UART / SYSTIMER / GPU clocks
  remain whatever VC firmware programmed at boot; CPRMAN
  returns `NotSupported` for them.

## Existing infrastructure we reuse

The major architectural pieces already exist; this plan adds
new drivers + new types behind the established patterns rather
than building new frameworks:

- **DTB walking** in `lockjaw-types/src/fdt.rs` — handles
  compatible-string hashing, reg/interrupts, `ranges` address
  translation. **Missing:** phandle resolution, `clocks`
  property, `#clock-cells` reading. (M0a adds these.)
- **Device manager** in `user/device-manager/src/main.rs` —
  serves `CMD_CLAIM_DEVICE` / `CMD_PROBE_DEVICE` /
  `CMD_CLAIM_BY_ADDR`, hands a driver an MMIO PageSet handle +
  INTID. We reuse this for emmc2 (M1) and extend it as the
  clock-provider registry (M0c).
- **Userspace driver pattern** — `user/uart-driver/`,
  `user/virtio-blk-driver/`, `user/ramfb-driver/`. Bootstrap →
  claim device → map MMIO via `sys_map_pages(...,
  MAP_FLAG_DEVICE)` → bind IRQ via `sys_bind_irq` → run engine
  loop. CPRMAN (M0b) and emmc2 (M1+) follow this shape.
- **Block-server framework** in
  `user/lockjaw-userlib/src/block.rs` — `BlockEngine` trait,
  `run_block_server`, `BlockClient`. IPC shape: `CMD_GET_INFO`
  / `CMD_ALLOC_BUFFER` / `CMD_READ` / `CMD_WRITE` /
  `CMD_FREE_BUFFER`. M7 implements the trait against emmc2.
  fat32-server consumes `BlockClient` unchanged.
- **IRQ binding** via `sys_bind_irq(intid, notif_handle)`
  (`src/syscall/handler.rs`) — fires a Notification waiter.
  Used by M2+ for transfer-complete and error interrupts.
- **`MAP_FLAG_DEVICE`** in `lockjaw-types/src/vmem.rs:225` →
  MAIR_DEVICE attributes. Used for MMIO mappings (M1+).
- **No new syscalls.** Capability-export + endpoint IPC cover
  the clock cap (M0c) and block server (M7).

## Open design questions — resolved

The user's plan flagged seven open questions. Recommendations
below; rationale in the per-milestone notes.

1. **PageSet donation lifetime for DMA.** *Donate-then-return*
   (caller donates buffer for the call, server returns it via
   reply). Matches `BlockEngine` semantics in
   `lockjaw-userlib/src/block.rs`; existing `BufferTracker`
   already handles per-caller-token isolation. Donate-then-
   remap adds capability bookkeeping for negligible zero-copy
   benefit on disk-IO timescales.
2. **Error path.** Synchronous status code in the IPC reply
   (matches `BLK_OK` / `BLK_TIMEOUT` / `BLK_CRC_ERROR` etc. in
   `lockjaw-types/src/block.rs`). Event-port handles for
   failures expand the protocol surface for marginal benefit.
3. **NormalNonCacheable mapping placement.** *Typed enum at
   the syscall boundary, kernel-side change is minimum-
   forced.* The MAIR/MMU is kernel-managed by hardware
   definition — userspace cannot install a non-cacheable PTE
   without telling the kernel. So `sys_map_pages` has to
   accept a memory-attribute selector. The minimum kernel
   surface: one new MAIR slot (`MAIR_NORMAL_NC`) + one new
   enum variant (`MapMemoryAttribute::NormalNonCacheable`).

   The variant is named after the MAIR regime
   (`NormalNonCacheable`), not the use case
   (`DmaCoherent` / `DeviceDmaCoherent`). The kernel reserves
   `Device` strictly for MMIO mappings; non-cached normal RAM
   is a different regime and the type system should reflect
   that. Future readers see exactly which MAIR slot the
   variant uses.

   Replace the existing bit-flag `flags: u64` parameter with
   the typed enum (one discriminant byte). Bit flags would
   invite illegal combinations (`DEVICE | NORMAL_NC`?
   undefined) — the same kind of runtime confusion typed
   enums prevent. Per Tier 3 #10 ("strong typing wherever
   possible") the syscall boundary should be typed when a
   typed alternative exists.

   *Migration cost:* every `sys_map_pages` caller (a small
   handful: process.rs, sys_map_pages handler, posix-server's
   mmap path, fat32-server's buffer mappings) updates to pass
   the enum variant. Mechanical. Worth doing in M6 alongside
   the new variant rather than later as cleanup.

4. **CMD12 vs Auto-CMD12 vs Auto-CMD23.** *Auto-CMD23*
   (set BlockCount before transfer, no post-data CMD12).
   Cleanest state machine, modern SD spec compliant.
5. **Bus sharing.** *Single driver owns the controller*
   (matches existing per-device claim model). Future
   queueing layer is a follow-up if multi-tenant block I/O
   becomes a need; out of scope.
6. **`ClockId` namespacing.** *Per-controller enums in
   lockjaw-types*. Define
   `lockjaw_types::clock::cprman::ClockId::Emmc2` rather than
   a global `ClockId::Emmc2`. Different controllers (a future
   I2C clock provider, etc.) get their own enum scope.
   Cleaner discoverability. The kernel never sees this enum —
   it's a userspace protocol type carried in IPC message
   bodies.
7. **CPRMAN ownership of unmanaged clocks.** *Read-but-refuse-
   to-disable.* CPRMAN takes ownership of the register region;
   reading any clock's registers is side-effect-free, but
   `disable(id)` for an unrecognised id returns
   `Err(NotSupported)` — we don't want to silently kill the
   clock the VC firmware brought up for UART.

8. **Clock cap shape (added by review).** *Plain
   `HandleKind::Endpoint` with kernel-assigned caller_token.*
   No new kernel feature. CPRMAN userspace owns the
   token→ClockId routing table; the table uses monotonic,
   never-reclaim entries (see M0c lifetime model) so no
   handle-close notification is needed from the kernel. The
   typed `ClockClient` / `ClockOp` wrappers live in
   `lockjaw-userlib`/`lockjaw-types`.

   The principles question is *where* type discipline
   belongs. Adding `HandleKind::Clock { kva, clock_id }` to
   the kernel would give us a typed cap variant — but the
   protocol it would type is a userspace driver protocol,
   and the kernel has no need to interpret clock_id. Per the
   new Tier 2 #6 ("reach for userspace before reaching for
   the kernel" — see "Required principles update" below),
   the right home for the typing is the userspace client
   wrapper plus the lockjaw-types message shapes.

   Same shape as `BlockEngine` (typed trait in userlib +
   generic Endpoint cap) and `DisplayEngine` — the
   established Lockjaw pattern for userspace driver
   protocols.

## Strategic intent

**Microkernel discipline first, typed regimes second.** The
clock capability could have a typed `HandleKind::Clock`
variant — but the kernel doesn't need to interpret clocks,
and adding a kernel feature for what userspace can solve
violates the microkernel principle. The typed regime lives
where the regime lives: typed `ClockClient` / `ClockOp`
wrappers in `lockjaw-userlib` + `lockjaw-types`, plain
`HandleKind::Endpoint` cap with caller_token in the kernel.
Same shape as `BlockEngine` / `DisplayEngine`.

The M6 memory-attribute enum is the only kernel-side typed-
regime addition in the plan, and it's forced by hardware: the
MMU/MAIR is kernel-managed by definition, so non-cacheable
mappings have to be selected at the kernel boundary. Even
there, the kernel surface is the absolute minimum: one new
MAIR slot + one new enum variant to select it.

**Pure logic in lockjaw-types, side effects in the userspace
driver.** The CPRMAN divider math (PLLD rate + target Hz →
integer + fractional divider, with rounding + clamping) is
pure and host-testable. The BCM2711 register offsets, MAIR
password (`0x5A`), and CM_/A2W_ register layout go in
`lockjaw-types/src/clock/cprman.rs` as typed constants with
compile-time invariants. The userspace CPRMAN driver does
only the volatile MMIO writes. Same shape as virtio types
(`lockjaw-types/src/virtio.rs`), block protocol
(`lockjaw-types/src/block.rs`), and the IPC state machine —
the established pattern for "hardware controller logic" in
Lockjaw (Tier 2 — minimize kernel work, push protocol logic
to userspace).

**Existing capability + driver patterns reused.** CPRMAN is
a userspace process; it gets its register region via the
device-manager claim path just like uart-driver and
virtio-blk-driver. Clock consumers (emmc2 driver in M0c+)
ask device-manager for a clock cap, which is just an exported
endpoint to CPRMAN scoped by caller_token. CPRMAN's userspace
routing table maps token → ClockId; consumers can't pivot
their cap to another clock because the token is kernel-
assigned and unforgeable.

**No kernel allocator changes.** Every page CPRMAN and emmc2
need (register MMIO, descriptor rings, DMA buffers) comes from
PageSets allocated via `sys_alloc_pages` and donated to the
right destination.

**Pi 4B in real hardware testing from M2 onward.** M0a–M0b are
host-testable (DTB parser additions + CPRMAN math are pure);
M0c is QEMU-testable (the cap path works on virt too — wire
a stub CPRMAN that returns NotSupported for everything). M1+
is real-hardware-only; QEMU virt's emmc model is incomplete
and would mislead more than help. The Pi tooling (kernel8.img
+ SD card workflow) is already proven (journal-10).

## Required principles update (prerequisite)

The "minimize kernel work" rule needs to be explicit in
`docs/ben_principles.md` before this plan implements. The
current principles say "use strong typing wherever possible"
and "fix substrate debt aggressively" — both pulled toward a
typed `HandleKind::Clock` variant on first reading, even
though the right answer is plain Endpoint + userspace
routing. The missing rule is the microkernel tiebreaker.

Three edits (drafted but not yet applied — they belong in a
separate commit before the M0 work starts):

1. **New Tier 2 principle** (between current #5 and #6),
   renumbering the rest:
   > **Reach for userspace before reaching for the kernel.**
   > Every kernel feature must be justified by what it
   > enables that userspace can't do alone. New HandleKind
   > variants, new syscalls, new kernel-side dispatch paths,
   > new kernel state — all last-resort. The kernel is for
   > hardware (MMU, GIC, TTBRn registers), substrate
   > (PageSets, KVM allocator, scheduler), capability
   > creation, and IPC routing. Everything else is userspace.
   >
   > When this fights Tier 3's "use strong typing" idiom: let
   > this principle win. Typed kernel cap variants enforcing
   > what is really a userspace-driver protocol pushes policy
   > into the kernel. Type the protocol where it lives — in
   > lockjaw-types message shapes and lockjaw-userlib client
   > wrappers — not in the cap layer.

No other principles edits. The new Tier 2 #6 rule is general
on its own; the temptation to add a scope clarifier to Tier
1 #3 ("but userspace token dispatch is fine") was case-
specific reasoning written to retroactively bless M0c.
Principles should stay general — if the new microkernel
tiebreaker is the right shape, it catches the case directly
without spelling out exemptions for our current decision.
Future cases get re-judged on the same general rule.

Ship as one principles-doc commit before M0a. The principles
update is the prerequisite that makes M0c's "plain Endpoint"
decision principled rather than accidental.

## Milestones

Each milestone ends with a runnable boot that prints a
specific success line (per the user's plan). My contribution
here is mapping each to the Lockjaw files involved and the
design decisions made inline.

### M0a — DTB clocks-reference resolution (host-testable)

**Files to extend:**
- `lockjaw-types/src/fdt.rs` — add `clocks` property
  recognition to the property handler (around line 500),
  add `#clock-cells` reading, add a phandle table built
  during the walk (today the parser explicitly omits phandle
  resolution per line 9).
- `lockjaw-types/src/fdt.rs` tests — new fixture-driven
  tests for `clocks = <&phandle N>` resolution against a
  real Pi 4B DTB blob (place a minimal one in
  `lockjaw-types/test-data/`).

**Output shape:** new `DeviceInfo` field `clocks:
[ClockRef; MAX_CLOCKS_PER_DEVICE]` where `ClockRef =
{controller_phandle: u32, clock_id: u32}`. Up to 4 entries
covers all current consumers (emmc2 has 1).

**Success line:** `[DTB] mmc@7e340000 clocks=[(cprman, EMMC2)] resolved`
(printed by device-manager after parsing).

**Single commit. No kernel changes; pure addition to FDT
parser + DeviceInfo. Host tests cover.**

### M0b — BCM2711 CPRMAN driver (storage scope, full API surface)

**New files:**
- `lockjaw-types/src/clock/mod.rs` — `ClockError` enum
  (`NotSupported(id)`, `OutOfRange`, `Hardware`, etc.),
  generic message types for the `set_rate` / `get_rate` /
  `enable` / `disable` IPC.
- `lockjaw-types/src/clock/cprman.rs` — `ClockId` enum
  (just `Emmc2` for now, NotSupported variants reserve
  the namespace), CM_/A2W_ register offsets, password
  constant, parent-PLL lookup table, **pure** divider-math
  function `compute_divider(parent_hz, target_hz) -> (int,
  frac, actual_hz)` with host tests.
- `user/cprman-driver/` — new userspace driver process,
  bootstrap → claim CPRMAN device by compatible string
  `brcm,bcm2711-cprman` → map register region → run server
  loop matching on `ClockOp` IPC messages.

**Implementation scope this milestone:** CM_EMMC2 only.
Other ClockIds return `Err(ClockError::NotSupported)`. The
register-region ownership is real (driver takes the cap, no
one else can map those pages); we just don't implement
divider/gate logic for the unmanaged leaves yet.

**Success lines:**
- `[CPRMAN] init: register region mapped, taking ownership`
- `[CPRMAN] EMMC2 set_rate(200_000_000) -> actual=200_000_000 enabled=1`
- `[CPRMAN] UART get_rate -> NotSupported (deliberate, not implemented this milestone)`

**Test for the third line is the proof of scope discipline.**

**Commit shape:** likely 2 commits — types + driver scaffold
(no behavior change), then CM_EMMC2 implementation. If host
tests for the divider math are isolated enough, can collapse
to one.

### M0c — Clock capability in the cap model

**Decision: plain `HandleKind::Endpoint` with kernel-assigned
caller_token. No kernel change.** Per the new Tier 2 #6
("reach for userspace before reaching for the kernel"), a
typed `HandleKind::Clock` variant would push protocol
interpretation into the kernel for what is fundamentally a
userspace concern. CPRMAN's userspace token→ClockId routing
table is the right home for the typing.

Same shape as `BlockEngine` (typed trait + generic Endpoint
cap, with `caller_token` for per-client isolation) and
`DisplayEngine`. Established Lockjaw pattern.

**Files:**
- `user/device-manager/src/main.rs` — provider-registration
  IPC + new command `CMD_GET_CLOCK_CAP(controller_phandle,
  clock_id) -> Handle`. **Device-manager caches caps per
  (requester_caller_token, ctrl_phandle, clock_id).**
  On repeat call from the same requester for the same clock,
  return the cached handle without going to CPRMAN — same
  caller_token, no new CPRMAN table entry. On first call,
  device-manager asks CPRMAN to mint a cap, stores in
  cache, returns to caller.

  This bounds CPRMAN's table at `(number of distinct
  requester caller_tokens) × (number of distinct clock_ids)`.
  Today: ~5 drivers × ~1 clock each = 5 entries. The fresh-
  token-per-export semantics means a client cannot exhaust
  the table by repeated calls — the cache deduplicates.

  Device-manager's cache is a fixed-size table sized
  generously for current Lockjaw shape (start at 32 entries;
  bump when a real workload approaches the limit). Over-
  cache returns a typed `Err(DevMgrError::CapCacheFull)`.

  Identifying "the requester" uses the caller_token of who
  is calling CMD_GET_CLOCK_CAP on the device-manager
  endpoint. That token is kernel-assigned and stable for the
  lifetime of the device-manager handle the requester holds.
  Same identity model device-manager already uses for
  per-(requester, device) state in CMD_CLAIM_DEVICE.

  **Caveat: cache identity is per-handle, not per-process.**
  If a client drops its device-manager endpoint and
  reacquires one, it gets a fresh caller_token and therefore
  a fresh cache entry + fresh CPRMAN entry. This is fine for
  Lockjaw's current model (drivers bootstrap once and keep
  their handles for the kernel's lifetime), and the
  monotonic-never-reclaim lifetime story above already
  accepts it. But it is **not** true per-process
  deduplication — a process that re-bootstraps (re-spawn,
  rebuild bootstrap chain) loses cache locality. Documented
  here so a future reader doesn't mistake the cache for
  something stronger than it is.
- `user/cprman-driver/src/main.rs` — owns a small fixed-
  size table mapping `caller_token → ClockId`. On every
  `set_rate` / `get_rate` / `enable` / `disable` IPC, look
  up the token, dispatch to the matching clock leaf. Token
  is unforgeable (kernel-assigned), so the cap can't be
  pivoted to a different clock by the holder.

  **Lifetime model: monotonic, never-reclaim tokens, bounded
  by device-manager-side caching.** The kernel does not
  notify CPRMAN when a client drops its clock cap, and
  adding such a notification path would be a new kernel
  feature (Tier 2 #6 says don't). The exhaustion concern
  Codex flagged — fresh caller_token per export means
  repeated CMD_GET_CLOCK_CAP calls would each create a new
  CPRMAN entry — is addressed by device-manager's cache: a
  given (requester, ctrl_phandle, clock_id) triple resolves
  to one CPRMAN entry, no matter how many times the
  requester calls.

  Practical consequences:
  - CPRMAN table is fixed-size (start at 32 entries, bump
    if a real workload approaches the limit). Each entry is
    one (caller_token, ClockId) pair.
  - Bounded by `(distinct requester caller_tokens issued
    a clock cap) × (distinct ClockIds asked for)`. With
    device-manager caching, a client cannot inflate the
    count by repeated calls.
  - Over-cap returns `Err(ClockError::ProviderTableFull)` —
    a typed, honest error per the "no silent stubs" scope-
    discipline rule.
  - Process exit doesn't collect entries (CPRMAN is
    untouched), but practical lifetime growth is one entry
    per (driver lifetime × clock used by that driver).
    Re-spawned drivers add new entries; for today's boot
    pattern (drivers spawn once at boot and live for the
    kernel's lifetime), the table doesn't grow after boot.
  - If a future Lockjaw use case drives many short-lived
    clock-cap acquisitions, revisit. Options at that point:
    liveness probes (CPRMAN periodically pings held
    endpoints; expired entries collected), or add the
    kernel notification path with a real justification.
- `lockjaw-userlib/src/clock.rs` — typed `ClockClient`
  wrapper around a Handle, exposing
  `set_rate(hz_target) -> Result<u64, ClockError>` etc.
  Caller never sees the underlying caller_token; the
  wrapper enforces method-level discipline at the
  userspace/Lockjaw boundary.
- `lockjaw-types/src/clock/mod.rs` — `ClockOp` enum +
  `ClockError` enum used by both ClockClient and CPRMAN's
  receive loop. Same pattern as
  `lockjaw-types/src/block.rs` defines BLK_OK / message
  shapes consumed by both BlockClient and BlockEngine.

**No kernel changes. No new HandleKind variant. No new
syscall. Existing `sys_export_handle` + `sys_call` cover
everything needed.**

**RAII note (Tier 3 #11 in current numbering, #12 after the
principles update):** The Endpoint cap is a Handle that
closes on drop, going through the existing `decide_close_
handle` path which decrements the endpoint refcount; nothing
clock-specific is needed. If a future requirement is "disable
clock when the last cap holder drops," CPRMAN can implement
that via the same refcounting pattern PageSets use (Phase
12), entirely in userspace.

**Success line:** `[CAP] emmc2-driver granted clk_handle for (cprman, EMMC2)`

**Single commit. No kernel surface change; all userspace +
lockjaw-types/lockjaw-userlib additions.**

**Codex-review checkpoint after M0c.** This is the load-
bearing capability shape decision; worth getting reviewed
explicitly before M1+ build on top of it.

### M1 — Map emmc2 MMIO, dump capabilities

**Files:**
- `user/emmc2-driver/` — new userspace driver, bootstrap →
  claim `brcm,bcm2711-emmc2` device → request clock cap via
  M0c → map MMIO PageSet → soft-reset (`SW_RST.ALL`) → read
  CAPABILITIES (offset 0x40) and CAPABILITIES_HI (0x44),
  decode + log.
- `lockjaw-types/src/sdhci.rs` — new module with SDHCI
  register offsets, capability decode functions (pure),
  command opcodes (CMD0/CMD8/CMD17 etc. enums for type
  safety later in M2+).

**No SD protocol yet. Just prove the device-cap + clock-cap
grant path works end-to-end on Pi.**

**Success line:** `[EMMC2:INIT] caps=0x... base_clk=200MHz adma2=1 v3=1 clk_handle=ok`

**Pi-only verification from here on. Smoke-test on Pi 4B SD
card boot.**

### M2 — Bus to 400 kHz via CPRMAN, CMD0 + CMD8

- Drive the clock cap: `set_rate(EMMC2, 200_000_000)`,
  `enable(EMMC2)`. The CPRMAN leaf rate is the SDHCI base
  clock; the SDHCI controller-internal divider drops to ID-
  mode (~400 kHz).
- Configure SDHCI `CLOCK_CTL` (internal clock enable, divider,
  SD clock enable, wait stable).
- Power the bus (`POWER_CTL` → 3.3 V). Pi 4B is fixed 3.3 V;
  no UHS 1.8 V switch.
- Issue CMD0 (no response), CMD8 (`0x1AA` arg, expect R7
  echo).

**Success line:** `[EMMC2:IDPHASE] CMD8 echo=0x1AA — card is SDv2+ (clk via cprman)`

### M3 — Full ID-mode enumeration

- ACMD41 loop (CMD55 + CMD41 with HCS=1) until ready.
- CMD2 (CID), CMD3 (RCA), CMD9 (CSD — decode capacity),
  CMD7 (SELECT_CARD).
- ACMD6 → 4-bit bus width; mirror on host.
- Raise card clock to 25 MHz via SDHCI internal divider.
  CPRMAN leaf rate unchanged.

**Success line:** `[EMMC2:READY] rca=0x... capacity=... GB bus=4bit card_clk=25MHz`

### M4 — Single-block PIO read of LBA 0

- `BLOCK_SIZE = 512`, `BLOCK_COUNT = 1`.
- CMD17 (READ_SINGLE_BLOCK), arg = LBA.
- Poll `NORMAL_INT_STAT.BUFFER_READ_READY`.
- Read 128 × u32 from `BUFFER_DATA`.
- Verify MBR signature `0x55AA` at offset 510.

**Success line:** `[EMMC2:READ] LBA0 MBR=0x55AA — partition table follows`

**This is the "real data off the card" milestone.**

### M5 — Multi-block PIO read + single-block write

- CMD18 (READ_MULTIPLE_BLOCK) with **Auto-CMD23**
  (BlockCount written before, no CMD12 needed).
- CMD24 (WRITE_BLOCK) of a known pattern to last LBA on card.
- Read back with CMD17, verify.

**Success line:** `[EMMC2:WRITE] verified roundtrip at LBA=...`

### M6 — ADMA2 data path + typed memory-attribute enum

**The only milestone touching kernel memory attributes.**

**Decision: replace the `flags: u32` bit field with a typed
`MapMemoryAttribute` enum, name the new variant
`NormalNonCacheable` after the MAIR regime** (per the
resolved open question #3). `Device` stays reserved for MMIO;
the DMA-coherent variant is normal RAM with non-cacheable
attributes — that's the MAIR semantic and the type should
reflect it.

**Files:**
- `lockjaw-types/src/vmem.rs` — add
  `pub enum MapMemoryAttribute { Normal, Device,
  NormalNonCacheable }` with `#[repr(u8)]`. Replace the
  current `MAP_FLAG_DEVICE` bit-flag constant + `select_attrs`
  helper with a `MapMemoryAttribute::to_pte_attrs()` method
  returning `(MAIR_idx, sh, ap)`.
- `lockjaw-types/src/page_table.rs` — audit the MAIR table;
  add a Normal-Non-Cacheable entry if not present. (MAIR has
  8 slots, current setup uses 2 — Device + Normal — so room
  exists.) Update `MAIR_EL1_VALUE` constant accordingly.
- `src/arch/aarch64/mmu.rs` — confirm the new MAIR slot is
  written into MAIR_EL1 at MMU init. Likely a one-line
  constant change.
- `src/syscall/handler.rs` (`sys_map_pages`) — accept the
  enum discriminant byte instead of a u64 flags. Decode at
  the syscall boundary; pass the typed value down to vmem.rs.
- `src/process.rs`, `user/posix-server/` (mmap path),
  `user/fat32-server/` (buffer mappings), every other
  `sys_map_pages` caller — update call sites to pass the enum
  variant. Mechanical; compiler points to each.
- `user/emmc2-driver/` — allocate ADMA2 descriptor table +
  scatter-gather buffer with
  `MapMemoryAttribute::NormalNonCacheable`, build
  descriptors, program `ADMA_SA_ADDR`, set
  `HOST_CTL1.DMA_SEL = ADMA2_32`, redo M4's CMD17 with DMA
  enabled.

**Sub-commit shape:** the enum-conversion + caller updates
ship as a separate preceding commit (no behavior change —
Normal/Device flags translate 1:1 to enum variants), THEN the
NormalNonCacheable variant + ADMA2 work as a second commit.
Keeps the type-system migration testable separately from the
DMA path itself.

**Why this milestone is needed (and QEMU virtio-blk wasn't):**
the BCM2711 SDHCI controller is real silicon with a real DMA
engine that reads/writes physical memory **without
participating in CPU cache coherency**. Without
non-cacheable mappings (or explicit cache maintenance per
transfer), the device sees stale memory and the CPU reads
stale data after device writes. QEMU virtio-blk has no real
DMA hardware — QEMU "performs" DMA via host pointers that go
through the host MMU + cache, which is coherent with the
guest's cached writes. So Lockjaw on QEMU never had to
confront non-coherent DMA. Pi forces the issue. We pick
non-cacheable mapping over per-transfer cache maintenance
because it's simpler, harder to get wrong, and matches the
Linux/Circle convention for SDHCI on Pi.

**Success line:** `[EMMC2:ADMA] LBA0 read via ADMA2-32, descriptors=1, t=...us`

**Includes a wall-clock comparison vs PIO for proof the DMA
path is actually being used.**

### M7 — Block-device userspace server

**Files:**
- `user/emmc2-driver/src/main.rs` — implement `BlockEngine`
  trait against the SDHCI hardware. The trait + IPC shape are
  in `user/lockjaw-userlib/src/block.rs:137-282`.
- Init process — spawn emmc2-driver (currently spawns
  virtio-blk-driver instead).
- Test client — tiny userspace process that calls
  `read_blocks(0, 1, …)` and prints MBR (or just rewire
  fat32-test).

**Success line:** `[BLOCKDEV] /dev/sd0 ready: 512B × ... blocks; client read OK`

**Side benefit: fat32-server, fat32-test, and posix-server
work on Pi without modification because they consume
`BlockClient` (the abstract IPC client) — the implementation
behind it just changed from virtio-blk to emmc2.**

## Kernel surface area touched

Almost all userspace. Two deliberate kernel changes, each
forced by hardware (not by typing preferences):

- **M0a**: FDT parser additions (lockjaw-types only).
  Lockjaw-types is host-testable; no runtime kernel impact
  until M0c calls into the new resolver.
- **M6**: typed `MapMemoryAttribute` enum replacing the
  `flags: u64` parameter to `sys_map_pages`, plus a
  `NormalNonCacheable` MAIR slot. Required because the MMU
  is kernel-managed — userspace cannot install a non-
  cacheable PTE without telling the kernel. The minimum
  surface: one new MAIR slot + one new enum variant + a
  mechanical pass through all `sys_map_pages` call sites
  (process.rs, posix-server mmap, fat32-server buffers).

**No** new HandleKind variants. **No** new syscalls. **No**
new kernel-side dispatch paths for the clock cap (per the
microkernel discipline — see "Required principles update").
**No** changes to scheduler, IPC state machine, address-
space builder, KVM allocator, or process creation. **No** new
state machines in the kernel.

## Verification

**Per-milestone gate:**
- M0a: lockjaw-types host suite green; new fdt tests for
  `clocks` resolution + `#clock-cells` pass; `make test` 87/87
  on QEMU (no behavior change visible to QEMU).
- M0b: lockjaw-types host suite green; CPRMAN divider-math
  tests cover the BCM2711 PLLD rate range (e.g., target rates
  100 MHz, 200 MHz, 400 MHz, 50 MHz); `make test` 87/87 on
  QEMU.
- M0c: `make test` 87/87 on QEMU; new integration assertion in
  cprman-driver bootstrap that prints the success line.
- M1+: Pi 4B verification only. Each milestone's success line
  serves as the gate. `make pi4` produces `kernel8.img`; copy
  to SD; boot; check serial output matches.

**Cross-milestone:**
- `cargo xtask check-stack` / `check-pointers` /
  `check-vtables` / `check-init-size` /
  `check-linker-symbols` all green per commit.
- No regression in QEMU integration (M1+ won't run on QEMU
  anyway because there's no emmc2 model, but the userspace
  cprman-driver bootstraps cleanly with NotSupported responses).

## Risks

1. **Pi 4B SDHCI register layout drift.** BCM2711 ARM
   Peripherals manual is incomplete on CPRMAN; cross-reference
   Linux `drivers/clk/bcm/clk-bcm2835.c` for register offsets.
   Acceptable risk; the math is well-understood and Linux + Circle
   are working references.

2. **DTB phandle resolution shape choice.** Adding phandles to
   FDT parser requires either a two-pass walk (collect
   phandles, then resolve) or a per-node phandle index built
   at first walk. Pick the simpler one (two-pass) unless walk
   performance matters.

3. **CPRMAN driver shape if device-manager doesn't extend
   cleanly.** If the clock-provider registry needs a new
   syscall after all (unlikely, but possible if HandleKind
   variants are too constrained), bump scope. Plan A is to
   reuse Endpoint+caller_token; plan B is a new HandleKind
   variant.

4. **DMA coherency on Pi 4B.** Confirmed BCM2711 SDHCI is not
   I/O coherent with A72 caches. M6's Normal-Non-Cacheable
   mapping is the standard Linux/Circle approach; should work.
   If it doesn't: fall back to explicit cache maintenance via
   `dc cvac` / `dc ivac` per buffer (worse performance, more
   code).

5. **Kernel ELF size growth from new userspace drivers.** The
   `xtask check-init-size` check has a budget for the embedded
   init image. Adding cprman-driver + emmc2-driver to the boot
   image grows it. Audit the budget before M0b ships; bump if
   needed.

6. **No QEMU CI for emmc2.** From M1 onward, every milestone
   is Pi-hardware-tested. Iteration speed drops; expect each
   milestone to take longer than its QEMU equivalent. Mitigate
   by keeping host tests for any pure logic (divider math,
   capability decode, register layout invariants).

## What this enables next

- Real Lockjaw boot from SD on Pi 4B with FAT32 access (the
  existing fat32-server consumes `BlockClient` which now talks
  to emmc2).
- musl-built binaries reading files from SD on Pi (POSIX Phase
  1 path now hardware-backed).
- `posix-hello` malloc tests on Pi (POSIX Phase 2 path —
  needs the Pi DTB-pageset alignment bug fixed too, see
  journal-10's "two unrelated Pi gaps surfaced" note).
- Future MMC/SD on other ARM boards: the CPRMAN driver is
  BCM2711-specific, but the SDHCI driver (M1+) is mostly
  spec-compliant and would work with any SDHCI v3 controller
  given a clock cap.

## Out-of-scope follow-ups (not this plan)

- Pi 4B DTB pageset alignment fix (init/devmgr see bad magic
  because Pi DTB starts at non-page-aligned address — see
  journal-10).
- SDHCI tuning, UHS-I, eMMC-specific commands.
- Generalising clock provider registry into a typed
  capability registry (could subsume MMIO + IRQ + clock under
  one model). Worth doing once we have a second clock
  consumer.
- File-system write path (POSIX Phase 3+).
