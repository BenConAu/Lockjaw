//! Composable boot + claim + IRQ runtime for user-mode drivers.
//!
//! Three tiers, each independently usable:
//!
//! - **Tier A** — primitive helpers:
//!   - `boot_stub!` macro generates the `LOCKJAW_HASH_SECTION` static
//!     and `#[no_mangle] _start` so the macro is the only place the
//!     `#[allow(unsafe_code)]` attributes live; the driver crate
//!     itself can run `#![forbid(unsafe_code)]`.
//!   - `driver_bootstrap() -> Result<DriverBootstrap, _>` performs
//!     the one-shot bootstrap IPC.
//!   - `probe_by_hash(boot, hash) -> Result<MmioProbe, _>` walks
//!     device-manager's probe protocol to find a matching device.
//!   - `bind_irq(intid, flags) -> Result<NotificationHandle, _>`
//!     allocates a notification and binds the IRQ.
//!
//! - **Tier B** — composed common shape: `standard_driver_init<T>(hash)
//!   -> Result<DriverCtx<T>, _>` for the 90% case (single MMIO +
//!   single IRQ + server endpoint).
//!
//! - **Tier C** — `driver_main!` macro: sugar over `boot_stub!` +
//!   `standard_driver_init`. The driver crate body becomes one
//!   invocation + one `fn driver_main(ctx) -> !`.
//!
//! Drivers that don't fit the standard shape (multi-MMIO, clock-first,
//! no-IRQ, non-server) compose Tier-A pieces themselves while still
//! consuming `boot_stub!` for the single audited `unsafe`-bearing
//! attribute. See `examples/escape_valve_cprman.rs` for the documented
//! pattern. ALL escape valves keep zero `#[allow(unsafe_code)]` in
//! driver source.

use crate::devmgr::{claim_typed, ClaimError};
use crate::handle::{
    bootstrap_endpoint, EndpointHandle, NotificationGuard, NotificationHandle,
    PageSetGuard, PageSetHandle, ReplyHandle,
};
use crate::print::{puts, puts2};
use crate::syscall::{
    sys_alloc_pages, sys_bind_irq_flags, sys_call_ret4, sys_create_notification,
    sys_create_reply, sys_exit, sys_receive, sys_reply, sys_wait_any, IRQ_FLAG_EDGE,
};
use lockjaw_mmio::region::MappedRegs;
use lockjaw_types::device::{CMD_PROBE_DEVICE, PROBE_END, PROBE_OK};
use lockjaw_types::wait::WaitEntry;

// ---------------------------------------------------------------------------
// Tier A — primitive helpers.
// ---------------------------------------------------------------------------

/// What `driver_bootstrap()` returns: the channels every driver needs
/// out of the one-shot bootstrap IPC.
pub struct DriverBootstrap {
    /// Endpoint to the device manager.
    pub devmgr_ep: EndpointHandle,
    /// Reply object for outbound IPC.
    pub reply_obj: ReplyHandle,
    /// Server endpoint the driver receives requests on. `None` if
    /// init did not give the driver a server endpoint (e.g. cprman's
    /// one-shot init shape — driver registers a clock then idles).
    ///
    /// Wire-protocol note: `init` returns the server endpoint in
    /// reply slot 0; a zero handle in that slot means "no server".
    /// No kernel-side change is required to surface this.
    pub server_ep: Option<EndpointHandle>,
}

/// Errors `driver_bootstrap()` can produce.
#[derive(Debug, Clone, Copy)]
pub enum BootstrapError {
    /// Could not allocate the page backing the reply object.
    AllocFailed,
    /// `sys_create_reply` failed.
    ReplyFailed,
    /// The bootstrap IPC itself failed (init endpoint unreachable).
    IpcFailed,
}

