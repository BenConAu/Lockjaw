# Technical Debt Log

Known limitations introduced for bootstrapping. Each item documents what we did, why, and what the eventual fix is. Unlike the YAGNI parking lot (removed code to add later), this tracks code that exists but is limited.

---

## Static IRQ binding table (96-slot cap)

**Where:** `src/arch/aarch64/irq_bind.rs`

**What:** IRQ-to-Notification bindings stored in a static array of 96 entries (raised from 32 once UART + virtio-blk + virtio-mmio device probing started filling it). No unbind, no conflict detection, no ownership tracking. The kernel still owns the table directly even though the device-manager process is now the policy authority for who gets which device.

**Why bootstrap:** A single UART IRQ binding doesn't need more, and the device manager + drivers we have today fit under 96. Moving the table into the device manager would require an IPC round-trip on every bind.

**Fix:** Move the binding table into the device manager process; the kernel syscall stays simple (one binding at a time), and the device manager arbitrates ownership and detects conflicts.

---

## Syscall handler code repetition

**Where:** `src/syscall/handler.rs`

**What:** The `lookup_handle` helper was extracted after noticing the same 10-line pattern copied across 8 syscall handlers. Other patterns may still be repeated — a systematic scrub has not been done.

**Why:** Each syscall was added incrementally, one per milestone. Code was written for the immediate need without looking back at what could be shared.

**Fix:** Periodically scrub the syscall handler for repeated patterns and extract helpers. The lookup_handle refactor is the model for how to do this.

---

## MAX_THREADS = 1024 static scheduler array

**Where:** `lockjaw-types/src/scheduler.rs::MAX_THREADS` (canonical const; imported by `src/sched/scheduler.rs`).

**What:** The run queue is a fixed-size array of 1024 slots (raised from 16, originally 8). Cannot grow. `add_thread` returns false when full → `SyscallError::QUEUE_FULL`. 1024 covers any realistic thread count for the platforms Lockjaw targets; modern aarch64 hardware comfortably runs thousands of Linux threads, so this is comparable to what userspace sees on a typical OS.

**Why bootstrap:** A dynamic run queue requires either kernel allocation (violates our no-kernel-alloc principle) or a user-donated page for the queue.

**Fix:** User-donated page for the run queue, similar to how endpoint/notification/reply pages work today. The scheduler would be initialized with a donated page that holds the thread array, and add_thread would fail with QUEUE_FULL only when the donated page itself is full. Until then, 1024 is generous enough that the cap is not a practical limit.

---

## Kernel-side allocation in process creation (violates microkernel principle)

**Status (post-NK3, 2026-06-10):** PageSet-header allocation closed by
NK2-A/NK2-B (bootstrap-allocated pool). `sys_create_thread`
TCB+stack allocation closed by NK3 (donate-and-claim). The KVM
page-table tree's own L2/L3 growth closed by NK1. **Remaining
runtime `kvm::alloc_kernel_pages` / `page_alloc::alloc_page`
callers:** `src/process.rs::provision_resources` (4 sites:
`proc_range`, `ht_range`, `tcb_stack_range`, `tcb_range`) plus
`src/arch/aarch64/vmem.rs::AddressSpaceBuilder::new` + `map_batch`
for per-process page-table internal nodes. NK4+NK5 close these per
the migration plan at `docs/architecture/no-kernel-alloc.md`.

**Where:** `src/process.rs::provision_resources` (4 sites: `proc_range`, `ht_range`, `tcb_stack_range`, `tcb_range`); `src/arch/aarch64/vmem.rs::AddressSpaceBuilder::new` + `map_batch` (page-table internal nodes).

**What:** `sys_create_process` allocates kernel-side pages internally via `kvm::alloc_kernel_pages` and `page_alloc::alloc_page`. Every alloc point is a potential `OUT_OF_MEMORY` syscall return that complicates error paths and weakens the type-level "kernel cannot fail from memory" invariant.

The endpoint/notification/reply/thread-creation syscalls now follow the seL4-style "user donates a page, kernel transmutes it in place" pattern (`kvm::map_existing` via `donate_one_kernel_page` rather than `kvm::alloc_kernel_pages`). Process creation deviates: ProcessObject, HandleTable, TCB, and the per-thread kernel stack are all kernel-allocated rather than user-donated.

**Why bootstrap:** Adding more PageSet arguments to `sys_create_process` was deferred — userspace would need to allocate 4 additional pages and donate them before each spawn, which the early-bootstrap init code wasn't yet ready to do.

**Categories with their fix paths:**

- *ProcessObject / HandleTable storage / TCB / kernel stack* — all donate-able. NK4 follows the `sys_create_thread` pattern just landed: spawn syscall takes additional PageSet handles, kernel calls `donate_one_kernel_page` per donated page. Removes 4 OOM sites with no architectural cost beyond a wider syscall signature.
- *Page-table internal nodes (L1/L2/L3 inside `AddressSpaceBuilder`)* — genuinely harder. seL4's strict model has userspace explicitly insert each page-table level via separate caps (verbose). The pragmatic compromise everyone takes is kernel-on-demand allocation. NK5 takes a per-process pre-allocated "page-table pool" (user donates N pages at spawn, kernel carves from them) — same shape as the run-queue fix above.

**Fix order (per NK plan):** NK4 = ProcessObject + HandleTable + TCB + kernel stack via donate-and-claim. NK5 = per-process page-table pool. NK6 = `BootstrapAllocator`/`RuntimeAllocator` typestate refactor (Mechanism A from the architecture doc). The donate-pattern syscalls are easier to add than the page-table case because the user already allocates the per-thread user stack PageSet — adding 4 more PageSet args is mechanical.

**Why this is a real violation, not just style:** every kernel alloc site is a runtime failure that needs error handling, type-level OOM in syscall returns, and rollback code on the failure path. The endpoint/notification/reply/thread-creation syscalls have zero of these because they don't allocate. Closing the remaining sites would shrink the kernel's failure surface meaningfully and make `OUT_OF_MEMORY` rare enough to treat as a hard invariant rather than a routine return.

---

## UnsafeCell globals serialized only by GKL

**Where:** `src/cap/pageset_table.rs`, `src/arch/aarch64/irq_bind.rs`

