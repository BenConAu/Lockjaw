# Lockjaw

A capability-based microkernel written in Rust, targeting AArch64 (ARMv8-A). Runs on QEMU `virt` machine. Inspired by seL4 and Zircon, but with its own object model.

## What is this?

Lockjaw is a from-scratch microkernel that explores a middle ground between seL4's rigorous user-controlled memory model and Zircon's pragmatic handle-based API. The kernel never dynamically allocates memory. Userspace requests physical pages, then either maps them for its own use or donates them to the kernel to create objects like threads, IPC endpoints, and handle tables.

The design follows a few core principles:

- **Kernel never allocates.** All object memory comes from user-donated pages (PageSets). The kernel has only a fixed-size boot region in BSS.
- **Handle-based access control.** Every kernel object is accessed through an integer handle with an associated rights bitmask. No handle, no access.
- **Vulkan-inspired create-info pattern.** Each object type has its own create-info struct used for both size queries and creation. Same struct, no mismatch.
- **Proven stack safety.** A custom build tool analyzes the call graph and per-function stack sizes from four entry points (_start, _secondary_start, __vec_sync_lower, __vec_irq) on every build. Indirect calls must be annotated or the build fails.
- **Map or donate, never both.** A PageSet is consumed when donated for a kernel object. Consumed headers are left as zeroed tombstones so stale exported handles safely read count=0.
- **Verified IPC state machine.** The IPC endpoint logic is driven by a pure state machine model that is exhaustively explored at test time -- all reachable states, all transitions, all effect orderings verified. Kernel IPC handlers match on typed decision enums (SendDecision, ReceiveDecision, CallDecision, ReplyDecision) returned by lockjaw-types. No inline state branching in kernel code.
- **Pull over push.** Kernel code is organized by integration shape: pull (types drives sequencing), plan/apply (types returns a decision, kernel executes), or push (kernel calls helpers). Push is treated as highest review-risk; the extraction rubric converts push to pull wherever possible.
- **All MMIO through the device manager.** Drivers cannot map arbitrary physical addresses. The device manager discovers hardware from the DTB and issues tracked PageSets for MMIO pages. Only processes that receive an MMIO PageSet can map device memory.
- **Unforgeable caller identity.** IPC endpoints carry kernel-assigned opaque caller tokens. When a handle is exported, the kernel assigns a monotonic per-endpoint token stored in the handle entry. Servers query the token after receive to scope resources per-client. Tokens identify handle lineage, not processes — delegates inherit the original token. Token 0 is receive-only; send/call with token 0 is rejected by the kernel.

## What works today

Lockjaw boots on QEMU with up to 4 cores (`-smp 4`), manages virtual memory with a buddy allocator supporting contiguous DMA allocation, handles interrupts, runs preemptively scheduled threads across multiple CPUs with a Giant Kernel Lock, serves 27 syscalls from EL0 userspace, passes messages between threads via synchronous IPC with Reply objects and kernel-assigned caller tokens for multi-client isolation, runs six isolated userspace processes loaded from ELF binaries, has a device manager that discovers hardware from the DTB with probe and claim-by-address protocols, a UART driver, a ramfb display driver, and a VirtIO block driver that reads from a virtual disk via virtqueues.

```
=== Lockjaw Microkernel v0.1.0 ===
Physical memory: 0x40000000 - 0x48000000 (32768 pages)
  Page allocator: 642 reserved, 32126 free
MMU enabled
Higher-half active
GIC initialized, timer PPI 27 enabled
Scheduler started.
Loading init process...
Dropping to EL0...
Hello from userspace init!
init: hello spawned OK
init: device-manager spawned OK
init: uart-driver spawned OK
init: ramfb-driver spawned OK
init: blk-driver spawned OK
devmgr: parsed DTB, 49 devices
uart-driver: claimed PL011
uart-driver: server ready
ramfb: claimed fw_cfg
ramfb: display configured
blk: found virtio-blk device            # with -drive flag
blk: selftest read OK, sector 0 = [4c 4f 43 4b ...]
blk: serving
[IPC BENCHMARK] 10000 call/reply round-trips in 74 ticks
[IPC BENCHMARK] 135 round-trips per tick
```