/// Acquire the conventional driver bootstrap state. Runs once at
/// `_start`; returns the channels every driver needs.
///
/// Resource lifetime: the reply object is backed by a freshly
/// allocated page. Between `sys_alloc_pages` and `sys_create_reply`
/// the bare PageSetHandle is wrapped in a `PageSetGuard` so a
/// `sys_create_reply` failure closes the pageset slot instead of
/// leaking it. On the success path, `sys_create_reply` consumes
/// the pageset slot (converting it to a reply slot in the
/// process's handle table) and we disarm the guard with `take()`.
pub fn driver_bootstrap() -> Result<DriverBootstrap, BootstrapError> {
    let ps_guard = PageSetGuard::new(
        sys_alloc_pages(1).map_err(|_| BootstrapError::AllocFailed)?
    );
    let reply_obj =
        sys_create_reply(ps_guard.handle()).map_err(|_| BootstrapError::ReplyFailed)?;
    // create_reply succeeded — the slot is now a reply slot, NOT a
    // pageset. Disarm the guard so we don't try to close the slot
    // as a pageset on drop.
    let _ = ps_guard.take();
    let reply = sys_call_ret4(bootstrap_endpoint(), reply_obj, 0, 0, 0, 0)
        .map_err(|_| BootstrapError::IpcFailed)?;
    let server_ep = if reply[0] == 0 { None } else { Some(EndpointHandle(reply[0])) };
    let devmgr_ep = EndpointHandle(reply[1]);
    Ok(DriverBootstrap { devmgr_ep, reply_obj, server_ep })
}

/// Result of a successful `probe_by_hash`.
#[derive(Debug, Clone, Copy)]
pub struct MmioProbe {
    /// Physical base address of the matched device's MMIO region.
    pub mmio_addr: u64,
    /// Allocated IRQ INTID for the device.
    pub intid: u32,
}

/// Errors `probe_by_hash` can produce.
#[derive(Debug, Clone, Copy)]
pub enum ProbeError {
    /// IPC to the device manager failed.
    IpcFailed,
    /// Walked the entire device list with no match at `skip`.
    NotFound,
}

/// Probe the device manager for the `skip`-th unclaimed device
/// matching `compatible_hash`. Returns DTB-derived identity only —
/// no MMIO peek. Per-family discriminators (e.g. virtio's DeviceID)
/// are the responsibility of family helpers in lockjaw-userlib (see
/// `virtio::probe_and_claim_virtio_device`).
///
/// Skipping protocol: probe returns `PROBE_OK` for unclaimed,
/// `PROBE_CLAIMED` for already-claimed (transparent skip), `PROBE_ERR`
/// for register/map failures (also skipped — devices that fail to
/// register are typically transient), `PROBE_END` when no more
/// matches at this skip index. The helper walks indices internally
/// and returns the first unclaimed match.
pub fn probe_by_hash(
    boot: &DriverBootstrap,
    compatible_hash: u64,
    initial_skip: u64,
) -> Result<MmioProbe, ProbeError> {
    let mut skip = initial_skip;
    loop {
        let probe = sys_call_ret4(
            boot.devmgr_ep,
            boot.reply_obj,
            CMD_PROBE_DEVICE,
            compatible_hash,
            skip,
            0,
        ).map_err(|_| ProbeError::IpcFailed)?;
        let status = probe[0];
        if status == PROBE_END {
            return Err(ProbeError::NotFound);
        }
        if status != PROBE_OK {
            skip += 1;
            continue;
        }
        return Ok(MmioProbe { mmio_addr: probe[1], intid: probe[2] as u32 });
    }
}

/// Errors `bind_irq` can produce.
#[derive(Debug, Clone, Copy)]
pub enum IrqBindError {
    /// Could not allocate the notification's backing page.
    AllocFailed,
    /// `sys_create_notification` failed.
    CreateFailed,
    /// `sys_bind_irq_flags` failed at the kernel boundary.
    BindFailed,
}

/// A freshly-bound IRQ notification — pair of the notification
/// handle and the initial wait-threshold the caller must use on the
/// first `sys_wait_notification` (or pass to `run_event_server`).
///
/// The kernel's notification counter starts at 0 and the first IRQ
/// delivery bumps it to 1; the first wait must therefore use
/// threshold = 1 (waiting for threshold = 0 returns immediately and
/// misses the first interrupt). Pairing the handle with its initial
/// threshold makes the contract type-level rather than doc-level.
pub struct BoundIrq {
    pub notif: NotificationHandle,
    pub initial_threshold: u64,
}

