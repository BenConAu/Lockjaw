# Lockjaw-M: ARMv8-M (MPU-class) Variant — Planning Document

Status: **Phase 0 not started** — this document is the Phase 0 input.
Targets: QEMU `mps3-an547` (Cortex-M55, dev loop), **Alif Ensemble (Cortex-M55, primary silicon)**, RP2350 (dual Cortex-M33, SMP-honesty secondary), QEMU `mps2-an505`/`an521` (M33 portability check).
Prior context: MPU microcontroller exploration (SAS framing), GIC/interrupt-spine review findings, scheduler refactor (idle-CPU paths), regspec Phase 8.

---

## 1. Goal and strategic rationale

Produce a Lockjaw variant for ARMv8-M mainline MPU-class hardware that preserves the
Lockjaw security model — Zircon-style handles with rights, unprivileged restartable
drivers, kernel-enforced device access — on microcontroller-class silicon with no MMU.

Why this is worth doing:

1. **Proof of architecture.** The pure-model split (lockjaw-types decision functions,
   kernel as executor) claims to be architecture-independent. This port is the
   experiment: if `SchedState`, the IPC decision functions, teardown plans, and the
   handle model drop in unchanged under a completely different memory-protection and
   exception model, the discipline was load-bearing. If they don't, we learn where the
   A-class assumptions leaked in.
2. **The M33-class is the incumbents' home turf.** FreeRTOS and Zephyr live on exactly
   this silicon. A Lockjaw-M demo — a driver crashing and restarting unprivileged on a
   $5 RP2350 — meets that argument on its own ground, and is a sharper exhibit than the
   A53-consolidation story alone.
3. **Hardware does work for us here.** ARMv8-M's exception model eliminates several
   entire bug classes found in the A-class review (see §4). The port is partly a
   *simplification*.

Non-goals (this phase): TrustZone-M secure/non-secure partitioning as a feature,
Helium/MVE exploitation, dynamic loading/PIC userspace, LPI-class interrupt scale,
power management beyond WFI.

---

## 2. Architecture summary

### 2.1 Single Address Space (SAS) with protection domains

No MMU means no per-process virtual address spaces. The model (settled in the earlier
MPU exploration) is **single address space, Mungi-style**: every thread sees the same
physical map; isolation comes from per-domain MPU configuration, not translation.

- **Protection domain** replaces "address space." A domain is a descriptor: an array of
  MPU region configurations (base, limit, attributes) plus a grant list.
- **Process = protection domain + handle table + threads**, same object shape as
  mainline. `ProcessObject` keeps its role; `ttbr0` field is replaced by a domain
  descriptor reference.
- **Context switch** = save/restore callee-saved registers (PendSV, §4.3) + reprogram
  MPU regions from the incoming domain's descriptor + `DSB; ISB`. This is the TTBR0-swap
  analog. No TLB exists; no flush cost; MPU reprogram is ~2 MMIO writes per region.
- **KernelVa becomes the identity map.** The `KernelVa` newtype survives (vindicating
  the typed-address discipline) but `as_u64()` is the physical address. The
  `KERNEL_VA_OFFSET` concept is deleted on this arch. Code that assumed
  KVA = PA + offset must already be going through the newtype — Phase 0 audits this.

### 2.2 Handles and grants

The handle model transfers intact. The PageSet analog is the **region grant**:

- A grant is a physically contiguous, 32-byte-aligned range of SRAM (ARMv8-M MPU
  granularity is 32 bytes — vastly better than v7-M power-of-2 sizing).
- Granting maps to: append a region config to the target domain's descriptor (consumes
  one of the domain's MPU region budget, §3.1). Revocation removes it; takes effect at
  the next MPU reprogram for that domain (immediately if the domain is current —
  reprogram in place + DSB/ISB).
- MMIO handles become region grants over device address space with Device-nGnRE
  attributes (MAIR index, §5.2). The device-manager invariant — drivers cannot map
  arbitrary physical addresses — is enforced identically: the kernel writes MPU
  registers, userspace never does.