**What:** Kernel globals use `UnsafeCell` with a manual `unsafe impl Sync` and a comment claiming "single-core kernel, no concurrent access." The comments are stale: SMP support landed (Phase 11), and the safety actually comes from the Giant Kernel Lock (`src/sched/gkl.rs`) which serializes all kernel-mode execution. The bare `UnsafeCell` is correct under GKL but the SAFETY comments lie about why.

**Why bootstrap:** Proper per-object locking would let cores execute non-conflicting kernel work in parallel; GKL is the placeholder until then.

**Fix:** Update the SAFETY comments to cite GKL (small fix, keeps current semantics correct on paper). Then, when the kernel grows beyond GKL — replace bare `UnsafeCell` wrappers with a kernel spinlock type (e.g. `SpinMutex<T>` that disables IRQs on lock) and audit every `unsafe impl Sync` for the same pattern.

---

## No safe page abstraction in userspace

**Where:** `user/lockjaw-userlib/src/lib.rs` (`zero_page_at_va`)

**What:** The kernel's `zero_page(PhysAddr)` is safe because `PhysAddr` is a trusted type from the page allocator. The userspace equivalent `zero_page_at_va(u64)` is unsafe because user virtual addresses are raw `u64` values with no type-level proof that they're mapped.

**Why bootstrap:** Userspace has no validated address types. Adding a `MappedPage` type returned by `sys_map_pages` would make page operations safe, but requires designing the userspace memory management API.

**Fix:** Introduce a `MappedPage` type in lockjaw-userlib that wraps a VA returned by a successful `sys_map_pages`. `zero_page_at_va` and other page operations would take `MappedPage` instead of raw `u64`, making them safe.

---

## Single-handle copy at process creation

**Where:** `src/process.rs:98-109`, `src/syscall/handler.rs` (sys_create_process x5 parameter)

**What:** sys_create_process copies at most one handle from the parent's handle table into the child's. x5 = handle index to copy, or u64::MAX for none. PageSet kinds are now explicitly rejected here (see the validate-phase check in `process.rs:98-109`) — callers that need to transfer a PageSet must use `sys_export_handle` instead.

**Why bootstrap:** The PL011 driver needs exactly one handle (the IPC endpoint from init). One handle is enough for current bootstrap shapes; everything else flows through `sys_export_handle` post-spawn.

**Fix:** Either extend to an array of handles (x5 = pointer to handle index array, x6 = count), or treat the current single-handle path as deprecated and route everything through IPC-based transfer. The latter is the proper microkernel approach — capabilities flow through IPC, not just at creation time.

---

## Kernel threads leave stale user TTBR0 in hardware

**Where:** `src/sched/scheduler.rs:435-442`

**What:** Kernel threads are created with `ttbr0_paddr = 0`. The scheduler only writes TTBR0_EL1 when `new_ttbr0 != 0`. When switching from a user process to a kernel thread, the previous user process's page table stays in TTBR0_EL1. An accidental lower-half access from a kernel thread would hit the previous process's memory instead of faulting cleanly.

**Why bootstrap:** Kernel threads only access higher-half addresses (via TTBR1). No kernel code accidentally touches the lower half. The risk is latent.

**Attempted fix (shelved):** Always write TTBR0 on context switch — `EMPTY_USER_L0` for kernel threads, user's table for user threads — with a conditional TLB flush only on kernel→user transitions to avoid a ~20x boot slowdown from per-switch `tlbi vmalle1is`. Patch archived at `docs/archive/fix2-ttbr0-always-write.patch`. Set aside because it still leaves a residual TLB-hit correctness gap and because the directional fix (below) subsumes it.

**Fix:** Now eligible. The kernel relink (commits `17baed3` + `c70c417`) moved the kernel image to L0[1] and removed every lower-half kernel reference; the user-TTBR0 kernel identity has since been deleted. The scheduler can now zero TTBR0 on switch to a kernel thread without breaking anything. The remaining work is: write TTBR0 = `EMPTY_USER_L0` (or zero) on kernel-thread switches, with a conditional TLB flush only on user→kernel transitions to avoid the boot slowdown.

---

## BlockToken discipline is opt-in

**Where:** `src/sched/scheduler.rs` (BlockToken, scoped_mut), consumed by `src/ipc/endpoint.rs`, `src/ipc/notification.rs`

**What:** The BlockToken + scoped_mut pattern enforces at compile time that no `&mut T` reference to a shared kernel object survives across `block_current()`. However, the protection is opt-in: code that uses bare `unsafe { &mut *ptr }` instead of `scoped_mut(ptr, &mut tok)` bypasses the guardrail entirely. The token only protects functions that participate in the protocol.

**Current state:** All four blocking IPC paths (ipc_send, ipc_receive, ipc_call, notification_wait) use the token protocol. Bare `&mut *ptr` casts in `endpoint.rs` (lines 103, 188, 207) and `notification.rs` (line 87) sit on fast paths that return without ever calling `block_current()` — the comments explain why. No machine enforcement yet.

**Fix:** Enforce via review and grep: `grep -n '&mut \*' src/ipc/` should return only `scoped_mut` calls in blocking functions. If a new blocking IPC function is added, it must use BlockToken. Consider an xtask lint that checks for bare `&mut *raw_ptr` in IPC files.

---

## SYS_RECV_NB naming inconsistency

**Where:** `lockjaw-types/src/syscall.rs:91`, `src/syscall/handler.rs:61`, `user/lockjaw-userlib/src/syscall.rs:271`

**What:** The syscall is named `SYS_RECV_NB` / `sys_recv_nb` but every other IPC syscall spells out "receive" (`SYS_RECEIVE`, `sys_receive`). Inconsistent abbreviation.

**Fix:** Rename to `SYS_RECEIVE_NB` / `sys_receive_nb` across all three files.

---

## lockjaw-userlib minor cleanup

**Where:** `user/lockjaw-userlib/`

**What:** Initial extraction left several minor issues. Status updated:

- ~~`ProcessMapping` is defined in both userlib and the kernel~~ — **resolved**: `user/lockjaw-userlib/src/process.rs:2` re-exports from `lockjaw-types::process::ProcessMapping`.
- No module-level doc comment on `lib.rs` — still missing.
- Inconsistent re-export strategy: `syscall::*` and `print::*` are glob-exported, `lockjaw_types::elf` is namespaced. Should be a documented choice.
- Empty `[lib]` section in `Cargo.toml` (line 11) — still present, no effect, just noise.

