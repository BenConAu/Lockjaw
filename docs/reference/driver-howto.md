# Writing a New Driver — Step by Step

This is the concrete walkthrough: from "no driver exists" to "the
new driver is in `make build` + `make test` + Pi flash". For *why*
the substrate is shaped this way (driver regime, DMA coherence
envelope, typed MMIO/wire), read
[`../architecture/04-driver-substrate.md`](../architecture/04-driver-substrate.md).
For the format references the regspec and wirespec steps below
consume, read [`regspec-format.md`](regspec-format.md) and
[`wirespec-format.md`](wirespec-format.md).

## Prerequisites

You need to be able to answer these about the device first:

- **What MMIO surface does it expose?** Read the datasheet. Decide
  whether the register set is generic enough to share (PL011 is —
  it's used by QEMU virt UART *and* Pi 4B) or device-family-specific.
- **Does it use DMA?** If yes, what's the completion signal (IRQ /
  poll / both), and what coherence direction(s) does each transfer
  have? Both questions feed into the DMA envelope, not into the
  driver body.
- **Does it raise IRQs?** Edge or level? On Pi 4B, most SPIs are
  level. On QEMU virt, edge.
- **What does the device-manager protocol need to know?** Most
  drivers claim their MMIO + IRQ + clock by hash through device-manager;
  see [`device-manager-protocol.md`](device-manager-protocol.md).

Pick a canonical sibling to copy from:

| Shape | Sibling driver | Why |
|---|---|---|
| Single MMIO + edge IRQ + event-loop | `user/pl011-driver/` | Smallest end-to-end exemplar. |
| VirtIO MMIO transport | `user/virtio-blk-driver/` | Probe/feature-negotiation + virtqueue. |
| No-IRQ "do its job once" | `user/ramfb-driver/` | `standard_init_no_irq` shape. |
| Level-triggered IRQ + DMA | `user/emmc2-driver/` | Tier-A `boot_stub!` + `LevelDriverCtx` + DMA envelope. |
| No-IRQ + cross-driver dependency | `user/cprman-driver/` | Tier-A with hash probe + claim. |

The rest of this doc walks the steps for the canonical edge-IRQ
shape (pl011-driver). Variants are called out inline.

## Step 1 — create the crate

Drivers live at `user/<name>-driver/`. The smallest layout (copied
from `user/pl011-driver/`):

```text
user/<name>-driver/
  Cargo.toml      # package + dependencies + release profile
  linker.ld       # the standard userspace link script
  build.rs        # standard build-script (links linker.ld, emits hash)
  src/main.rs     # the entire driver
```