/// Create a notification handle and bind it to `intid` with `flags`
/// (default `IRQ_FLAG_EDGE` for level-triggered platforms). Returns
/// the bound notification AND the initial wait-threshold so the
/// caller cannot accidentally start at 0 and miss the first IRQ.
///
/// Resource lifetime: two intermediate handles are guarded.
/// 1. PageSetHandle between `sys_alloc_pages` and
///    `sys_create_notification` — failure of `create_notification`
///    must close the pageset.
/// 2. NotificationHandle between `sys_create_notification` and
///    `sys_bind_irq_flags` — failure of bind must close the unbound
///    notification.
pub fn bind_irq(intid: u32, flags: u64) -> Result<BoundIrq, IrqBindError> {
    let ps_guard = PageSetGuard::new(
        sys_alloc_pages(1).map_err(|_| IrqBindError::AllocFailed)?
    );
    let notif =
        sys_create_notification(ps_guard.handle())
            .map_err(|_| IrqBindError::CreateFailed)?;
    // create_notification consumed the pageset slot. Disarm the
    // pageset guard and arm a notification guard for the bind step.
    let _ = ps_guard.take();
    let notif_guard = NotificationGuard::new(notif);
    let err = sys_bind_irq_flags(intid as u64, notif, flags);
    if !err.is_ok() {
        // notif_guard drops here and closes the unbound notification.
        return Err(IrqBindError::BindFailed);
    }
    Ok(BoundIrq { notif: notif_guard.take(), initial_threshold: 1 })
}

// ---------------------------------------------------------------------------
// Tier B — DriverCtx + standard_driver_init.
// ---------------------------------------------------------------------------

/// Public driver-facing surface produced by `standard_driver_init`.
///
/// Exposes only what the 90%-case driver needs: typed registers, IRQ
/// notification, server endpoint, and the bootstrap channels for
/// drivers that want to do further claim/probe on top.
///
/// `ClaimedDevice` is `pub(crate)` in `devmgr`; drivers never name
/// it. Mapping ownership of the MMIO pageset stays internal —
/// destruction is process-lifetime.
pub struct DriverCtx<T: 'static> {
    /// Typed MMIO registers.
    pub regs: MappedRegs<T>,
    /// IRQ INTID for this device.
    pub irq_intid: u32,
    /// Notification handle bound to the IRQ.
    pub irq_notif: NotificationHandle,
    /// Initial wait-threshold for the IRQ notification — pass to
    /// `run_event_server` or to the first `sys_wait_notification`.
    /// Type-level contract (was a hard-coded `1` with an inline doc
    /// comment in driver code).
    pub irq_initial_threshold: u64,
    /// Server endpoint the driver receives requests on.
    pub server_ep: EndpointHandle,
    /// Device-manager endpoint, exposed so drivers can issue
    /// follow-on claims (multi-MMIO drivers, clock acquisition).
    pub devmgr_ep: EndpointHandle,
    /// Reply object for outbound IPC.
    pub reply_obj: ReplyHandle,
    /// MMIO pageset handle. Crate-private — drivers don't touch
    /// this. Phase 9 finalizes device-manager's release path; until
    /// then the field stays `pub(crate)` so accidental driver-side
    /// "convenience" reads don't lock in a dependency that would
    /// regress the surface. Currently unread inside the crate too —
    /// the `#[allow(dead_code)]` is intentional: removing the field
    /// would force a layout change when Phase 9 wires it up.
    #[allow(dead_code)]
    pub(crate) mmio_pageset: PageSetHandle,
}

/// Errors `standard_driver_init` can produce.
#[derive(Debug, Clone, Copy)]
pub enum DriverInitError {
    /// `driver_bootstrap` failed.
    Bootstrap(BootstrapError),
    /// `probe_by_hash` failed.
    Probe(ProbeError),
    /// `claim_typed` failed.
    Claim(ClaimError),
    /// `bind_irq` failed.
    IrqBind(IrqBindError),
    /// `init` returned no server endpoint, but the standard shape
    /// requires one. Drivers that don't have a server use the
    /// Tier-A escape valve.
    NoServerEndpoint,
}

