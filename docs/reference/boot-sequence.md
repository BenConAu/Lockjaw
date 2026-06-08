# Boot Sequence

From `_start` (the linker entry) to init running at EL0. This is the
narrative walkthrough of every step CPU 0 takes between reset and
handing control to userspace, plus how the secondary CPUs come up
alongside.

The companion docs for the pieces this sequence touches:
[`memory-model.md`](memory-model.md) (page allocator init),
[`higher-half-kernel.md`](higher-half-kernel.md) (the TTBR0/TTBR1
split this sequence sets up), [`threads.md`](threads.md) (the
scheduler this sequence starts).

## CPU 0 — the long path

Entry assembly lives at `src/arch/aarch64/boot.rs:8` (`_start`).
The Rust handoff into `kmain` lives at `src/main.rs:92`. The asm
prelude does the minimum required to call into Rust:

### Assembly prelude (`boot.rs:8-90`)

1. **Save the firmware-passed DTB pointer (x0).** Stashed in x20
   until BSS zero is done; the firmware passes a DTB paddr to
   `_start` and clobbering it would lose the only handle to
   platform info.
2. **Mask all exceptions** (`msr DAIFSet, #0xf`). The kernel runs
   IRQ-masked from `_start` until the scheduler starts.
3. **Compute the physical-vs-linker offset.** `phys_offset =
   actual_addr(_start) - linked_addr(_start)`. On QEMU `-kernel`
   load this is 0; on Pi 4B the firmware loads us at `0x80000` and
   the linker assigns higher addresses, so the prelude is
   position-independent.
4. **Drop from EL2 to EL1 if needed.** QEMU's default entry is
   EL2; Pi enters EL2 too. The kernel runs at EL1 — set
   `HCR_EL2.RW = 1`, build a fake SPSR_EL2, `eret` to `.Lat_el1`.
5. **Enable FP/NEON** via `CPACR_EL1.FPEN = 0b11`. The compiler
   may emit NEON instructions and trapping them would crash.
6. **Set up the boot stack** at `__stack_top` (linker symbol,
   adjusted by phys_offset).
7. **Zero BSS** in a tight loop.
8. **Store the saved DTB paddr** into `BOOT_DTB_PADDR` (now safe
   that BSS is zeroed).
9. **`bl kmain`** with x0 = DTB paddr.

The asm halts in `wfi` if kmain ever returns; it never does.

### `kmain` — phase 1: pre-MMU

`src/main.rs:92`. Everything in this section runs with the MMU
*off* — raw physical addresses, no VA translation.

1. **DTB discovery.** Read `BOOT_DTB_PADDR`. If zero, search
   `arch::aarch64::platform::QEMU_DTB_SEARCH_ADDR` (QEMU
   `-kernel` puts the DTB at the start of RAM).
2. **`platform::discover(dtb_paddr)`** parses the DTB and populates
   the `PlatformInfo` static (UART base, GIC base, RAM range, CPU
   topology, SMP boot method). Halts with no diagnostic on failure
   — UART address isn't known yet, so we have no way to print.
3. **UART init.** `Pl011::set_base(plat.pl011_base)` + `init_baud`.
   First print happens here.
4. **Print boot banner + memory layout** from linker symbols.
5. **`page_alloc::init_with_gap(...)`** registers all-RAM-minus-
   reserved with the buddy allocator (see
   [`memory-model.md`](memory-model.md) for the five-region carve).

### `kmain` — phase 2: MMU bring-up

6. **`mmu::init_boot_page_tables()`** populates the boot L0/L1/L2/L3
   trees as identity mapping + planned higher-half mapping.
7. **`mmu::enable_mmu()`** writes TCR_EL1, MAIR_EL1, TTBR0_EL1,
   TTBR1_EL1, and finally SCTLR_EL1.M. UART access is preserved
   because the boot tables identity-map device MMIO.
8. **`mmu::enable_higher_half()`** + **`Pl011::use_high_addresses()`**
   — kernel code can now reach addresses via the linear
   higher-half map at `KERNEL_VA_OFFSET = 0xFFFF_0000_0000_0000`.
   UART pointer rewritten to use the high mapping.
