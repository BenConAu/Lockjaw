# Typed-MMIO substrate + register-spec codegen — driver-shaping infrastructure

**Archive note.** This is the frozen snapshot of the active plan as
it stood at the end of Phase 8 (commits c4f5531 / b7f06d4 / 1b8e658
/ acb1c48). Forward-looking work for Phases 9-10 lives in
`~/.claude/plans/nifty-rolling-naur.md` — see the active plan's
Phases 1-8 summary section for the link back here.

## Status (2026-05-20)

| Phase | State | Headline |
|------|------|------|
| 1 | DONE 26c29c0 | `lockjaw-mmio`: cells, MappedRegs, DmaCell/Slice, barriers, DmaValue |
| 2 | DONE 26c29c0 | `xtask gen-regs` + `lockjaw-regs` + PL011 end-to-end |
| 3 | DONE 597db1f | `virtio-blk-driver` typed-MMIO + `claim_typed` + CMD_RELEASE_BY_ADDR |
| 4A | DONE a80e0a3 | Codegen semantic upgrade (`xtask` + `lockjaw-regs`) |
| 4B | DONE 76a16e3 | Userlib hardening (composable boot, init builder, DMA ownership, virtqueue ergonomics) |
| 5 | DONE 9f0a148 | `uart-driver` — first validation of 4A/4B on a new driver |
| 6 | DONE 74f0e1d | `ramfb-driver` + BE/stream codegen + sealed `DmaValue` + fwcfg family helpers |
| **7** | **NEXT — NEW** | **Wire-DTO codegen (`xtask gen-wires` + `lockjaw-types::wire`)** — DMA layout owned by framework, mirror of `lockjaw-regs` MMIO codegen |
| 8 | pending (was 7) | `cprman-driver` + passwd_protected codegen |
| 9 | pending (was 8) | `emmc2-driver` + combined_trigger/aliased/descriptors codegen (ADMA descriptors emit through Phase 7 wire-DTO codegen) |
| 10 | pending (was 9) | Final lockdown: `check-driver-unsafe`, docs |

## Context

After Phase 3 commit `597db1f`, paired architectural review
(`docs/typed-mmio-feedback-597db1f.md` Codex,
`docs/typed-mmio-feedback-597db1f-claude.md` Claude) converged
independently on the same set of framework shortcomings. Subsequent
review of *this plan's first draft* added a tighter set of corrections
about scope, abstraction-layer mixing, naming, and acceptance gates.
This revision incorporates both rounds.

The substrate is correct and the vertical (substrate → generated regs
→ userlib glue → unsafe-free driver) is well-shaped. What is missing
is the part that makes the driver short. Two reviewers, three
sentences:

- "the framework gives you safe primitives; it doesn't yet give you a
  *short* driver" (Claude)
- "the codegen is still too weak ... typed regmap emitter, not
  driver construction language" (Codex)
- "this is disciplined, promising, and more correct than most
  systems codebases. That is not the final aesthetic target" (Codex)

Doing the remaining four driver conversions (uart, ramfb, cprman,
emmc2) on top of Phase 3's shape would re-imprint the wrong shape
four more times: a 220-line `_start`, three `#[allow(unsafe_code)]`
boot stubs, free-form init sequences, hand-rolled bitflag operators,
manual low/high register composition, fuzzy DMA ownership. Cleanup
later costs N×.

This revision inserts **Phase 4A** (codegen semantic upgrade) and
**Phase 4B** (userlib hardening) before the driver conversions. They
are deliberately separated because they sit at different abstraction
layers and should ship as separate commits. Phase 4B can depend on
Phase 4A. Phases 5–9 are renumbered from the original 4–8 and
inherit the new shape.

## Strategic intent

- **Define each device once.** TOML spec is the source of truth for
  register layout and access semantics. Reviewers audit the spec;
  generated code follows by construction.
- **No escape hatches for register definitions — INTENDED state.**
  The eventual regime is: every layout/access-semantics pattern in
  the supported device corpus is spec-expressible. Today, some
  patterns are spec-expressible but emitter-incomplete (`combined_trigger`,
  `aliased`, `stream`, big-endian, `passwd_protected`, descriptors);
  they land in their respective driver phases. The plan is honest
  about the gap rather than claiming the regime already exists.
- **Make illegal states unrepresentable.** Construction-safe APIs,
  distinct step variants, narrow return types. Where the type system
  can prevent a bug class, prefer that over runtime assertions.
- **Drivers shrink to protocol logic.** A converted driver contains
  declarative boot (macro or composed pieces) + protocol state
  machine + command sequences. No bit twiddling, no offset
  arithmetic, no masking, no raw syscalls for claim/IRQ/bootstrap,
  no raw MMIO/DMA/barrier operations, no `unsafe` blocks. The
  crate root is `#![deny(unsafe_code)]` (not `forbid` — see
  "Why deny, not forbid" below) and ZERO `#[allow(unsafe_code)]`
  attributes appear in driver source files. The macro-emitted
  per-item allows that boot stubs require expand inside
  `lockjaw-userlib`; the seal between driver source and macro
  body is what the acceptance grep enforces.
- **Why deny, not forbid.** Phase 4B/5/6 settled on
  `#![deny(unsafe_code)]` because `boot_stub!` and `driver_main!`
  emit `#[allow(unsafe_code)]` on the `#[no_mangle] _start` and
  `#[link_section] LOCKJAW_HASH_SECTION` items (the `unsafe_code`
  lint flags those ABI/linker attributes as unsafe). `forbid`
  rejects ANY per-item `#[allow]` including macro-emitted ones,
  so `forbid` + the boot macros are mutually exclusive. Lockjaw
  picked the boot macros (the asterisk-free version of the "no
  unsafe in drivers" claim lives at the GREP level — driver
  source files contain zero allow attributes; allows live only
  inside the macro expansion in `lockjaw-userlib`). If a future
  redesign eliminates per-item allows in the boot macros (one
  approach: move `_start` into `lockjaw-userlib` and have it
  call a driver-side `extern "Rust" fn driver_main()` via a
  Sealed trait that drivers implement — but extern-rust-fn linkage
  still requires `#[no_mangle]` so this is harder than it looks),
  the gate can re-tighten to `forbid`. Phase 10's check enforces
  the settled `deny` policy.
- **Layer separation.** Codegen (lockjaw-regs) emits register
  language. Userlib (lockjaw-userlib) provides driver runtime +
  device-family policy. Drivers consume both but the layers are
  architecturally distinct and ship as distinct commits.
- **Framework before drivers.** When paired reviews converge on a
  shortcoming, fix it BEFORE the next driver locks in the wrong
  shape. Phases 5–9 inherit Phase 4A/4B's shape.

---

## Phase 4A — Codegen semantic upgrade

**Layer:** `xtask` + `lockjaw-regs` (codegen substrate only).
**Goal:** push the generator from "typed regmap emitter" toward
"semantic register language." Does NOT touch userlib or driver
crates beyond regenerating `lockjaw-regs/src/*.rs`.

### Status table — spec features

The "no escape hatches" claim is aspirational. The honest current
state, with each phase that closes the gap:

| Feature | Parser | Emitter | Production use |
|---|---|---|---|
| ro/rw/wo/w1c at u8/u16/u32/u64 | ✅ Ph2 | ✅ Ph2 | Ph3 (virtio-blk) |
| named bit-range fields | ✅ Ph2 | ✅ Ph2 | Ph3 |
| enum-valued fields | ✅ Ph2 | ✅ Ph2 | Ph3 |
| trigger | ✅ Ph2 | ✅ Ph2 | Ph3 (queue_notify) |
| `[[flags]]` bitflag types | ⬅ Ph4A | ⬅ Ph4A | Ph4B (virtio Status), Ph5 (PL011 IM), Ph7 (cprman), Ph8 (SDHCI) |
| `[[u64_pairs]]` paired 64-bit | ⬅ Ph4A | ⬅ Ph4A | Ph4B (virtio blk_capacity, queue_*), Ph8 (SDHCI adma_address, system_address) |
| `[[windowed]]` selector+value | ⬅ Ph4A | ⬅ Ph4A | Ph4B (virtio device_features, driver_features) |
| `verify_against` (OFFSETS only) | ✅ Ph2 (parsed unused) | ⬅ Ph4A | Ph4B + ongoing |
| stream | partial (rejected at gen_regs:377) | Ph6 | Ph6 (fw_cfg) |
| big-endian | ✅ Ph6 | ✅ Ph6 | Ph6 (fw_cfg selector/dma_addr) |
| stream | ✅ Ph2 | ✅ Ph2 | Ph2 (PL011 data) / Ph6 (fwcfg data) |
| passwd_protected | ❌ | Ph8 | Ph8 (cprman) |
| aliased | partial (rejected gen_regs:400) | Ph9 | Ph9 (SDHCI sysaddr/arg2) |
| combined_trigger | partial (rejected gen_regs:400) | Ph9 | Ph9 (SDHCI xfer_mode+cmd) |
| `[[descriptors]]` (regspec) | ❌ — superseded | n/a | Replaced by `[[wire]]` in Ph7 wirespec |

Where columns are parser/emitter/production, "✅ Ph2" means the
feature lands in that phase.

### 4A.1 — `[[flags]]` bitflag emit

**TOML schema:**
```toml
[[flags]]
name = "Status"
width = 32
bits = [
  { name = "ACKNOWLEDGE", bit = 0, description = "Guest noticed the device" },
  { name = "DRIVER",      bit = 1 },
  { name = "DRIVER_OK",   bit = 2 },
  { name = "FEATURES_OK", bit = 3 },
  { name = "VERSION_1",   bit = 5 },
  { name = "FAILED",      bit = 7 },
]
```

**Generated:** newtype + `pub const ACKNOWLEDGE: Status = Status(1);`
per bit + `BitOr/BitAnd/Not/empty/bits/contains/insert/remove`
implementations. Eliminates the 12-lines-per-flag-type hand-rolled
boilerplate at `user/lockjaw-regs/src/virtio_mmio.rs:105-116`.

**Generated tests:**
- Roundtrip: `(a | b).bits() == a.bits() | b.bits()` for arbitrary
  disjoint a, b.
- Empty: `Status::empty().bits() == 0` and `Status::empty().contains(Status::empty())`.
- Unknown-bit preservation: `Status(0xFF).bits() == 0xFF`.

### 4A.2 — `[[u64_pairs]]` paired 64-bit register

