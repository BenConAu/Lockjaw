# Exception Handling

Every trap into the kernel — syscall, IRQ, page fault, undefined
instruction, async error — lands in the AArch64 exception vector
table at `VBAR_EL1`. This doc covers the table layout, the
handlers each vector slot routes to, the GKL discipline, and how
`ESR_EL1` is decoded to classify a synchronous exception.

For the SVC-specific path (the syscall surface), see
[`syscalls.md`](syscalls.md). For the IPC-specific shape of what
SVC dispatches to, see [`ipc.md`](ipc.md).

## The vector table

AArch64's exception vector is 16 slots arranged as **4 groups of 4**:

```text
+--------------------------------------+
| Offset | Group           | Vector    |
+--------+-----------------+-----------+
| 0x000  | Current EL,     | Sync      |
| 0x080  | SP_EL0          | IRQ       |
| 0x100  |                 | FIQ       |
| 0x180  |                 | SError    |
+--------+-----------------+-----------+
| 0x200  | Current EL,     | Sync      |  <-- kernel fault
| 0x280  | SP_ELx          | IRQ       |  <-- timer/IRQ while in kernel
| 0x300  |                 | FIQ       |
| 0x380  |                 | SError    |
+--------+-----------------+-----------+
| 0x400  | Lower EL,       | Sync      |  <-- syscalls / user faults
| 0x480  | AArch64         | IRQ       |  <-- timer/IRQ while in userspace
| 0x500  |                 | FIQ       |
| 0x580  |                 | SError    |
+--------+-----------------+-----------+
| 0x600  | Lower EL,       | Sync      |
| 0x680  | AArch32         | IRQ       |  (never reached; kernel is AArch64-only)
| 0x700  |                 | FIQ       |
| 0x780  |                 | SError    |
+--------+-----------------+-----------+
```

The table is 2 KiB-aligned (a hardware requirement of `VBAR_EL1`),
defined in `src/arch/aarch64/exceptions.rs:303+`, and installed at
boot via `exceptions::init` (`:390`) which writes `VBAR_EL1` and
issues `isb`.

Each slot is a `VECTOR_STUB` (asm macro) that saves the user/kernel
GPRs into an `ExceptionContext` frame on the kernel stack, then
`bl`s the named handler. The four named entry asm symbols are:

| Symbol | Used by slots |
|---|---|
| `__vec_sync`       | 0x000, 0x200, 0x600 (kernel/AArch32) |
| `__vec_sync_lower` | 0x400 (userspace AArch64) |
| `__vec_irq`        | 0x080, 0x280, 0x480, 0x680 |
| `__vec_fiq`        | 0x100, 0x300, 0x500, 0x700 |
| `__vec_serror`     | 0x180, 0x380, 0x580, 0x780 |

`__vec_sync` vs `__vec_sync_lower` is the load-bearing split: a
synchronous trap from kernel mode is a fatal bug (the kernel
faulted on itself), while a synchronous trap from userspace is the
normal syscall path.

## The handlers

All in `src/arch/aarch64/exceptions.rs`. The asm stubs hand each
handler a `&ExceptionContext` (mutable for the syscall path so the
return values can be written back).

| Handler | Line | What it does |
|---|---|---|
| `handle_exception_sync(ctx)` | `:154` | Kernel synchronous fault. Print fault + halt in `wfi` loop. Kernel cannot recover from faulting on itself. |
| `handle_exception_sync_lower(ctx)` | `:162` | Userspace synchronous trap. **Takes GKL**, calls `classify_sync_exception(esr)`. Syscall → `syscall::handler::handle_syscall(ctx)`. UserFault → print fault + halt (with GKL still held; kernel is dead). Releases GKL before eret. |
| `handle_exception_irq(ctx)` | `:182` | IRQ from any group. **Takes GKL**, calls `arch::aarch64::irq_dispatch()`. Releases GKL before eret. |
| `handle_exception_fiq(ctx)` | `:190` | FIQ. Lockjaw doesn't use FIQ for anything today — any FIQ is treated as a fatal fault and halts. |
| `handle_exception_serror(ctx)` | `:196` | Async aborts (uncorrectable memory errors, etc.). Print + halt. |

## The GKL discipline

The userspace-sync and IRQ handlers acquire the Giant Kernel Lock
on entry and release it before the `eret` back to EL0. The kernel
runs all its work with the GKL held; this is the regime documented
in [`threads.md`](threads.md). The fault handlers (`__vec_sync`,
`__vec_fiq`, `__vec_serror`) don't take the GKL because they
don't return — they halt in `wfi`.

The kernel-side rule is **never acquire the GKL twice**. A timer
tick while the GKL is held re-enters `handle_exception_irq`, which
would deadlock on the lock. The protection is that the GKL is held
*with IRQs masked* — the timer can't fire while another kernel
path holds the lock.

## The ExceptionContext frame

`lockjaw-types/src/exception.rs:28`:

