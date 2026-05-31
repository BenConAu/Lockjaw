# Operation Envelope

A typestate-gated operation layer that owns the safe pre/kick/await
sequence for an external device, holding a generic ordering envelope
(e.g. `run_dma_transfer` for cache coherence) inside. The driver
expresses *intent* (issue this command, set this state); the framework
owns every register access, every status poll, every IRQ wait, every
coherence operation.

## The shape

Three composable pieces:

1. **A pure typestate** in `lockjaw-types/src/<device>/{response,
   card_state, operation}.rs` — ZST state markers (e.g. `OpIdle`,
   `Idle`/`Ready`/`Stby`/`Tran`), sealed marker traits, typed
   response shapes (`R1`/`R2`/`R6`/...), and value-level enums that
   mirror the markers for host-tested reference models. No I/O.
2. **A capability-gated register layer** in `lockjaw-regs/` — the
   codegen emits `&<Device>OpToken<'_>` parameters on every accessor
   the controller samples during operations. The token's only mint
   path lives behind a name the `check-driver-unsafe` xtask
   blacklists in driver source, so driver code physically cannot
   call those accessors. Structural enforcement, not visibility.
3. **An envelope layer** in `lockjaw-userlib/src/<device>.rs` — owns
   the only legal mint of the capability token; wraps the
   register-program → kick → await → status-cleanup sequence behind
   typed operation methods (`issue_no_data::<R>`, `issue_data_transfer`,
   etc.); holds the generic coherence envelope (`run_dma_transfer`)
   for transfers that touch DMA. Returns family-generic error
   variants the driver maps to its own error type. The outer layer
   (`MmcCard<S>` for SDHCI cards) drives the device-state machine on
   top of the per-operation envelope.

The driver `use`s only `lockjaw-userlib::<device>::*` re-exports —
never `lockjaw_regs::<device>::*` directly. The xtask backstop
enforces this at build time.

## Why