### Completed phases

**Phase 1 -- Boot to UART.** Bare-metal Rust binary boots on QEMU `virt`, prints to PL011 UART via MMIO, has a formatted `kprintln!` macro and a panic handler that prints file/line/message.

**Phase 2 -- Memory Management.** Buddy allocator over 128 MB of RAM (32,768 pages) with contiguous multi-page allocation for DMA buffers. AArch64 4-level page tables with identity mapping, then higher-half kernel mapping via TTBR1 (kernel at `0xFFFF_0000_xxxx_xxxx`). Unmapped guard page below the kernel stack with a canary value checked on every context switch.

**Phase 3 -- Exceptions and Interrupts.** Exception vector table with full register save/restore (31 GPRs + ELR/SPSR/ESR). GICv3 interrupt controller initialization. Virtual timer firing every 10ms for preemptive scheduling. Structured crash diagnostics: ESR decode, address classification, stack overflow detection, thread ID, syscall breadcrumb.

**Phase 4 -- Kernel Object Model.** Typed kernel objects created in user-donated pages via the Vulkan-style create-info pattern (query size, allocate PageSet, donate, create). Handle tables with insert/lookup/remove and rights checking (Read, Write, Grant). PageSets consumed on donation to prevent reuse.

**Phase 5 -- Threads and Context Switching.** Thread Control Blocks with per-thread stacks. Assembly `context_switch` saves/restores callee-saved registers (SavedContext struct with compile-time offset assertions against the assembly) and swaps SP. Round-robin scheduler driven by the timer interrupt. Preemptive multithreading verified with interleaved output from concurrent threads.

**Phase 6 -- Syscall Interface.** Userspace code runs at EL0 (unprivileged). SVC traps to kernel via separate lower-EL exception vector. Syscall dispatch on x8 register. Typed error returns: x0 = SyscallError (always), x1 = value, x1-x4 = IPC message words. User page tables in TTBR0 with PXN/UXN security bits.

**Phase 7 -- IPC.** Synchronous rendezvous message passing through Endpoint objects. Four message registers (x1-x4) transferred between threads. Send/receive with blocking, call/reply for client/server patterns using per-client Reply objects (eliminates multi-caller corruption). Non-blocking receive. Multiplexed wait (sys_wait_any) with threshold-based readiness. IPC state machine exhaustively verified: 89 reachable states (3-thread model), 8 invariants checked. The kernel's IPC is driven entirely by the verified model.

**Phase 8 -- Userspace Processes.** Per-process TTBR0 page tables swapped by the scheduler on context switch. ELF64 parser loads the init process from an embedded binary. Init runs at EL0 and spawns child processes entirely from userspace. Bootstrap channel protocol (Zircon-inspired): child calls handle 0, parent exports handles via sys_export_handle, replies with indices.

**Phase 9 -- Userspace Drivers.** UART driver runs entirely in userspace. Receives its server endpoint and device-manager endpoint via bootstrap. Event loop using sys_wait_any multiplexes IPC requests and hardware interrupts. Notification objects serve as timeline semaphores for IRQ delivery. Init prints messages through the UART driver via IPC.

**Phase 10 -- Device Manager and Display Driver.** Device manager process parses the Flattened Device Tree (DTB) at boot to discover hardware. Serves CMD_CLAIM_DEVICE, CMD_PROBE_DEVICE (with explicit status codes: PROBE_OK/END/CLAIMED/ERR), and CMD_CLAIM_BY_ADDR (TOCTOU-safe claim by stable MMIO address) requests from drivers via IPC. Probe uses absolute indexing over all matching DTB nodes for stable concurrent enumeration. Creates tracked MMIO PageSets with sub-page offset support (multiple virtio-mmio devices share a 4K page). ramfb display driver claims fw_cfg from the device manager, allocates a contiguous DMA framebuffer, configures the display via the fw_cfg DMA protocol, and renders a test pattern.