**TOML schema:**
```toml
[[u64_pairs]]
name = "blk_capacity"
low = "blk_capacity_low"
high = "blk_capacity_high"
endian = "little"     # which half is least-significant
```

**Generated:**
```rust
pub fn read_blk_capacity(&self) -> u64 {
    self.read_blk_capacity_low() as u64
        | ((self.read_blk_capacity_high() as u64) << 32)
}
```

Low/high accessors remain as escape hatches (some specs may need
them independently). Eliminates driver-side composition at
`user/virtio-blk-driver/src/main.rs:374-375`.

**Generated test:** **cross-validation roundtrip** —
`read_blk_capacity()` equals manual composition of two
read_*_low/_high calls. This is the trust mechanism for the
synthesized semantic: not a constant cross-check (no canonical
64-bit constant to verify against), but a property that the
synthesized operation matches the manual composition for the
same underlying registers.

### 4A.3 — `[[windowed]]` selector+value access

**TOML schema:**
```toml
[[windowed]]
name = "device_features"
selector = "device_features_sel"
value = "device_features"
chunk_width = 32
chunk_count = 2          # synthesizes 64-bit accessor
direction = "read"       # or "write"
```

**Generated:**
```rust
pub fn read_device_features_64(&self) -> u64 {
    self.write_device_features_sel(0);
    let lo = self.read_device_features() as u64;
    self.write_device_features_sel(1);
    let hi = self.read_device_features() as u64;
    lo | (hi << 32)
}
```

For `direction = "write"`:
```rust
pub fn write_driver_features_64(&self, v: u64) {
    self.write_driver_features_sel(0);
    self.write_driver_features(v as u32);
    self.write_driver_features_sel(1);
    self.write_driver_features((v >> 32) as u32);
}
```

Replaces the manual selector/value sequences at
`user/virtio-blk-driver/src/main.rs:311-314, 324-327`.

