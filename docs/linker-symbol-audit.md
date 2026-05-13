# Linker-symbol audit

Every site in `src/` that takes the address of a linker-defined
symbol (`__kernel_start`, `__per_cpu_stacks`, `_secondary_start`,
the per-CPU `__stack_*` / `__guard_page_*`, `__exception_vectors`,
…) or of a kernel boot static (`BOOT_*`, `KERNEL_*`,
`KERNEL_IMAGE_*`) is listed here with a classification. The
classification determines what — if anything — has to change when
the kernel image is relinked into a fixed higher-half VA region
(see `docs/relink-notes.md` and the relink plan).

The `xtask check-linker-symbols` command enforces this list:
every linker-symbol-to-integer site in `src/` must appear here,
and any unclassified addition fails CI.

## Classifications

- **VA-image** — the value is used as a virtual address that
  lives inside the kernel image region (post-relink: L0[1]).
  The compiler enforces the regime with `KernelImageVa`. No PA
  conversion needed.

- **PA-prepivot** — the value is consumed as a physical address
  (`page_alloc::init_with_gap`, PSCI entry, `setup_guard_pages`,
  TTBR install). The site runs **pre-pivot**, where PC-relative
  `&__sym as u64` returns the runtime PA naturally because PC
  and the linker have a uniform PIE shift (`load_PA -
  LINKER_BASE`). After the relink, this still holds; no
  conversion is needed at these sites. **If a future change
  moves any of these sites past `_pivot_to_higher_half`, the
  bare cast yields a kernel-image VA instead and the site must
  switch to `mmu::kernel_image_kva_to_pa(KernelImageVa::new(…))`
  to recover the PA.**

- **PA-prepivot-static** — `&raw const STATIC` for a kernel
  boot-time static (`BOOT_*`, `KERNEL_*`, `KERNEL_IMAGE_*`).
  Same rationale as PA-prepivot: PC-relative produces PA
  pre-pivot. Used for hardware register values (TTBR0/1) and
  for PTE table-descriptor installs.

- **DISPLAY** — value is only printed, never consumed. The
  printout label may want adjusting after relink (e.g., "kernel
  load addr" → "kernel link addr") but no runtime behavior
  depends on the value.

## Sites

### `src/main.rs` — kernel boot orchestration

| Line | Classification | Symbol | Consumer / notes |
|------|---|---|---|
| 128  | DISPLAY        | `__bss_start` | local for kprintln of BSS range |
| 130  | DISPLAY        | `__bss_end`   | local for kprintln of BSS range |
| 132  | DISPLAY        | `__kernel_end`| local for kprintln of kernel-end addr |
| 134  | DISPLAY        | `__stack_bottom` | local for kprintln of stack range |
| 136  | DISPLAY        | `__stack_top` | local for kprintln of stack range |
| 140  | DISPLAY        | `__kernel_start` | kprintln "kernel load" — relabel post-relink |
| 155  | PA-prepivot    | `__kernel_start` | `PhysAddr::new(...)` → `page_alloc::init_with_gap` |
| 157  | PA-prepivot    | `__kernel_end`   | same |
| 159  | PA-prepivot    | `__per_cpu_stacks` | same |
| 161  | PA-prepivot    | `__per_cpu_stacks_end` | same |
| 260  | PA-prepivot    | `__guard_page_0` | `setup_guard_pages` (expects PA) |
| 262  | PA-prepivot    | `__guard_page_1` | same |
| 264  | PA-prepivot    | `__guard_page_2` | same |
| 266  | PA-prepivot    | `__guard_page_3` | same |
| 302  | PA-prepivot    | `_secondary_start` (`fn` cast) | PSCI cpu_on entry — needs PA |
| 647  | VA-image       | `__stack_bottom` | wrapped as `KernelImageVa::new(...)` → `create_idle_tcb` |
| 673  | VA-image       | `__guard_page_0` (+4096) | wrapped as `KernelImageVa::new(...)` → secondary idle |
| 675  | VA-image       | `__guard_page_1` (+4096) | same |
| 677  | VA-image       | `__guard_page_2` (+4096) | same |
| 679  | VA-image       | `__guard_page_3` (+4096) | same |

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

### `src/arch/aarch64/mmu.rs` — boot page tables + L0[1] kernel-image map

| Line | Classification | Symbol | Consumer / notes |
|------|---|---|---|
| 139  | PA-prepivot-static | `BOOT_L0` | which 1 GB block holds the kernel |
| 168  | PA-prepivot-static | `BOOT_L1` | L0[0] table descriptor PA |
| 216  | PA-prepivot-static | `BOOT_L0` | TTBR0 install |
| 285  | PA-prepivot-static | `KERNEL_L0` | `kernel_l0_paddr()` accessor |
| 290  | PA-prepivot-static | `BOOT_L0` | which 1 GB block holds the kernel |
| 313  | PA-prepivot-static | `KERNEL_L1` | L0[0] table descriptor PA |
| 324  | PA-prepivot-static | `KERNEL_L0` | TTBR1 install |
| 383  | PA-prepivot    | `__kernel_start` | discover load PA → KERNEL_PHYS_OFFSET |
| 388  | PA-prepivot    | `__per_cpu_stacks_end` | image span computation |
| 391  | PA-prepivot    | `__guard_page_0` | guard PA — skip in L3 walk |
| 392  | PA-prepivot    | `__guard_page_1` | same |
| 393  | PA-prepivot    | `__guard_page_2` | same |
| 394  | PA-prepivot    | `__guard_page_3` | same |
| 428  | PA-prepivot-static | `KERNEL_IMAGE_L3` | L2 table descriptor PA (per L3) |
| 432  | PA-prepivot-static | `KERNEL_IMAGE_L2` | L1[0] table descriptor PA |
| 435  | PA-prepivot-static | `KERNEL_IMAGE_L1` | L0[1] table descriptor PA |
| 470  | PA-prepivot-static | `BOOT_L0` | secondary CPU TTBR0 install |
| 474  | PA-prepivot-static | `KERNEL_L0` | secondary CPU TTBR1 install |
| 552  | PA-prepivot-static | `KERNEL_L3_GUARD` | guard-page L3 table descriptor PA |
| 557  | PA-prepivot-static | `KERNEL_L2_RAM`  | guard-page L2 table descriptor PA |

## What changed in the relink (1b)

- `linker.ld` ORIGIN moved from `0x40200000` (a paddr) to
  `0xFFFF_0080_0000_0000` (L0[1] base — a fixed VA).
- Pre-pivot PA-recovery sites are unchanged and continue to
  yield correct PAs because PC-relative honors the PIE shift
  (`load_PA - LINKER_BASE`) automatically.
- New sites in `mmu.rs::init_kernel_image_map` (entries above)
  read kernel symbols pre-MMU to discover load PA, build the
  L3 mapping, and skip guard pages.
- Post-pivot the kernel image is reachable through the L0[1]
  mapping; PC-relative `&__sym` now returns L0[1] VAs. Any new
  PA-recovery site introduced post-pivot must use
  `mmu::kernel_image_kva_to_pa`.