```rust
#[repr(C)]
pub struct ExceptionContext {
    pub gpr:       [u64; 31],   // x0-x30
    pub elr:       u64,         // ELR_EL1 — PC to return to
    pub spsr:      u64,         // SPSR_EL1 — PSTATE at exception
    pub esr:       u64,         // ESR_EL1 — exception cause
    pub sp_el0:    u64,         // user SP (saved so syscalls/IRQs don't lose it)
    pub tpidr_el0: u64,         // user TLS pointer (musl __init_tls writes this)
}
```

The asm `SAVE_REGS` macro pushes this whole frame onto the kernel
stack (`EXCEPTION_FRAME_SIZE` bytes — `lockjaw-types/src/exception.rs:49`);
`RESTORE_REGS` pops it before `eret`. The layout is the ABI between
the asm stubs and the Rust handlers — `OFF_ELR`/`OFF_SPSR`/`OFF_ESR`
(`:55+`) are exported so the asm can write back through the
struct field offsets without hardcoding.

For syscalls, the handler indexes the frame: `ctx.gpr[8]` is the
syscall number, `ctx.gpr[0..=5]` are args, and the handler writes
return values back into `ctx.gpr[0..=4]` per the two-register
convention in [`syscalls.md`](syscalls.md).

## ESR decoding — classifying a sync exception

`ESR_EL1` bits [31:26] are the EC (Exception Class) field. The
authoritative list of values lives at
`lockjaw-types/src/exception.rs:84-97`:

| EC | Meaning |
|---|---|
| `0x00` EC_UNKNOWN | Unrecognized exception |
| `0x01` EC_TRAPPED_WFI_WFE | `WFI`/`WFE` trapped (Lockjaw doesn't trap these) |
| `0x15` EC_SVC_AARCH64 | **SVC from AArch64 — this is a syscall** |
| `0x18` EC_TRAPPED_MSR_MRS | System register access trapped |
| `0x20` EC_INSTRUCTION_ABORT_LOWER | Instruction fetch fault from userspace |
| `0x21` EC_INSTRUCTION_ABORT_SAME | Instruction fetch fault from kernel |
| `0x22` EC_PC_ALIGNMENT | PC misalignment |
| `0x24` EC_DATA_ABORT_LOWER | Data abort from userspace (page fault) |
| `0x25` EC_DATA_ABORT_SAME | Data abort from kernel (kernel page fault) |
| `0x26` EC_SP_ALIGNMENT | SP misalignment |
| `0x2C` EC_TRAPPED_FP | FP/NEON trapped (we don't trap these post-boot) |
| `0x30` EC_BREAKPOINT_LOWER | Userspace breakpoint |
| `0x31` EC_BREAKPOINT_SAME | Kernel breakpoint |
| `0x3C` EC_BRK_INSTRUCTION | `BRK` instruction |

The pure classifier is at `:154`:

```rust
pub enum SyncExceptionAction {
    Syscall,
    UserFault,
}

pub fn classify_sync_exception(esr: u64) -> SyncExceptionAction {
    match esr_exception_class(esr) {
        EC_SVC_AARCH64 => SyncExceptionAction::Syscall,
        _              => SyncExceptionAction::UserFault,
    }
}
```

The kernel's `handle_exception_sync_lower` matches on the returned
variant. The classifier is pure and host-tested; the kernel side
is a thin shell. Today every non-SVC sync exception from userspace
is treated as a fatal UserFault and halts the system — there is no
page-fault-driven on-demand-mapping; userspace must pre-map all
pages it intends to touch (see [`process-creation.md`](process-creation.md)).

For data aborts specifically, the **DFSC** subfield (bits [5:0] of
ESR) names the precise fault. `data_fault_name(dfsc)` at `:121`
maps the codes to readable strings for diagnostic output.

## Where the rest lives

- **Vector table asm** and the `VECTOR_STUB` macro:
  `src/arch/aarch64/exceptions.rs:303+`.
- **Handler installation** (`VBAR_EL1` write): `exceptions.rs:390::init`.
- **IRQ dispatch** (called from `handle_exception_irq`):
  `src/arch/aarch64/mod.rs::irq_dispatch` (`:17`) and the GIC
  layer at `src/arch/aarch64/gic/`.
- **Syscall dispatch** (called from `handle_exception_sync_lower`):
  `src/syscall/handler.rs` — see [`syscalls.md`](syscalls.md).
- **Pure decode + classifier** (host-tested):
  `lockjaw-types/src/exception.rs`.

## What is NOT done in the handlers

A few things a reader from Linux/seL4 might expect that Lockjaw
deliberately omits:

- **No demand paging.** All user pages are mapped at process
  creation; a data abort from userspace is a fatal user bug, not
  an opportunity to fault-in a page.
- **No FIQ handling.** Lockjaw doesn't route any device to FIQ;
  any FIQ is a misconfiguration and halts.
- **No kernel-fault recovery.** A synchronous fault from kernel
  mode (faulting on `ctx`, dereferencing a bad pointer, etc.) is
  treated as a corrupted kernel and the system halts. There is no
  in-kernel exception handler that can recover from a kernel page
  fault.

Each of these is a deliberate scope cut that keeps the kernel
small. If the project ever needs one, it lands as its own
classifier variant + handler arm; the pure-classifier shape is
designed for that extension.