- DMA PageSet donation maps directly: a grant whose backing range is handed to a
  device. On M55 with D-cache enabled (optional, up to 64 KB), the BCM2711 coherency
  discipline transfers — clean/invalidate by MVA via SCB cache-maintenance registers
  (`SCB_DCIMVAC` etc.). On cacheless M33 (RP2350), DMA is coherent by construction and
  the cache ops compile to nothing. Keep the discipline in the code; cfg the ops.

### 2.3 What transfers, what is replaced

| Subsystem | Mainline (A-class) | Lockjaw-M | Transfer level |
|---|---|---|---|
| Scheduler model | `SchedState` + BFS | identical | **unchanged** |
| IPC decisions | lockjaw-types fns | identical | **unchanged** |
| Handle table / teardown plans | lockjaw-types | identical | **unchanged** |
| Ticket lock / GKL | aarch64 atomics | ARMv8-M `LDREX/STREX` (compiler-generated) | **unchanged source** |
| regspec emitter | MMIO kinds | + new kinds (§8) | extended |
| lockjaw-rt | core+alloc userspace | identical model | **unchanged** |
| vmem / page tables | TTBR0/1, 4-level PT | **deleted** → domain manager + region allocator | replaced |
| PT-arena | handle-funded PT pages | **deleted** (no PTs) → region-allocator funding question (§6.1) | replaced |
| Exception vectors | VBAR_EL1 asm table | vector table of fn pointers; hardware stacks r0–r3/r12/lr/pc/xPSR | replaced (smaller) |
| context_switch | 624 B frame, full NEON | PendSV, r4–r11 + EXC_RETURN; lazy FP in hardware | replaced (smaller) |
| GIC v2/v3 | IAR/EOIR protocol | NVIC: auto-vectoring, no ack protocol | replaced (smaller) |
| Timer | CNTV_EL0 + GIC PPI 27 | SysTick (24-bit) per core | replaced |
| musl personality | POSIX server | **out of scope** — lockjaw-rt binaries only | dropped |
| crash diagnostics | ESR/FAR decode | CFSR/HFSR/MMFAR/BFAR decode in lockjaw-types | ported pattern |

---

## 3. The MPU in detail (ARMv8-M PMSAv8)

### 3.1 Region budget arithmetic

Region counts are implementation-defined: M55 configurable {0,4,8,12,16}; M33 {0,4,8,16}.
**RP2350 implements 8 per core.** Design to a budget of 8 so M33 silicon is in reach;
treat 16 as headroom on M55, not a dependency.

Key trick: **`MPU_CTRL.PRIVDEFENA = 1`** — privileged execution falls back to the
default system address map when no region matches. The kernel therefore consumes
**zero regions**; all 8 belong to the current domain's unprivileged view.

Per-domain fixed cost (typical):

| Region | Contents | Attributes |
|---|---|---|
| 0 | .text (+ .rodata if contiguous) | RO, X, unprivileged |
| 1 | .data + .bss + heap | RW, XN, unprivileged |
| 2 | thread stack | RW, XN, unprivileged |
| 3 | (optional) .rodata if split, or shared lockjaw-rt text | RO, X/XN |

Leaving **4–5 regions for grants** (MMIO, shared buffers, DMA donations) on an
8-region implementation. This number is the central scarcity of the whole port; the
domain manager must fail grant operations with a typed error (`REGION_BUDGET_EXHAUSTED`,
distinct from OOM — same failure-class discipline as `QUEUE_FULL`).

Multi-threaded processes: threads share regions 0–1 and grants; each thread needs its
own stack region **or** stacks are carved from region 1 with PSPLIM as the only guard
(§4.4). Decision deferred to Phase 0; PSPLIM-only is likely sufficient and saves a
region per extra thread.

### 3.2 Register interface (regspec target: `mpu.toml`)

All SCS — MPU, NVIC, SCB, SysTick — is **memory-mapped MMIO** at architecturally fixed
addresses (0xE000E000 SCS base). No system-register asm accessors needed (unlike GICv3
ICC); the existing MMIO emitter path covers everything, plus new kinds in §8.

- `MPU_TYPE` (0xE000ED90): DREGION field = implemented region count. **Read at boot,
  assert ≥ 8** — ground-truth discipline, the MPU's GICD_TYPER analog.
