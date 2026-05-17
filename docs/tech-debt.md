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

**Where:** `src/process.rs::provision_resources` (4 sites: `proc_range`, `ht_range`, `tcb_stack_range`, `tcb_range`); `src/syscall/handler.rs::sys_create_thread` (2 sites: `stack_range`, `tcb_range`); `src/arch/aarch64/vmem.rs::AddressSpaceBuilder::new` + `map_batch` (page-table internal nodes).

**What:** `sys_create_process` and `sys_create_thread` allocate kernel-side pages internally via `kvm::alloc_kernel_pages` and `page_alloc::alloc_page`. Every alloc point is a potential `OUT_OF_MEMORY` syscall return that complicates error paths and weakens the type-level "kernel cannot fail from memory" invariant.

The endpoint/notification/reply syscalls already follow the seL4-style "user donates a page, kernel transmutes it in place" pattern (`kvm::map_existing` rather than `kvm::alloc_kernel_pages`). Process creation deviates: ProcessObject, HandleTable, TCB, and the per-thread kernel stack are all kernel-allocated rather than user-donated.

**Why bootstrap:** Adding more PageSet arguments to `sys_create_process` was deferred — userspace would need to allocate 4 additional pages and donate them before each spawn, which the early-bootstrap init code wasn't yet ready to do.

**Categories with their fix paths:**

