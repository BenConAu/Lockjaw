# Kernel relink notes

Reference material captured during the kernel image relink work
(plan: `~/.claude/plans/nifty-rolling-naur.md`).

---

## Phase 0: what depended on the user-TTBR0 kernel identity

### The setup

Before this work, every user TTBR0 carried a kernel identity
map at `L1[1]` (1 GB block of all RAM) plus `L2[4]` (device
MMIO), installed in `AddressSpaceBuilder::new`
(`src/arch/aarch64/vmem.rs:87-117`). The comment at lines
77–80 named the dependency without pinpointing it: *"Removing
this causes immediate crash on EL0 entry — must investigate
which code path."*

### The diagnostic experiment

Locally commented out the L1[1] block install + the device-MMIO
block install in `AddressSpaceBuilder::new`. Built clean. Ran
`make test`.

### Observed behavior

Boot proceeds normally through scheduler start. Exact kernel
output up to the failure:

```
Address space created: TTBR0 = 0x403e3000
Scheduler started.
[SCHED-KERNEL-PHASE] 0 context switches, TTBR0 writes: 0
Dropping to EL0...
========================================
[FAULT:KERN]  HARDWARE EXCEPTION
[FAULT:KERN]  Kernel stack canary: INTACT
========================================  ← recursive fault here
[FAULT:KERN]  HARDWARE EXCEPTION
[FAULT:KERN]  Kernel stack canary: INTACT
[FAULT:KERN]  ESR:  0x96000005 — Data Abort from same EL — ...
... (loops indefinitely) ...
```

### Decoding the fault

`ESR_EL1 = 0x96000005`:
- `EC = 0x25` — Data Abort *taken without a change in EL*
  (kernel-mode fault).
- `DFSC = 0x05` — Translation fault, level 1.

The kernel itself (EL1) tried to dereference an address whose
page-table walk hit a missing L1 entry. Combined with the
removed mapping: the kernel is accessing an address in the
**TTBR0** range (lower half) — when TTBR0 was the user's table
and we removed `L1[1]`, the walk through user TTBR0 found no
L1 entry for the kernel's PA range and faulted.

### Recursive crash — why FAR/ELR aren't shown

The first fault prints "HARDWARE EXCEPTION" + "Kernel stack
canary: INTACT" then immediately re-faults. Output sequence in
`print_fault` (`src/arch/aarch64/exceptions.rs:62-85`):

1. `kprintln!("HARDWARE EXCEPTION")` — printed.
2. `mm::stack::check_canary_report(prefix)` — printed.
3. `crate::crash::print_thread_context(prefix)` — **faults
   here**. No further output reaches the UART.

`print_thread_context` (`src/crash.rs:26`) reads the current
TCB. The TCB lives in the KVM pool (TTBR1, unaffected by user
TTBR0 changes), so the read itself shouldn't fault — but
something inside that path does. The recursive crash means the
exact culprit is hidden behind the second fault.

### What the fault tells us anyway

Even without the precise PC/FAR, three things are clear:

1. **Some kernel-mode code accesses a TTBR0-range
   (lower-half) address** in the EL0-entry path. The L1[1]
   identity in user TTBR0 is what made that work. After the
   relink, kernel symbols become higher-half VAs (TTBR1) so
   any `&__symbol`-via-PC-relative or static-data reference
   automatically moves to TTBR1 and stops needing the
   identity.
2. **The fault arrives between `drop_to_el0_with_ttbr0`
   installing the user TTBR0 and the first piece of
   diagnostic output that would identify the offender** —
   meaning the fault happens either in `drop_to_el0`'s asm
   sequence (unlikely — it's all register-only after TLBI),
   or during `eret` / EL0 entry's first instruction fetch /
   the first IRQ that fires after EL0 starts. Sub-second
   timer interrupt is the most likely first trigger.
3. **The crash handler itself depends on the identity** for
   `print_thread_context` to succeed. After the relink,
   crash-handler reads of static globals move to TTBR1 and
   the recursion goes away — so the first-fault output
   becomes useful.

### Implications for Phase 1 / Phase 2

- The relink is the *correct* fix. After kernel symbols are
  linked at L0[1] KVAs (rather than implied PA + linear-map
  shift), every kernel-mode reference resolves through TTBR1
  and the user TTBR0 doesn't need the kernel identity.
- Phase 2's verification gate is "this exact failure mode no
  longer reproduces" — boot proceeds past the EL0 drop and
  init runs.
- If after Phase 1 the same fault still reproduces, that means
  some hardcoded lower-half PA escaped the linker-symbol
  audit. The audit doc + `xtask check-linker-symbols` exist
  to make that miss visible.

### Caveat: the precise offender remains unidentified

Phase 0's stated approach was "find the exact code path." The
recursive crash made the fault opaque from kernel logs alone.
Pinpointing the instruction would require either:
- a minimal early-fault diagnostic that prints `FAR_EL1` /
  `ELR_EL1` *before* anything that itself depends on the
  identity, or
- running under QEMU's `-d int,cpu_reset,guest_errors` /
  `-monitor` to inspect the first fault's hardware state.

For the relink, we don't strictly need the offender's identity
— we have a strong hypothesis (kernel-image PA reference via
either inline asm literal or stale post-pivot pointer) and
Phase 2's verification gate proves the hypothesis correct or
exposes survivors. If the post-Phase-1 verification surfaces a
survivor, return here and add the deeper diagnostic.