- `MPU_CTRL`: ENABLE, HFNMIENA, PRIVDEFENA.
- `MPU_RNR` / `MPU_RBAR` / `MPU_RLAR`: region select, base (bits [31:5] + SH/AP/XN),
  limit (bits [31:5] + AttrIndx + EN). Also `MPU_RBAR_A1..A3`/`RLAR_A1..A3` aliases for
  batched 4-region programming — use them; context switch reprograms up to 8 regions
  and the aliased form halves the RNR writes.
- `MPU_MAIR0/1`: 8 attribute slots, MAIR-style encoding (same scheme as A-class MAIR_EL1
  — the encoding knowledge transfers). Fixed kernel-defined palette: Normal WB-RA-WA,
  Normal non-cacheable, Device-nGnRE. Domains reference indices, never raw attributes.
- Programming sequence rule: region must be disabled (RLAR.EN=0) or MPU disabled while
  changing its config; `DSB; ISB` after enable. This is a **wait/sequence constraint
  for the regspec** (§8.3) — same family as RWP, learned from the GIC review.

### 3.3 TrustZone-M posture

ARMv8-M mainline implementations may include the Security Extension. **Decision: run
entirely in the state the boot environment hands us and ignore the other state.**
QEMU AN547/AN505 boot Secure; RP2350's boot ROM can hand off either way depending on
image metadata. Phase 0 documents per-target which state we own; the kernel never
performs SG/BLXNS transitions. The NS-banked register aliasing lesson from the GIC-400
review (armstub8 dependency) applies in spirit: **document the boot-chain contract per
board** (what the boot ROM configured: SAU, MPU_NS, clock tree) in a `boot-contract.md`
per target, before first silicon boot.

---

## 4. Exception model: what the hardware now does for us

This section exists because several A-class review findings are *architecturally
impossible* on M, and the plan should claim those wins explicitly.

### 4.1 Vectoring and the death of the GIC protocol

NVIC vectoring is automatic: the vector table (VTOR-relative) is an array of handler
function pointers; the hardware reads the pending-priority winner and jumps. There is
no IAR read, no EOIR write, no spurious-1023, no EOI-ordering hazard, no
priority-drop-vs-deactivate split. The entire EOI-before-context-switch analysis from
the interrupt-spine review has no M-class analog. `irq_dispatch` becomes per-vector
handlers; INTID = exception number − 16.

### 4.2 Hardware register stacking

Exception entry hardware-pushes r0–r3, r12, lr, pc, xPSR (8 words, "basic frame") onto
the *current* stack (PSP for thread mode, MSP for handlers), loads LR with an
EXC_RETURN magic value, and runs the handler as a normal AAPCS function. SAVE_REGS
shrinks to: push r4–r11 (+ EXC_RETURN if switching). **Handlers are plain
`extern "C"` Rust functions** — the asm surface of the M-class kernel is a fraction of
mainline's. The ELR-corruption tripwire pattern ports as a check on the stacked PC.

### 4.3 Context switch: the canonical PendSV pattern

- SVC = syscall entry (immediate, synchronous). SysTick/IRQ handlers that need to
  reschedule **pend PendSV** (write ICSR.PENDSVSET) instead of switching inline.
- PendSV is configured to the **lowest priority**; it runs after all other handler
  activations unwind. The switch itself: save r4–r11 + PSP + EXC_RETURN into outgoing
  TCB, reprogram MPU from incoming domain, restore, `BX` EXC_RETURN.
- This *replaces* the schedule-from-inside-the-tick-handler shape. The
  `schedule_from_idle` / abandoned-frame / IDLE_SP machinery from the scheduler
  refactor **does not port** — an idle core sits in thread mode (or handler-return-to
  -idle-thread) and PendSV tail-chains naturally. The pure model's
  `step_from_idle` survives; its kernel-side caller is rewritten and much simpler.
  Path-5-style hand proofs about abandoned frames are not needed.

### 4.4 Stack limits: a review finding retired by hardware

**PSPLIM and MSPLIM** are architectural stack-limit registers (ARMv8-M mainline).
A push below the limit raises UsageFault.STKOF *before* the write. The
kernel-stack-overflow silent-recursion finding from the vectors review — and the
emergency-stack asm it would have required — does not exist here. Arm MSPLIM to the
kernel stack guard at boot and PSPLIM per-thread at every switch. Stack canaries stay
as defense-in-depth, but the primary mechanism is now hardware.