**Phase 11 -- SMP.** Secondary CPUs booted via PSCI CPU_ON. Per-CPU stacks in the linker script (2MB-aligned, 4 guard+stack pairs). Per-CPU data via TPIDR_EL1 with narrow accessors. Giant Kernel Lock (ticket lock from lockjaw-types, host-testable with multi-threaded tests) serializes all kernel execution. Scheduler model adapted for per-CPU current threads. Exception handlers acquire/release GKL. Kernel threads run cooperatively under the GKL with IRQs masked. Idle threads release GKL and halt in wfi. Process entry releases GKL before eret to EL0. INTID 0 reserved for future cross-core reschedule SGI (parked until fine-grained locking).

**Phase 12 -- PageSet Lifecycle.** Mapping tracking, ownership transfer with ProcessTransferPlan, refcounting with free-on-zero, process exit cleanup via ProcessTeardownPlan with construction-safe narrowing (separate step variants for with/without address space, making illegal unmap-during-teardown unrepresentable).

**Phase 13 -- Caller Tokens.** HandleEntry redesigned with typed HandleKind enum (repr(C, u8) with per-type metadata: caller_token on Endpoint, mapped_va_page on PageSet). Kernel assigns monotonic u64 tokens per endpoint on sys_export_handle and create_process handle copy. Token 0 = receive-only; send/call with token 0 is rejected. Servers query tokens via SYS_QUERY_CALLER_TOKEN (syscall 26). Tokens identify handle lineage for capability delegation. Integration test verifies nonzero token delivery.

**Phase 14 -- VirtIO Block Driver.** VirtIO MMIO transport with modern (non-legacy) device support. Pure types in lockjaw-types (register offsets, virtqueue layout calculator, feature negotiation model, block request types). Virtqueue runtime in userlib with volatile access and AArch64 memory barriers (dmb ishst/ish/ishld). BlockEngine trait + run_block_server() framework (same pattern as display DDI). Per-device GIC trigger mode (sys_bind_irq flags parameter). Device-manager probe protocol with explicit status codes (PROBE_OK/END/CLAIMED/ERR). Sub-page MMIO offset for virtio-mmio devices (8 per 4K page). Driver selftest reads sector 0 and prints content.

### Unsafe reduction

The kernel's unsafe usage has been systematically hardened through a multi-round review process with a second AI reviewer (Codex):

- **KernelRef/KernelMut** wrappers concentrate the `paddr + KERNEL_VA_OFFSET` cast in one place. All kernel object field access uses Rust struct syntax, not pointer arithmetic.
- **object_ops facade** provides narrow, operation-level safe methods (send, receive, call, signal, wait) that never expose `&mut T` to callers. Same pattern as CurrentThread.
- **BlockToken** enforces at compile time that no `&mut T` reference to a shared kernel object survives across `block_current()`. The borrow checker prevents moving the token while a scoped reference borrows it.
- **UserAddressSpace** wraps TTBR0 for safe `copy_from_user`. **PageSetRef** wraps validated PageSet IDs. **HandleTableRef** wraps handle table operations.
- Syscall handler has 4 remaining unsafe blocks, all at genuine machine boundaries (page table writes, GIC MMIO, readiness waiter registration, cross-object Reply pointer chasing).

### Testing

Three layers of automated testing run on every build:

| Layer | Count | What it tests |
|-------|-------|---------------|
| Unit tests (host) | 417 | Scheduler model, IPC state machine (exhaustive) + decision functions, process lifecycle + transfer plan + teardown plan, buddy allocator, page tables, ExceptionContext ABI, ESR decode, HandleKind + handle ops, VirtIO types + layout, block protocol, FDT parser, device probe protocol, notifications, wait readiness, ticket lock (multi-threaded), feature negotiation |
| Integration tests (QEMU) | 43 | Full boot through 13 phases, scheduler/MMU integration, IPC bootstrap, caller token delivery (positive + negative assertions), thread exit cleanup, thread creation |
| Stack analysis | 4 entry points | No recursion, depth within 8KB budget, per-function 1536B cap, all indirect calls annotated, both debug and release profiles |
| Pointer cast lint | 70+ | Every `as *const` / `as *mut` in kernel code has a SAFETY comment |