**Why:** Low-priority cleanup. None of these cause bugs.

**Fix:** Add doc comment. Standardize re-exports. Drop the empty `[lib]` section.

---

## Display DDI mode index race with hotplug displays

**Where:** `lockjaw-types/src/display.rs` (DisplayRequest::SetMode, GetMode)

**What:** The DDI identifies modes by list index. If the display is hotpluggable (e.g., HDMI), the mode list can change between `list_modes`/`get_mode` and `set_mode`. A client could request mode index 2 while the list has shifted, setting the wrong resolution.

**Why:** QEMU ramfb has a static mode list. No hotplug, no race.

**Fix:** Stable mode IDs (e.g., hash of width+height+refresh+format), or a generation counter the client passes with `set_mode` so the driver can reject stale requests.

---

## sys_close_handle ownership semantics

**Where:** `src/syscall/handler.rs` (sys_close_handle), all userspace programs

**What:** `sys_close_handle(handle)` reclaims the handle table slot and, for PageSet handles, decrements refcount via the revocation path (`free-on-zero` semantics). For non-PageSet handles (endpoint, notification, reply), the slot is freed but the underlying object is not — by design, since these are donated pages owned by the consuming process and never refcounted. The original "no backing memory freed" framing conflated these two cases.

**Current state:** PageSet handles correctly free pages on last-close (handle revocation walks all processes' handle tables before freeing). Non-PageSet objects remain effectively immortal — their `refcount` field on `ObjectHeader` exists but is not wired into any lifecycle for endpoint/notification/reply types.

**Fix:** Decide whether endpoints/notifications/replies should be refcounted (matches the "everything is a handle" microkernel ideal) or remain donation-owned (matches the current "you donated the page, you decide when to reclaim" model). If refcounted, extend the close path to drop them on last-close; if donation-owned, document that explicitly somewhere user-visible.

---

## KVM free path: 64-page deferred-dealloc buffer

**Where:** `src/mm/kvm.rs::free_kernel_pages_inner` (the `to_free: [PhysPage; 64]` buffer + the in-loop `dealloc_page` fallback when the buffer fills).

**What:** Backing frames captured during `KvmFreeWalk` are queued in a fixed-size 64-entry stack array and drained AFTER the post-clear TLBI sequence. This guarantees no other caller can re-allocate a frame while its translation might still be cached in the TLB. If a free request exceeds 64 pages, the overflow path falls back to deallocating mid-walk — BEFORE the TLBI runs.

**Why it's safe today:** Single-core kernel + GKL serializes all kernel paths; IRQs are masked through the create_process critical section. No concurrent caller can observe the freed-but-still-mapped frame. The largest current KVM allocation is `header_pages_for(MAX_PRACTICAL_PAGES_PER_SET=16384) = 33` pages, well under 64.

**When it becomes a real bug:** (a) KVM is extended to back larger objects (>64 pages); (b) GKL breaks up for SMP and another CPU can observe the stale TLB entry → frame-aliasing memory corruption. Either makes the dead branch reachable.

**Fix:** Defer all backing-frame deallocations through a mechanism that doesn't have a fixed-size stack buffer. Options: a per-CPU pending-free list maintained outside the walk; chaining the freed paddrs through the (now-cleared) L3 PTE slots themselves; or splitting large frees into ≤64-page batches each with their own TLBI sweep. Track this when KVM picks up its second user (handle tables, mapping scratch, etc.) or as part of the SMP work.

---

## Audit: drop guards for resource cleanup

**Status (post-NK3, 2026-06-10):** `sys_create_thread`'s manual
rollback is gone — donate-and-claim + `MappedKvmRangeGuard` (inside
`donate_one_kernel_page`) + scheduler `has_room()` preflight
replaced it. The remaining manual-rollback paths are in
`src/process.rs` (process creation) and will be cleaned up by
NK4+NK5 as those phases migrate to donate-and-claim.

**Where:** Any kernel path that allocates multiple resources and rolls back manually on failure. After NK3, the prominent remaining example is `src/process.rs` (process creation), where ProcessObject / HandleTable / TCB / stack allocations are still rolled back by hand.

**What:** The `HeaderPageGuard` pattern in `src/cap/pageset_table.rs` is the model: RAII guards that free resources on drop unless explicitly taken. `create_process` uses `PageGuard` + `Ttbr0Guard` bundled into a `ProvisionedResources` struct returned by `provision_resources`; the orchestrator defuses each guard explicitly before handing the addresses off to apply. NK3's `donate_one_kernel_page` uses `MappedKvmRangeGuard` to recover the KVA range on init-failure paths.

**Fix:** Continue applying the guard pattern to remaining manual-rollback paths. Each new fallible allocation chain should reach for guards by default; multi-resource allocations should bundle them in a struct so future additions force an explicit defuse step.

---

## DTB-driven baud rate computation

**Where:** `src/arch/aarch64/pl011.rs` (init_baud)

**What:** `init_baud()` hardcodes IBRD=26 / FBRD=3 for a 48 MHz UARTCLK (Pi 4B and QEMU). Future platforms with a different UARTCLK will produce the wrong baud rate. The proper fix is to read `clock-frequency` from the DTB's UART node and compute the divisors dynamically.

**Why bootstrap:** The Pi 4B DTB does not expose `clock-frequency` directly on the UART node — it uses phandle references into the VideoCore clock controller, which would require a full clock framework to resolve. Both currently supported platforms (QEMU virt, Pi 4B) use 48 MHz UARTCLK.

**Fix:** Implement a minimal clock resolution pass: follow the UART node's `clocks` phandle to its parent clock node and read `clock-frequency`. Compute IBRD/FBRD from the discovered clock rate. Until then, init_baud is called unconditionally with fixed 48 MHz divisors.

---

## Audit: push kernel state into lockjaw-types

**Where:** Kernel-side modules in `src/` that contain pure state machines, data structures, or decision logic with no hardware or `unsafe` dependencies.

**What:** Substantial progress: lockjaw-types has grown to ~16 K LOC across 25+ modules covering IPC state machine, scheduler model, process lifecycle/transfer/teardown, PageSet table + variable-size header, POSIX dispatch + VA layout, FAT32, FDT, and ELF loader. Kernel-side `src/cap` + `src/ipc` + `src/sched` together still hold ~3300 LOC. `docs/tracking/extraction-roadmap.md` lists the remaining priority targets.

**Outstanding push-shaped kernel code:**
- `sys_map_pages` VA decision (Priority 1 in extraction-roadmap)
- PageSet alloc rollback (Priority 1)
- Endpoint and notification runtime handlers (~560 LOC combined)
- Scheduler context-switch integration (~730 LOC)
- Revocation walks (~260 LOC) — recently added; could likely have parts extracted

**Fix:** Continue the push→pull conversion per `docs/tracking/extraction-roadmap.md`. Each new feature should land its decision logic in lockjaw-types first; the kernel mechanically executes.

---

## emmc2: CAPABILITIES.base_clock_mhz == 0 fallback not implemented

**Where:** `user/emmc2-driver/src/main.rs` (M2 entry point: the explicit `caps.base_clock_mhz == 0` guard).

**What:** Per `lockjaw_types::sdhci`, `base_clock_mhz == 0` is the SDHCI capability hole that means "controller did not advertise its base clock; driver must source the value elsewhere." The driver currently logs a clear diagnostic and `sys_exit()`s, which is safe but doesn't unblock any controller that hides this field. On Pi 4B and QEMU virt this never triggers — the BCM2711 emmc2 advertises 100 MHz — so the gap is latent.

The Pi log also showed a separate but related fact: `[CPRMAN] EMMC2 set_rate(...) -> actual=...` reported 200 MHz from cprman's *self-test*, while the emmc2 driver later asked for and got 100 MHz back from CPRMAN. CAPABILITIES (100 MHz) and what CPRMAN can deliver (200 MHz) aren't necessarily the same number, and assuming they are baked the M2-v1 bug into a "matched" state by accident.

**Why bootstrap:** Every controller we've tested advertises a non-zero `base_clock_mhz`, so the immediate-exit path was the cheapest correct behavior for M2.

**Fix:** Two clean options — either is fine, both are bigger than M2's scope:
1. **DTB-sourced rate.** The bcm2711-emmc2 DTB node has a `clock-frequency` (or assigned-clock-rates) property. Read it via the existing devmgr DTB pass and pass into the driver alongside the MMIO claim.
2. **CPRMAN-as-source-of-truth.** Treat `clk.get_rate()` as authoritative — ask CPRMAN what it's actually delivering, ignore CAPABILITIES, derive the SD divisor from CPRMAN's number. Means cprman needs a `get_rate` syscall path (today only `set_rate` exists for EMMC2).

Pick (2) when CPRMAN gets `get_rate` for EMMC2; (1) is a working fallback meanwhile.

---

## pl011-driver bind_irq fails on Pi 4B

**Where:** `user/pl011-driver/src/main.rs` IRQ-bind step. Surfaces in Pi log as `pl011-driver: bind IRQ FAILED` after `pl011-driver: notification created`. QEMU virt path works fine.

**What:** PL011 IRQ binding succeeds on QEMU virt (GICv3, INTID assigned by virt platform code) but fails on Pi 4B (GICv2, INTIDs come from the BCM2711 DTB). The Pi log shows devmgr decoding INTID 153 for `0xfe201000` — but `153` is in the kernel-reserved range as labeled in the same log line (`intid=153 (kernel, reserved)`), so the bind syscall refuses. Either the DTB decode is wrong (PL011 IRQ is somewhere else on BCM2711) or the kernel-reserved range is too aggressive on Pi.

The user-facing symptom is no UART input from the Pi — output works (kernel UART path) but typed characters don't make it to userspace.

**Why bootstrap:** Pi 4B was added late and the GICv2 + BCM2711 IRQ map differs from QEMU virt enough that the M0/M1 milestones got the platform booting on hardware but didn't gate the userspace PL011 path on Pi.

**Fix:** Audit the BCM2711 DTB IRQ encoding for PL011 (probably `interrupts = <GIC_SPI 121 IRQ_TYPE_LEVEL_HIGH>` per the BCM2711 binding, not 153). If the decode is right and 153 is just out of the kernel's whitelist on Pi, narrow the reserved range. Out of the emmc2 critical path; lands when someone needs Pi UART input.

---

## Per-process UART output not serialized on Pi

**Where:** Kernel UART writer in `src/arch/aarch64/pl011.rs` (or wherever `puts` lands per platform). QEMU virt path holds the GKL across the whole emit so output never interleaves; Pi 4B path doesn't.

**What:** On Pi log captures, lines from concurrent userspace processes regularly interleave mid-string (e.g. `devmgr: PL011 at 0xfe201000[EXIT] Thread 6 cleaned up...`). On QEMU output is character-by-character clean because GKL ordering happens to serialize the writes. On Pi the same code path is racier — possibly because BCM2711 PL011 init differs, or because the puts loop on Pi releases and reacquires something the QEMU path doesn't.

The functional correctness isn't affected, but every Pi log read is harder than it should be — grepping for a specific message can miss it because the prefix and suffix landed on different "lines" of stdout.

**Why bootstrap:** The QEMU path looks clean, so the bug-class hid until we started capturing Pi logs in earnest during the sleep-primitive plan.

**Fix:** Make the UART writer atomic w.r.t. other CPUs/processes — a simple mutex around the per-byte loop, or a proper kernel print buffer. The right shape is probably a ticket-locked per-CPU print buffer flushed on either newline or timer tick, but a coarse spinlock around the existing path would fix the readability problem immediately.

---

## xtask lint for userspace busy-spin anti-patterns

**Where:** `xtask/src/main.rs`, alongside the existing check-* commands.

**What:** Search every `user/*/src/**.rs` binary for the two anti-patterns:
  - `loop { sys_yield(); }` or `loop { sys_yield() }` (variants).
  - `loop { unsafe { asm!("wfi"); } }` or any `loop { wfi }` shape.

Both keep the thread in `Running`/`Ready` state from the scheduler's POV. The first contends for fair round-robin slots every tick; the second is also `Running` because EL0 `wfi` just pauses the CPU until the next IRQ — the scheduler doesn't see a state change. Either pattern monopolizes a scheduler slot on a system that should be idle.

The right primitives — `park_forever` (block-forever) and `sys_exit` (terminate) — exist in `lockjaw_userlib`. The lint forces drivers/clients to use them instead of reinventing footguns.

**Why bootstrap:** Caught during the scheduler-refactor cleanup pass on 2026-05-17. The `init` heartbeat (`loop { ipc_puts; sys_yield(); }`) and several daemons' `halt() = loop { wfi }` were silently inflating perf measurements; the workaround was to audit each binary manually and replace with `park_forever`/`sys_exit`. A CI gate would have caught all of them at once and would prevent reintroduction.

**Fix:** Add `cargo xtask check-userspace-loops` that walks `user/*/src/**.rs` (the existing crates' main bodies are simple enough to grep) and fails on either pattern. Wire into the `build:` Makefile target alongside the other check-* xtasks.

---

## emmc2 SD card has intermittent ~5.6ms read stalls

**Where:** SD card hardware (the Samsung 128GB card we're testing with). Not our driver, not the BCM2711 SDHCI controller, not the kernel scheduler.

**What:** Single ADMA2 reads from LBA 0 occasionally pay an extra ~5.6ms beyond the card-only ceiling (~41us per block at 25 MHz × 4-bit). The 2026-05-17 investigation showed:

- The slow set is **not deterministic between boots**. One boot had n=4 and n=16 consistently slow across 10 passes; a fresh boot of the same kernel/card had n=1, n=8, n=4, n=16, or n=127 slow depending on which pass — different iterations each boot. So it's not block-count-specific.
- `PRESENT_STATE.DAT_INHIBIT` drops at the exact moment `DATA_COMPLETE` fires (`dat_idle == data_to_complete` on every measurement, slow or fast). The controller is not delaying the completion signal; the card is genuinely holding DAT busy for the full stall duration.
- The cmd phase is constantly ~10us regardless of block count, so CMD18+Auto-CMD23 sequencing is fine.
- `d_cs=0` and `new_tick_max_us=0` across the sweep — no scheduler contribution.
- The ~5.6ms quantum is consistent: when a stall happens, it's always ~5.5-5.7ms, never 1ms or 20ms. Even an 11ms outlier on n=127 decomposes as "expected 5500us + one 5500us stall."

The most likely cause is internal flash management on the card (wear leveling, garbage collection, cache refresh, sequencer protocol switch) that takes a fixed ~5.6ms when triggered. The driver waits correctly via `DATA_COMPLETE` polling, so the only effect is observed latency.

**Why bootstrap:** Pi 4B emmc2 M6 ADMA perf measurements showed wild variance (5-30ms slack). The scheduler refactor (commits 960cb67 / f4504dc / aa86ff1) removed several real lockjaw-side contributors. The remaining ~5.6ms variance turned out to be card-side; codex (consulted 2026-05-17) agreed: *"diagnosis lands at card-side read stalls, not ADMA programming, controller bookkeeping, or scheduler interference."*

**Fix:** Nothing to fix in lockjaw. Downstream:

- M7 BlockEngine should measure throughput across many transfers, not single-transfer latency. A single slow read in 1000 amortizes to ~5.6us per read.
- If the application needs latency SLOs, consider testing with a different SD card (industrial / health-monitored cards have lower variance).
- The lockjaw emmc2 driver could optionally retry transfers with abnormally long observed latency, but this is anti-pattern (the card will still complete, just slowly; retrying mid-transfer is racy). Don't.

**Diagnostic primitives kept in tree from this investigation:**
- `sys_sched_telemetry` syscall (kernel + userlib): exposes tick count, ctx switches, TTBR0 writes, tick-handler peak cycles.
- `tick_self_timing` in `src/arch/aarch64/timer.rs`: tracks tick handler `last/max` cycles, exposed via syscall.
- emmc2 perf sweep keeps cmd-vs-data phase split timings (helped this investigation rule out CMD18+Auto-CMD23 sequencing as a cause).

These would help diagnose similar latency mysteries in future drivers.

---

## 72ms tick-handler peak on Pi 4B boot

**Where:** `src/arch/aarch64/timer.rs::handle_tick` (with `tick_self_timing` enabled).

**What:** During a Pi 4B boot the kernel timer handler hit a 72.7 ms peak (`tick_max_us_at_end=72756` after the emmc2 perf sweep, with `new_tick_max_us=0` across every sweep iteration — so the peak happened before the sweep started). The tick handler's body is just `arm_timer + wake_expired_deadlines + tick()`; on paper that should be tens of microseconds, not 72 ms. Something in the boot phase makes one tick handler take ~3 orders of magnitude longer than expected.

`new_tick_max_us=0` per sweep iteration means whatever causes this does NOT recur during steady-state operation — only during boot. So it's not affecting current driver work, but a 72ms tick handler is concerning as the kernel grows (longer handlers eat into latency budgets for real-time-ish drivers).

Likely candidates to investigate:
1. **Cache-cold scan of `wake_expired_deadlines` across MAX_THREADS=1024 TCBs** — if the first tick walks slots that are still in L2/L3 from boot allocation patterns, the scan could be slow. But ~72ms for a 1024-slot walk is hard to justify even cache-cold.
2. **GKL contention during secondary CPU bring-up** — if a secondary CPU is holding the lock for a long boot section while the timer IRQ fires on CPU 0, the IRQ handler waits.
3. **Interrupt-disabled kernel section that runs unusually long during boot** — `init`'s ELF parse + spawn loop holds GKL throughout. If a tick is queued during a long spawn, it fires when GKL releases, but the time-to-acquire-GKL is counted in our self-timing window.
4. **Something specific to Pi 4B boot ordering** (cprman initialization, MMIO probe loops, etc.) — would not appear on QEMU.

**Why bootstrap:** Surfaced 2026-05-17 during the emmc2 perf-sweep investigation when we added `tick_self_timing` and printed `tick_max_us_at_end`. Was tactical to the ADMA chase; deferred to here once the ADMA chase resolved as card-side.

**Fix:** First instrument: log every tick that exceeds e.g. 1 ms with its elapsed time and a label of what was running before/after. Tools to use:
- `tick_self_timing` already in `src/arch/aarch64/timer.rs` (CNTVCT-based).
- `try_current_tcb_kva` to identify the displaced thread.
- A small ring buffer of `(timestamp, elapsed_cycles, current_thread_name)` for the worst N ticks since boot.

After the worst tick is identified, the candidate-list above points at where to look.

---

## QEMU partitioned-image integration test (task #130 deferred)

**Where:** `tests/qemu_integration.sh` + `Makefile test-img` target. Would add a second image `partitioned.img` (MBR + FAT32 partition at LBA 2048) and a second QEMU run asserting the MBR code path.

**What:** The `PartitionBlockEngine::read/write` sector translation (`start_lba + sector` via `checked_add`) is exercised only when `start_lba != 0`. Bare-FAT QEMU runs use `start_lba=0` so the translation is a no-op and a regression in that line would not fail QEMU. The MBR partitioned image would catch the bug class on QEMU without needing a Pi reflash.

**Why bootstrap:** The bug class is already covered from two other angles:
- Host parser tests (`lockjaw-types::partition` — 15 cases) cover the MBR parsing logic.
- Pi 4B flash (task #131) exercises the real path with real `start_lba=2048` against actual SD-card hardware.

The QEMU MBR test would be a defense-in-depth middle layer, not unique coverage. Skipped to avoid spending the file-format setup (printf/dd of a 16-byte MBR partition entry + 2-byte signature into a regular file, plus `mformat -i partitioned.img@@1M`) on a test whose value is mostly redundant once #131 passes.

**Fix:** When a future regression in the sector-translation path makes us want the middle-layer test:
1. Extend `test-img` target with the `partitioned.img` recipe (regular-file ops — `dd` zeros, `printf | dd seek=446` writes the MBR bytes, `mformat -i partitioned.img@@1M` formats the FAT32 partition).
2. Add `test-qemu-gicv3-partitioned` Makefile target running a second QEMU instance with `partitioned.img` and a parameterized version of the integration script that asserts `partmgr: MBR FAT32 partition found` plus the fat32 read.

---

## emmc2: CMD18+Auto-CMD23 cold-boot validation

**Where:** `user/emmc2-driver/src/main.rs::adma2_transfer` (the CMD18 read / CMD25 write + Auto-CMD23 path). Currently `#[allow(dead_code)]`. `Emmc2BlockEngine::read` loops CMD17 one block at a time instead; `Emmc2BlockEngine::write` returns `Err(Unsupported)`.

**What:** The M7 implementation collapsed CMD17 and CMD18+Auto-CMD23 into a single multi-block path. First Pi 4B flash with that code returned `signature=0x0` on the selftest read — no preceding PIO primer was running ahead to mask any CMD18-from-cold issue. The cause is unidentified: could be Auto-CMD23 specifically, could be CMD18 from cold without prior CMD17, could be a controller state-machine gap on BCM2711 BCM2711 emmc2 that the QEMU virtio path hides.

Until that's diagnosed, all reads use CMD17 single-block (validated on Pi as M6 sub-commit 2b sequence), looped for multi-sector requests. Writes are off entirely. The CMD18 dispatch path is removed from production callers; the function is kept as a reference for re-enablement.

**Why bootstrap:** Pi 4B end-to-end target (task #131) is FAT32 read through POSIX. That's all single-block reads in a loop; writes aren't on the critical path; multi-block throughput isn't on the critical path. Disabling CMD18/CMD25 takes the cold-boot question off the table for #131 while still proving the partition-manager → fat32 → POSIX chain works on real hardware.

**Fix:**
1. Establish a controlled A/B test on Pi: same M7 selftest, one flash with CMD17 (current state), one flash with CMD18+Auto-CMD23. Compare the diagnostic output (`buf_phys`, `desc_phys`, first 32 bytes after read) between them to localise where the data is dropped.
2. If CMD18-from-cold is the issue, prepend a CMD17 primer at driver init (read LBA 0 single-block, discard) before declaring the controller ready. Then re-enable CMD18 for multi-block.
3. If Auto-CMD23 specifically is broken, switch to explicit CMD23 (SET_BLOCK_COUNT) before CMD18/CMD25 — same hardware sequence, separate controller register write.
4. Performance impact of CMD17-per-sector: worth measuring once #131 is green. If it's significant (likely on cold-boot bulk reads), re-enabling CMD18 becomes higher priority.

---

## device-manager: incomplete release coverage

**Where:** `user/device-manager/src/main.rs`. Phase 3 added `CMD_RELEASE_BY_ADDR` (paired with `CMD_CLAIM_BY_ADDR`) and `claim_typed` calls it on error paths. Three gaps remain:

1. `CMD_CLAIM_DEVICE` (the older skip-count-based claim) still has no release counterpart. Callers that use that path leak the claim on error.
2. Driver-process exit doesn't notify the device manager. A driver that successfully claims a device, then panics or exits without calling release, leaves the `claimed` bit stuck until device-manager restart.
3. `CMD_RELEASE_BY_ADDR` trusts the caller to have closed the exported MMIO pageset handle BEFORE issuing the release. `claim_typed` enforces this ordering by dropping the guard first, but direct callers could race: clear `claimed`, second driver claims, both ends up mapped. The device-manager has no kernel-side handle-reference visibility today and cannot enforce the discipline itself.

**Fix:**
1. Add `CMD_RELEASE_DEVICE` mirroring `CMD_CLAIM_DEVICE`.
2. Plumb a kernel-side "process exit" hook into device-manager so it can sweep its `claimed[]` table when a driver dies. This needs new kernel surface (likely a notification queue the device manager subscribes to).
3. Add a "release if no references remain" path: either pass the pageset handle in the release RPC so device-manager can ask the kernel about outstanding references, or build a typed `Drop` on `ClaimedDevice` that performs `close-then-release` atomically so direct misuse becomes impossible.

**Why bootstrap:** all current drivers either use `CMD_CLAIM_BY_ADDR` via `claim_typed` (which enforces the ordering) or halt on failure. The leaks only matter for graceful-restart flows that the kernel doesn't yet expose, and the race only manifests if a future caller bypasses `claim_typed`.

---

## PL011 TX wait is unbounded — **DONE (P2 of pl011 plan, 2026-05-31)**

Acceptance met: `user/pl011-driver/src/main.rs::pl011_putc` wraps
`lockjaw_userlib::pl011::write_byte_deadline`, which builds on the
shared `lockjaw_userlib::time::spin_until_or_deadline` primitive
added in P1 (commit `41cb644`). On `TxTimeout` the byte is dropped
and the driver continues — no more infinite-loop on a stuck FIFO.

Per-board deadline at `TX_TIMEOUT_NANOS = Nanos(10_000_000)` (10 ms)
in `pl011-driver/src/main.rs`, tunable without touching lockjaw-userlib.

Trade-off (documented in plan): current callsites (banner / IPC TX
/ IRQ echo) discard `TxTimeout` via `let _`; re-architecting every
TX caller to propagate the error is out of this plan's scope. The
visible improvement is "driver cannot hang," not "every TX failure
visible to caller."

The SDHCI bare-`>=` poll-loop carryforward is its own entry below
("SDHCI poll loops should adopt `spin_until_or_deadline`").

---

## SDHCI poll loops should adopt `spin_until_or_deadline` — **DONE (P4 of pl011 plan, 2026-06-07)**

Acceptance met across all eight bare-`>=` poll loops in
`user/lockjaw-userlib/src/sdhci.rs`. Two clean simple-spin loops
migrated to `spin_until_or_deadline` directly:
- `soft_reset_all` (SW_RST_ALL clear wait).
- `configure_clock`'s INT_CLK_STABLE wait.

Six loops kept their loop structure but swapped bare `>=` for
`deadline.has_expired(monotonic_now())` — the helper would have lost
load-bearing local context:
- `issue_no_data` pre-issue inhibit poll (captures `ps.bits()` in
  `InhibitStuck` error).
- `issue_no_data` CMD_COMPLETE / ERROR poll (three exits with W1C
  side-effects and response-register reads).
- `SdhciDataCompletion` DAT_INHIBIT drain (captures `ps.bits()` in
  `DatInhibitStuck` error).
- `issue_data_transfer` pre-kick inhibit poll (captures `ps.bits()`
  in `InhibitStuck` error).
- `MmcCard<Ready>::power_up_to_ready` ACMD41 loop (10 ms `sleep_for`
  between CMD55+ACMD41 retries; tightening to a busy-spin would
  violate SD §4.2.3.1 timing).
- `MmcCard<Stby>::select` CMD7 DAT0 busy poll (success path does
  the typestate transition with CSD capture inline).

Both shapes get the same correctness benefit: the named
`MonoTicks::has_expired` accessor handles the `NO_DEADLINE` sentinel
correctly. No callsite uses the sentinel today; the structural
property is that a future caller passing `MonoTicks(NO_DEADLINE)` to
mean "wait indefinitely" would be honored.

---

## DmaPool pages are not zeroed on alloc

**Where:** `src/cap/pageset_table.rs::alloc_dma_pages` (kernel-side allocator) → consumed by `sys_alloc_dma_pages` (userland-facing syscall).

**What:** When a user driver calls `sys_alloc_dma_pages(count)`, the kernel returns a PageSet handle backed by `count` contiguous pages from the DMA pool, but the bytes inside those pages can carry stale data from the previous owner. The driver-side `OwnedDmaMapping::alloc_contiguous` calls right after sys_alloc_dma_pages do not zero either. Consumers that want a known-zero buffer (e.g., emmc2 selftest, which uses `ptr::write_bytes(va, 0, 512)` immediately after mapping) must zero it themselves.

Pre-C1 of the cacheable-DMA migration there was a structural reason to skip kernel-side zeroing: the pool was excluded from the kernel direct map, so zero-through-direct-map would create the mixed-attribute alias the M6 substrate forbade. Post-C1 the pool participates in the direct map cacheably and kernel-side zeroing is mechanically possible.

**Fix:** Decide whether `sys_alloc_dma_pages` should zero on alloc. Arguments for: data-leak-by-default prevention if a DMA buffer ever crosses a privilege boundary (today no boundary crossing — drivers are all single-process per buffer — but the rule would be defensive). Arguments against: zeroing 2 MiB worst-case has nonzero cost; most DMA consumers promptly overwrite (driver fills the buffer or kicks the device to fill it), eating a duplicate write. If landed: kernel-side `zero_range_via_direct_map(pool_base + N*PAGE_SIZE, count * PAGE_SIZE)` followed by `cache::clean_range` so the device sees zeros on its first DMA read. Could also be a flag argument on `sys_alloc_dma_pages` so the caller opts in.

**Why bootstrap:** the comment at the alloc site historically cited an alias argument that is no longer the reason. C1's stale-comment cleanup pass made the comment honest about the absence of zeroing without making the policy decision. Track here so the design question is not lost.

---

## emmc2 error-IRQ enable is Pi-fault-path-only (make test cannot guard it)

**Where:** `user/emmc2-driver/src/main.rs` — `Emmc2BlockEngine::new` SIGNAL_ENABLE composition, and the status-decode in `adma2_single_block_read`'s IRQ loop.

**What:** The driver enables three normal-interrupt signals for the data path — `CMD_COMPLETE | DATA_COMPLETE | ERROR` — and the IRQ wait loop decodes the same three. `ERROR` (NORMAL_INT_SIGNAL_ENABLE bit 15) is the master gate for error IRQ delivery; drop it and data-path CRC/timeout/end-bit errors never wake the loop — they surface only as the 1-second `TransferCompleteTimeout` fallback instead of a precise `DataError { err_int_status }`.

**The gap:** `make test` cannot catch a regression that drops `ERROR` (or any required signal-enable bit) from the composition. Two reasons compound:
1. On QEMU virt the emmc2-driver takes the device-absent exit (`[EMMC2:INIT] no bcm2711-emmc2 device on this platform`) before `Engine::new` ever runs — the composition is never executed under test.
2. Even if it ran, QEMU's block path is virtio-blk; there is no SDHCI/Arasan emulation that generates CRC/timeout/end-bit errors, so the error-IRQ path is not exercised anywhere in CI. The failure is Pi-hardware-fault-path-only and silent until a real bus error occurs.

This already bit us once: the P9.6 typed-accessor conversion dropped `| ERROR` from the composition (the generated value-test `NormalIntSignalEnable::ERROR.bits() == 1<<15` did not catch it, because that test is itself generated from the regspec flag — it guards the constant's value, not the driver's use of it). Caught in review, not by a test.

**What would actually catch it:**
- **QEMU SDHCI error injection** — a test device model that asserts ERROR_INT_STATUS bits on command, so the IRQ-driven error path runs in CI. Largest effort; also the only thing that exercises the full error decode, not just the enable bit.
- **Driver host-test harness** — extract `Engine::new`'s register-programming sequence into a host-testable pure step that records its MMIO writes (the P9.0a thread-local MMIO op recorder in lockjaw-mmio already exists for codegen tests) and assert the SIGNAL_ENABLE write includes bit 15. Medium effort; catches the enable-composition regression specifically but not the decode side.
- **Shared "IRQ-driven SDHCI mandatory signal set" const** in a host-tested crate that both the driver consumes and a unit test asserts contains ERROR. Smallest, but a layering smell (emmc2-specific data in shared lockjaw-regs) and only the rule-of-two would justify it — deferred until a second IRQ-driven SDHCI consumer exists.

**Why bootstrap:** emmc2 is the only IRQ-driven SDHCI consumer today and the only block backend on Pi 4B. The error path is correct as written (verified against SDHCI 3.0 §2.2.21 + Linux's `host->ier`); the gap is test-coverage, not behaviour. Tracked here so the next person who touches the signal-enable composition or the regspec int-enable flags knows the change is invisible to `make test` and must be reasoned about against the spec.

---

## emmc2: pre-CMD `NORMAL_INT_STATUS` write-all clear (B4.2)

**Where:** `user/emmc2-driver/src/main.rs::adma2_single_block_read` and `adma2_transfer`, before the `SDHCI_COMMAND` write.

**What:** U-Boot writes `0xFFFF` to `NORMAL_INT_STATUS` before issuing `COMMAND` to guard against stale `CMD_COMPLETE` / `DATA_COMPLETE` bits from a prior operation. Lockjaw only w1c-clears specific bits it observed. Latent: today's interleaving doesn't surface stale-bit races, but a future async/timeout/recovery path could.

**Fix:** Before each command-issuing write in `adma2_single_block_read` and `adma2_transfer`, call `sdhci.clear_normal_int_status(NormalIntStatus(0xFFFF))` (`user/lockjaw-regs/src/sdhci.rs:938`) and `sdhci.clear_error_int_status(ErrorIntStatus(0xFFFF))` (`:944`) to ack any leftover bits. The existing W1C-all idiom is already in use at `user/emmc2-driver/src/main.rs:1195` for the normal-status register. Originally planned in `docs/history/post-c1-fix-plan.md` §B4.2; deferred there because the C1 gate was met without it.

**Why bootstrap:** the current call paths don't observe stale interrupts; only matters when a future caller restarts after a previous abort. Pair with the SdhciCommandInit gating below — both are latent-hygiene items the type-state layer will subsume.

---

## emmc2: `SIGNAL_ENABLE = 0` after soft reset (B4.4)

**Where:** `user/emmc2-driver/src/main.rs` bootstrap, immediately after `soft_reset_all`.

**What:** After `soft_reset_all`, `SDHCI_SIGNAL_ENABLE` carries whatever value the firmware / reset left behind. Today nothing exercises the phantom-IRQ path because IRQ binding is explicit per-bit. Latent: if a future revision enables IRQ mode without writing `SIGNAL_ENABLE` first, stale firmware bits could deliver phantom IRQs.

**Fix:** After `soft_reset_all`, mask both signal-enable registers (SDHCI splits them — normal at 0x038, error at 0x03A — and both have to be zeroed for a full mask): `sdhci.set_normal_int_signal_enable(NormalIntSignalEnable(0))` (`user/lockjaw-regs/src/sdhci.rs:972`) and `sdhci.set_error_int_signal_enable(ErrorIntSignalEnable(0))` (`:983`). Leave a comment that per-bit unmasking is the responsibility of each IRQ binding site. Originally planned in `docs/history/post-c1-fix-plan.md` §B4.4; deferred there because the C1 gate was polling-only.

**Why bootstrap:** the polling path doesn't use SIGNAL_ENABLE; the explicit-write hygiene only matters when IRQ mode is reactivated for a path that doesn't already write the register itself. Land with the next emmc2 IRQ-conversion edit.

---

## `SdhciCommandInit<S>`: no-bypass operation layer — **DONE (O1–O7, 2026-05-31)**

Acceptance met: every `ADMA_ADDRESS` write and every command-issuing
setter in driver source flows through
`lockjaw_userlib::sdhci::SdhciCommandInit` rather than through bare
`sdhci.write_adma_address` / `set_transfer_mode_command`; the type
system + the `check-driver-unsafe` xtask reject programming the
controller without first opening the operation layer. See
[`../architecture/patterns/operation-envelope.md`](../architecture/patterns/operation-envelope.md)
for the family pattern + the canonical SDHCI consumer.

Follow-up **DONE (rename + regime extension plan, 2026-06-07)**:
the four non-SDHCI device families are all migrated.
- `pl011` — pl011 framework-mediation plan, commits
  `41cb644..788c106` (re-export + ban + helpers + SDHCI poll-loop
  follow-on).
- `cprman`, `fw_cfg`, `virtio_mmio` — Phase B of the rename +
  regime extension plan. `lockjaw_userlib::cprman` created;
  `lockjaw_userlib::fwcfg` extended with `pub use FwCfg`;
  `virtio-blk-driver` source was already structurally clean
  (consumed through `lockjaw_userlib::virtio::*` since the SDHCI
  plan landed) — Phase B's ban entry locks in the existing
  property.
`BANNED_DRIVER_MODULE_PATHS` now lists all five families and the
xtask reports `5 driver crates clean`. The structural property —
"no driver source has an AST-level `lockjaw_regs::<module>` use or
path reference" — is now uniform across the codebase. (Some driver
comments still spell `lockjaw_regs::<module>` as intentional ban
self-documentation; the syn-based checker doesn't read comment
text, so they're not part of the enforced contract.) Rule-of-two
extraction for the operation-envelope pattern still waits for a
second consumer
(emmc1, a NIC, etc.) — see the pattern doc's "Variants" section.
