# Codex Kernel Architecture Work Items

Status updated 2026-04-27.

This captures the synthesized architectural review of the kernel with the guiding rule:

- Move pure layout, state machines, validation, bounded collections, planners, and decode logic into `lockjaw-types`.
- Keep the kernel as assembly, pointer mutation, MMIO/sysreg access, page allocation, PTE writes, TLB maintenance, scheduler wake/block effects, and object lifetime side effects.

## Highest-Value Work Items

1. ~~Move `ExceptionContext`, frame layout metadata, and ESR/FAR classification out of exceptions.rs into a new `lockjaw-types` exception module.~~ **DONE.** `lockjaw-types/src/exception.rs`: ExceptionContext with pinned ABI offsets, ESR decode (esr_exception_class, esr_data_fault_status, exception_class_name, data_fault_name), classify_sync_exception. 16 host tests. classify_address and stack overflow detection stay in kernel (layout-coupled).
2. ~~Move `SavedContext` and `Tcb` layout into `lockjaw-types`.~~ **DONE.** `lockjaw-types/src/thread.rs`: SavedContext, Tcb, TcbCreateInfo, ThreadBootstrap, Tcb::init_in_place. 10 host tests with pinned crash-sensitive offsets.
3. ~~Make handle access policy pure.~~ **DONE.** CloseHandleResult and TeardownHandleAction for close/release lifecycle. Pure slot operations (insert, lookup, remove, rights checking) in `lockjaw-types/src/handle_ops.rs`. Rights::contains() and HandleEntry::EMPTY. Kernel handle_table.rs is now thin wrappers: table_slots() then delegate. 22 + 10 host tests across handle_ops and object modules.
4. Add pure constructors for object headers in `lockjaw-types`: replace scattered literal writes in [src/cap/object.rs](/Users/Ben/Code/Lockjaw/src/cap/object.rs) and [src/cap/process_obj.rs](/Users/Ben/Code/Lockjaw/src/cap/process_obj.rs) with `ObjectHeader::new(...)`, `HandleTableHeader::new(...)`, and related constructors.
5. ~~Finish extracting `create_process` decision logic.~~ **DONE.** ProcessTransferPlan in lockjaw-types/src/process.rs with 11 host tests. HandleCleanup in lockjaw-types/src/object.rs with 6 host tests. create_process is now orchestration + side effects only.
6. Move `ProcessMapping` and thread-start validation out of [src/process.rs](/Users/Ben/Code/Lockjaw/src/process.rs) and [src/syscall/handler.rs](/Users/Ben/Code/Lockjaw/src/syscall/handler.rs) into `lockjaw-types::process`.
7. Extract boot-memory reservation planning from [src/mm/page_alloc.rs](/Users/Ben/Code/Lockjaw/src/mm/page_alloc.rs) `init_with_gap()` into `lockjaw-types`, returning free ranges for the kernel allocator to apply.
8. Extract stack layout policy from [src/mm/stack.rs](/Users/Ben/Code/Lockjaw/src/mm/stack.rs) into `lockjaw-types`: stride, guard-page offset, canary region, fill window. Kernel should only write/check the computed range.
9. Add a pure page-table teardown/free-walk model to `lockjaw-types::page_table`, then make [src/arch/aarch64/vmem.rs](/Users/Ben/Code/Lockjaw/src/arch/aarch64/vmem.rs) `free_address_space()` deallocate-only.
10. Move IRQ binding policy from [src/arch/aarch64/irq_bind.rs](/Users/Ben/Code/Lockjaw/src/arch/aarch64/irq_bind.rs) into `lockjaw-types`: reserved INTIDs, duplicate rejection, lookup semantics.

## Second Wave