`Cargo.toml` is `[workspace]`-isolated (drivers do NOT join the
root `Cargo.toml` members list — they're built per-driver):

```toml
[package]
name = "lockjaw-<name>-driver"
version = "0.1.0"
edition = "2021"

[workspace]

[dependencies]
lockjaw-userlib = { path = "../lockjaw-userlib" }
lockjaw-mmio    = { path = "../lockjaw-mmio" }   # if you hold MappedRegs<T>

# Note: lockjaw-regs is NOT a default driver dependency. Drivers
# consume generated register families through lockjaw_userlib::<family>
# re-exports (enforced by xtask check-driver-unsafe — see Step 6).
# Only add lockjaw-regs if your driver's family is not yet migrated
# to lockjaw-userlib (at time of writing: cprman, fw_cfg, virtio_mmio).
# A new driver should not need it — migrate the family to
# lockjaw-userlib first if it's missing.

[profile.release]
panic = "abort"
opt-level = 2
```

The root `Cargo.toml`'s `members = ["xtask", "lockjaw-types"]` list
does NOT include drivers — each driver is its own no_std build.

## Step 2 — register set (if new)

If your device's register layout doesn't already exist in
`user/lockjaw-regs/src/`, write a regspec TOML at
`user/regspecs/<device>.toml` and generate:

```bash
cargo xtask gen-regs   # writes user/lockjaw-regs/src/<device>.rs
```

The TOML schema is in [`regspec-format.md`](regspec-format.md);
look at `user/regspecs/pl011.toml` for a worked example. Drift
between the regspec source and the generated Rust is detected by
`cargo xtask gen-regs --check` (run inside `make build`).

If the layout exists already, import through the family's `lockjaw-userlib`
surface — drivers consume the safe types via `lockjaw_userlib::<family>`
and never name `lockjaw_regs` directly (the xtask `check-driver-unsafe`
regime enforces this; see Step 6):

```rust
use lockjaw_userlib::pl011::{
    drain_rx_fifo, set_interrupt_masks, write_byte_deadline, Imsc, Pl011,
};
```

## Step 3 — wire DTOs (if new)

If your device exchanges DMA-shared structured data with hardware
(virtio descriptors, framebuffer config, request headers), write a
wirespec at `user/wirespecs/<family>.toml` and generate:

```bash
cargo xtask gen-wires  # writes lockjaw-types/src/wire/<family>.rs
```

Schema is in [`wirespec-format.md`](wirespec-format.md); the
canonical example is `user/wirespecs/virtio.toml` (split-virtqueue
descriptors + virtio-blk request header). Wire DTOs land in
`lockjaw-types::wire::*` because they're shared between the driver
and any consumer (e.g., posix-server's understanding of block
requests). `cargo xtask gen-wires --check` catches drift.

## Step 4 — pick the boot ceremony

Three doors, each shaped for a class of driver. All three converge
on the same `#![deny(unsafe_code)]`-driver-body property; the
difference is in IRQ handling and which init helpers are composed.

| Door | Source | When |
|---|---|---|
| `driver_main!` | `user/lockjaw-userlib/src/driver_runtime.rs:627` | Single device, edge IRQ, event-loop. **The default.** |
| `virtio_driver_main!` | `user/lockjaw-userlib/src/virtio.rs:425` | VirtIO transport (probe magic, feature negotiation, virtqueue setup). |
| `boot_stub!` (Tier-A escape valve) | `user/lockjaw-userlib/src/driver_runtime.rs:589` | No IRQ at all (use `standard_init_no_irq`), level-triggered IRQ (use `standard_driver_init_level` / `LevelDriverCtx`), or a per-crate failure log line the macro hardcodes. |

`driver_main!` invocation pattern (from pl011-driver tail):

```rust
driver_main! {
    name        = "pl011-driver",
    hash        = LOCKJAW_SOURCE_HASH,  // declared by the macro expansion
                                        // from the build-script's source_hash.rs
    probe_hash  = PL011_HASH,           // from lockjaw_userlib (re-exported
                                        // from lockjaw_types::device:23)
    layout      = Pl011,                // your typed register set
    main        = pl011_main,           // your user-fn taking DriverCtx<Pl011>
}
```

The macro expands into the `_start` entry, the boot-stub
`#[allow(unsafe_code)]` site, and a call to
`standard_driver_init::<Pl011>` that hands your `pl011_main` a
`DriverCtx<Pl011>` with mapped regs + bound IRQ notification +
server endpoint. The single audited per-item allow lives inside
the macro expansion; your driver source stays unsafe-free.

## Step 5 — implement the engine trait

The driver body is an `EventEngine` (`BlockEngine` /
`DisplayEngine` for storage / display) impl. The framework owns
the receive/dispatch/reply loop, IRQ-threshold bookkeeping, and
the `sys_wait_any` substrate; you write the device-specific
behavior on `on_ipc` and `on_irq`. From pl011-driver:

```rust
struct Pl011Engine {
    regs: MappedRegs<Pl011>,
}

impl Pl011Engine {
    // Field-access helper. The MappedRegs<T> wrapper hides the raw
    // pointer; `.regs()` projects to the typed `&Pl011`.
    fn regs(&self) -> &Pl011 { self.regs.regs() }
}

impl EventEngine for Pl011Engine {
    fn on_ipc(&mut self, msg: u64) -> u64 {
        pl011_putc(self.regs(), msg as u8);
        0
    }

    fn on_irq(&mut self) {
        // Framework `drain_rx_fifo` owns the FIFO-empty check; the
        // closure decides what to do with each byte.
        let regs = self.regs();
        drain_rx_fifo(regs, |ch| pl011_putc(regs, ch));
    }
}
```

Your `pl011_main(ctx: DriverCtx<Pl011>)` constructs the engine
(`Pl011Engine { regs: ctx.regs }`), then calls
`run_event_server(&mut engine, ctx.server_ep, ctx.irq_notif, ctx.irq_initial_threshold)`
(see `user/lockjaw-userlib/src/driver_runtime.rs:733` — the first
arg is `&mut E`, not by-value). The function never returns.

DMA-using drivers additionally drive the coherence envelope via
`run_dma_transfer` in `user/lockjaw-userlib/src/dma_transfer.rs` —
see `user/emmc2-driver/src/main.rs:1838` for a worked two-envelope
example, and chapter 04 of the architecture book for the rationale.

## Step 6 — register with the regime gate

Add your crate path to the `DRIVER_CRATES` allowlist in
`xtask/src/check_driver_unsafe.rs:65`:

```rust
const DRIVER_CRATES: &[&str] = &[
    "user/cprman-driver",
    "user/emmc2-driver",
    "user/ramfb-driver",
    "user/pl011-driver",
    "user/virtio-blk-driver",
    "user/<name>-driver",   // <-- add here, sorted
];
```

Once added, `cargo xtask check-driver-unsafe` (run inside
`make build`) will gate every commit that touches your driver
against the four regime rules:

1. `#![deny(unsafe_code)]` at the crate root.
2. Zero `#[allow(unsafe_code)]` attributes.
3. No raw `syscall::*` path beyond the allowlist (`sys_exit`,
   `sys_debug_puts`).
4. No reference to a `(crate, module)` pair in
   `BANNED_DRIVER_MODULE_PATHS` (currently all five driver-imported
   families: `lockjaw_regs::sdhci`, `lockjaw_regs::pl011`,
   `lockjaw_regs::cprman`, `lockjaw_regs::fw_cfg`,
   `lockjaw_regs::virtio_mmio`) via any path/use/macro/alias/raw-ident
   shape — drivers consume those module families through the
   `lockjaw-userlib` re-exports instead. See
   [`../architecture/patterns/operation-envelope.md`](../architecture/patterns/operation-envelope.md)
   for the structural rationale.

The check uses `syn` AST + `visit_macro` + raw-ident
normalization — see chapter 04 for why each layer is load-bearing.

## Step 7 — wire into init + the build

Drivers don't self-start. `user/init/src/main.rs` is the system
launcher; it spawns each driver via `spawn_elf` (`init/src/main.rs:122`).
The real signature takes the ELF as `&[u8]` (NOT a path string) —
you `include_bytes!` the driver binary at the top of init:

```rust
static PL011_DRIVER_ELF: &[u8] = include_bytes!(
    "../../pl011-driver/target/aarch64-unknown-none/release/lockjaw-pl011-driver"
);
// ... then in init's main:
spawn_elf(
    PL011_DRIVER_ELF,
    "pl011-driver",
    map_array_va,       // pre-mapped scratch for the ProcessMapping array
    temp_base_va,       // temp VA range init uses while copying segments
    plan_buf_va,        // proc-page scratch
    scratch_ps,         // PageSetHandle for the scratch PageSet
    handle_to_copy,     // bootstrap endpoint the child needs (parent copy)
    stack_pages,        // child's stack size in pages
);
```

Look at how an existing driver is spawned for the wire-up of each
arg — the bootstrap endpoint pattern in particular is per-service.

The Makefile builds every driver in `user/*-driver/` automatically;
you don't need to edit it. The driver binary lands as
`user/<name>-driver/target/aarch64-unknown-none/release/<name>-driver`
and init's `include_bytes!` pulls it in at build time.

For QEMU integration tests, look at how an existing driver is
exercised in `tests/qemu_integration.sh` — typically the test asserts
"PASS: <something>" on the second UART, and your driver prints that
on a known input.

## Step 8 — verify

```bash
make test       # 104/104 + your new asserts
make build      # gen-regs --check, gen-wires --check,
                # check-driver-unsafe (your crate in the list),
                # check-linker-symbols, check-vtables,
                # check-kernel-no-neon
```

For Pi 4B drivers (anything touching `bcm2711`-specific MMIO):
flash and confirm your driver's success line appears alongside the
existing gates (`[BLOCKDEV] /dev/sd0 ready ... selftest read OK`,
`[FAT32-TEST] read 17 bytes`, `posix-hello: hello from fat32`,
`[NEON-CANARY] PASS`).

## The escape-valve recipe (Tier-A)

If the standard `driver_main!` / `virtio_driver_main!` shape
doesn't fit (no IRQ, level IRQ, or a custom probe), the pattern is
`boot_stub!` + invoke ONE of the standard Tier-B helpers from your
entry function. The helper does the bootstrap/probe/claim/bind
sequence internally — you don't compose those primitives by hand.

```rust
boot_stub! {
    hash = LOCKJAW_SOURCE_HASH,  // only these two fields exist on
    main = my_main,              // boot_stub!; no `name` field.
}

fn my_main() -> ! {
    // Pick exactly one Tier-B helper for your IRQ shape:
    //   standard_driver_init        — edge IRQ (driver_main! sugar)
    //   standard_init_no_irq        — no IRQ at all (ramfb, cprman)
    //   standard_driver_init_level  — level-triggered IRQ (emmc2)
    let ctx = standard_driver_init_level::<MyLayout>(
        "<name>", MY_DEVICE_HASH,
    ).expect("init failed");
    // ... your event/poll loop, e.g. run_event_server(&mut engine, ...) ...
}
```

Existing escape-valve users to copy from:
`user/ramfb-driver/src/main.rs` (no-IRQ via `standard_init_no_irq`),
`user/cprman-driver/src/main.rs` (no-IRQ + cross-driver dep),
`user/emmc2-driver/src/main.rs:445` (level-IRQ via
`standard_driver_init_level`).

## What goes WRONG most often

- **Driver source picks up a raw `sys_*`.** `check-driver-unsafe`
  fails immediately. The fix is in the framework — add the wrapper
  to `lockjaw-userlib` that exposes the capability properly. If
  what you need genuinely isn't in the framework, that's a tech-debt
  entry, not a one-off bypass in driver source.
- **DMA buffer aliasing.** Two paths hold `&mut` to the same DMA
  region across a `run_dma_transfer`. Compile error: the envelope
  takes the region by value through `DmaCell` borrowing rules.
  Refactor so each region has one owning path.
- **Forgot to add to the xtask allowlist.** `check-driver-unsafe`
  reports "fewer driver crates than expected" or silently skips
  yours. Always re-run `cargo xtask check-driver-unsafe` after
  step 6.
- **MMIO probe fails on Pi but passes on QEMU.** Usually a
  coherent-bus assumption — see the memory note on the DMA
  coherence review trap. Validate on Pi early, not after everything
  else passes.