The IPC state machine test exhaustively explores all reachable system states (endpoint state x per-client reply state x thread states) via BFS with a 3-thread model and verifies: no kernel-caused deadlocks, all 8 invariants hold, all effect orderings correct (BlockCurrent always last, UnblockThread before ClearReply).

### Build tools

**`cargo xtask check-stack`** runs automatically before every `make build` and verifies both debug and release profiles:

- **Four entry points** -- _start, _secondary_start, __vec_sync_lower, __vec_irq (not just the boot path)
- **Combined budget** -- max(normal, secondary) + max(sync exception, IRQ) <= 8192 bytes
- **Per-function cap** -- any single function exceeding 1536 bytes fails immediately
- **No recursion** -- detects cycles in the call graph (DFS on disassembly, allowed_cycles for guarded paths)
- **All indirect calls annotated** -- every `BLR` instruction must be listed in `xtask/stack-annotations.toml` with its known targets, or the build fails
- **Tail call modeling** -- `b <symbol>` parsed as inter-function branches (conservative)
- **No silent gaps** -- missing stack size data is a hard error (assembly functions in `[known_assembly]`, core library with conservative estimates)

**`cargo xtask check-pointers`** runs automatically before every `make build` and verifies:

- **Every pointer cast documented** -- every `as *const` / `as *mut` in `src/` must have a `// SAFETY:` comment explaining why the address is valid (kernel VA, MMIO, linker symbol, etc.)
- Prevents the TTBR0 race class of bugs: user VAs must go through `UserAddressSpace::read`, never raw pointer casts

## Building and running

### Prerequisites

```
rustup target add aarch64-unknown-none
cargo install cargo-binutils rustfilt
rustup component add llvm-tools
brew install qemu  # or apt install qemu-system-aarch64
```

### Build, run, and test

```sh
make build            # Build (runs stack + pointer checks first)
make run              # Build and run in QEMU
make run-display      # Build and run with ramfb display window
make run-blk          # Build and run with virtio-blk disk (creates test.img)
make test             # Run all tests (unit + integration + stack)
make test-unit        # Host-side unit tests only
make test-qemu        # QEMU integration tests only
make check-stack      # Stack depth and call graph analysis
make check-pointers   # Pointer cast SAFETY annotation check
make objdump          # Disassemble the kernel
```

QEMU is invoked with two UARTs (UART0 for kernel debug, UART1 for userspace driver), GICv3, and a serial mux. Press Ctrl-A then X to exit. `make run-display` adds `-device ramfb -display cocoa` for the framebuffer window. `make run-blk` adds a 1MB virtio-blk device with modern MMIO transport. See `Makefile` for the full command.

## Project structure

