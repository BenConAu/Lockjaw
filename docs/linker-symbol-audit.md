# Linker-symbol audit

Every site in `src/` that takes the address of a linker-defined
symbol (`__kernel_start`, `__per_cpu_stacks`, `_secondary_start`,
the per-CPU `__stack_*` / `__guard_page_*`, `__exception_vectors`,
…) is listed here with a classification. The classification
determines what — if anything — has to change when the kernel
image is relinked into a fixed higher-half VA region (see
`docs/relink-notes.md` and the relink plan).

The `xtask check-linker-symbols` command enforces this list:
every linker-symbol-to-integer site in `src/` must appear here,
and any unclassified addition fails CI.

## Classifications

- **VA-image** — the value is used as a virtual address that
  lives inside the kernel image region. Pre-relink that's the
  linear higher-half map (`PA + KERNEL_VA_OFFSET`); post-relink
  it's the dedicated L0[1] kernel-image VA range. The compiler
  enforces the regime with `KernelImageVa`. No PA conversion
  needed.

- **PA** — the value is consumed as a physical address
  (`page_alloc::init_with_gap`, PSCI entry, `setup_guard_pages`,
  TTBR register install). Pre-relink the bare `&__sym as u64`
  happens to equal PA because linker_VA == PA on QEMU (and the
  `phys_offset` trick handles boards where it doesn't).
  Post-relink the bare cast yields a kernel-image KVA and must
  be converted via `mmu::kernel_image_kva_to_pa` (added in 1b).

- **PA-prepivot-static** — `&raw const STATIC` for a kernel
  internal static (e.g., the boot page tables `BOOT_L0`,
  `KERNEL_L1`). Used as a PA for hardware register values
  (TTBR0/1) or PTE installs. Works because the consumer runs
  pre-pivot where the PC-relative computation naturally
  produces a PA. Survives the relink unchanged for the same
  reason — pre-MMU/pre-pivot, PC = PA regardless of what the
  linker says.

- **DISPLAY** — value is only printed, never consumed. The
  printout label may want adjusting after relink (e.g., "kernel
  load addr" → "kernel link addr") but no runtime behavior
  depends on the value.

## Sites

### `src/main.rs` — kernel boot orchestration

| Line | Classification | Symbol | Consumer / notes |
|------|---|---|---|
| 113  | DISPLAY        | `__bss_start` | local for kprintln of BSS range |
| 115  | DISPLAY        | `__bss_end`   | local for kprintln of BSS range |
| 117  | DISPLAY        | `__kernel_end`| local for kprintln of kernel-end addr |
| 119  | DISPLAY        | `__stack_bottom` | local for kprintln of stack range |
| 121  | DISPLAY        | `__stack_top` | local for kprintln of stack range |
| 125  | DISPLAY        | `__kernel_start` | kprintln "kernel load" — relabel post-relink |
| 140  | PA             | `__kernel_start` | `PhysAddr::new(...)` → `page_alloc::init_with_gap` |
| 142  | PA             | `__kernel_end`   | same |
| 144  | PA             | `__per_cpu_stacks` | same |
| 146  | PA             | `__per_cpu_stacks_end` | same |
| 224  | PA             | `__guard_page_0` | `setup_guard_pages` (expects PA) |
| 226  | PA             | `__guard_page_1` | same |
| 228  | PA             | `__guard_page_2` | same |
| 230  | PA             | `__guard_page_3` | same |
| 254  | PA             | `_secondary_start` (`fn` cast) | PSCI cpu_on entry — needs PA |
| 587  | VA-image       | `__stack_bottom` | wrapped as `KernelImageVa::new(...)` → `create_idle_tcb` |
| 613  | VA-image       | `__guard_page_0` (+4096) | wrapped as `KernelImageVa::new(...)` → secondary idle |
| 615  | VA-image       | `__guard_page_1` (+4096) | same |
| 617  | VA-image       | `__guard_page_2` (+4096) | same |
| 619  | VA-image       | `__guard_page_3` (+4096) | same |

### `src/mm/stack.rs` — stack canary helpers

| Line | Classification | Symbol | Consumer / notes |
|------|---|---|---|
| 28   | VA-image       | `__stack_bottom_0` | display / sanity print |
| 29   | VA-image       | `__stack_top_0`    | display / sanity print |
| 40   | VA-image       | `__per_cpu_stacks` | per-CPU stack base computation (VA) |
| 74   | VA-image       | `__stack_bottom_0` | canary write through VA |
| 97   | VA-image       | `__stack_bottom_0` | canary read through VA |

### `src/arch/aarch64/exceptions.rs` — VBAR setup

| Line | Classification | Symbol | Consumer / notes |
|------|---|---|---|
| 395  | VA-image       | `__exception_vectors` | `VBAR_EL1` install — VBAR is a VA |

### `src/sched/tcb.rs` — synthesized SavedContext

| Line | Classification | Symbol | Consumer / notes |
|------|---|---|---|
| 52   | VA-image       | `thread_entry` (`fn` cast) | bootstrap `lr` — kernel code VA |

### `src/arch/aarch64/mmu.rs` — boot page tables

These use `&raw const STATIC` for kernel-internal statics, NOT
linker `__symbols`. Consumers run pre-pivot where PC = PA, so
PC-relative naturally produces PAs. Listed for completeness.

| Line | Classification | Symbol | Consumer / notes |
|------|---|---|---|
| 54   | PA-prepivot-static | `BOOT_L0` | which 1 GB block holds the kernel |
| 83   | PA-prepivot-static | `BOOT_L1` | L0[0] table descriptor PA |
| 131  | PA-prepivot-static | `BOOT_L0` | TTBR0 install |
| 200  | PA-prepivot-static | `KERNEL_L0` | `kernel_l0_paddr()` accessor |
| 205  | PA-prepivot-static | `BOOT_L0` | which 1 GB block holds the kernel |
| 228  | PA-prepivot-static | `KERNEL_L1` | L0[0] table descriptor PA |
| 232  | PA-prepivot-static | `KERNEL_L0` | TTBR1 install |
| 289  | PA-prepivot-static | `BOOT_L0` | secondary CPU TTBR0 install |
| 293  | PA-prepivot-static | `KERNEL_L0` | secondary CPU TTBR1 install |
| 371  | PA-prepivot-static | `KERNEL_L3_GUARD` | guard-page L3 table descriptor PA |
| 376  | PA-prepivot-static | `KERNEL_L2_RAM`  | guard-page L2 table descriptor PA |

## What changes in 1b

After the relink commits land:
- Every **PA** entry's source site adds an explicit
  `mmu::kernel_image_kva_to_pa(KernelImageVa::new(&__sym as u64))`
  conversion (or wraps the symbol via a typed helper that does
  the same).
- **VA-image** entries are unchanged — they already have the
  right semantic and (after 1a) are wrapped in `KernelImageVa`
  at the relevant call sites.
- **PA-prepivot-static** entries are unchanged.
- **DISPLAY** entries get cosmetic relabeling.

The audit list updates to reflect any new sites added by 1b's
mmu boot-trampoline code (which will read kernel symbols to
build the L0[1] mapping).