9. **`cache::init_and_check()`** reads `CTR_EL0.DminLine` and
   verifies the silicon's cache line size matches the constant in
   `lockjaw_types::cache::CACHE_LINE_BYTES` — mismatch would break
   the DMA sync primitives' range arithmetic.
10. **`mm::kvm::kvm_init()`** + **`mm::kvm::boot_self_test()`**
    (`src/main.rs:201-202`). Installs the KVM L1 table at
    `KERNEL_L0[KVM_L0_INDEX = 256]` and runs the boot self-test
    (33-page allocation that proves the stitched higher-half
    mapping works). Any kernel-object allocation (TCBs, endpoints,
    handle tables, PageSet headers, process pages) made after
    this point lands in the KVM pool — see
    [`memory-model.md`](memory-model.md). A hang at this stage
    is almost always a buddy-exhaustion or L1 install bug.

### `kmain` — phase 3: DMA pool + per-CPU init

10. **DMA pool invalidate.** The DMA pool's PA range was just
    pulled into the kernel TTBR1 direct map as Cacheable; any
    firmware-era cache lines for that range are now reachable.
    `cache::invalidate_range(pool_kva, pool_bytes)` forecloses
    the non-deterministic first-allocation corruption bug class.
11. **`stack::init_canary()`** + check.
12. **`percpu::init_percpu(0)`** writes CPU 0's `TPIDR_EL1` — runs
    twice (once pre-pivot with the PA, once post-pivot with the
    L0[1] kernel-image VA) so panic/exception during the SMP-boot
    or pivot window can still resolve per-CPU data.

### `kmain` — phase 4: secondary CPU release

13. **SMP boot loop** (`src/main.rs:323`). Read `MPIDR_EL1` to find
    CPU 0's own ID; for every other CPU in `plat.cpus`:
    - **PSCI path:** `arch::aarch64::psci::cpu_on(mpidr, entry,
      context_id, hvc)`. The kernel calls SMC (`hvc=false`) or
      HVC (`hvc=true`) to ask firmware to start the CPU at
      `_secondary_start`.
    - **Spin-table path:** `spin_table::write_release_addr(release_addr, entry)`
      stores the entry point into each CPU's per-CPU release
      address; a single `SEV` after all writes wakes all secondaries
      at once.
    - **`SmpMethod::None`:** single-core, no SMP bringup.
14. Brief spin-loop delay so secondaries print their online lines
    before primary boot continues.

### `kmain` — phase 5: pivot to higher-half

15. **`_pivot_to_higher_half(offset)`** updates PC, SP, and FP to
    L0[1]-kernel-image addresses. After this, the kernel no longer
    depends on TTBR0 identity — VBAR will be set to a TTBR1
    address, and the kernel can survive a TTBR0 swap to a user
    page table.
16. **`exceptions::init`** installs the post-pivot VBAR. Vector
    table now lives in TTBR1.
17. Various subsystems (timer, GIC) initialize through here in
    the live source — too many to enumerate; the next section
    calls out the ones that gate userspace. (KVM pool init
    happens earlier in phase 2 — see step 10.)

### `kmain` — phase 6: launch init

The kernel's last act is to repurpose CPU 0's boot thread as init's
first thread, rather than `context_switch`ing into a fresh thread:

1. **Parse init's ELF** (`parse_elf` on `INIT_ELF` `include_bytes!`).
2. **Allocate + map** every PT_LOAD segment via the same
   PageSet + map_pages dance described in
   [`process-creation.md`](process-creation.md).
3. **Build the user TTBR0** for init's address space.
4. **Allocate init's handle table** + create a ProcessObject
   (`cap::process_obj::create_process_object` at `main.rs:784`).
5. **Re-point CPU 0's TCB** to init's ProcessObject. CPU 0's boot
   TCB has no synthetic `SavedContext` because nothing ever
   `context_switch`es *into* it (see `boot_tcb_entry_unreachable`
   at `main.rs:877` — placeholder that panics loudly if it ever
   executes).
6. **Flush I-cache** (`ic iallu`; `dsb ish`; `isb`) to commit the
   ELF copies.