### 4.5 Lazy FP context

M55/M33 with FPU implement **lazy FP state preservation** (FPCCR.LSPEN): exception
entry reserves space for the extended frame but defers the actual s0–s15/FPSCR stacking
until the first FP instruction in the handler. Combined with CONTROL.FPCA tracking,
threads that never touch FP never pay the spill. This is the lazy-NEON optimization
deliberately deferred on A-class, delivered free by hardware. Rule inherited from
mainline: **kernel code is FP-free** (build kernel crate soft-float ABI; the
xtask no-NEON check gets an M-class sibling: no FP/MVE instructions in kernel text).
MVE (Helium, M55) state interacts with the same mechanism; out of scope but not
foreclosed.

### 4.6 Masking and the GKL

PRIMASK is the DAIF.I analog (BASEPRI available for graded masking; start with
PRIMASK-only for the DAIF-equivalent simplicity). On **single-core targets** the GKL
degenerates to PRIMASK critical sections — keep the `gkl_lock()` API, implement as
mask, so the source stays unified. On **RP2350 (dual-core)** the ticket lock runs
unmodified (`AtomicU32` lowers to LDREX/STREX on v8-M) and the GKL story — including
the obligation-transfer reasoning and the loom test — stays live. This is a major
reason RP2350 is the hardware target: it keeps the SMP discipline honest.

WFE/SEV exist on v8-M and the ticket-lock WFE optimization noted in review applies
here too (more valuable on battery-class silicon).

---

## 5. Memory map and userspace model

### 5.1 Memory budget (two geometries)

**RP2350 (520 KB SRAM, no caches)** is the tight case and sizes the design floor;
**Alif Ensemble (per-core ITCM/DTCM + multi-MB SRAM + MRAM, M55-HP cached)** is the
roomy case where *placement* replaces *scarcity* as the decision. Indicative carve
(Phase 0 finalizes both):

- Kernel .text/.rodata: XIP flash (RP2350) / MRAM or ITCM (Alif — hot path in TCM is
  the Phase 0 placement decision) / ITCM (AN547).
- Kernel .data/.bss + kernel stacks: ~48–64 KB SRAM. `MAX_THREADS` and `MAX_BINDINGS`
  must become per-arch consts (1024-thread table = 16 KB is affordable but pointless;
  64 threads / 64 bindings is the right M-class shape). **Action: parameterize, do not
  fork** — `lockjaw_types::scheduler::MAX_THREADS` becomes a cfg-selected const with
  the BFS bound decoupled from it (the BFS already projects to 4).
- Domain slots: N fixed userspace slots (start N=4) of fixed size (e.g. 64–96 KB each),
  each slot = link address for that slot's binary (§5.2) + its heap + stacks.
- Grant pool: remaining SRAM under the region allocator (§6.1).

### 5.2 Userspace linking: fixed slots first

No MMU ⇒ no shared link address. Options considered:

1. **Fixed per-slot link addresses** — each binary linked for its slot
   (`lockjaw-rt` linker-script parameterized by slot). Simple, zero runtime cost,
   restart = reload same slot. Limitation: binary is slot-bound; N copies needed to run
   the same driver in two slots. **Chosen for Phases 4–6.**
2. ROPI/RWPI position independence — Rust/LLVM support is rough; static relocation at
   load time is a viable middle path later. **Deferred; tracked in tech-debt.md.**
3. XIP-from-flash text + per-slot RAM data — attractive on RP2350 (text never occupies
   SRAM); compatible with option 1. Evaluate in Phase 6.

musl/POSIX personality is **dropped** for this variant — userspace is lockjaw-rt
`core + alloc` binaries per the existing rubric (bootstrap/functional/provenance/TCB
razors all point the same way at this scale). The `#[entry]` proc macro and IPC console
macros port directly; the allocator gets a fixed-region backend.

---

## 6. The two genuinely new design problems

### 6.1 Region allocator (the PT-arena of this port)