```
src/
  main.rs                    # kmain, panic handler, boot banner, init ELF loading
  print.rs                   # kprintln! macro
  crash.rs                   # Crash diagnostics (syscall breadcrumb, thread context)
  percpu.rs                  # Per-CPU data via TPIDR_EL1 (narrow accessors)
  process.rs                 # sys_create_process kernel-side implementation
  elf.rs                     # ELF section lookup for build hash verification
  arch/aarch64/
    boot.rs                  # _start and _secondary_start entry points (EL2→EL1, stack, BSS)
    psci.rs                  # PSCI CPU_ON for secondary core boot (HVC #0)
    uart.rs                  # PL011 UART driver (kernel debug on UART0)
    mmu.rs                   # Boot page tables, MMU enable, higher-half, guard page
    vmem.rs                  # Dynamic per-process page table management
    exceptions.rs            # Exception vectors, crash diagnostics (imports types for ESR decode)
    gic.rs                   # GICv3 interrupt controller
    timer.rs                 # Virtual timer (10ms periodic ticks)
    irq_bind.rs              # IRQ-to-notification binding table
    mod.rs                   # IRQ dispatch (GIC ack, notification signal, timer tick)
  mm/
    addr.rs                  # PhysAddr, PhysPage newtypes, paddr_of_raw
    page_alloc.rs            # Buddy allocator wrapper (single + contiguous allocation)
    kernel_ptr.rs            # KernelRef/KernelMut typed pointer wrappers
    page_table.rs            # PageTableEntry, PageTable types (re-exports from lockjaw-types)
    stack.rs                 # Stack canary init/check
    user_access.rs           # UserAddressSpace, copy_from_user via page table walk (TTBR1)
  cap/
    object.rs                # ObjectType, create-info pattern, query/create
    object_ops.rs            # Safe IPC/notification facade (narrow operation methods)
    handle_table.rs          # HandleTableRef, handle insert/lookup/remove with rights
    rights.rs                # Rights bitmask
    pageset.rs               # PageSet state machine
    pageset_table.rs         # PageSetRef, PageSet tracking table, contiguous allocation
  sched/
    tcb.rs                   # TCB creation (imports Tcb/SavedContext from lockjaw-types)
    context.rs               # context_switch assembly (imports SavedContext from lockjaw-types)
    scheduler.rs             # Per-CPU round-robin scheduler, BlockToken, scoped_mut
    gkl.rs                   # Giant Kernel Lock (wraps TicketLock from lockjaw-types)
    current.rs               # CurrentThread safe facade (narrow per-field accessors)
  ipc/
    endpoint.rs              # Endpoint object, send/receive/call (matches on IPC decisions from types)
    reply.rs                 # Reply object, ipc_reply (matches on ReplyDecision from types)
    notification.rs          # Notification object (timeline semaphore for IRQ delivery)
    ep_queue.rs              # Intrusive FIFO waiter queue on endpoints

lockjaw-types/               # Pure-logic library crate, testable on host
  src/
    addr.rs                  # PhysAddr, PhysPage, PAGE_SIZE
    buddy.rs                 # Buddy allocator (bitmap-per-order, contiguous support)
    page_table.rs            # PageTableEntry, PageTable, PageTableWalk, MapWalk
    rights.rs                # Rights bitmask
    object.rs                # ObjectType, HandleKind enum, HandleEntry, CloseHandleResult, TeardownHandleAction
    handle_ops.rs            # Pure handle-table slot operations (insert/lookup/remove/rights)
    virtio.rs                # VirtIO MMIO registers, virtqueue types, block request types, feature negotiation
    block.rs                 # Block device IPC protocol (CMD_GET_INFO/ALLOC_BUFFER/READ/WRITE/FREE_BUFFER)
    ipc_state.rs             # IPC state machine model + kernel-facing decision functions (decide_send/receive/call/reply)
    exception.rs             # ExceptionContext ABI, ESR decode, sync exception classification
    thread.rs                # SavedContext, Tcb, TcbCreateInfo, ThreadBootstrap ABI
    notification_state.rs    # Notification timeline semaphore model
    pageset_table.rs         # PageSet table model, refcount/map_count lifecycle
    process.rs               # ProcessLifecycle, ProcessTransferPlan, ProcessTeardownPlan
    vmem.rs                  # Page table walk/map validation, index computation
    wait.rs                  # sys_wait_any readiness model, ReadinessWaiter
    fdt.rs                   # Flattened Device Tree parser
    device.rs                # Device types, compatible string hashing, probe/claim protocol constants
    elf.rs                   # ELF64 parser
    syscall.rs               # Syscall numbers (27), SyscallError type, syscall_name()
    constants.rs             # Stack canary, fill pattern, stack base address
    scheduler.rs             # Round-robin scheduling model
    user_pod.rs              # UserPod trait for safe copy_from_user

user/                        # Userspace binaries (separate Cargo projects)
  init/                      # Init process -- spawns and bootstraps all children
  hello/                     # Hello process -- bootstrap protocol test
  uart-driver/               # UART driver -- claims PL011 from device manager
  device-manager/            # Device manager -- DTB parsing, probe/claim-by-addr IPC
  ramfb-driver/              # Display driver -- fw_cfg DMA, contiguous framebuffer
  virtio-blk-driver/         # VirtIO block driver -- MMIO transport, virtqueue, BlockEngine
  display-test/              # Display DDI test client -- queries modes, draws gradient
  lockjaw-userlib/           # Shared library (syscalls, display DDI, block DDI, virtqueue, PageSetGuard)

docs/                        # Book of Lockjaw -- design documentation
  memory-model.md            # Why the kernel never allocates
  object-model.md            # PageSets, handles, the create-info pattern
  higher-half-kernel.md      # Why the kernel lives in the upper VA half
  kernel-drivers.md          # Why GIC and timer are the only kernel drivers
  threads.md                 # Context switching and preemptive scheduling
  syscalls.md                # Syscall ABI, EL0 drop, yield
  ipc.md                     # IPC design, the two ABIs, message registers
  process-creation.md        # Userspace-driven process creation
  stack-budget.md            # Stack budget analysis and rationale for 8KB
  tech-debt.md               # Known limitations and planned fixes
  types-extraction-plan.md   # Extraction roadmap: what moves to lockjaw-types
  yagni-parking-lot.md       # Removed code tracked for future phases
  development-journal.md     # Journal entries from the AI collaborator (1-6)

xtask/                       # Build tools
  src/main.rs                # check-stack and check-pointers commands
  stack-annotations.toml     # Indirect call targets for BLR verification

tests/
  qemu_integration.sh        # Boot QEMU, assert expected serial output
```