11. Move timer bookkeeping/tick policy from [src/arch/aarch64/timer.rs](/Users/Ben/Code/Lockjaw/src/arch/aarch64/timer.rs) into `lockjaw-types`; keep CNTV programming and interrupt ack in-kernel.
12. Move pure GIC geometry from [src/arch/aarch64/gic.rs](/Users/Ben/Code/Lockjaw/src/arch/aarch64/gic.rs) into `lockjaw-types`: INTID-to-register math, redistributor geometry, priority-byte addressing.
13. Consolidate platform and per-CPU stack constants from [src/arch/aarch64/platform.rs](/Users/Ben/Code/Lockjaw/src/arch/aarch64/platform.rs) and [src/mm/stack.rs](/Users/Ben/Code/Lockjaw/src/mm/stack.rs) into shared `lockjaw-types` facts so boot, MMU, GIC, UART, and stack code use one source of truth.
14. Promote the `PageSet` value object from [src/cap/pageset.rs](/Users/Ben/Code/Lockjaw/src/cap/pageset.rs) into `lockjaw-types`, leaving allocation/rollback in the kernel.
15. Move owned-page dedup/bounds semantics out of [src/cap/process_obj.rs](/Users/Ben/Code/Lockjaw/src/cap/process_obj.rs) into a pure `OwnedPageList` or equivalent in `lockjaw-types::process`.
16. ~~Move handle-table search policy from handle_table.rs into `lockjaw-types` helpers: first-free-slot, lookup shape, release decision.~~ **DONE.** find_empty_slot, slot_lookup, slot_insert, slot_remove, slot_remove_all_by_object, slot_get/set_mapped_va in handle_ops.rs.
17. ~~Make endpoint.rs and reply.rs execute ipc_state decisions.~~ **DONE.** Kernel-facing decision functions (decide_send/receive/call/reply) in ipc_state.rs. Raw constants, IpcError, typed conversions moved. Kernel handlers rewritten to match-on-decision with typed state inputs. 22 host tests.
18. Promote the queue contract in [src/ipc/ep_queue.rs](/Users/Ben/Code/Lockjaw/src/ipc/ep_queue.rs) to the pure queue model in [lockjaw-types/src/ipc_state.rs](/Users/Ben/Code/Lockjaw/lockjaw-types/src/ipc_state.rs); keep intrusive pointer surgery in-kernel.
19. Move `ReplyObject` liveness/state tags to match `lockjaw_types::ipc_state::ReplyState` exactly.
20. Extract a pure `wait_any` planner from [src/syscall/handler.rs](/Users/Ben/Code/Lockjaw/src/syscall/handler.rs): readiness snapshot, mask computation, waiter registration plan, wake cleanup plan.

## Low-Cost Cleanups

21. ~~Move syscall-name metadata from crash.rs into `lockjaw-types::syscall`.~~ **DONE.** syscall_name() in lockjaw-types/src/syscall.rs, kernel re-exports.
22. Remove [src/elf.rs](/Users/Ben/Code/Lockjaw/src/elf.rs) as a kernel shim and import `lockjaw_types::elf` directly.
23. Keep [src/mm/user_access.rs](/Users/Ben/Code/Lockjaw/src/mm/user_access.rs) thin; optional only: add a typed `UserBufferSpec` in `lockjaw-types`.
24. Keep [src/cap/object_ops.rs](/Users/Ben/Code/Lockjaw/src/cap/object_ops.rs) thin and consume the new pure handle-access helpers instead of repeating checks.

## Kernel-Only Boundary To Preserve

- Assembly trampolines, EL transitions, `context_switch`, PSCI/HVC, UART/GIC register I/O, sysreg reads/writes, page allocation/free, zeroing, PTE writes, TLB invalidation, intrusive queue pointer mutation, scheduler block/unblock/wakeup effects, and object free/consume side effects.

## Best Verification Payoff

- ~~Host tests for exception frame layout and decode.~~ **DONE.** 16 tests in exception.rs.
- ~~Host tests for handle access validation and header constructors.~~ **DONE.** 22 tests in handle_ops.rs + 10 in object.rs.
- ~~Host tests for `create_process` planning and partial-unmap rejection.~~ **DONE.** ProcessTransferPlan tests.
- Host tests for boot memory layout and per-CPU stack layout.
- Host tests for page-table teardown planning.
- Trace-level tests that kernel IPC wrappers match `lockjaw-types::ipc_state::step()`.