This pattern closes the **R3** ("the sanctioned transfer path is the
only path") gap from the DMA coherence rubric — the property that no
code path can kick the device outside the framework's coherence
envelope. Pre-operation-envelope, drivers held raw DMA PAs and called
register setters directly; an inattentive refactor could fire a
command without the surrounding clean→kick→await→invalidate sequence,
silently corrupting transfers under cache coherence. The operation
envelope makes the wrong-shape transfer **unrepresentable from driver
source**:

- The driver cannot mint the capability token (xtask ban on the mint
  symbol).
- The framework's envelope method is the only path that mints the
  token, programs the controller, and runs the coherence envelope —
  so any device kick is necessarily wrapped.
- The outer typestate (`MmcCard<S>`) makes "read before select" a
  compile error instead of a runtime mistake.

See [`../01-architecture.md`](../01-architecture.md) §"push / pull /
plan-apply" for the broader rubric. See `docs/tracking/tech-debt.md`
(historical entry, dropped in O7) for the R3 framing that motivated
the SDHCI consumer.

## Variants

**Empty today.** The pattern has one current consumer: SDHCI
(`lockjaw-userlib::sdhci`), which serves `user/emmc2-driver/`. Per
Lockjaw's rule-of-two for abstractions, this doc is bootstrapped
from a single consumer — the missing pattern doc itself was
identified as an AI-native substrate gap during the SdhciCommandInit
design, and writing it before a second consumer arrives is a
deliberate exception to rule-of-two. **The second consumer (emmc1, a
NIC, or a VirtIO transport that needs the same shape) triggers
extraction**: at rule-of-two the family-specific bits in
`lockjaw-userlib::sdhci` get factored into a generic substrate, and
the second consumer's driver/framework pair ports onto it. Until
that happens this section stays empty.

## Canonical example

The SDHCI operation envelope for the BCM2711 emmc2 SD/MMC controller.
File:line refs below pin each piece to its current location;
`git log --follow` traces moves across the O1–O7 history.

**Pure typestate** (`lockjaw-types/src/sdhci/`):

- `response.rs` — `ResponseShape` sealed trait + `R0`/`R1`/`R1b`/`R2`/
  `R3`/`R6`/`R7` typed responses. Each carries `FLAGS: u8` (the
  COMMAND-register flag bits), `READS_LONG_RESPONSE: bool` (gates
  read of `RESPONSE_1..3`), and a pure `decode([u32; 4]) -> Self::Decoded`.
- `card_state.rs` — six state markers (`Uninit`/`Idle`/`Ready`/`Ident`/
  `Stby`/`Tran`) used as the `S` phantom of `MmcCard<'a, S>`. Sealed
  `CardLifecycleState` trait with `VARIANT: CardState` const tying
  each marker to its value-level analog.
- `operation.rs` — four op-state markers (`OpIdle`/`OpArmed`/
  `OpKicked`/`OpCompleted`) used as the `S` phantom of
  `SdhciCommandInit<'a, S>`. Pure linear cycle, host-tested.

**Capability gate** (`user/lockjaw-regs/src/sdhci.rs`, generated):

- `SdhciOpToken<'a>` at `user/lockjaw-regs/src/sdhci.rs:804` —
  `pub struct` with private `_no_ctor: ()` field and
  `PhantomData<&'a Sdhci>` lifetime tying the token to the
  controller borrow.
- `__sdhci_internal_mint(&Sdhci) -> SdhciOpToken<'_>` at
  `user/lockjaw-regs/src/sdhci.rs:814` — `pub` mint function. The
  `check-driver-unsafe` xtask (`xtask/src/check_driver_unsafe.rs`,
  `BANNED_DRIVER_MODULE_PATHS` constant) denies any
  `lockjaw_regs::sdhci` segment-pair in `user/*-driver/` source via
  use trees, path-qualified calls, `extern crate`, raw-ident, and
  macro bodies; the bare-alias `use lockjaw_regs as x;` is also
  blocked at the use site so the alias-bypass shape cannot reach
  the banned module under a renamed crate. The mint is physically
  unreachable from driver source.
- 19 register accessors in the same generated file take
  `&SdhciOpToken<'_>` per the regspec's property-driven rule ("any
  register the controller reads or writes during a command/data
  operation, or whose state controls behavior such that concurrent
  mutation is unsafe"). Aliased registers (`sysaddr`/`argument2`)
  inherit the gate.

**Envelope** (`user/lockjaw-userlib/src/sdhci.rs`):

- `SdhciCommandInit<'a, S: OpState>` at `:112` — per-operation
  envelope. `open(&'a Sdhci)` at `:123` mints the internal
  `SdhciOpToken<'a>`. Two issue methods on the `OpIdle` impl:
  - `issue_no_data::<R: ResponseShape>(cmd, arg)` at `:158` —
    ID-phase polling command. Wraps inhibit poll → ARGUMENT →
    single-store TRANSFER_MODE+COMMAND → status poll → typed
    response decode.
  - `issue_data_transfer(params, regions, completion)` at `:692` —
    data-phase DMA command. Wraps inhibit poll +
    `run_dma_transfer`'s clean→kick→await→invalidate envelope; the
    kick programs the full ADMA2 register sequence; the completion
    is `SdhciDataCompletion`'s IRQ wait + B4.1 DAT_INHIBIT drain.
- `SdhciDataCompletion<'a>` (defined in the same file, family-generic
  `DmaCompletion` impl) — the SDHCI IRQ-driven data-phase done signal.
- `MmcCard<'a, S: CardLifecycleState>` at `:880` — outer card-state
  typestate. Linear chain `uninit → go_idle → verify_sdv2_if_cond
  → power_up_to_ready → identify → publish_rca → select →
  set_bus_width_4bit → into_parts → CardInfo`. The `select()` impl
  at `:1031` folds CMD9 + CMD7 so reaching `Tran` guarantees `csd`
  is captured (structural proof for `into_parts()`'s `expect`).
  `CardInfo` at `:844` has a `pub(crate)` constructor at `:855`;
  only `MmcCard::<Tran>::into_parts()` at `:1102` mints it.
  `Emmc2BlockEngine::new` (`user/emmc2-driver/src/main.rs`) requires
  `CardInfo` as the compile-time proof token.
- Init-time free function helpers: `soft_reset_all` (`:277`),
  `configure_clock` (`:303`), `set_power_on_and_settle`,
  `set_int_enable_masks`, `set_timeout_dat_counter`,
  `set_bus_width_4bit` (`:391`), `init_adma2_32` (`:420`),
  `enable_irq_signaling` (`:443`). Each mints the token internally
  and uses it within the function body.

**Driver consumer** (`user/emmc2-driver/src/main.rs`):

- The Cargo.toml has no `lockjaw-regs` dependency. The driver
  `use`s the lockjaw-userlib re-export instead:
  `use lockjaw_userlib::sdhci::{ErrorIntSignalEnable,
  ErrorIntStatusEnable, NormalIntSignalEnable,
  NormalIntStatusEnable, PowerControlBusVoltage, Sdhci};` (at
  `:36`) plus a second use line for the operation surface
  (`SdhciCommandInit`, `SdhciInitError`, the typestate types) at
  `:64`. The `check-driver-unsafe` ban (`xtask/src/check_driver_unsafe.rs`)
  ensures `lockjaw_regs` cannot be named directly.
- The ID-phase boot sequence is a typestate chain
  (`card.go_idle()?.power_up_to_ready(arg)? ... .select()?
  .set_bus_width_4bit()?.into_parts()`). Out-of-order calls are
  compile errors. See `emmc2_entry` for the full sequence with the
  per-step log+sys_exit pattern.
- The block-engine data read (`adma2_single_block_read`) is one
  method call: `SdhciCommandInit::open(sdhci).issue_data_transfer(
  params, &[desc_region], SdhciDataCompletion::new(sdhci, irq))`.
  The driver builds the descriptor in coherent memory, supplies
  the params and completion, and hands off — the framework does
  the rest.
- Driver-side error mapping translates family-generic
  `SdhciCommandError` / `SdhciDataTransferError` /
  `SdhciDataCompletionError` / `MmcCardError` into driver-specific
  `Emmc2Error` for logging (`put_emmc2_error`, `put_mmc_card_error`).

## Anti-patterns

The shape exists to prevent these:

- **Holding a raw DMA PA.** Pre-envelope: driver kept `buf.backing.pa`
  in scope and wrote it to `ADMA_ADDRESS` directly. Today the
  descriptor PA only flows in through `SdhciDataTransfer.adma_descriptor_pa`,
  and `ADMA_ADDRESS` is in the gated-setter list — the only writer
  is the framework's kick closure.
- **Firing a command outside the coherence envelope.** Pre-envelope:
  driver could call `sdhci.set_transfer_mode_command(...)` between
  cache-sync calls (or omit the syncs entirely). Today
  `set_transfer_mode_command` is gated and only the framework's
  `issue_no_data` / `issue_data_transfer` callers reach it; both wrap
  the controller kick in the appropriate envelope (`run_dma_transfer`
  for data-phase).
- **Reading the wrong response shape.** Pre-typestate: driver called
  `sdhci.read_response_0()` after every command and decoded into
  whatever struct seemed right (or just used `u32`). Today
  `issue_no_data::<R>` returns `R::Decoded` — picking `R3` (OCR) for a
  CMD that returns R2 (long) is a compile error.
- **Calling card-state-changing commands out of order.** Pre-typestate:
  `engine.read()` could be called immediately after `CMD0` if a
  contributor skipped the rest of the chain (the driver would crash
  on the first IRQ wait, but not at compile time). Today
  `Emmc2BlockEngine::new(regs, info, irq)` requires `info: CardInfo`,
  whose only mint is `MmcCard::<Tran>::into_parts()`, whose only
  construction path is through the full linear chain. Out-of-order
  calls compile-fail.
- **Latent dependency on field initialization.** Pre-`select`-folds-CMD9:
  `MmcCard<Stby>::select()` could be called before `read_csd()`,
  leaving `csd: None` on the eventual `MmcCard<Tran>` and panicking
  at `into_parts()`. Today `select()` runs CMD9 + CMD7 internally;
  every `MmcCard<Tran>` was constructed via this method so `csd` is
  always `Some`. The `.expect()` in `into_parts()` is structurally
  safe.

## Recognizing push-shaped code that wants this pattern

Look for:

- **A driver that holds a raw device address** (DMA PA, MMIO offset)
  in scope across multiple lines, then writes it into a controller
  register at the bottom of the function. The raw value is a
  capability the operation envelope should mint and hold.
- **A driver that orchestrates a multi-step ID/setup sequence** where
  each step depends on the last, and the order is enforced by comments
  rather than the type system. Typestate candidate.
- **A driver that wraps `run_dma_transfer` with hand-rolled register
  programming inside the kick closure**, where the closure body
  could plausibly be wrong (missing a write, wrong order) without a
  compile error. The closure body is operation envelope work.
- **A driver crate that imports `lockjaw_regs::<device>::*` directly**
  alongside `lockjaw_userlib::*`. The lockjaw_regs import is the
  signal that the device family doesn't have an operation envelope
  yet — every typed register call site is a potential R3 bypass.

For non-command devices (clocks, simple framebuffers, UARTs without
data protocols), the pattern is overkill — there's no state machine
to gate, no DMA envelope to compose. The shape pays off when the
device has a multi-step protocol AND a DMA path. SDHCI is the
canonical fit; VirtIO transports are the obvious next.