Grants require physically contiguous, 32-byte-aligned ranges from a few hundred KB,
with **no compaction possible** (no relocation without an MMU). Fragmentation is
permanent. This is the design-heavy stretch of the whole port and wants the full
treatment: typed model in lockjaw-types, host-tested, property/fuzz coverage for
fragmentation behavior, before any kernel wiring.

Staged approach:

- **v1 (Phase 4): static partitioning at spawn.** Each domain's slot pre-carves its
  grant arena; grants allocate bump/freelist within it. No cross-domain fragmentation
  by construction. This is Tock's grant model in spirit — **study Tock's grant region
  design as prior art** (Rust, MPU, exactly this constraint set) before finalizing.
- **v2 (post-Phase 6): shared pool, buddy or best-fit with alignment-aware splitting,
  per-grant max-size classes.** The pure model should be designed in v1 so v2 is a
  policy swap, not a redesign.
- Funding question (PT-arena analog): who pays for the *descriptor* storage of grants
  and domains? Answer should mirror the PT-arena resolution — descriptor space is a
  pre-flighted, handle-funded cost checked in the validate phase, so grant operations
  are atomic (no half-applied grants; the `add_thread` validate-then-apply lesson).

### 6.2 IRQ binding, redesigned once, landing here first

The irq_bind review found: no unbind, dangling notification/owner KVAs after process
death (device-triggered UAF), unrestartable drivers, PPI-class hijack. **The fix is a
design, not a patch, and Lockjaw-M is where it lands natively:** the binding is an
object referenced by the owning process's handle table, so the existing teardown
machinery revokes it structurally (NVIC disable + notification decref + slot free), and
driver restart = new bind through a fresh handle. Mainline then back-ports the design.
M-class specifics: bindable range is external interrupts only (exception numbers ≥ 16);
SysTick/PendSV/SVC/fault vectors are kernel-reserved by construction (they're not NVIC
lines at all — a nice hardening freebie vs. the INTID-27 hijack found on mainline).
Level-vs-edge masking discipline ports: NVIC `ICER` disable after signaling
level sources, `sys_unmask_irq` → `ISER`, owner-gated, same confused-deputy reasoning.

---

## 7. Phase plan

Idiom matches kv260-bringup.md: each phase has an exit criterion; no phase starts with
debt from the previous one.

### Phase 0 — Design + A-class assumption audit
- File-by-file audit of kernel crate for: 4 KB page assumptions, `KERNEL_VA_OFFSET`,
  EL-model assumptions (EL0/EL1, eret, DAIF), KVA≠PA assumptions, `MAX_*` sizing.
  Output: `lockjaw-m-assumptions.md` with per-file disposition (ports / cfg-split /
  replaced).
- Workspace decision: **arch split, not fork.** `kernel/src/arch/{aarch64,armv8m}/`,
  shared `lockjaw-types`/`lockjaw-rt`/regspec; target selection via cargo target +
  feature. Exit: `cargo check` passes for both targets with armv8m stubs.
- Finalize: domain/grant descriptor types, region budget policy, SVC ABI (syscall
  number in r12 or SVC immediate — pick one; immediate-decoding requires reading the
  stacked PC, r12 is simpler), `MAX_THREADS`/`MAX_BINDINGS` parameterization, slot map.
- Decide BFS/loom obligations for the new domain-manager model up front.
- **Alif track:** `boot-contract-alif.md` (SE/ATOC/SETOOLS chain, security state at
  handoff, dev-loop cycle time measured), UART IP identification for the silicon
  regspec, TCM/SRAM/MRAM placement decision.

### Phase 1 — Boot to UART (QEMU `mps3-an547`)
- `thumbv8m.main-none-eabi` (soft-float kernel ABI), linker script, vector table
  (initial-SP word at offset 0 — essentially no boot asm), `cmsdk_uart.toml` regspec.
- Read and assert ground truth: CPUID, MPU_TYPE.DREGION, FPU presence (MVFR), ICTR.
- Exit: banner + kprintln on QEMU AN547 and AN505 (proves M55/M33 portability of the
  boot path on day one).