/// Standard "boot → probe → claim → bind IRQ → return ctx" for
/// drivers with the conventional single-MMIO + single-IRQ + server
/// shape. Built from Tier-A pieces; no magic — drivers that need
/// different shapes can call the Tier-A helpers directly.
///
/// Emits branded phase-progress log lines (`<name>: bootstrapped`,
/// `<name>: IRQ bound`) so the integration-test harness can verify
/// init progressed past each step. Atomic per line via `puts2` so
/// concurrent driver output cannot interleave between the brand
/// prefix and the message.
pub fn standard_driver_init<T: 'static>(
    name: &str,
    compatible_hash: u64,
) -> Result<DriverCtx<T>, DriverInitError> {
    let boot = driver_bootstrap().map_err(DriverInitError::Bootstrap)?;
    puts2(name, ": bootstrapped\n");
    let server_ep = boot.server_ep.ok_or(DriverInitError::NoServerEndpoint)?;
    let probe = probe_by_hash(&boot, compatible_hash, 0)
        .map_err(DriverInitError::Probe)?;
    let claimed = claim_typed::<T>(boot.devmgr_ep, boot.reply_obj, probe.mmio_addr)
        .map_err(DriverInitError::Claim)?;
    let bound = bind_irq(claimed.irq_intid, IRQ_FLAG_EDGE)
        .map_err(DriverInitError::IrqBind)?;
    puts2(name, ": IRQ bound\n");
    Ok(DriverCtx {
        regs: claimed.regs,
        irq_intid: claimed.irq_intid,
        irq_notif: bound.notif,
        irq_initial_threshold: bound.initial_threshold,
        server_ep,
        devmgr_ep: boot.devmgr_ep,
        reply_obj: boot.reply_obj,
        mmio_pageset: claimed.mmio_pageset,
    })
}

// ---------------------------------------------------------------------------
// Tier C — driver_main! and its building blocks.
// ---------------------------------------------------------------------------

/// Print `msg` and exit the driver process. Used by the macro-emitted
/// boot-stub failure path so the driver crate doesn't need its own
/// halt fn.
pub fn boot_puts_and_halt(msg: &str) -> ! {
    puts(msg);
    sys_exit();
}

/// Generate the boot-entry stubs for a driver crate.
///
/// Emits:
/// - `LOCKJAW_SOURCE_HASH` const sourced from `OUT_DIR/source_hash.rs`
///   (the driver's `build.rs` is responsible for writing it).
/// - `LOCKJAW_HASH_SECTION` static placed in `.lockjaw_hash`.
/// - `#[no_mangle] _start` which calls the user-supplied `$main` fn.
///
/// The `#[allow(unsafe_code)]` attributes the boot stubs need live
/// inside this macro expansion. Driver crates run `#![forbid(unsafe_code)]`.
///
/// Macro-hygiene discipline: generated identifiers prefix
/// `__lockjaw_` so they cannot collide with driver-side names. The
/// macro must be invoked at most once per crate (duplicate
/// invocation produces a duplicate-`_start` link error, which is
/// the intended failure mode).
#[macro_export]
macro_rules! boot_stub {
    (hash = $hash:ident, main = $main:ident $(,)?) => {
        #[allow(unsafe_code)]
        const $hash: u64 = include!(concat!(env!("OUT_DIR"), "/source_hash.rs"));

        #[allow(unsafe_code)]
        #[used]
        #[link_section = ".lockjaw_hash"]
        static __LOCKJAW_HASH_SECTION: u64 = $hash;

        #[allow(unsafe_code)]
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            $main()
        }
    };
}