7. **`sched::scheduler::start()`** — the scheduler is now live.
   Future timer ticks will preempt; secondary CPUs that have been
   parked in `idle_wait` will start picking ready threads.
8. **`mmu::drop_to_el0_with_ttbr0(ttbr0, entry_point, user_stack_top, 0)`**
   installs init's TTBR0 and `eret`s to EL0 at init's entry point.

CPU 0 is now running init's first thread at EL0. The kernel never
returns to phase 6's call frame; the next time CPU 0 enters EL1
will be through a syscall, IRQ, or exception trap.

## Secondary CPU — the short path

Entry assembly: `_secondary_start` in `src/arch/aarch64/boot.rs:91`.
Rust handoff: `secondary_main(cpu_id: u64)` at `src/main.rs:890`.

Secondaries don't re-do everything CPU 0 did. The asm prelude:

1. Derives `cpu_id` from `MPIDR_EL1.Aff0` (works for both PSCI —
   where x0 = `context_id` — and spin-table — where x0 is
   undefined; this is single-cluster linear topology, multi-cluster
   would need `Aff1:Aff0` mapping).
2. Skips BSS zeroing (CPU 0 already did it).
3. Sets up the per-CPU boot stack from `__per_cpu_stacks + cpu_id * stride`.
4. `bl secondary_main` with x0 = cpu_id.

`secondary_main` (`main.rs:890`):

1. **`mmu::enable_mmu_secondary()`** installs the same TTBR1
   trees CPU 0 set up.
2. **`percpu::init_percpu(cpu_id)`** pre-pivot (PA pointer in
   `TPIDR_EL1` so a panic during pivot can still resolve per-CPU
   data through the identity map).
3. **Pivot to higher-half** (same shift as CPU 0's pivot).
4. **`percpu::init_percpu(cpu_id)`** post-pivot (refresh to the
   L0[1] VA so it survives a future user-TTBR0 install).
5. **`exceptions::init()`** installs the per-CPU VBAR_EL1 so this
   CPU's traps land in the post-pivot vector table.
6. **`stack::init_canary_for_cpu(cpu_id)`** writes the canary for
   this CPU's stack.
7. **`gic::init_cpu(cpu_id)`** initializes the per-CPU GIC
   redistributor + CPU interface. Without this the CPU cannot
   receive IRQs.
8. **`timer::init_secondary()`** programs CNTV_CVAL.
9. **`idle_wait(cpu_id)`** — the secondary parks. When the
   scheduler has a Ready thread and this CPU's slot is empty,
   `unblock_thread`'s on-CPU branch (see
   [`threads.md`](threads.md)) wakes the secondary into the new
   thread.

Secondaries do not run init's startup code; they exist to be
target slots for the scheduler to dispatch into.

## What gates userspace

The kernel cannot drop to EL0 until *all* of the following are
true:

- MMU enabled, higher-half pivot done, VBAR pointing at L0[1].
- Cache line size verified.
- Page allocator initialized with the full RAM partition.
- DMA pool initialized + invalidated for first-use safety.
- KVM allocator installed at L0[256].
- GIC initialized; timer programmed.
- Per-CPU init done for CPU 0 (TPIDR_EL1 set).
- Scheduler started (`scheduler::start()`).
- init's ProcessObject + handle table + TTBR0 + TCB all exist.

The `drop_to_el0_with_ttbr0` call is the boundary. After it,
everything is userspace + IPC + the trap-driven syscall + IRQ
paths described in [`syscalls.md`](syscalls.md) and
[`ipc.md`](ipc.md).

## Linker symbols this sequence depends on

Listed for cross-reference; the authoritative classification lives
in [`linker-symbol-audit.md`](linker-symbol-audit.md), which
`cargo xtask check-linker-symbols` enforces on every build.

- `__bss_start`, `__bss_end` — BSS zero loop in the asm prelude.
- `__stack_bottom`, `__stack_top` — boot stack range.
- `__kernel_start`, `__kernel_end` — page-allocator reserved
  region.
- `__per_cpu_stacks`, `__per_cpu_stacks_end` — per-CPU stack
  region (2 MiB aligned).
- `BOOT_DTB_PADDR` — global written by the asm prelude, read by
  `kmain`.