### Phase 2 — Faults, SysTick, SVC
- Fault decode in lockjaw-types (`armv8m::fault`): CFSR (MMFSR/BFSR/UFSR incl. STKOF),
  HFSR, MMFAR/BFAR validity bits — the ESR-decode pattern, host-tested.
- Structured crash output ported (classify_address gets an M-class memory-map table).
- MSPLIM armed; deliberate-overflow selftest proves STKOF reporting.
- SysTick configured from ground truth (SYST_CALIB or platform clock); SVC handler
  dispatching a null syscall.
- Exit: tick counter advancing; SVC round-trip; all fault classes produce readable
  crash reports in QEMU.

### Phase 3 — Threads, PendSV, scheduler drop-in  *(proof-of-architecture milestone)*
- TCB (M-class `SavedContext`: r4–r11, PSP, EXC_RETURN, PSPLIM — layout-asserted in
  lockjaw-types beside the A-class one; `SavedContext::synthetic()` constructor from
  day one, per the review finding).
- PendSV switch path; SysTick pends PendSV; `SchedState` + `step`/`step_from_idle`
  used **unchanged**.
- Exit: two kernel threads round-robin on QEMU; scheduler host tests (incl. widened
  two-CPU BFS, once it exists on mainline) pass identically for both arches.

### Phase 4 — MPU domains + unprivileged execution
- `mpu.toml` regspec (needs §8 emitter features — schedule those first).
- Domain manager + region allocator v1 (static slots); domain descriptors; MPU
  reprogram in PendSV path.
- First thread drops to unprivileged thread mode (CONTROL.nPRIV) on PSP with PSPLIM;
  SVC syscalls; MemManage faults on domain violations produce the Phase 2 crash report
  with domain/grant context.
- Exit: unprivileged thread runs, syscalls, and a deliberate out-of-domain store
  produces a clean MMFSR report identifying the domain and faulting region.

### Phase 5 — IPC, grants, NVIC binding
- Handle table + notifications + endpoints (lockjaw-types unchanged); grant
  create/transfer/revoke as handle ops; teardown plan extended with M-class steps
  (RevokeGrants, UnbindIrqs — the redesigned binding object, §6.2).
- NVIC regspec (`nvic.toml`) with w1s/w1c kinds (§8.1) and byte-lane IPR (§8.2).
- Exit: two unprivileged processes exchange messages; an IRQ-bound notification wakes
  a driver thread; killing the driver revokes its binding and grants (verified by a
  host-side teardown-plan test *and* a QEMU integration test), and the driver restarts
  into the same slot.

### Phase 6 — lockjaw-rt userspace + first real unprivileged driver  *(README milestone)*
- lockjaw-rt M-class backend (allocator over slot heap, console over IPC, `#[entry]`).
- One real driver unprivileged end-to-end. QEMU: CMSDK UART or PL011 on the AN547 FPGA
  map. Silicon: Alif UART (or a simple sensor); RP2350 UART as the SMP-build variant.
- The demo: driver panics (deliberate), kernel reports it, revokes, restarts it, system
  continues. Record it; this is the artifact for the FreeRTOS conversation.
- **Post-Phase 6 showcase (Alif):** unprivileged Ethos-U55 NPU driver with DMA grant
  donation — the strongest single exhibit this plan can produce.
- Exit: demo reproducible from a clean checkout on QEMU and at least one silicon target.

### Hardware bring-up track (parallel from Phase 3)

**Primary: Alif Ensemble (M55).** DevKit/AppKit class board, dual asymmetric M55
(HP @ ~400 MHz with I/D caches + Helium, HE @ ~160 MHz) — target the **HP core first**.
Specifics that shape the plan:

- **Boot is owned by the Secure Enclave (SE) processor.** Images live in MRAM under an
  ATOC (application table of contents) provisioned via Alif SETOOLS; the SE loads and
  releases application cores per that config, and debug is SE-mediated. This is the
  heaviest boot contract of any Lockjaw target to date — `boot-contract-alif.md` is a
  **mandatory Phase 0 deliverable** (what the SE configures: clocks, power domains,
  security state handed to the core, MRAM/XIP state, TCM init) and the dev-loop
  definition (MRAM program cycle time, SE debug stub) must be measured before
  committing to it as the iteration vehicle. QEMU AN547 remains the fast loop.