Emitter inserts barriers between selector access and value access
when the spec sets `requires_barrier = true` on the windowed entry
(default `false`; virtio doesn't need it; some chips will).

**Generated test:** **MMIO sequence assertion** via a mock-cell
test harness — assert that `read_device_features_64()` produces
the expected sequence of underlying volatile operations
(sel-write 0, value-read, sel-write 1, value-read) when wired to
a recording mock. This is the trust mechanism for windowed
semantics: an offset cross-check cannot validate sequencing, but
a host-side mock can.

### 4A.4 — `verify_against` cross-check emission (OFFSETS ONLY)

**Scope:** `verify_against` validates **register offset
consistency** between `lockjaw-regs` (generated) and `lockjaw-types`
(hand-written constants). It does **not** validate composed
semantics (u64-pair composition, windowed sequencing) — those have
their own validation mechanisms above. The plan is honest about
this limitation.

**Coverage visibility requirement (so coverage does not silently
stagnate):**
- Each generated module emits a coverage-summary header comment:
  ```
  // verify_against: lockjaw_types::virtio
  // Coverage: 8/12 registers cross-checked against constants.
  // Unmatched (no constant in lockjaw_types::virtio):
  //   - queue_desc_low, queue_desc_high
  //   - queue_driver_low, queue_driver_high
  ```
- `cargo xtask gen-regs --check` prints the same summary per spec
  to stderr; if any spec's coverage decreases vs the committed
  generated file, the check fails (catches "someone removed a
  constant and the assert silently disappeared").
- Phase 9's `check-driver-unsafe` companion check audits aggregate
  coverage and surfaces unmatched registers as a release-readiness
  signal (not a hard gate, but visible).

**Current state:** `xtask/src/gen_regs.rs:107-114` parses
`verify_against` but uses `#[allow(dead_code)]` — value is
discarded.

**Change:** emitter consumes `verify_against = "lockjaw_types::virtio"`
and for each register with a matching `_OFFSET` constant in that
module emits:
```rust
#[cfg(test)]
mod _verify {
    use static_assertions::const_assert_eq;
    const_assert_eq!(
        core::mem::offset_of!(super::VirtioMmio, magic_value),
        lockjaw_types::virtio::MAGIC_VALUE_OFFSET
    );
}
```

Drops the `#[allow(dead_code)]`. Moving a constant in lockjaw-types
breaks the generated module's build.

**Coverage gap policy:** If a constant doesn't exist for a register,
the emitter skips that register silently. Phase 9 audits coverage
and surfaces unmatched registers. The plan acknowledges this is
pragmatic, not maximal.

**Future work (out of Phase 4A scope):** for synthesized operations
(u64 reads, windowed reads), validation is via host-side
property/sequence tests (4A.2, 4A.3). A stronger form — generating
lockjaw-types-side u64 constants and cross-checking — is feasible
but requires lockjaw-types refactor; defer.

### 4A.5 — MockCell substrate for codegen sequence tests

The 4A.2 and 4A.3 generated tests require a host-side mock that
records the order of volatile reads/writes against a region.
Promoting this from "implied" to "explicit deliverable":

**New `user/lockjaw-mmio/src/mock.rs`** (gated `#[cfg(test)]` or
`#[cfg(feature = "mock")]`):
```rust
pub struct MockMmioRegion {
    bytes: RefCell<Vec<u8>>,
    log: RefCell<Vec<MmioOp>>,
}
pub enum MmioOp {
    Read { offset: usize, width: usize, value: u64 },
    Write { offset: usize, width: usize, value: u64 },
}
impl MockMmioRegion {
    pub fn new(size: usize) -> Self;
    pub fn as_mapped_regs<T>(&self) -> MappedRegs<T>;  // wraps the region
    pub fn ops(&self) -> Vec<MmioOp>;
    pub fn poke(&self, offset: usize, width: usize, value: u64);
    pub fn peek(&self, offset: usize, width: usize) -> u64;
}
```

Reusable substrate: codegen tests use it (4A.2, 4A.3); the typed
init builder tests (4B) use it; emmc2's combined_trigger test
(Phase 8) will use it. Worth the small upfront cost.

**Trade-off note:** `MappedRegs<T>::new(va)` currently takes a raw
VA. The mock either (a) allocates a `Box<[u8]>` and casts `as *mut
T` (no architectural changes), or (b) `lockjaw-mmio` gains a thin
abstraction over the backing store. Prefer (a) for now — it's a
test-only construct and shouldn't add real-substrate complexity.

### 4A.6 — Emmc2 reverse-scan prerequisite

Before 4B locks in DMA / virtqueue / boot shape, read
`user/emmc2-driver/src/main.rs` end-to-end and confirm the 4A/4B
shape can express what emmc2 needs. Specifically check:
- ADMA descriptor table: requires multi-page typed DMA. Today
  `OwnedDmaMapping` is single allocation; `alloc_contiguous(pages)`
  exists. Confirm descriptor table sits inside one
  `OwnedDmaMapping` (typed via `slice::<Adma2Descriptor>(0, n)`)
  rather than needing per-descriptor typed mapping (which would
  push TypedDmaPage forward from Phase 8 to Phase 4).
- W1C interrupt pump: confirm generated `clear_*_ack(mask)` shape
  from Phase 2 emitter covers SDHCI Normal/Error Interrupt Status
  semantics.
- Combined-trigger ordering: confirm TRANSFER_MODE+COMMAND ordering
  fix from `39f46a6` can be expressed as a single typed combined
  newtype write (Phase 8 spec format) without a Phase 4 escape
  hatch.

If any of the three pushes work into Phase 4, surface it BEFORE 4B
commits. Result: a short note appended to this plan ("emmc2 scan:
no Phase-4 gaps found" / "emmc2 scan: TypedDmaPage required, moving
to 4B") before 4B implementation begins.

**Scan result (2026-05-19): no Phase-4 gaps found.** emmc2's ADMA2
descriptor table fits `OwnedDmaMapping::slice::<Adma2Descriptor>(0, 1)`
(single contiguous descriptor, reused in place). DMA buffers are
already shape-compatible (caller alloc, PA tracked per slot, ≤127
sectors per transfer). W1C interrupt clear pattern maps directly to
the planned `clear_*_ack(mask)` accessor. MMIO width mix (u8/u16/u32)
is per-register-clean. Boot shape (clock-first, multi-step init) is
exactly the Tier-A escape-valve path. Two minor notes deferred to
Phase 8: (P1) current driver issues TRANSFER_MODE+COMMAND as two
u16 writes instead of one u32 — works on BCM2711, fixed by the
planned `kind = "combined_trigger"` codegen; (P2) inhibit-poll →
HOST_CONTROL → ADMA_ADDRESS ordering is currently ad-hoc — could
become a typestate builder (`SdhciCommandInit<S>`) analogous to
`VirtioTransportInit<S>`, but that's a Phase 8 nice-to-have, not a
Phase 4 blocker.

### 4A.7 — DEFER

Aliased (Ph8), combined_trigger (Ph8), stream (Ph6), big-endian
(Ph6), passwd_protected (Ph7), descriptors (Ph8). Each lands with
the driver that needs it. Phase 4A does NOT introduce them
speculatively.

### Files — Phase 4A

| Action | File |
|---|---|
| MOD | `xtask/src/gen_regs.rs` (schema additions, emitters, generated-test code) |
| MOD | `xtask/Cargo.toml` (if static_assertions needs to be added to lockjaw-regs deps) |
| MOD | `user/regspecs/virtio-mmio.toml` (add `[[flags]] Status`, `[[u64_pairs]] blk_capacity`, `[[windowed]] device_features`, `[[windowed]] driver_features`, `verify_against`) |
| MOD | `user/regspecs/pl011.toml` (add `[[flags]]` — dogfood the emitter on a 2nd device before Phase 5) |
| REGEN | `user/lockjaw-regs/src/virtio_mmio.rs` (bitflag boilerplate disappears) |
| REGEN | `user/lockjaw-regs/src/pl011.rs` |
| MOD | `user/lockjaw-regs/Cargo.toml` (add static_assertions if not present) |

### Acceptance — Phase 4A

These are **capability gates**, not line-count gates:

- `cargo xtask gen-regs --check` regenerates virtio-mmio.rs and
  pl011.rs cleanly (no drift).
- Generated virtio_mmio.rs contains NO hand-rolled bitflag
  operator impls (Status `BitOr/BitAnd/Not` are emitter output, not
  templates inside the spec).
- `cargo test -p lockjaw-regs --target aarch64-apple-darwin` passes
  with new generated tests: bitflag roundtrip + empty + unknown-bit;
  u64-pair cross-validation; windowed MMIO-sequence assertion;
  verify_against offset const-asserts.
- `verify_against` produces at least one const-assert per
  lockjaw-types-side constant that exists for the virtio module.
- The status table above remains the canonical reference for which
  features are parser-supported, emitter-supported, and used in
  production.
- `make test` — full QEMU integration green (Phase 3 driver still
  works against regenerated lockjaw-regs). Counts grow as later
  phases add granular assertions; see per-phase Acceptance for
  the count expected at that point.

---

## Phase 4B — Userlib hardening

**Layer:** `lockjaw-userlib` + driver crate (virtio-blk only).
**Goal:** address driver-facing shortcomings flagged by both
reviewers — boot ceremony, init free-form-ness, DMA ownership
muddiness, virtqueue ergonomics, ClaimedDevice surface. Ships as a
separate commit from Phase 4A.

**Layer separation note:** This phase is intentionally about driver
RUNTIME and DEVICE-FAMILY policy, not codegen. Reviewer feedback:
"Phase 4 mixes two abstraction levels that should be kept distinct."
The previous draft bundled them. They are now split: 4A is codegen,
4B is userlib.

### 4B.1 — Composable boot harness with explicit escape valve

**Problem the previous draft had:** the single `driver_main!` macro
was carrying too much policy: boot stubs + bootstrap IPC + probe-
by-hash + claim typed MMIO + IRQ create + IRQ bind + server endpoint
+ handoff. Reviewer feedback: "That is a lot. It risks becoming hard
to debug, too opinionated for drivers that differ slightly, a second
hidden framework, not just a ceremony remover."

Specifically, the all-in-one shape breaks down for drivers that need:
- claim-by-compatible-string instead of claim-by-hash
- no IRQ (purely polled)
- multiple MMIO regions
- clock acquisition before MMIO claim
- non-server shape (e.g. one-shot init then exit)

**Revised approach: layered composable pieces.**

**Tier A — primitive helpers in `lockjaw-userlib::driver_runtime`**
(each individually callable; no macro needed):

```rust
// lockjaw-userlib/src/driver_runtime.rs

/// Macro that installs the LOCKJAW_HASH_SECTION static and emits a
/// #[no_mangle] _start. The macro is the only place in the driver
/// boot path that bears #[allow(unsafe_code)] — wrapped at the
/// macro expansion site, not the call site.
#[macro_export]
macro_rules! boot_stub {
    (hash = $hash:expr, main = $main:ident $(,)?) => { ... };
}

/// Acquire the conventional driver bootstrap state. Runs once at
/// _start; returns the channels every driver needs.
pub fn driver_bootstrap() -> Result<DriverBootstrap, BootstrapError>;

pub struct DriverBootstrap {
    pub devmgr_ep: EndpointHandle,
    pub reply_obj: ReplyHandle,
    pub server_ep: Option<EndpointHandle>,  // None if the driver wasn't given one
}
// Wire-protocol note: the kernel's existing bootstrap reply already
// includes a server-endpoint slot per spawned driver. For drivers
// the spawner doesn't intend to make addressable (e.g. cprman before
// it exposes a clock-management endpoint), the slot arrives as a
// zero handle. `driver_bootstrap` checks for `EndpointHandle(0)` and
// returns `server_ep: None` accordingly — no kernel changes
// required. If the spawner protocol later gains a richer "I have no
// server" sentinel, `driver_bootstrap` translates it here.

/// Probe device-manager by source-hash; returns the matched
/// device's MMIO physical address. Lockjaw's device-manager API is
/// hash-oriented; this is the canonical probe path.
pub fn probe_by_hash(boot: &DriverBootstrap, hash: u64)
    -> Result<MmioProbe, ProbeError>;

/// Create + bind an IRQ notification for a known IRQ INTID.
pub fn bind_irq(intid: u32, flags: u32)
    -> Result<NotificationHandle, SyscallError>;
```

**Compat-string probe path — DEFERRED.** String-based compat
lookup in no_std userland would force a string-IPC boundary that
Lockjaw's hash-oriented device-manager API does not have today.
The right shape is to hash the compat string at compile time (const-
eval or build-script-derived constant), then call `probe_by_hash` —
i.e., the userlib never IPCs a string. That requires a small
extension to the device-manager probe-protocol vocabulary and is
out of scope for Phase 4B; defer to a follow-up commit when an
actual driver needs it.

**Tier B — composed common shape** (the 90% case):

```rust
/// Public driver-facing surface. ClaimedDevice is internal —
/// drivers consume regs/irq_intid via DriverCtx, not via a wrapped
/// ClaimedDevice. This makes the driver-side surface minimal and
/// stable: regs + irq_intid + irq_notif + the boot channels for
/// drivers that need to do further claim/probe themselves.
pub struct DriverCtx<T: 'static> {
    pub regs: MappedRegs<T>,
    pub irq_intid: u32,
    pub irq_notif: NotificationHandle,
    pub server_ep: EndpointHandle,
    pub devmgr_ep: EndpointHandle,
    pub reply_obj: ReplyHandle,
}

/// Standard "boot → probe → claim → bind IRQ → return ctx" for
/// drivers with the conventional single-MMIO + single-IRQ + server
/// shape. Built from Tier A; no magic.
pub fn standard_driver_init<T: 'static>(
    hash: u64,
) -> Result<DriverCtx<T>, DriverInitError>;
```

`ClaimedDevice<T>` becomes `pub(crate)` in `lockjaw-userlib::devmgr`
— `claim_typed` still returns it for internal composition into
`DriverCtx`, but external users (driver crates) only see
`DriverCtx`. The `release()` shutdown method moves onto a private
`InternalClaimedDevice::release(self, ...)` helper consumed by
`DriverCtx::release(self, ...)`. Resolves the "splits the
difference" critique by picking one public surface.

**Tier C — syntactic sugar for the most common case** (driver body
becomes one fn + one macro invocation):

```rust
#[macro_export]
macro_rules! driver_main {
    (
        name = $name:literal,
        hash = $hash:expr,
        layout = $layout:ty,
        main = $main:ident $(,)?
    ) => {
        $crate::boot_stub! {
            hash = $hash,
            main = __lockjaw_driver_entry,
        }
        fn __lockjaw_driver_entry() -> ! {
            let ctx: $crate::DriverCtx<$layout> =
                match $crate::standard_driver_init($hash) {
                    Ok(c) => c,
                    Err(_) => {
                        // $name is the puts prefix on bootstrap failure:
                        $crate::boot_puts_and_halt(concat!($name, ": init failed\n"))
                    }
                };
            $main(ctx)
        }
    };
}
```

(`$name` is consumed as the prefix for the bootstrap-failure log line
so multiple drivers' failure messages are distinguishable in mixed
output. The macro must use `$name`; if it's omitted the macro doesn't
match — keeps the field load-bearing.)

**Macro hygiene discipline:**
- Macro-generated identifiers use the `__lockjaw_` prefix (e.g.
  `__lockjaw_driver_entry`, `__lockjaw_hash_section_static`) so they
  can't collide with driver-side names.
- All references to userlib items use `$crate::` so the macro works
  when invoked from a crate that doesn't `use` everything explicitly.
- `boot_stub!` and `driver_main!` must each be invocable at most
  once per crate — generated identifiers are not parameterized by
  invocation site. Document this in the macro doc comment;
  duplicate invocations should produce a clear compile error
  (likely "duplicate definition of `_start`" which is already
  sensible).
- Test the hygiene boundary by deliberately defining a local
  `__driver_entry` in the driver and confirming compilation
  succeeds (passes if the prefix worked) or fails with a clear
  message (also acceptable; just must not silently shadow).

**Escape valves — documented and compiled (not just markdown):**

A driver that doesn't fit the standard shape (multi-MMIO,
clock-first, no IRQ, non-server) does NOT use `driver_main!`.
Instead it composes from Tier-A directly:

```rust
// In cprman-driver/src/main.rs (Phase 7 — clock-first, no MMIO IRQ):
boot_stub! { hash = CPRMAN_HASH, main = cprman_entry }

fn cprman_entry() -> ! {
    let boot = driver_bootstrap().unwrap();
    let probe = probe_by_hash(&boot, CPRMAN_HASH).unwrap();
    let claim = claim_typed::<CprmanRegs>(boot.devmgr_ep, boot.reply_obj, probe.mmio_addr).unwrap();
    // No IRQ for cprman; just run the server.
    run_clock_server(&claim.regs, boot.server_ep.unwrap())
}
```

The escape-valve driver still consumes `boot_stub!` (so the single
`#[allow(unsafe_code)]` site stays in the macro body inside
`lockjaw-userlib`). The driver crate itself is `#![deny(unsafe_code)]`
with ZERO `#[allow(unsafe_code)]` attributes in driver source.
See "Why deny, not forbid" in Strategic Intent.

**Documentation requirement:** to keep this from drifting before
Phase 7 actually converts cprman, the escape-valve example lives as
a compiled example at `user/lockjaw-userlib/examples/escape_valve_cprman.rs`
(or as a doctest in `driver_runtime.rs`), NOT just as a markdown
snippet. `cargo check --examples` in CI keeps it honest. The
docs chapter cross-links to the compiled example.

**Files:**
- NEW `user/lockjaw-userlib/src/driver_runtime.rs`
- MOD `user/lockjaw-userlib/src/lib.rs` (module decl, re-exports)
- MOD `user/virtio-blk-driver/src/main.rs` (use `driver_main!`,
  delete existing 220-line `_start` at 237-456 + the three
  `#[allow(unsafe_code)]` attributes at 11, 14, 236)

### 4B.2 — VirtIO MMIO typed init builder

**Layer note:** This is **device-family policy**, distinct from
codegen. It belongs in `lockjaw-userlib::virtio`, not in
`lockjaw-regs::virtio_mmio` (which is mechanical register access).
Keeping them separate is one of the explicit reviewer corrections.

**Naming discipline (resolve VirtioMmio×3 collision):**
- `lockjaw_regs::virtio_mmio::VirtioMmio` — generated register
  layout (unchanged).
- `lockjaw_userlib::virtio::VirtioTransportInit<S>` — typed init
  builder (NOT `VirtioMmioInit<S>` — too easy to confuse with the
  generated layout under `use`).
- `lockjaw_userlib::virtio::VirtioTransport` — post-init runtime
  wrapper, single concrete type (constructed only by `.driver_ok()`).

**New `user/lockjaw-userlib/src/virtio.rs`:**

```rust
pub struct VirtioTransportInit<S> {
    regs: MappedRegs<VirtioMmio>,
    _state: PhantomData<S>,
}

// State markers (zero-sized; only the type matters). Each step is a
// distinct type so the spec's "write features, then check FEATURES_OK
// acceptance" really IS two transitions in the type system:
pub struct Reset;
pub struct Acknowledged;
pub struct DriverSelected;
pub struct FeaturesWritten;     // driver_features set; FEATURES_OK not yet asserted
pub struct FeaturesAccepted;    // FEATURES_OK acked by device
pub struct QueuesReady;

#[derive(Debug)]
pub enum VirtioInitError {
    NotVirtio { magic: u32 },         // magic value check failed
    UnsupportedVersion(u32),
    DeviceRejectedFeatures,           // FEATURES_OK readback cleared the bit
    QueueTooLarge { requested: u16, max: u16 },
}

// Linear API; each method returns the next state type:
impl VirtioTransportInit<Reset> {
    /// Wraps the typed MMIO region. Performs MAGIC_VALUE + VERSION
    /// check up front so a non-virtio mapping can never reach
    /// .acknowledge(). Resolves "who owns the magic check": here.
    pub fn reset(regs: MappedRegs<VirtioMmio>)
        -> Result<Self, VirtioInitError> { ... }
    pub fn acknowledge(self) -> VirtioTransportInit<Acknowledged> { ... }
}
impl VirtioTransportInit<Acknowledged> {
    pub fn driver(self) -> VirtioTransportInit<DriverSelected> { ... }
}
impl VirtioTransportInit<DriverSelected> {
    pub fn negotiate(self, wanted: u64)
        -> Result<VirtioTransportInit<FeaturesWritten>, VirtioInitError> { ... }
}
impl VirtioTransportInit<FeaturesWritten> {
    /// Sets FEATURES_OK and re-reads Status to confirm device accepted.
    /// Distinct return type — "written" and "accepted" are different
    /// facts the spec cares about.
    pub fn features_ok(self)
        -> Result<VirtioTransportInit<FeaturesAccepted>, VirtioInitError> { ... }
}
impl VirtioTransportInit<FeaturesAccepted> {
    /// Resolves the "init builder + virtqueue allocation" composition
    /// hole: queue_num_max can only be read AFTER features_ok, but
    /// Virtqueue::new needs that value. The builder selects the
    /// queue, reads queue_num_max, calls the factory closure (which
    /// owns the OwnedDmaMapping allocation and Virtqueue construction),
    /// then writes desc/avail/used PAs from the returned vq's layout.
    pub fn setup_queue<F>(
        self,
        index: u16,
        make_vq: F,
    ) -> Result<(VirtioTransportInit<QueuesReady>, Virtqueue), VirtioInitError>
    where
        F: FnOnce(u16 /* queue_num_max */) -> Result<Virtqueue, VirtioInitError>,
    { ... }
}
impl VirtioTransportInit<QueuesReady> {
    pub fn driver_ok(self) -> VirtioTransport { ... }
}

// Post-init runtime. Single concrete type; constructor is private.
pub struct VirtioTransport {
    regs: MappedRegs<VirtioMmio>,
}
impl VirtioTransport {
    pub fn queue_notify(&self, q: u16);
    pub fn read_interrupt_status(&self) -> u32;
    pub fn clear_interrupt_ack(&self, mask: u32);
}
```

`do_io` and similar driver methods take `&VirtioTransport` so
issuing I/O against an un-init device is a type error.

**Magic-value ownership:** `VirtioTransportInit::reset()` performs
MAGIC_VALUE + VERSION check. Drivers no longer write the check; a
non-virtio mapping can't reach `.acknowledge()`.

**In-scope for the builder:** linear init transitions, single- or
multi-queue (via repeated `setup_queue`), 64-bit feature negotiation,
virtio MMIO transport version 2 (modern).

**OUT of scope** — not modeled; will be added by per-family modules
(`virtio_net`, `virtio_gpu`) when those land:
- MSI-X / per-vector configuration (not present on MMIO transport)
- Indirect descriptors (a virtqueue capability, not a transport one)
- Config-change callbacks (virtio-net/gpu need these)
- VirtIO PCI transport (only MMIO modeled)
- Reset re-entry after Running (treat as one-shot init for now)

**Files:**
- NEW `user/lockjaw-userlib/src/virtio.rs`
- MOD `user/lockjaw-userlib/src/lib.rs` (module decl)
- MOD `user/virtio-blk-driver/src/main.rs` — replace the free-form
  init at lines 304-371 with builder invocation; engine now stores
  `VirtioTransport` instead of raw `MappedRegs<VirtioMmio>`.

### 4B.3 — DMA mapping ownership types

**Reviewer correction:** "OwnedDmaPage and MappedDmaPage is better
than current DmaPage, but the names still center 'page' rather than
'mapping ownership.' You may want the ownership story to be the
primary noun, not an adjective on DmaPage."

**Revised names:** the noun becomes the mapping; the ownership
distinction becomes the type:

- `OwnedDmaMapping` — produced by `alloc()` /
  `alloc_contiguous(pages)`. Owns the underlying pageset handle.
  `Drop` calls `sys_unmap_pages + VMEM.free + sys_close_handle(pageset)`.
  Errors swallowed (Drop can't return).
- `BorrowedDmaMapping` — produced by `map_existing(pageset, pages)`.
  Adopts the mapping but caller retains pageset ownership. `Drop`
  calls `sys_unmap_pages + VMEM.free` only — does NOT close pageset.

Both retain `.unmap(self) -> Result<(), SyscallError>` for callers
that want the error back.

Both implement the same view-construction API: `.cell::<T>(off)`,
`.slice::<T>(off, n)`, `.pa()`, `.va()`, `.pa_offset(o)`,
`.size_bytes()`, `.zero()`. Common methods either via a
`DmaMappingView` trait or via macro-expansion-of-identical-bodies —
pick whichever produces fewer types to reason about. Prefer the
trait approach so the two structs share one impl.

**Trait factoring discipline:** if a `DmaMappingView` trait is
introduced, it contains ONLY the view-construction methods listed
above. NO methods related to allocation, mapping lifecycle, sub-
mapping creation, or ownership transfer. NO sub-traits, NO blanket
impls beyond the two concrete structs. The trait exists to share
identical-body methods, not to seed a framework hierarchy. If a
future need pushes against this — STOP and add the method to both
structs directly rather than extending the trait surface.

**Why this resolves the muddiness:** the type name tells the reader
who is responsible for the pageset. `OwnedDmaMapping` Drop closes;
`BorrowedDmaMapping` Drop doesn't. No `owned: bool` field; no docs
required to explain ownership — the type IS the ownership story.

**Callsite updates:**
- `user/lockjaw-userlib/src/virtqueue.rs:34-45` —
  `Virtqueue.backing: OwnedDmaMapping` (virtqueue allocates its own
  backing).
- `user/virtio-blk-driver/src/main.rs` — `req_page: OwnedDmaMapping`;
  selftest `test_page: BorrowedDmaMapping`.

**Host test:** deliberately drop an `OwnedDmaMapping` without
`unmap()`, re-allocate same VA via VMEM, verify mapping is
re-establishable. Proves Drop ran VMEM.free + close. Mirror test
for `BorrowedDmaMapping`: drop, re-allocate VA, but original pageset
handle remains accessible to the test.

**Files:**
- MOD `user/lockjaw-userlib/src/dma.rs` (rename + split types)
- MOD `user/lockjaw-userlib/src/virtqueue.rs` (Virtqueue.backing
  type)
- MOD `user/virtio-blk-driver/src/main.rs` (field types)

### 4B.4 — Magic-offset removal (shallow fix only)

Delete `const STATUS_BYTE_OFFSET: u64 = 16;` at
`user/virtio-blk-driver/src/main.rs:45-47`. Replace each callsite
with `size_of::<VirtioBlkReqHeader>() as u64`. Defends CLAUDE.md
"Types over constants" — the literal example for this pattern.

**This is the SHALLOW fix.** The deeper version (typed
`ReqStatusPage` DMA struct with `header_cell()` / `status_cell()`
accessors so the offset never appears in driver code at all) is
TypedDmaPage<L> codegen — explicitly deferred per the Phase 4A.6
emmc2 reverse-scan outcome. Reassess at Phase 8: if 2+ drivers
(virtio-blk request/status page + emmc2 ADMA descriptor table) show
the same statically-partitioned-typed-DMA shape, land TypedDmaPage
then. Until then, `size_of::<T>()` is the principled minimum.

### 4B.5 — Reduce ClaimedDevice surface

Resolved at the `DriverCtx` level in 4B.1: `ClaimedDevice<T>`
becomes `pub(crate)` in `lockjaw-userlib::devmgr` entirely.
Drivers only ever see `DriverCtx<T>` (public, minimal field set).
Internal `claim_typed` still returns `ClaimedDevice<T>` for
composition into `DriverCtx`. Explicit-release path:
`DriverCtx::release(self, devmgr_ep, reply_obj)` calls the audited
shutdown.

Audit (Phase 3 was greenfield; should be clean): no current driver
or userlib code reads `mmio_pageset` externally. Grep before
landing.

### 4B.6 — Segment-based virtqueue submission

Current: `alloc_chain3(buf0_pa, len0, flags0, ..., buf2_pa, len2,
flags2)` — 9 positional args at
`user/lockjaw-userlib/src/virtqueue.rs:145-150`. The shape of the
bug it's trying to prevent.

**Change in `user/lockjaw-userlib/src/virtqueue.rs`:**

```rust
pub enum Direction { DeviceReadable, DeviceWritable }

pub struct Segment {
    pub pa: u64,
    pub len: u32,
    pub direction: Direction,
}

impl Segment {
    pub fn readable(pa: u64, len: u32) -> Self { ... }
    pub fn writable(pa: u64, len: u32) -> Self { ... }
}

pub fn submit_chain(&mut self, segs: &[Segment]) -> Option<u16>;
```

Descriptor-flag computation (VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE)
moves inside virtqueue.rs once. Driver never sees the flag
constants. Scales to non-3-segment chains.

**Generated host test:** submit-chain with 2/3/4 segments; verify
descriptor-ring contents + `next`/`flags` chaining.

### 4B.7 — `Virtqueue::wait_for_completion`

Bake the canonical IRQ-wait + ack + poll loop (currently at
`user/virtio-blk-driver/src/main.rs:202-217`) into virtqueue.rs:

```rust
pub fn wait_for_completion(
    &mut self,
    irq_notif: NotificationHandle,
    irq_threshold: &mut u64,
    mmio: &VirtioTransport,         // typed-init result from 4B.2
) -> Result<(u16, u32), VirtqueueError>;
```

The driver call becomes `let (id, len) = self.vq.wait_for_completion(...)?;`
— 16 lines → 1.

### Files — Phase 4B

| Action | File |
|---|---|
| NEW | `user/lockjaw-userlib/src/driver_runtime.rs` (boot_stub!, driver_bootstrap, probe_by_*, bind_irq, standard_driver_init, DriverCtx) |
| NEW | `user/lockjaw-userlib/src/virtio.rs` (VirtioTransportInit + VirtioTransport) |
| NEW | `user/lockjaw-userlib/examples/escape_valve_cprman.rs` (compiled example demonstrating Tier-A composition for the no-IRQ cprman shape — keeps the documented escape valve from drifting before Phase 7) |
| MOD | `user/lockjaw-userlib/src/lib.rs` (module decls + re-exports) |
| MOD | `user/lockjaw-userlib/src/dma.rs` (split OwnedDmaMapping / BorrowedDmaMapping, add Drop) |
| MOD | `user/lockjaw-userlib/src/devmgr.rs` (ClaimedDevice surface, release()) |
| MOD | `user/lockjaw-userlib/src/virtqueue.rs` (Segment API, wait_for_completion, switch backing to OwnedDmaMapping) |
| MOD | `user/virtio-blk-driver/src/main.rs` (driver_main!, init builder, Segment, wait_for_completion, remove STATUS_BYTE_OFFSET, swap DMA types — comprehensive driver rewrite to validate the framework) |
| NEW | `docs/book-of-lockjaw/05-driver-construction.md` (Tier A/B/C boot layering, escape valves, driver_main! expansion, init builder pattern as plan-apply exemplar) |

### Acceptance — Phase 4B

**Capability gates**, not line-count gates (reviewer correction:
"line-count is easy to game and slightly distorts design toward
line-count golf"):

1. **Binary: ZERO `#[allow(unsafe_code)]` attribute lines in driver
   source.** `grep -rEn '^[[:space:]]*#!?\[allow\(unsafe_code'
   user/virtio-blk-driver/src/` returns nothing (walk the crate,
   NOT just `main.rs` — drivers with helper modules like uart and
   emmc2 would leak past a single-file check). Crate-root attribute
   is `#![deny(unsafe_code)]` — `forbid` rejects ANY per-item
   `#[allow]` including macro-emitted ones, and the boot macros
   need them on `#[no_mangle] _start` / `#[link_section]
   LOCKJAW_HASH_SECTION` items. The seal between driver source and
   macro body is what the grep enforces: macro expansions of
   `boot_stub!` produce the allows inside `lockjaw-userlib`'s
   macro definition, which is the single audited location for them.
   See "Why deny, not forbid" in Strategic Intent.
2. **No boot ceremony in driver crate.** No `#[no_mangle] fn _start`,
   no `LOCKJAW_HASH_SECTION` declaration, no `#[link_section]`,
   no manual `sys_call_ret4(bootstrap_endpoint(), ...)`.
3. **No raw syscalls for claim / bootstrap / IRQ.** Driver does not
   call `sys_call_ret4(devmgr_ep, ..., CMD_CLAIM_BY_ADDR, ...)`,
   `sys_create_notification`, `sys_bind_irq_flags`, or
   `sys_create_reply` directly. Uses `claim_typed` / `bind_irq` /
   `driver_bootstrap` instead.
4. **No raw MMIO/DMA/barrier operations.** Driver does not call
   `lockjaw_mmio::cell::*` directly, does not call
   `lockjaw_mmio::barrier::*` directly, does not call
   `DmaCell::at` / `DmaSliceDyn::at` directly. All access through
   `lockjaw_userlib::virtio::*` and `dma::OwnedDmaMapping::cell` /
   `slice`.
5. **VirtIO init via typed builder.** No raw `set_status` calls in
   driver code; init invocation uses
   `VirtioTransportInit::reset(regs)?.acknowledge().driver().negotiate(...)
   .features_ok()?.setup_queue(...).driver_ok()` chain.
6. **DMA mapping types are ownership-clear.** Driver source uses
   `OwnedDmaMapping` and/or `BorrowedDmaMapping` by name; the type
   makes ownership explicit at the field declaration.
7. **`ClaimedDevice` surface is minimal.** External readers of
   `mmio_pageset` are zero. Driver consumes `device.regs` and
   `device.irq_intid` (via DriverCtx) only.
8. **Escape valve documented and validated.** `docs/book-of-lockjaw/
   05-driver-construction.md` enumerates the escape valves (no-IRQ,
   multi-MMIO, clock-first, non-server) and shows how to use Tier-A
   pieces directly. At least one escape-valve flow has a host-side
   sanity test (smoke test that `boot_stub!` + manual `driver_bootstrap`
   + manual `claim_typed` composes without conflict).
9. **`cargo test -p lockjaw-userlib --target aarch64-apple-darwin`**
   passes: host tests for OwnedDmaMapping Drop, BorrowedDmaMapping
   Drop, Segment-based submit_chain, and (mock-MMIO) typed init
   builder state transitions.
10. **`make test`** — QEMU integration green on both GICv3 and
    GICv2 (count was 99 at Phase 4B; phases after add granular
    assertions). Phase 14 still exercises virtio-blk end-to-end
    through the new framework.
11. **Paired Codex + Claude review on the Phase 4B commit** (same
    shape as Phase 3 review that produced this revision).

**Smell signal (not a gate):** lines of driver code in
virtio-blk-driver/src/main.rs should drop substantially. The
expected magnitude is roughly: 220-line `_start` → 0 (macro), 67-line
init → ~10 (builder), 16-line completion loop → 1 (wait_for_completion).
If the totals don't move significantly downward, that's a smell —
investigate, but capability gates 1–8 above are the actual bar.

### Risks — Phase 4B

1. **Composable boot harness must actually feel composable.** Tier
   A pieces must work together without surprising interactions.
   Mitigation: write the cprman-style escape-valve example as part
   of Phase 4B docs (no driver conversion yet — Phase 7 — but the
   composed example exists as a doc-validated pattern).
2. **Typed init builder must be device-family-shaped, not blk-
   shaped.** Sanity check against virtio-net + virtio-gpu specs
   upfront. If the shapes don't all fit, push blk-specific bits out
   of the builder into the driver.
3. **DMA mapping rename ripples.** Search for every reference to
   `DmaPage` and migrate; the type split changes ownership semantics
   in subtle ways. Update tests alongside the type change in one
   commit.
4. **Macro debuggability.** `boot_stub!` + `driver_main!` expansion
   must be inspectable via `cargo expand`; documented expansion in
   the new docs chapter. Test by deliberately breaking the
   invocation and confirming the compile error has a sensible span.

---

## Phase 5 — `uart-driver` (was Phase 4)

**Goal:** first validation of Phase 4A + 4B on a new driver. Inherits
`driver_main!`, bitflag codegen (for PL011 interrupt-mask flags),
typed mapping types, and `#![deny(unsafe_code)]` with zero
`#[allow(unsafe_code)]` attribute lines in driver source. The
hand-written PL011 reference module becomes unnecessary; generated
`lockjaw_regs::pl011` replaces it.

**New files:** none (PL011 spec exists from Phase 2, extended in
Phase 4A.1 to use `[[flags]]`).

**Modified files:**
- `user/uart-driver/Cargo.toml` — add `lockjaw-regs`, `lockjaw-mmio`,
  consume `lockjaw_userlib::driver_main!`
- `user/uart-driver/src/main.rs` — convert via `driver_main!` (or
  Tier-A escape valve if shape demands); meet all Phase 4B capability
  gates.

**Acceptance:** all Phase 4B capability gates on uart-driver
(particularly gate #1: zero `#[allow(unsafe_code)]` attribute
lines); `make test` green (99/99 at Phase 5 landing); Pi flash
UART continues to flow; paired Codex+Claude review.

---

## Phase 6 — `ramfb-driver` + big-endian + stream support (was Phase 5)

**Spec format additions:**
- Per-register `endian = "big"` attribute (gen_regs.rs:393-398
  currently panics).
- `kind = "stream"` accessor-method emission (gen_regs.rs:377-384
  currently partially scaffolded; Stream case is the only one not
  panicked).

**New files:**
- `user/regspecs/fwcfg.toml` — fw_cfg + ramfb register spec
- `user/lockjaw-regs/src/fwcfg.rs` — generated

**Modified files:**
- `xtask/src/gen_regs.rs` — endian + stream emitter completion
- `user/ramfb-driver/{Cargo.toml,src/main.rs}` — convert via
  `driver_main!` or escape-valve composition

**Acceptance:** Phase 4B gates on ramfb-driver; regspec status
table updates (stream + big-endian → ✅ Production); `make test`
100/100 (Phase 6 added the ramfb directory-walk assertion);
paired Codex+Claude review.

---

## Phase 7 — Wire-DTO codegen (NEW)

**Layer:** `xtask` + `lockjaw-types` (codegen substrate only; mirror of
the Phase 2 `lockjaw-regs` MMIO codegen but for DMA-shared structs).

**Goal:** make the framework own DMA layout the way it owns MMIO
layout — generated from TOML specs, drivers express intent over typed
values, no hand-rolled byte packing. Today the corpus is seven
hand-written `#[repr(C)]` (or `#[repr(C, packed)]`) DTOs with
hand-encoded BE/LE conventions; the next phase (emmc2) wants to add
ADMA descriptors, the largest wire format in the entire device
corpus. Land the codegen now so emmc2 inherits it.

### Why this phase exists (Phase 6 retrospective)

Phase 6 surfaced a real soundness bug: `RamfbConfig` was
`#[repr(C)]` with 4 bytes of trailing padding (Rust adds it to
align the leading `u64`), which violates `DmaValue`'s "no padding
bytes that could be undef" contract. The quick fix landed
(`#[repr(C, packed)]` + a sealed `dma_value_impl!` macro with a
compile-time const_assert) and the bug class is now structurally
unrepresentable for hand-written types. But the analysis revealed
the deeper pattern:

- MMIO works because **the driver expresses intent over typed
  values**; the framework owns layout, endianness, alignment, the
  volatile-access mechanics. `regs.write_selector(0x19)` →
  generated to `self.selector.write(v.to_be())`. Driver authors
  can't write to a read-only register, can't take a `&` to a
  register field, can't forget `to_be()` on a BE field.
- DMA was solved one layer down: drivers hand-wrote
  `#[repr(C)] struct RamfbConfig { addr_be: u64, ... }`,
  hand-wrote a constructor applying `to_be()` per field, and
  hand-applied `#[repr(C, packed)]` when padding bit. The driver
  still expresses layout. The framework just checks the result.

Wire-DTO codegen pulls DMA up to the MMIO level: drivers consume
generated typed DTOs and never touch byte layout, BE/LE, or
padding. The framework forecloses the mistake.

The forcing function: emmc2 (Phase 9) has ADMA descriptors with
multiple field layouts. If wire-DTO codegen slips past Phase 9,
ADMA descriptors get hand-written, and the migration backlog
doubles. The reviewer cycle ("agent quick-fix vs framework fix")
that produced the Phase 6 sealed-DmaValue rework would repeat —
emmc2's ADMA review would force the same extraction. Doing it
now keeps Phase 9's review focused on emmc2 protocol, not on
wire-format hygiene.

### Strategic intent

- **Define each DMA struct once.** TOML wirespec is the source
  of truth for layout + per-field byte order. Reviewers audit
  the spec; the struct, constructor, accessors, and DmaValue
  impl all follow by construction.
- **Make padding unrepresentable.** Generated DTOs are
  `#[repr(transparent)]` newtypes over `[u8; N]`. Size is
  exactly N, alignment is 1, padding is impossible. The DmaValue
  contract is satisfied trivially.
- **Drivers don't see bytes.** All access is through generated
  field accessors. Driver code reads `desc.addr()`, not
  `u64::from_le_bytes(desc.0[0..8].try_into().unwrap())`.
- **Endian discipline lives in the spec.** Per-field (or
  per-DTO default) `endian = "big"` / `endian = "little"` is the
  one place a maintainer changes byte order. Driver code never
  calls `to_be()` / `from_be()` directly.
- **Hand-written code stays in family modules.** Generated
  files only contain DTOs (struct + accessors + DmaValue impl +
  tests). Application-logic helpers (constants, decoded variants
  like `FwCfgFile`, semantic methods like `is_complete()`,
  convenience constructors like `write_to_selector`) stay in
  `lockjaw-types/src/<family>.rs` and re-export from the
  generated wire module.

### 7.1 Wirespec TOML schema

New directory `user/wirespecs/`. One file per device family;
multiple `[[wire]]` entries per file via TOML's array-of-tables
syntax.

```toml
# user/wirespecs/virtio.toml
# Multiple wire DTOs in one file (one family per file).

[[wire]]
name = "VirtqDesc"
description = "Split virtqueue descriptor (spec 2.7.5)"
size = 16
endian = "little"               # default applies to all fields

[[wire.fields]]
name = "addr"
offset = 0
width = 64
description = "Buffer guest-phys"

[[wire.fields]]
name = "len"
offset = 8
width = 32
description = "Buffer length in bytes"

[[wire.fields]]
name = "flags"
offset = 12
width = 16
description = "VIRTQ_DESC_F_* flags"

[[wire.fields]]
name = "next"
offset = 14
width = 16
description = "Next-descriptor index (if F_NEXT set)"

# Next DTO in the same file:
[[wire]]
name = "VirtqAvail"
size = 4
endian = "little"
# ... fields ...
```

For BE families (fwcfg):
```toml
# user/wirespecs/fwcfg.toml
[[wire]]
name = "FwCfgDmaAccess"
size = 16
endian = "big"

[[wire.fields]]
name = "control"
offset = 0
width = 32
[[wire.fields]]
name = "length"
offset = 4
width = 32
[[wire.fields]]
name = "address"
offset = 8
width = 64
```

For fields that the constructor should NOT take (e.g. virtio-blk
header's `reserved` field is always 0):
```toml
[[wire.fields]]
name = "reserved"
offset = 4
width = 32
default = 0                     # constructor omits this; accessor still emitted
```

**Validation rules** (rejected at `gen-wires --check` time):
- Field offsets monotonic + fit within `size`.
- Field widths are 8 / 16 / 32 / 64.
- `size == sum_of_field_widths_in_bytes` (no padding tolerated at
  spec level; the resulting struct is `[u8; size]` and per-field
  offsets must tile it).
- `endian` per-field overrides per-DTO default if both present.
- No duplicate DTO names within a file or across families.

### 7.2 `xtask gen-wires` emitter

New `xtask/src/gen_wires.rs`, parallel structure to
`gen_regs.rs`:

- `run(check: bool)` — discover `user/wirespecs/*.toml`, parse,
  validate, emit `lockjaw-types/src/wire/<family>.rs`.
- `--check` mode: regenerate in-memory, diff against committed
  files, exit non-zero on drift. Wired into `make build`
  alongside `gen-regs --check`.

**Emitted module shape** (per family file):

```rust
//! GENERATED from user/wirespecs/virtio.toml
//! Do not edit by hand. Regenerate with `cargo xtask gen-wires`.
//! Drift caught by `cargo xtask gen-wires --check` in CI.

#![allow(dead_code, missing_docs)]

use crate::dma_value_impl;

// ---------- VirtqDesc ----------

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct VirtqDesc([u8; 16]);

impl VirtqDesc {
    pub fn new(addr: u64, len: u32, flags: u16, next: u16) -> Self {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..16].copy_from_slice(&next.to_le_bytes());
        Self(b)
    }
    pub fn addr(&self) -> u64 {
        let mut bs = [0u8; 8]; bs.copy_from_slice(&self.0[0..8]);
        u64::from_le_bytes(bs)
    }
    pub fn len(&self) -> u32 { /* same shape */ }
    pub fn flags(&self) -> u16 { /* ... */ }
    pub fn next(&self) -> u16 { /* ... */ }
}

dma_value_impl!(VirtqDesc, size = 16);

// ... next DTO ...
```

Key emitter choices:
- `#[repr(transparent)]` over `[u8; N]` — size is N, alignment is 1
  (no padding possible, ever). Replaces all `#[repr(C, packed)]`
  discipline.
- Constructor takes host-order args, applies endian conversion per
  field. Field order in the constructor matches spec order.
- Field accessors return host-order values via `from_le_bytes` /
  `from_be_bytes`. Each accessor is one stack-local + one copy +
  one decode — compiler optimizes to a single aligned read in
  release builds.
- `dma_value_impl!` is emitted next to the type so the size +
  sealing live with the spec.

### 7.3 Generated tests (per DTO)

The codegen emits `#[cfg(test)] mod tests { ... }` per generated
file with:

- **Layout assertion**: `assert_eq!(core::mem::size_of::<T>(), N)`
  (redundant with `dma_value_impl!`'s const_assert but explicit;
  alignment also asserted: `align_of::<T>() == 1`).
- **Field offset assertions**: by reconstructing each field's
  expected byte range and comparing extraction with the accessor's
  output on a deliberately-distinguishable byte pattern.
- **BE/LE roundtrip**: for each field, construct via `new(...)`
  with a value whose byte pattern differs visibly between BE and
  LE (e.g. `0x1234_5678`); peek the raw bytes at the field's
  offset via `&value.0[A..B]` and assert the expected byte
  sequence; read back via the accessor and assert host-order
  recovery.
- **Default-field assertion**: for fields with `default = N`, the
  constructor's omission of the field is checked by asserting the
  accessor returns `N` for a newly-constructed value.

### 7.4 Migration: 7 hand-written DTOs → generated

| DTO | Current location | Wirespec | Notes |
|---|---|---|---|
| VirtqDesc | lockjaw-types/src/virtio.rs:123 | virtio.toml | LE |
| VirtqAvail | lockjaw-types/src/virtio.rs:148 | virtio.toml | LE; 4-byte header (ring entries are separate slice) |
| VirtqUsed | lockjaw-types/src/virtio.rs:171 | virtio.toml | LE; same shape |
| VirtqUsedElem | lockjaw-types/src/virtio.rs:161 | virtio.toml | LE |
| VirtioBlkReqHeader | lockjaw-types/src/virtio.rs:246 | virtio.toml | LE; `reserved` field uses `default = 0` |
| FwCfgDmaAccess | lockjaw-types/src/fwcfg.rs:62 | fwcfg.toml | BE; convenience `write_to_selector` constructor stays hand-written |
| RamfbConfig | lockjaw-types/src/fwcfg.rs:171 | fwcfg.toml | BE; `#[repr(C, packed)]` discipline disappears |

**Module reorganization:**
- New `lockjaw-types/src/wire/mod.rs` — `pub mod virtio;` `pub mod fwcfg;`
- New `lockjaw-types/src/wire/virtio.rs` — GENERATED (5 DTOs).
- New `lockjaw-types/src/wire/fwcfg.rs` — GENERATED (2 DTOs).
- Existing `lockjaw-types/src/virtio.rs` — hand-written content
  stays (constants `VIRTIO_F_VERSION_1`, `BLK_DRIVER_WANTED`,
  `VIRTIO_BLK_T_IN`, etc.; `FeatureNegotiation` model;
  `virtqueue_layout()`). Adds `pub use crate::wire::virtio::*;`
  at the top — re-exports the generated DTOs so existing
  `lockjaw_types::virtio::VirtqDesc` import paths work unchanged.
- Existing `lockjaw-types/src/fwcfg.rs` — hand-written content
  stays (`FwCfgFile` post-decode struct, `DMA_CTRL_*` constants,
  `FW_CFG_FILE_DIR`, `RAMFB_FORMAT_XRGB8888`,
  `RAMFB_CONFIG_WIRE_SIZE`). Adds convenience methods on the
  generated types via `impl FwCfgDmaAccess { ... }` blocks:
  `write_to_selector`, `is_complete`, `is_error`.
- `lockjaw-types/src/dma.rs` — removes the 5 virtio +
  the (now-moved) fwcfg `dma_value_impl!` calls; the generated
  files emit them.

### 7.5 Driver-side migrations

| Call site | Old shape | New shape |
|---|---|---|
| `virtqueue.rs:168-173` (init_free_chain) | `VirtqDesc { addr, len, flags, next }` struct literal | `VirtqDesc::new(addr, len, flags, next)` |
| `virtqueue.rs` (submit_chain, free_chain) | `desc.read(i).next` field access | `desc.read(i).next()` method |
| `virtqueue.rs` (avail/used header reads) | `header.read().idx` / `.flags` | `header.read().idx()` / `.flags()` |
| `virtqueue.rs:230` (poll_used) | `elem.id`, `elem.len` field access | `elem.id()`, `elem.len()` |
| `virtqueue.rs` (avail header write) | `VirtqAvail { flags, idx }` struct literal | `VirtqAvail::new(flags, idx)` |
| `virtio-blk-driver/src/main.rs:170-174` | `VirtioBlkReqHeader { req_type, reserved: 0, sector }` | `VirtioBlkReqHeader::new(req_type, sector)` (constructor omits `reserved`) |
| `ramfb-driver/src/main.rs:73` (RamfbConfig) | `RamfbConfig::new(...)` | unchanged |
| `lockjaw-userlib/src/fwcfg.rs:78-95` (FwCfgDmaAccess) | `::new(...)` / `::write_to_selector(...)` | unchanged (convenience constructor preserved via hand-written `impl` block) |

Driver-side churn is bounded: ~10 line edits in virtqueue.rs +
1 in virtio-blk-driver. ramfb and lockjaw-userlib::fwcfg need no
changes because they already use `Type::new()`.

### 7.6 Make build integration

`Makefile`:
- `make build` runs `cargo xtask gen-wires --check` alongside the
  existing `cargo xtask gen-regs --check`.
- `make test` regenerates both before host-test runs.

### Files — Phase 7

| Action | File |
|---|---|
| NEW | `user/wirespecs/virtio.toml` (5 [[wire]] entries) |
| NEW | `user/wirespecs/fwcfg.toml` (2 [[wire]] entries) |
| NEW | `xtask/src/gen_wires.rs` (parser + emitter + tests, parallel structure to gen_regs.rs) |
| NEW | `lockjaw-types/src/wire/mod.rs` |
| NEW | `lockjaw-types/src/wire/virtio.rs` (GENERATED) |
| NEW | `lockjaw-types/src/wire/fwcfg.rs` (GENERATED) |
| MOD | `xtask/src/main.rs` — wire up `gen-wires` subcommand |
| MOD | `xtask/Cargo.toml` — share serde/toml deps if needed (gen_regs already has them) |
| MOD | `lockjaw-types/src/lib.rs` — `pub mod wire;` |
| MOD | `lockjaw-types/src/dma.rs` — remove the 5 virtio `dma_value_impl!` calls (now emitted by generated wire/virtio.rs) |
| MOD | `lockjaw-types/src/virtio.rs` — `pub use crate::wire::virtio::*;` + retain hand-written non-DTO content (constants, FeatureNegotiation, virtqueue_layout) |
| MOD | `lockjaw-types/src/fwcfg.rs` — `pub use crate::wire::fwcfg::*;` + retain hand-written content (FwCfgFile, constants, convenience methods via `impl` blocks on the generated types) |
| MOD | `Makefile` — add `gen-wires --check` to `make build` |
| MOD | `user/lockjaw-userlib/src/virtqueue.rs` — migrate VirtqDesc struct-literal → `::new`; field reads → accessor calls |
| MOD | `user/virtio-blk-driver/src/main.rs` — migrate VirtioBlkReqHeader struct-literal → `::new` |

### Acceptance — Phase 7

These are **capability gates**, not line-count gates:

1. **Zero `#[repr(C, packed)]` in `lockjaw-types/src/`.** The
   `RamfbConfig` discipline that Phase 6 documented disappears —
   `#[repr(transparent)]` over `[u8; N]` makes padding impossible.
2. **Zero hand-written `unsafe impl DmaValue for ...` or
   `dma_value_impl!(...)` in non-generated files.** Every
   invocation lives in a generated `lockjaw-types/src/wire/*.rs`.
   `grep -rn 'dma_value_impl' lockjaw-types/src/dma.rs
   lockjaw-types/src/virtio.rs lockjaw-types/src/fwcfg.rs`
   returns nothing.
3. **`cargo xtask gen-wires --check`** in `make build` — drift
   caught at CI time.
4. **All 7 existing DTOs covered.** No struct using
   `#[repr(C)] { addr_be: u64, ... }` survives in `lockjaw-types`.
5. **`cargo test -p lockjaw-types`** — generated tests pass for
   every DTO (size, alignment, field offsets, BE/LE roundtrip,
   default-field defaults).
6. **`make test`** — 100/100 QEMU integration green (no driver-
   visible behavior change; only the layout-encoding mechanism
   changed).
7. **Driver call-site grep**: zero `to_be_bytes` / `from_be_bytes`
   / `to_be` / `from_be` calls in driver source. The generated
   accessors are the only place these primitives are called.
8. **Paired clean-Claude + Codex review** on the Phase 7 commit
   (same shape as Phase 4B / Phase 6 commit reviews).

### Risks — Phase 7

1. **Newtype-over-bytes accessor cost.** Each `.field()` call
   copies bytes to a local then calls `from_le_bytes`. In debug
   builds this is real work; in release with inlining the
   compiler should reduce to a single aligned read. Mitigation:
   include a release-mode codegen check (`cargo asm` or
   `objdump`) on `VirtqDesc::next()` confirming the generated
   machine code matches a hand-written `unsafe { ptr::read_volatile }`
   in instruction count. If not, reconsider the
   `#[repr(transparent)]` shape vs `#[repr(C, packed)]` with a
   sealed PaddingFree marker trait.
2. **`#[repr(transparent)]` over `[u8; N]` interaction with
   `DmaCell`.** `DmaCell<T>` does `ptr::read_volatile(self.0.get())`
   returning `T`. `T` must be `MmioWord` or pass the `DmaValue`
   trait bound — confirm a 28-byte newtype passes the latter
   (it does: the sealed trait is implemented by the macro). The
   volatile read of 28 bytes is multi-instruction on AArch64
   (no 28-byte load); confirm this matches the current `#[repr(C, packed)]`
   semantics via codegen comparison.
3. **Multi-byte volatile semantics.** Writing a 28-byte
   `RamfbConfig` via `DmaCell::write` is multiple stores. QEMU's
   DMA reads from the resulting bytes; the device sees a
   consistent snapshot only after all writes complete. The
   `dma_write` helper already inserts `dsb_sy` before the MMIO
   trigger, so this is handled — but the codegen comment on the
   generated DTO should document the "multi-store; needs barrier
   before device read" invariant explicitly.
4. **Spec/codegen complexity creep.** Wirespec format must NOT
   grow to express anything the corpus doesn't need. Bit-fields,
   enum-valued fields, alignment overrides, packed sub-structs —
   all out of scope until a real device needs them. Mitigation:
   land the 7 migrations FIRST, then add features only when a
   driver demands them. Same discipline as Phase 4A's deferred
   codegen features.

### What this enables next

- **Phase 9 (emmc2) gets ADMA descriptors as wirespec entries**
  instead of hand-written. The largest wire-format work in the
  corpus inherits the typed-DTO discipline.
- **New device families start with `<family>.toml` for both
  regspec and wirespec.** The "define each device once" framing
  extends to DMA layouts.
- **Driver authors stop knowing what byte order a field is.**
  Spec carries it; accessor handles it. Reviewers audit the
  spec; generated code follows by construction.
- **DmaValue corpus closes.** No hand-written impls; all
  invocations are generated. The sealed trait + const_assert
  guard the trait surface; the codegen + spec validation guard
  the type shapes. Bug class is structurally extinct.

---

## Phase 8 — `cprman-driver` + passwd_protected (was Phase 7)

**Spec format addition:** `kind = "passwd_protected"` register
attribute that prepends BCM2711 CM_PASSWD on every write. Generated
test examines the written value to confirm PASSWD is always
included.

**Likely escape-valve user:** cprman has no MMIO IRQ; uses Tier-A
composable pieces (per Phase 4B.1 example) rather than
`driver_main!`.

**New files:**
- `user/regspecs/cprman.toml`
- `user/lockjaw-regs/src/cprman.rs` — generated

**Modified files:**
- `xtask/src/gen_regs.rs` — passwd_protected emitter
- `user/cprman-driver/{Cargo.toml,src/main.rs}` — convert

**Acceptance:** Phase 4B gates on cprman-driver; regspec status
table updates (passwd_protected → ✅ Production); `make test`
green (count carries forward from Phase 7 — likely 100/100
unchanged since cprman is QEMU-absent and exits gracefully like
emmc2 does today); Pi flash CPRMAN log line continues; paired
Codex+Claude review.

---

## Phase 9 — `emmc2-driver` (was Phase 7, then Phase 8)

**Wire-DTO codegen now in scope (per Phase 7).** ADMA2 descriptors
use the `[[wire]]` schema from `user/wirespecs/sdhci.toml`, not
hand-written `#[repr(C)]` structs. The descriptor table is a
`[Adma2Descriptor; N]` of generated newtypes; field accessors
(attr, length, address) are emitted, the `DmaValue` impl is
emitted, the no-padding invariant is mechanical.

**Biggest payoff, highest risk.** 40 unsafe blocks, 76 MMIO call
sites, W1C interrupt registers, combined-trigger
TRANSFER_MODE+COMMAND, ADMA descriptor writes. Just stabilized in
#131. Save for last.

**Regspec additions** (MMIO codegen, `xtask gen-regs`):
- `kind = "combined_trigger"` — TRANSFER_MODE+COMMAND emitted as one
  combined newtype whose write path is exactly one volatile 32-bit
  write (assertion in generated test).
- `kind = "aliased"` — SYSADDR/ARGUMENT2 at 0x000: two accessor
  names for the same cell.

**Wirespec additions** (DMA codegen, `xtask gen-wires`, landed
in Phase 7):
- `user/wirespecs/sdhci.toml` — new `[[wire]] Adma2Descriptor`
  entry: 8-byte LE descriptor with fields `attr: u8` (offset 0,
  bit-packed flag combinations: NOP, TRAN, TRAN+END, LINK),
  `length: u16` (offset 2), `address: u32` (offset 4 — ADMA2-32
  is the 32-bit variant; SDHCI 4.x adds ADMA2-64 which we don't
  need for emmc2). Generated `Adma2Descriptor::new(attr, length,
  address) -> Self`, plus accessors. `dma_value_impl!` is
  emitted automatically.
- Driver builds the descriptor table as
  `OwnedDmaMapping::alloc_contiguous(pages)` +
  `slice::<Adma2Descriptor>(0, n)` writes — same shape as the
  Phase 7 virtio virtqueue case.

**Possibly:** if 2+ drivers showed typed-DMA-page recurrence by this
point, evaluate `TypedDmaPage<L>` codegen as a follow-up commit.

**Acceptance:** Phase 4B gates on emmc2-driver (zero
`#[allow(unsafe_code)]` attribute lines, zero raw `sys_*`
outside the allowlist, zero unsafe items); regspec status table
updates (combined_trigger / aliased ✅); wirespec status table
adds `Adma2Descriptor` to Phase 7's generated corpus; ADMA
descriptors emit from `user/wirespecs/sdhci.toml`, NOT
hand-written; zero `#[repr(C, packed)]` or hand-written
`unsafe impl DmaValue` introduced; `make test` 100/100; Pi flash
full fat32 → posix → HELLO.TXT chain works; **most thorough
Codex review** — pre-commit on staged diff AND post-commit
`codex review --commit <SHA>` (per #131 pattern).

---

## Phase 10 — Final lockdown (was Phase 8, then Phase 9)

**Modified files:**
- `user/lockjaw-userlib/src/devmgr.rs` — final audit that
  `claim_typed::<T>` is the only sanctioned driver-side claim path.
- `xtask/src/check_driver_unsafe.rs`:
  - Explicit driver-crate allowlist.
  - For each: walk `src/**/*.rs` (NOT just `main.rs`), verify
    `#![deny(unsafe_code)]` at the crate root AND zero
    `#[allow(unsafe_code)]` attribute lines anywhere in any source
    file. (This is the binary gate from Phase 4B raised to a
    CI-enforced check; walking the crate is required so drivers
    with helper modules can't leak past a single-file check. The
    `deny` policy — not `forbid` — is the settled regime; see
    "Why deny, not forbid" in Strategic Intent.)
  - Compiler is the source of truth for actual lint enforcement;
    xtask just verifies the lint level is set AND no per-item
    `#[allow]` attributes appear in driver source. Macro-expanded
    allows in `lockjaw-userlib` are out of scope (they're the
    intentional single audited location).
  - Also walks driver source for raw `sys_*` calls and rejects
    anything outside the `sys_exit` / `sys_debug_puts` allowlist
    (per CLAUDE.md engineering principle landed in Phase 5).
  - Wired into `make build`.
  - Smoke tests:
    - Remove `#![deny(unsafe_code)]` from one driver → xtask fails.
    - Insert `#[allow(unsafe_code)] fn x() { unsafe { } }` into a
      driver source file → xtask fails on the attribute line.
    - Insert a raw `sys_alloc_pages(...)` call into a driver
      source file → xtask fails on the not-in-allowlist syscall.
- `xtask/src/main.rs` — `cargo xtask gen-regs --check` runs in
  `make build`; both checks gate the build.
- `docs/book-of-lockjaw/<chapter>.md` — finalize "MMIO + DMA +
  barriers + register codegen + driver runtime: the user-mode
  driver substrate" chapter started in Phase 4B.
- `CLAUDE.md` Engineering Principles — add: "User-mode driver code
  consumes `lockjaw-regs` (generated from `user/regspecs/*.toml`),
  `lockjaw-mmio` primitives, and the boot harness from
  `lockjaw_userlib::driver_runtime`. `unsafe` belongs only in
  `lockjaw-mmio` and the audited macro expansions inside
  `lockjaw-userlib`. New devices start with a spec file."

**Acceptance:**
- `make build` runs `gen-regs --check`, `gen-wires --check`,
  AND `check-driver-unsafe`.
- `make test` green (whatever count Phase 9 leaves).
- Regspec status table shows ✅ Production for every spec feature.
- Wirespec status table shows ✅ Production for every DTO
  (`Adma2Descriptor` migrated in Phase 9 is the last addition).
- Final Pi flash sanity boot.

---

## What the spec formats must express

Two spec systems, two scopes. The status table in Phase 4A is the
authoritative source of truth for which features are parser-
supported, emitter-supported, and used in production. This section
documents the intended end state per system, split by what each
system owns.

### Regspec — MMIO register layout (`user/regspecs/`, generated to `lockjaw-regs`)

Owns: what the device exposes through memory-mapped registers.

- ro/rw/wo/w1c access kinds at u8/u16/u32/u64 widths
- Named bit-range fields with optional enum_values
- Enum-valued fields with reserved/unknown handling
- Bitflag types (`[[flags]]`)
- Paired 64-bit registers (`[[u64_pairs]]`)
- Windowed registers (`[[windowed]]`)
- `verify_against` cross-check against lockjaw-types — OFFSETS ONLY
- Aliased registers (same offset, different names)
- Combined-trigger register pairs
- Stream-port registers (fw_cfg-style data ports)
- Big-endian / little-endian per register
- PASSWD-protected writes
- Reserved bit lists for preservation

DMA descriptor layouts are NOT a regspec concern — they live in
wirespec.

### Wirespec — DMA shared-memory layout (`user/wirespecs/`, generated to `lockjaw-types::wire`, lands in Phase 7)

Owns: what the device reads from or writes to in guest RAM via
DMA (or any other shared-memory channel — block request headers,
virtqueue descriptors, ramfb config, ADMA descriptors).

- Per-DTO size, with the resulting Rust struct being
  `#[repr(transparent)]` over `[u8; N]` so the size matches the
  wire size by construction and padding is structurally
  impossible.
- Per-field offset + width (u8/u16/u32/u64).
- Per-field big-endian or little-endian byte order, with a
  per-DTO default and per-field override.
- Optional per-field `default = N` to mark fields the
  constructor should omit and always emit as N (e.g. virtio-blk
  header's `reserved` field is always 0).
- Generated `Type::new(...)` constructor taking host-order
  args, applying byte-order conversion per field.
- Generated per-field accessors returning host-order values.
- Auto-emitted `dma_value_impl!(Type, size = N)` — the sealed
  DmaValue invariant rides along by construction.

Wirespec EXCLUDES: bit-field decoding within a wire field (handled
in hand-written family modules — e.g. `FwCfgDmaAccess`'s
`is_complete()` / `is_error()` are convenience methods on the
generated DTO via `impl` blocks, not wirespec features); enum
decoding within fields (same argument); convenience constructors
that combine flags with field values (e.g.
`FwCfgDmaAccess::write_to_selector` lives in hand-written
fwcfg.rs).

---

## Out of scope

- **Proc-macros for codegen** — keep as build-time xtask
- **SVD or third-party spec format** — TOML home-grown is fine for
  ~6 devices
- **`bitflags` external dep** — generated code uses hand-rolled
  const masks (Phase 4A.1 emits the operators)
- **Drop semantics on MappedRegs / DmaCell** — driver MMIO mappings
  are process-lifetime; DMA mappings have Drop because they own
  mutable VA + pageset (Phase 4B.3)
- **Cross-thread driver concurrency** — cells `!Sync`, MappedRegs
  `!Send + !Sync`; all drivers single-threaded today
- **TypedDmaPage<L> codegen** — both reviewers defer until shape
  recurs across 2+ drivers; revisit after Phase 8
- **Generic `DeviceEngine` trait / generic server loop** — both
  Phase 3 reviewers explicitly say resist
- **Codegen for kernel-side MMIO** — kernel has its own unsafe
  surface separate from user-mode drivers
- **`verify_against` for composed semantics (u64/windowed)** —
  validated via roundtrip / MMIO-sequence tests in codegen (4A.2,
  4A.3) rather than via lockjaw-types constants; deeper cross-check
  via synthesized lockjaw-types constants is feasible but deferred
- **Atomic hex/decimal output helpers** — small; do alongside any
  driver that needs them
- **Operation-centric generated façades** (reviewer's "didn't take
  my advice enough" #2: codegen still register-centric, not
  operation-centric) — Phase 4A is the directional step; deeper
  push toward generated device-language façades is a follow-up
  after the device-family layer (Phase 4B-style userlib modules)
  shows what canonical operations recur

---

## Risks (cross-phase)

1. **Phase 8 (emmc2) regression risk.** Just stabilized in #131;
   structural change could re-introduce subtle ordering bugs.
   Mitigation: save for last; most thorough Codex review.
   **Cheap pre-emptive hedge:** Phase 4A.6 (emmc2 reverse-scan)
   reads emmc2's current main.rs during 4A planning to confirm 4B's
   DMA + virtqueue + boot shape can express what emmc2 needs.
   Surfacing a gap NOW is N× cheaper than discovering it in Phase 8
   and retrofitting 4B.
2. **Phase 4A + 4B is the second load-bearing inflection.** Phase 2
   was the first (codegen design); Phase 4 is the second (driver-
   shape decisions propagate to drivers 5-8). Paired Codex+Claude
   review on the Phase 4B commit, same shape as the Phase 3 pair
   that produced this revision.
3. **Drift between specs and generated code.** Mitigation: `cargo
   xtask gen-regs --check` in `make build`.
4. **Spec format complexity creep.** Each new pattern adds spec
   format complexity. Mitigation: each pattern justified by a
   device that needs it. Phase 4A adds bitflag/u64_pair/windowed/
   verify_against because virtio + PL011 + future drivers all need
   them; resist speculative additions.
5. **`#![deny(unsafe_code)]` + `asm!`.** `asm!` requires unsafe.
   Resolution: driver-needed `asm!` lives in substrate (barrier
   helpers in `lockjaw-mmio::barrier`) — drivers call safe
   `dmb_ish()` etc. wrappers, never `asm!` directly. `deny` (not
   `forbid`) accommodates the boot-macro per-item allows; see
   "Why deny, not forbid" in Strategic Intent.
6. **`driver_main!` / `boot_stub!` debuggability** [Phase 4B]:
   compile errors must have clear spans; `cargo expand` recipe in
   the docs chapter.
7. **Composable harness must compose.** [Phase 4B] — risk that
   Tier-A pieces don't actually combine cleanly into the escape
   valves they promise. Mitigation: write the cprman-style escape-
   valve example into the docs chapter during Phase 4B (no driver
   conversion yet — but the composed example exists and compiles).

---

## What this enables next

- **New devices start with a spec.** Ethernet, GPIO, second SD slot:
  write the TOML, run codegen, write the protocol state machine.
- **`driver_main!` makes 90% of drivers short** (name + hash +
  layout + main fn). For the 10% that don't fit, Tier-A pieces
  compose without abandoning the pattern.
- **Specs are canonical.** TOML → code stays consistent.
  `verify_against` enforces offset alignment with lockjaw-types.
- **AI-assisted driver work runs on rails.** One correct way to
  describe a device; codegen enforces consistency; macro enforces
  shape; capability gates enforce no-unsafe.
- **Doors open to formal verification.** Specs are declarative
  data; generated code follows deterministic rules.
- **"Every asm line commented" narrows.** Substrate + kernel +
  `boot_stub!` macro expansion have asm; drivers have none.
- **Driver review focus shifts** from "did the bit math work?" to
  "is the state machine correct?"
