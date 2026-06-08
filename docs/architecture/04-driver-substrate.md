# The Driver Substrate

> *Drivers were the hardest layer to keep honest. The substrate
> makes that honesty mechanical.*

Lockjaw's user-mode drivers were, for most of the project's history,
the layer with the most leeway: they ran out of process and so were
sandboxed by construction, but inside that sandbox they spoke raw
syscalls, did manual DMA cache-coherence, and reimplemented IPC
framing per crate. The early bug shapes — a forgotten
`sys_dma_sync_for_cpu`, an invalidate that ran before the device
finished writing, a virtio driver that acked the IRQ in the wrong
order — were all the same shape: a driver author had to remember a
sequence, and forgetting it produced silent corruption rather than a
crash.

This chapter is about the substrate that took those sequences away
from driver authors. It is the third installment of the project's
central move — same as the [push→pull
conversion](01-architecture.md) at the kernel boundary, same as the
[device manager](03-non-virtualizable-hardware.md) for shared
hardware — applied to the place a driver author has to type. The
worked example is **DMA cache coherence by construction**: a bug
class that bit emmc2 specifically and would have bitten every future
non-coherent-bus driver, now made unrepresentable by the type system
and an envelope.

## The regime, in one paragraph

A user-mode driver in Lockjaw is `#![no_std]`, `#![no_main]`, and
`#![deny(unsafe_code)]`. The driver source contains zero `unsafe`
blocks and zero `#[allow(unsafe_code)]` attributes. It names exactly
two raw syscalls — `sys_exit`, for the panic handler, and
`sys_debug_puts`, for diagnostic prints — because both are
inviolable: the panic handler runs after every other invariant has
failed, and the diagnostic print is the side channel that lets us
debug when the regular log path is broken. Everything else —
allocation, mapping, IPC framing, IRQ binding, DMA coherence,
register access — goes through `lockjaw-userlib`. The discipline is
quoted in `CLAUDE.md` ("User-mode drivers consume `lockjaw-userlib`,
period"); this chapter explains why it has to be construction, not
detection.

## Three doors, one ceremony

The substrate exposes three boot ceremonies, each shaped for a
distinct class of driver:

`driver_main!` is for the canonical case: a single MMIO device, a
single edge-triggered IRQ, a server endpoint, and an event loop that
alternates between IPC requests and IRQ deliveries. It expands to a
`boot_stub!` invocation plus a call to `standard_driver_init`, and
hands the driver body a `DriverCtx<T>` with typed registers, the IRQ
notification, and the server endpoint already wired up. The pl011
driver — see `user/pl011-driver/src/main.rs` — is the canonical
consumer: thirteen lines of imports, a four-method `EventEngine`
impl, and one macro invocation at the bottom.

`virtio_driver_main!` is the virtio family's analogue. The virtio
mmio probe sequence is not the same as a plain device probe: the
transport magic must be checked, the device ID matched, and phantom
empty slots in QEMU's mmio range must be silently skipped (their
magic is zero, not the virtio sentinel). The shape is otherwise
identical — `boot_stub!` underneath, a `DriverCtx<T>` out the top —
but the probe step is virtio-aware. `user/virtio-blk-driver/src/main.rs`
uses it directly.

A third sibling, `standard_driver_init_level` /
`LevelDriverCtx`, is the level-triggered IRQ variant — same shape as
`standard_driver_init`, but binds the IRQ as level-sensitive (the
SDHCI / GIC SPI default on Pi 4B). emmc2 uses it through Tier-A
composition rather than a wrapper macro; the framework piece is at
`user/lockjaw-userlib/src/driver_runtime.rs:401`.

Tier-A `boot_stub!` is the escape valve. Some drivers have a shape
the standard helpers can't reach — ramfb has no IRQ at all
(`user/ramfb-driver/src/main.rs` composes `standard_init_no_irq`
instead), emmc2 needs the level-IRQ variant plus a crate-specific
failure log line that the `driver_main!` macro hardcodes
(`user/emmc2-driver/src/main.rs:445`, with the rationale inlined
above the macro call). The escape valve still emits the single
audited `#[allow(unsafe_code)]` in macro-generated code; the driver
body remains `#![deny(unsafe_code)]`. The macro is the only
sanctioned site for that attribute in any driver binary.