- **M55-HP has real I/D caches** — the DMA clean/invalidate discipline (cfg'd cache ops,
  §2.2) is live on first silicon, not compiled out. SCB cache-maintenance-by-MVA path
  gets a selftest in Phase 4.
- **Memory geometry:** per-core ITCM/DTCM + multi-MB shared SRAM + MRAM. Placement
  decision (Phase 0): kernel hot path in TCM, userspace slots in SRAM, text XIP from
  MRAM. Slot/fragmentation pressure is far lower than the 520 KB RP2350 case — but the
  region *count* budget is unchanged, so §3.1 arithmetic still governs.
- **HP+HE are AMP, not SMP** — separate subsystems, no inter-core cache coherency. On
  Alif the GKL degenerates to PRIMASK (single-core build). Future work, not this plan:
  dual Lockjaw-M instances with inter-core IPC over the MHU mailboxes.
- **Ethos-U55 NPU** is the showcase driver target post-Phase 6: an unprivileged,
  restartable NPU driver with DMA grant donation is the strongest possible artifact
  for the FreeRTOS/Zephyr-displacement argument (this is exactly the silicon class
  that conversation is about).
- Peripheral regspecs: Alif's UART IP differs from AN547's CMSDK UART — confirm the IP
  (vendor docs / HAL headers) in Phase 0 and budget a second `*.toml`; the regspec
  layer makes this a data swap, not a port.

**Secondary: RP2350 (Pico 2).** Retained deliberately even with Alif primary, because
Alif demotes SMP to AMP: the Pico 2's dual coherent-by-construction M33s are the only
M-class target where the GKL, ticket lock, obligation-transfer reasoning, and the
two-CPU BFS run in anger. $5 keeps it in the loop. Boot via boot ROM (UF2/picotool);
no DTB — ground truth is a `platform` const table asserted against ID registers. Core 1
launch is the SIO mailbox/FIFO protocol (not PSCI); boot ROM uses SIO spinlock 31
(leave it alone). 8 MPU regions per core is why the §3.1 design floor stays at 8.

M55 ISA-level work (lazy FP, MVE-exclusion check, cache ops) is carried by QEMU AN547
between silicon sessions.

---

## 8. regspec emitter features required (schedule before Phase 4/5)

All four are general-purpose and pay off on mainline too (NVIC and GIC share idioms):

1. **`w1s`/`w1c` paired registers** — NVIC ISER/ICER/ISPR/ICPR (and GICD
   ISENABLER/ICENABLER on mainline). Emit `set_bit(n)`-only accessors; make RMW
   *unrepresentable* for these kinds (an RMW on a w1c pair is a bug, not a style issue).
2. **Byte-lane / packed-field array registers** — NVIC_IPR (one priority byte per IRQ,
   four per word; only top N bits implemented — read PRIGROUP/implemented-bits at boot)
   and GICD_IPRIORITYR/ITARGETSR on mainline. Generated `priority(irq).write(p)` with
   internal aligned-word RMW; kills the hand-rolled `(n % 4) * 8` arithmetic the GIC
   review flagged.
3. **Write sequencing/wait constraints** — declarative "disable before reconfigure,
   DSB/ISB after enable" for MPU regions; same feature family as GICv3 RWP and SDHCI
   inhibit polling. Generated write returns only after the constraint is satisfied.
4. **(Nice-to-have) banked/aliased annotations** — `banked = per_core` (RP2350: each
   core has its own NVIC/MPU/SysTick) as machine-readable documentation in the TOML, so
   the boot-contract facts live in the single source of truth, not comments.

No sysreg-block emitter needed for M (everything is MMIO) — that feature remains a
GICv3-only want.

---

## 9. Verification strategy

- **Unchanged-model regression:** the scheduler BFS, IPC decision tests, and teardown
  plan tests must pass identically from both arch builds' perspective — CI runs the
  lockjaw-types suite once, but Phase 3's exit criterion includes demonstrating the
  M-class kernel consumes the same model APIs (no forked model code).
- **New models, same treatment:** domain manager + region allocator get
  reachable-state/property tests *before* kernel wiring (fragmentation invariants:
  no overlap, alignment, budget never exceeded, revoke restores budget, grant ops
  atomic under failure injection).