/// Sugar over `boot_stub!` + `standard_driver_init` for the common
/// driver shape.
///
/// Driver crate body becomes:
/// ```ignore
/// driver_main! {
///     name = "virtio-blk",
///     hash = MY_SOURCE_HASH,
///     probe_hash = lockjaw_types::device::VIRTIO_MMIO_HASH,
///     device_id_filter = Some(DEVICE_ID_BLOCK),
///     layout = lockjaw_regs::virtio_mmio::VirtioMmio,
///     main = virtio_blk_main,
/// }
/// fn virtio_blk_main(ctx: DriverCtx<VirtioMmio>) -> ! { ... }
/// ```
///
/// For drivers that don't fit (no-IRQ, multi-MMIO, clock-first):
/// invoke `boot_stub!` directly and compose Tier-A pieces inside
/// `_start`. See `examples/escape_valve_cprman.rs`.
#[macro_export]
macro_rules! driver_main {
    (
        name = $name:literal,
        hash = $hash:ident,
        probe_hash = $probe_hash:expr,
        layout = $layout:ty,
        main = $main:ident $(,)?
    ) => {
        $crate::boot_stub! {
            hash = $hash,
            main = __lockjaw_driver_entry,
        }
        fn __lockjaw_driver_entry() -> ! {
            $crate::puts(concat!($name, ": starting\n"));
            let ctx: $crate::driver_runtime::DriverCtx<$layout> =
                match $crate::driver_runtime::standard_driver_init::<$layout>(
                    $name,
                    $probe_hash,
                ) {
                    Ok(c) => c,
                    Err(_) => $crate::driver_runtime::boot_puts_and_halt(
                        concat!($name, ": init failed\n")
                    ),
                };
            $main(ctx)
        }
    };
}

// ---------------------------------------------------------------------------
// Event-loop server (the second canonical driver shape — server endpoint +
// IRQ notification multiplexed via sys_wait_any).
//
// First canonical shape: block-style request/response (`run_block_server`,
// virtio-blk, future ramfb / emmc2 / fat32 etc.).
// Second canonical shape: input device + IPC TX (uart-driver, future
// console / keyboard / serial / polled-but-async devices).
//
// The two named bit constants and the IRQ-threshold increment used to
// live in every event-loop driver as bare `mask & 1` / `mask & 2` magic
// numbers. Baking them into the framework means a third event-loop
// driver inherits the right shape automatically and cannot silently
// reorder the IPC and IRQ branches.
// ---------------------------------------------------------------------------

/// Canonical event-loop driver — implementations get an IPC callback
/// and an IRQ callback. The framework owns the `sys_wait_any` loop,
/// the IRQ-threshold bookkeeping, the IPC receive/reply syscalls, and
/// the bit-mask constants.
///
/// Drivers using this trait have ZERO raw `sys_*` calls in driver
/// source (except `sys_exit` and panic-handler diagnostics).
pub trait EventEngine {
    /// Called when an IPC message arrives on the server endpoint.
    /// `msg` is the first message word (matches `sys_receive`'s
    /// return shape). Return the reply word; the framework calls
    /// `sys_reply(reply_word, 0, 0, 0)` automatically. Drivers that
    /// don't need to reply with data return `0`.
    fn on_ipc(&mut self, msg: u64) -> u64;

    /// Called when the bound IRQ fires. Implementation drains FIFOs
    /// / advances state / clears device interrupt status — the
    /// framework owns IRQ-threshold bookkeeping.
    fn on_irq(&mut self);
}

/// Run the canonical event-loop server for a single-IPC + single-IRQ
/// driver. Diverging — calls `engine.on_ipc` and `engine.on_irq` as
/// events fire on `server_ep` / `irq_notif`.
///
/// `irq_initial_threshold` should come from `DriverCtx.irq_initial_threshold`
/// (or the `BoundIrq` returned by `bind_irq`); see `BoundIrq` for the
/// kernel-counter-vs-threshold contract.
pub fn run_event_server<E: EventEngine>(
    engine: &mut E,
    server_ep: EndpointHandle,
    irq_notif: NotificationHandle,
    irq_initial_threshold: u64,
) -> ! {
    const SERVER_BIT: u64 = 1 << 0;
    const IRQ_BIT: u64 = 1 << 1;
    let mut irq_threshold = irq_initial_threshold;
    let mut entries = [
        WaitEntry { handle: server_ep.0, threshold: 0 },
        WaitEntry { handle: irq_notif.0, threshold: irq_threshold },
    ];
    loop {
        let mask = match sys_wait_any(&entries) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if mask & SERVER_BIT != 0 {
            let reply = if let Ok(msg) = sys_receive(server_ep) {
                engine.on_ipc(msg)
            } else {
                // Receive failed: still reply to unblock the caller
                // (matches the pre-Phase-5 ad-hoc loop behaviour).
                0
            };
            sys_reply(reply, 0, 0, 0);
        }
        if mask & IRQ_BIT != 0 {
            engine.on_irq();
            irq_threshold += 1;
            entries[1].threshold = irq_threshold;
        }
    }
}