- *ProcessObject / HandleTable storage / TCB / kernel stack* — all donate-able. Should follow the endpoint pattern: spawn syscall takes additional PageSet handles, kernel calls `kvm::map_existing` per donated page. Removes 4–6 OOM sites with no architectural cost beyond a wider syscall signature.
- *Page-table internal nodes (L1/L2/L3 inside `AddressSpaceBuilder`)* — genuinely harder. seL4's strict model has userspace explicitly insert each page-table level via separate caps (verbose). The pragmatic compromise everyone takes is kernel-on-demand allocation. Could move to a per-process pre-allocated "page-table pool" (user donates N pages at spawn, kernel carves from them) — same shape as the run-queue fix above.
- *PageSet header (`src/cap/pageset_table.rs::alloc_and_insert_header`)* — kernel-side allocation of the metadata page that tracks user-donated pages. Self-referential (can't be donated as part of the same PageSet it would track). Could take an extra PageSet for the header, or carve the header out of the donated data range.

**Fix order:** ProcessObject and TCB first (lowest friction, removes the most-frequently-hit OOM sites). Then HandleTable and kernel stack. Then page-table pools. The donate-pattern syscalls are easier to add than the page-table case because the user already allocates the per-thread user stack PageSet — adding 4 more PageSet args is mechanical.

**Why this is a real violation, not just style:** every kernel alloc site is a runtime failure that needs error handling, type-level OOM in syscall returns, and rollback code on the failure path. The endpoint/notification/reply syscalls have zero of these because they don't allocate. Closing the remaining sites would shrink the kernel's failure surface meaningfully and make `OUT_OF_MEMORY` rare enough to treat as a hard invariant rather than a routine return.

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

**Why bootstrap:** The UART driver needs exactly one handle (the IPC endpoint from init). One handle is enough for current bootstrap shapes; everything else flows through `sys_export_handle` post-spawn.

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

**Where:** `src/syscall/handler.rs` (sys_create_thread), and any kernel path that allocates multiple resources and rolls back manually on failure.

**What:** The `HeaderPageGuard` pattern in `src/cap/pageset_table.rs` is the model: RAII guards that free resources on drop unless explicitly taken. `create_process` now uses `PageGuard` + `Ttbr0Guard` bundled into a `ProvisionedResources` struct returned by `provision_resources`; the orchestrator defuses each guard explicitly before handing the addresses off to apply. The pattern still needs to spread to other manual-rollback paths — most visibly `sys_create_thread` in `src/syscall/handler.rs` (around line 757) where TCB and stack pages are deallocated by hand on error.

**Fix:** Continue applying the guard pattern to remaining manual-rollback paths. Each new fallible allocation chain should reach for guards by default; multi-resource allocations should bundle them in a struct so future additions force an explicit defuse step.

---

## DTB-driven baud rate computation

**Where:** `src/arch/aarch64/uart.rs` (init_baud)

**What:** `init_baud()` hardcodes IBRD=26 / FBRD=3 for a 48 MHz UARTCLK (Pi 4B and QEMU). Future platforms with a different UARTCLK will produce the wrong baud rate. The proper fix is to read `clock-frequency` from the DTB's UART node and compute the divisors dynamically.

**Why bootstrap:** The Pi 4B DTB does not expose `clock-frequency` directly on the UART node — it uses phandle references into the VideoCore clock controller, which would require a full clock framework to resolve. Both currently supported platforms (QEMU virt, Pi 4B) use 48 MHz UARTCLK.

**Fix:** Implement a minimal clock resolution pass: follow the UART node's `clocks` phandle to its parent clock node and read `clock-frequency`. Compute IBRD/FBRD from the discovered clock rate. Until then, init_baud is called unconditionally with fixed 48 MHz divisors.

---

## Audit: push kernel state into lockjaw-types

**Where:** Kernel-side modules in `src/` that contain pure state machines, data structures, or decision logic with no hardware or `unsafe` dependencies.

**What:** Substantial progress: lockjaw-types has grown to ~16 K LOC across 25+ modules covering IPC state machine, scheduler model, process lifecycle/transfer/teardown, PageSet table + variable-size header, POSIX dispatch + VA layout, FAT32, FDT, and ELF loader. Kernel-side `src/cap` + `src/ipc` + `src/sched` together still hold ~3300 LOC. `docs/extraction-roadmap.md` lists the remaining priority targets.

**Outstanding push-shaped kernel code:**
- `sys_map_pages` VA decision (Priority 1 in extraction-roadmap)
- PageSet alloc rollback (Priority 1)
- Endpoint and notification runtime handlers (~560 LOC combined)
- Scheduler context-switch integration (~730 LOC)
- Revocation walks (~260 LOC) — recently added; could likely have parts extracted

**Fix:** Continue the push→pull conversion per `docs/extraction-roadmap.md`. Each new feature should land its decision logic in lockjaw-types first; the kernel mechanically executes.

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

## uart-driver bind_irq fails on Pi 4B

**Where:** `user/uart-driver/src/main.rs` IRQ-bind step. Surfaces in Pi log as `uart-driver: bind IRQ FAILED` after `uart-driver: notification created`. QEMU virt path works fine.

**What:** PL011 IRQ binding succeeds on QEMU virt (GICv3, INTID assigned by virt platform code) but fails on Pi 4B (GICv2, INTIDs come from the BCM2711 DTB). The Pi log shows devmgr decoding INTID 153 for `0xfe201000` — but `153` is in the kernel-reserved range as labeled in the same log line (`intid=153 (kernel, reserved)`), so the bind syscall refuses. Either the DTB decode is wrong (PL011 IRQ is somewhere else on BCM2711) or the kernel-reserved range is too aggressive on Pi.

The user-facing symptom is no UART input from the Pi — output works (kernel UART path) but typed characters don't make it to userspace.

**Why bootstrap:** Pi 4B was added late and the GICv2 + BCM2711 IRQ map differs from QEMU virt enough that the M0/M1 milestones got the platform booting on hardware but didn't gate the userspace UART path on Pi.

**Fix:** Audit the BCM2711 DTB IRQ encoding for PL011 (probably `interrupts = <GIC_SPI 121 IRQ_TYPE_LEVEL_HIGH>` per the BCM2711 binding, not 153). If the decode is right and 153 is just out of the kernel's whitelist on Pi, narrow the reserved range. Out of the emmc2 critical path; lands when someone needs Pi UART input.

---

## Per-process UART output not serialized on Pi

**Where:** Kernel UART writer in `src/io/uart.rs` (or wherever `puts` lands per platform). QEMU virt path holds the GKL across the whole emit so output never interleaves; Pi 4B path doesn't.

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