- **Fault-injection selftests on target:** deliberate STKOF, deliberate MemManage,
  deliberate driver panic+restart — each a named selftest, run in QEMU CI for both
  AN547 and AN505.
- **Ticket lock:** the loom test (mainline action item) covers RP2350's dual-core use
  for free.
- **No-FP-in-kernel check:** xtask sibling scanning kernel text for FP/MVE encodings
  (the no-NEON check's M-class analog) — guards the lazy-stacking and FP-free-kernel
  assumptions.

---

## 10. Risks and open questions

1. **Region scarcity vs. driver reality** — a driver needing MMIO + DMA buffer +
   shared ring + its 3–4 fixed regions is at budget on 8-region silicon. Mitigations:
   coalesce fixed regions (text+rodata), PSPLIM-only thread stacks, grant coalescing.
   If 8 proves unlivable, M55's 16 regions becomes a real tiering decision. **Resolve
   with arithmetic in Phase 0 against the Phase 6 driver's actual needs.**
2. **Fragmentation under restart churn** — v1 static slots dodge it; v2 must prove
   (host properties) that kill/restart cycles don't strand the grant pool.
3. **Fixed-slot linking ergonomics** — N-slot × M-binary build matrix; acceptable for
   the demo, annoying at scale. Static relocation at load is the planned escape hatch.
4. **Alif Secure Enclave boot contract** — the SE/SETOOLS/ATOC chain is the most
   opaque boot environment of any Lockjaw target; provisioning friction could make the
   silicon dev loop slow enough that QEMU drift becomes a real risk. Mitigate: measure
   the MRAM program cycle in Phase 0, keep AN547 as the inner loop, and treat
   `boot-contract-alif.md` as a gating deliverable (the armstub8 lesson, applied in
   advance). RP2350's boot ROM contract is the simpler sibling — same doc treatment.
5. **SMP coverage depends on the secondary target** — Alif is AMP, so dropping RP2350
   from the plan would leave the GKL/ticket-lock/two-CPU-BFS story untested on M-class
   entirely. The Pico 2 track is cheap insurance, not optional polish. Dual-core
   RP2350 bring-up (SIO mailbox launch for core 1) is a small new code path.
6. **QEMU fidelity** — AN547's MPU/fault modelling is good but QEMU has historically
   been forgiving about MPU programming-sequence violations; the §8.3 sequencing
   constraints exist precisely so QEMU-passing code is also silicon-correct (the
   GICv3 reset-state lesson, applied forward).
7. **Erratum exposure** — M-class parts have errata too (RP2350-E9 pad-isolation is
   famous, GPIO-relevant for Phase 6 hardware demos). Track per-board in the boot
   contract doc.

---

## 11. Relationship to mainline / sequencing

- Mainline review fixes that should land **before** the port forks attention:
  irq_bind redesign (designed once, per §6.2 — implement M-first or design-doc-first,
  but one design), `SavedContext::synthetic` (Phase 3 needs the M version anyway),
  loom test + two-CPU BFS (Phase 3/RP2350 depend on them as the safety net),
  `MAX_*` parameterization (Phase 0 deliverable touching shared crates).
- Mainline items explicitly *not* blocking: GICv3 hardening cluster, KV260 phases
  (parallel track; no shared files beyond lockjaw-types, which is additive here).
- Naming: working name **Lockjaw-M**; kernel crate feature `arch-armv8m`; docs under
  `docs/plans/lockjaw-m/`.

## 12. Immediate next actions

1. Phase 0 assumption audit (mechanical; good Opus batch task with file list from
   `grep -rn 'KERNEL_VA_OFFSET\|PAGE_SIZE\|TTBR\|DAIF\|eret' kernel/src/`).
2. Domain/grant descriptor type design in lockjaw-types (design session — the one
   decision everything in Phases 4–5 hangs off).
3. regspec emitter: w1s/w1c kind (smallest of the §8 features; immediately useful for
   GIC mainline too — do it first as the warm-up).
4. Order hardware: Alif Ensemble DevKit (primary) + Pico 2 (SMP secondary); start the
   SETOOLS/boot-contract reading while they ship.