All three doors converge on the same property: the audited unsafe
lives in one place — the macro expansion — and the body of every
driver crate is structurally unable to add another one. The check is
not "the reviewer noticed it"; the check is `rustc -D unsafe_code`
followed by the `check-driver-unsafe` xtask that says zero matches.

## Construction, not detection

The interesting design move is that the regime is enforced by what
`use lockjaw_userlib::*;` brings into scope.

`user/lockjaw-userlib/src/lib.rs:54` reads, in full,
`pub use syscall::{sys_exit, sys_debug_puts};`. That line is
load-bearing. The crate's `syscall` module contains the full surface
— `sys_alloc_pages`, `sys_map_pages`, `sys_dma_sync_for_cpu`, every
wrapper the kernel exposes — but only the two allowlisted names
escape into the root namespace. A driver writing
`use lockjaw_userlib::*;` literally cannot pull a forbidden syscall
into scope. To name one, it would have to write the qualified path:
`lockjaw_userlib::syscall::sys_alloc_pages`.

That path is what the `check-driver-unsafe` xtask
(`xtask/src/check_driver_unsafe.rs`) scans for. Its primary pass
uses `syn` to walk every driver crate's AST and flag any path
segment named `syscall` whose leaf isn't on the allowlist —
spacing, brace-form `use`, renames, UFCS qself, and turbofish are
all covered because the scan is on the parsed AST, not on the
formatted source. Two narrower passes close the corners syn can't
see structurally: an `ident_str` normalization that strips the `r#`
raw-ident prefix (so `r#syscall` matches), and a `visit_macro`
token-stream walk that catches any `syscall` ident inside a
`macro_rules!` rule body or a macro invocation's arg tokens (which
syn keeps opaque). The construction half (the trimmed re-export) is
the primary gate; the xtask is the backstop for the rare driver that
types out the qualified path.

The combination is the load-bearing pattern: the bug class is
*expensive to express*. A driver author who wants to make a
forbidden syscall has to spell out an obviously suspicious path and
then ignore a failed CI gate. The compile error is the gate; the
xtask is the receipt.

## The worked example — DMA coherence

DMA cache coherence is the bug class this chapter is built around.
On QEMU virt the bus is coherent and the bug never appears, which is
exactly why it festered: every QEMU-passing driver was a latent
hardware-fail driver, and the test harness could not see it. The
real shape only surfaced when emmc2 started running ADMA2 reads on
Pi 4B and the selftest read back zeros for buffers the kernel had
correctly DMA'd into.

The pre-substrate emmc2 driver issued `sys_dma_sync_for_cpu` and
`sys_dma_sync_for_device` by hand around every transfer. Five things
had to be right: the right pageset, the right offset, the right
direction, the right *number* of syncs (the `FromDevice` pre-clean
in B2.2 was missed for months — see `docs/history/post-c1-fix-plan.md`), and
the right *order* relative to the device's completion signal
(invalidate had to run *after* the device finished, not after the
kick returned — see B4.1, the DAT_INHIBIT drain). It was a
five-axis correctness problem and the driver author had to remember
all five every time.

The substrate makes four of the five mechanical and the fifth
visible.