## Roadmap

| Phase | Status | Description |
|-------|--------|-------------|
| 1. Boot to UART | Done | Bare-metal Rust on QEMU virt, PL011 UART, kprintln!, panic handler |
| 2. Memory Management | Done | Buddy allocator, 4-level page tables, identity + higher-half mapping, guard pages |
| 3. Exceptions and Interrupts | Done | Vector table, GICv3, virtual timer, structured crash diagnostics |
| 4. Kernel Object Model | Done | Vulkan-style create-info, handle tables, rights checking, PageSet donation |
| 5. Threads and Context Switching | Done | TCBs, assembly context_switch, round-robin scheduler, preemptive multithreading |
| 6. Syscall Interface | Done | EL0 userspace, SVC dispatch, typed error returns, PXN/UXN security bits |
| 7. IPC | Done | Synchronous rendezvous, call/reply with per-client Reply objects, non-blocking receive, sys_wait_any |
| 8. Userspace Processes | Done | Per-process TTBR0, ELF loader, init spawns children, bootstrap channel protocol |
| 9. Userspace Drivers | Done | UART driver in userspace, event loop with sys_wait_any, notification-based IRQ delivery |
| 10. Device Manager and Display | Done | DTB parsing, device claim IPC, MMIO PageSets, ramfb driver with DMA framebuffer |
| 11. SMP | Done | Per-CPU stacks, PSCI secondary boot, Giant Kernel Lock, per-CPU scheduler and idle threads |
| 12. PageSet Lifecycle | Done | Mapping tracking, ownership transfer, refcounting, free-on-zero, process exit cleanup |
| 13. Caller Tokens | Done | Kernel-assigned opaque per-endpoint caller tokens for multi-client IPC isolation |
| 14. VirtIO Block Driver | Done | VirtIO MMIO transport, split virtqueue with barriers, block engine + server framework, per-device GIC trigger mode, selftest reads sector 0 |
| 15. Architecture Hardening | In progress | Extracting pure logic to lockjaw-types (push→pull), making illegal states unrepresentable |
| 16. Real Hardware | Planned | Bring-up on a simple AArch64 board (Raspberry Pi 4 or similar) |
| 17. POSIX Compatibility | Planned | POSIX personality server in userspace, musl libc port |

## License

See [LICENSE](LICENSE).