First, **origin types** make a sync-incapable allocation a compile
error. `user/lockjaw-userlib/src/dma.rs` defines two zero-size
markers: `BuddyOrigin` for general-RAM pages (coherent on a coherent
bus, rejected by the kernel's `sys_dma_sync_*`) and `DmaPoolOrigin`
for the cache-maintenance pool. They share a sealed `DmaOrigin`
trait so a driver cannot mint a third origin. A separate
`SyncCapable` trait is implemented only for `DmaPoolOrigin`. The
`OwnedDmaMapping<O>` and `DmaBacking<O>` types carry `O` as a
`PhantomData` parameter, so the origin is in the *type* of the
mapping. `BorrowedDmaMapping` (`user/lockjaw-userlib/src/dma.rs:431`)
is the third mapping shape, for adopting an existing pageset whose
allocation a different actor owns — same origin discipline, but
`Drop` does NOT close the underlying pageset.

Second, **region construction is gated by `SyncCapable`**. The only
way to obtain a `DmaRegion` — the value the envelope consumes — is
through `OwnedDmaMapping::<O>::dma_region` or
`DmaBacking::<O>::dma_region`, each `impl<O: SyncCapable>`. Handing
a `BuddyOrigin` mapping to a sync is not a runtime
`INVALID_PARAMETER` from the kernel; it is a compile error at the
`.dma_region(...)` call site. The kernel's runtime check is still
there as a depth-in-defense, but a driver that compiles cannot
reach it through this path.

Third, **the envelope owns the ordering**. `run_dma_transfer` in
`user/lockjaw-userlib/src/dma_transfer.rs` takes a slice of regions,
a `DmaCompletion`, and a `kick` closure. It runs:

```text
clean every region (both directions — B2.2)
  → kick (driver programs + issues)
  → completion.await_complete()
  → invalidate every FromDevice region
```

The `FromDevice` pre-clean is the B2.2 fix in code: a buffer the
device is about to write to must have its CPU-dirty cache lines
written back first, or those dirty lines will land on top of the
device's freshly DMA'd bytes after the transfer. A driver author
will not think of this. The envelope thinks of it; the driver passes
a `DmaDir::FromDevice` and the substrate emits the pre-clean.

Fourth, **completion is a trait the driver implements once**.
`DmaCompletion::await_complete` is the device-specific piece — it
knows when the device is done — and supplying it as input is what
makes "invalidate before the device finished" structurally
impossible: the envelope cannot run the post-clean until the
completion returns. emmc2's `SdhciDataCompletion`
(`user/emmc2-driver/src/main.rs:1141`) wraps the level-IRQ
CMD_COMPLETE / DATA_COMPLETE wait *and* the B4.1 post-DATA_COMPLETE
`DAT_INHIBIT` poll — the controller-side drain that the cache sync
alone cannot order against. The IRQ machinery, the W1C status
clears, and the GIC unmask are device-specific and stay in the
driver; the *fact that completion comes before invalidate* is the
substrate's job.

The fifth axis — what regions exist, in what direction — is the one
piece the driver still names. It cannot be mechanized; only the
driver knows what its transfer touches. But that's the *minimum*
information needed, and the substrate makes it the *only*
information needed.

emmc2's read path is the existence proof
(`user/emmc2-driver/src/main.rs:1838`). A buffer-level outer
envelope declares the destination buffer as a `FromDevice` region
and supplies `Immediate` completion. Its `kick` runs an inner loop;
each iteration calls `adma2_single_block_read`, which sets up its
own envelope for the ADMA2 descriptor as a `ToDevice` region with
`SdhciDataCompletion` doing the level-IRQ wait. Two envelopes,
nested, both correct by construction. The buffer's outer clean
covers the B2.2 case; the descriptor's inner clean covers the
post-cacheable-DMA-migration C1 case; the buffer's outer invalidate
covers the post-DATA_COMPLETE read-back. Every cache-coherence event
in the read path is the substrate's emission of `dc cvac` (clean) or
`dc civac` (clean-and-invalidate, the B2.1 replacement for the
unpredictable-on-dirty-lines `dc ivac`) against ranges the driver
named once, in the type system.

## The operation envelope — closing R3

The DMA-coherence envelope (above) is correct by construction *as
long as the driver uses it*. The remaining gap is **R3** ("the
sanctioned transfer path is the only path"): nothing structural
prevents the driver from kicking the controller *outside* the
envelope. Pre-operation-envelope emmc2 held raw DMA PAs in scope
across multiple function calls and could fire
`set_transfer_mode_command` directly; an inattentive refactor could
drop the surrounding clean→kick→await→invalidate sequence without
the compiler complaining.

The **operation envelope** is the structural fix. It composes three
layers:

- A **capability token** on the dangerous register accessors
  (`SdhciOpToken<'_>` for SDHCI) whose only mint path is xtask-
  blacklisted in driver source. Drivers physically cannot reach the
  mint, so they cannot call the gated setters.
- An **operation-layer envelope** (`SdhciCommandInit<'a, S>`) that
  holds the only legal mint of the capability token and wraps the
  register-program → kick → await → status-cleanup sequence behind
  typed methods (`issue_no_data::<R>`, `issue_data_transfer`).
  Inside `issue_data_transfer` lives the DMA-coherence envelope —
  the operation layer composes the generic envelope rather than
  exposing the seams.
- An **outer device-state typestate** (`MmcCard<'a, S>`) that
  drives the multi-step protocol with consumed-self transitions —
  calling `engine.read()` before `select()` is a compile error
  because `Emmc2BlockEngine::new` requires a `CardInfo` token
  whose only mint is `MmcCard::<Tran>::into_parts()`.

Result: every SDHCI command in driver source flows through the
framework's typed methods. The driver expresses *intent* (issue this
command, transition to this state); the framework owns every
register access, every status poll, every IRQ wait, every coherence
operation. See
[`patterns/operation-envelope.md`](patterns/operation-envelope.md)
for the family pattern, the canonical SDHCI consumer, and the
extension trigger for the second consumer.

## What the substrate is for

The kernel-side story in this book has been about narrowing what the
kernel does ([chapter 1](01-architecture.md) calls this "the kernel
should be thin"). The driver-side story is the same move at a
different layer: narrow what a driver does so that the surface a
future driver author has to learn is small, opinionated, and
inviolable.

A new driver in Lockjaw today consists of:

- **One macro invocation** (`driver_main!` /
  `virtio_driver_main!` / `boot_stub!` + a composed init helper).
- **One engine struct** that implements an event-shape trait
  (`EventEngine`, `BlockEngine`, `DisplayEngine`) describing what
  to do on IPC and what to do on IRQ.
- **The device-specific logic**: register sequences, the
  driver-only completion semantics for any DMA the device does,
  any policy that the framework genuinely cannot know.

Every other concern — IPC framing, the syscall return convention,
the IRQ wait/threshold/unmask sequence, the DMA coherence ordering,
the bootstrap reply-handle dance — lives in `lockjaw-userlib` and
shows up in the driver as a method on `ctx` or a parameter to a
substrate function.

This is the same payoff as the kernel's push→pull conversions: a
class of sequencing bug is moved from "the author had to remember
it" to "the type system enforces it." The kernel is thin because its
decisions live in `lockjaw-types`; the driver is thin because its
ceremony lives in `lockjaw-userlib`. Both layers shed their
sequencing risk into a host-testable substrate, and what remains in
each layer is the side effects only it can perform.

The substrate is also why the next driver is cheap. Adding the
ramfb driver, the virtio-blk driver, and the second-iteration emmc2
driver each took less work than the first time emmc2 did the same
DMA ceremony by hand — because the second author inherits the
ceremony, audited and locked.

## Where to go next

The [pattern catalog](patterns/) is the technique reference for
the pure-side decisions the substrate composes — pure decisions,
pure state machines, plan/apply, pure data structures, plus the
[operation envelope](patterns/operation-envelope.md) for devices
with both a multi-step protocol AND a DMA path. Driver authors
should know they exist; the substrate uses all five underneath.

`CLAUDE.md` is the load-bearing summary of the regime. This chapter
is the why; that file is the rule.

The format references for register specs and wire DTOs —
[`../reference/regspec-format.md`](../reference/regspec-format.md) and
[`../reference/wirespec-format.md`](../reference/wirespec-format.md) — describe the TOML
shapes that generate the typed MMIO surfaces (`lockjaw_regs::*`,
`Sdhci`, `Pl011`, `FwCfg`) and the typed wire DTOs
(`lockjaw_types::wire::*`, `VirtioBlkReqHeader`, `Adma2Descriptor`,
`RamfbConfig`) the substrate hands to drivers. Together they close
the loop: no driver type-fields a register bit, no driver hand-packs
a wire struct, no driver remembers a sync. The bug shapes that used
to live in driver source live in generated code and in the
substrate; the substrate is host-tested and audited once.

If the substrate ever doesn't expose what a new driver needs, the
rule is the one quoted in `CLAUDE.md`: fix the substrate. Copying a
raw `sys_*` into driver source is debt the next driver author
inherits, and the construction half of the regime is what stops it
from being a free choice.
