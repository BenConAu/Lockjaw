# Lockjaw

A capability-based microkernel written in Rust, targeting AArch64 (ARMv8-A). Runs on QEMU `virt` machine. Inspired by seL4 and Zircon, but with its own object model.

## What is this?

Lockjaw is a from-scratch microkernel that explores a middle ground between seL4's rigorous user-controlled memory model and Zircon's pragmatic handle-based API. The kernel never dynamically allocates memory. Userspace requests physical pages, then either maps them for its own use or donates them to the kernel to create objects like threads, IPC endpoints, and handle tables.

The design follows a few core principles:

- **Kernel never allocates.** All object memory comes from user-donated pages (PageSets). The kernel has only a fixed-size boot region in BSS.
- **Handle-based access control.** Every kernel object is accessed through an integer handle with an associated rights bitmask. No handle, no access.
- **Vulkan-inspired create-info pattern.** Each object type has its own create-info struct used for both size queries and creation. Same struct, no mismatch.
- **Proven stack safety.** A custom build tool analyzes the call graph and per-function stack sizes on every build. Indirect calls must be annotated or the build fails.
- **Map or donate, never both.** A PageSet is consumed when donated for a kernel object. This prevents userspace from reading kernel object internals or reusing the page.
- **Verified IPC state machine.** The IPC endpoint logic is driven by a pure state machine model that is exhaustively explored at test time -- all reachable states, all transitions, all effect orderings verified. The kernel executes effects mechanically; the model makes all decisions.
- **All MMIO through the device manager.** Drivers cannot map arbitrary physical addresses. The device manager discovers hardware from the DTB and issues tracked PageSets for MMIO pages. Only processes that receive an MMIO PageSet can map device memory.

## What works today

Lockjaw boots on QEMU, manages virtual memory, handles interrupts, runs preemptively scheduled threads, serves 19 syscalls from EL0 userspace, passes messages between threads via synchronous IPC, runs multiple isolated userspace processes loaded from ELF binaries, and has a device manager that discovers hardware from the DTB and allocates devices to drivers via IPC.

```
=== Lockjaw Microkernel v0.1.0 ===
Physical memory: 0x40000000 - 0x48000000 (32768 pages)
MMU enabled
Higher-half active
GIC initialized, timer PPI 27 enabled
Loading init process...
Dropping to EL0...
Hello from userspace init!
init: alloc_pages(1) OK, id=1
init: map_pages OK
init: DTB PageSet OK, magic valid
init: hello spawned OK
init: device-manager spawned OK
init: uart-driver spawned OK
init: hello bootstrapped
init: devmgr bootstrapped
init: uart bootstrapped
devmgr: DTB mapped
devmgr: parsed DTB, 49 devices
devmgr: PL011 at 0x9000000 intid=33 (kernel, reserved)
devmgr: PL011 at 0x9040000 intid=40
devmgr: serving
devmgr: claimed device at 0x9040000
uart-driver: claimed PL011
uart-driver: MMIO mapped
uart-driver: IRQ bound
uart-driver: UART1 active
uart-driver: server ready
hello: got handle 1
child: alive
init: alive (via IPC)
```

### Completed phases

**Phase 1 -- Boot to UART.** Bare-metal Rust binary boots on QEMU `virt`, prints to PL011 UART via MMIO, has a formatted `kprintln!` macro and a panic handler that prints file/line/message.

**Phase 2 -- Memory Management.** Bitmap page allocator over 128 MB of RAM (32,768 pages). AArch64 4-level page tables with identity mapping, then higher-half kernel mapping via TTBR1 (kernel at `0xFFFF_0000_xxxx_xxxx`). Unmapped guard page below the kernel stack with a canary value checked on every context switch.

**Phase 3 -- Exceptions and Interrupts.** Exception vector table with full register save/restore (31 GPRs + ELR/SPSR/ESR). GICv3 interrupt controller initialization. Virtual timer firing every 10ms for preemptive scheduling. Structured crash diagnostics: ESR decode, address classification, stack overflow detection, thread ID, syscall breadcrumb.

**Phase 4 -- Kernel Object Model.** Typed kernel objects created in user-donated pages via the Vulkan-style create-info pattern (query size, allocate PageSet, donate, create). Handle tables with insert/lookup/remove and rights checking (Read, Write, Grant). PageSets consumed on donation to prevent reuse.

**Phase 5 -- Threads and Context Switching.** Thread Control Blocks with per-thread stacks. Assembly `context_switch` saves/restores callee-saved registers and swaps SP. Round-robin scheduler driven by the timer interrupt. Preemptive multithreading verified with interleaved output from concurrent threads.

**Phase 6 -- Syscall Interface.** Userspace code runs at EL0 (unprivileged). SVC traps to kernel via separate lower-EL exception vector. Syscall dispatch on x8 register. Typed error returns: x0 = SyscallError (always), x1 = value, x1-x4 = IPC message words. User page tables in TTBR0 with PXN/UXN security bits.

**Phase 7 -- IPC.** Synchronous rendezvous message passing through Endpoint objects. Four message registers (x1-x4) transferred between threads. Send/receive with blocking, call/reply for client/server patterns. Non-blocking receive. Multiplexed wait (sys_wait_any) with threshold-based readiness. IPC state machine exhaustively verified: 20 reachable states, 36 transitions, all invariants checked. The kernel's IPC is driven entirely by the verified model.

**Phase 8 -- Userspace Processes.** Per-process TTBR0 page tables swapped by the scheduler on context switch. ELF64 parser loads the init process from an embedded binary. Init runs at EL0 and spawns child processes entirely from userspace. Bootstrap channel protocol (Zircon-inspired): child calls handle 0, parent exports handles via sys_export_handle, replies with indices.

**Phase 9 -- Userspace Drivers.** UART driver runs entirely in userspace. Receives its server endpoint and device-manager endpoint via bootstrap. Event loop using sys_wait_any multiplexes IPC requests and hardware interrupts. Notification objects serve as timeline semaphores for IRQ delivery. Init prints messages through the UART driver via IPC.

**Phase 10 -- Device Manager.** Device manager process parses the Flattened Device Tree (DTB) at boot to discover hardware. Serves CMD_CLAIM_DEVICE requests from drivers via IPC. Creates tracked MMIO PageSets (sys_register_device_page) so drivers can map device memory without knowing physical addresses. UART0 reserved for kernel debug. UART driver claims UART1 dynamically from the device manager.

### Testing

Three layers of automated testing run on every build:

| Layer | Count | What it tests |
|-------|-------|---------------|
| Unit tests (host) | 172 | Address types, PTE bitfields, rights, IPC state machine, PageSet table, page table walk/map, FDT parser, wait readiness, waiter identity |
| Integration tests (QEMU) | 29 | Full boot through all phases, expected serial output |
| Stack analysis | 3 | No recursion, depth within budget, all indirect calls annotated |
| Pointer cast lint | 72 | Every `as *const` / `as *mut` in kernel code has a SAFETY comment |
| **Total** | **276** | `make test` runs unit + integration + stack |

The IPC state machine test exhaustively explores all reachable system states (endpoint state x thread states) via BFS and verifies: no kernel-caused deadlocks, all invariants hold, all effect orderings correct (BlockCurrent always last, UnblockThread before ClearCaller).

### Build tools

**`cargo xtask check-stack`** runs automatically before every `make build` and verifies:

- **No recursion** -- detects cycles in the call graph (DFS on disassembly)
- **Stack depth within budget** -- sums per-function frame sizes along the worst-case path (normal path budget: 3072 bytes, interrupt path: 1024 bytes)
- **All indirect calls annotated** -- every `BLR` instruction must be listed in `xtask/stack-annotations.toml` with its known targets, or the build fails

**`cargo xtask check-pointers`** runs automatically before every `make build` and verifies:

- **Every pointer cast documented** -- every `as *const` / `as *mut` in `src/` must have a `// SAFETY:` comment explaining why the address is valid (kernel VA, MMIO, linker symbol, etc.)
- Prevents the TTBR0 race class of bugs: user VAs must go through `copy_from_user`, never raw pointer casts

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
make test             # Run all tests (unit + integration + stack)
make test-unit        # Host-side unit tests only
make test-qemu        # QEMU integration tests only
make check-stack      # Stack depth and call graph analysis
make check-pointers   # Pointer cast SAFETY annotation check
make objdump          # Disassemble the kernel
```

QEMU is invoked with two UARTs (UART0 for kernel debug, UART1 for userspace driver), GICv3, and a serial mux. Press Ctrl-A then X to exit. See `Makefile` for the full command.

## Project structure

```
src/
  main.rs                    # kmain, panic handler, boot banner, init ELF loading
  print.rs                   # kprintln! macro
  crash.rs                   # Crash diagnostics (syscall breadcrumb, thread context)
  process.rs                 # sys_create_process kernel-side implementation
  arch/aarch64/
    boot.rs                  # _start entry point (EL2 to EL1, FP enable, stack, BSS)
    uart.rs                  # PL011 UART driver (kernel debug on UART0)
    mmu.rs                   # Boot page tables, MMU enable, higher-half, guard page
    vmem.rs                  # Dynamic per-process page table management
    exceptions.rs            # Exception vectors, ESR decode, stack overflow detection
    gic.rs                   # GICv3 interrupt controller
    timer.rs                 # Virtual timer (10ms periodic ticks)
    irq_bind.rs              # IRQ-to-notification binding table
  mm/
    addr.rs                  # PhysAddr, PhysPage newtypes
    page_alloc.rs            # Bitmap page allocator
    page_table.rs            # PageTableEntry, PageTable types (re-exports from lockjaw-types)
    stack.rs                 # Stack canary init/check
    user_access.rs           # copy_from_user via page table walk (TTBR1)
  cap/
    object.rs                # ObjectType, create-info pattern, query/create
    handle_table.rs          # Handle insert/lookup/remove with rights
    rights.rs                # Rights bitmask
    pageset_table.rs         # PageSet tracking table (wraps lockjaw-types model)
  sched/
    tcb.rs                   # Thread Control Block with handle tables and TTBR0
    context.rs               # context_switch assembly, thread_entry trampoline
    scheduler.rs             # Round-robin scheduler with block/unblock and TTBR0 swap
  ipc/
    endpoint.rs              # Endpoint object, effect-driven send/receive/call/reply
    notification.rs          # Notification object (timeline semaphore for IRQ delivery)
  syscall/
    handler.rs               # Syscall dispatch (19 syscalls)

lockjaw-types/               # Pure-logic library crate, testable on host
  src/
    addr.rs                  # PhysAddr, PhysPage, PAGE_SIZE
    page_table.rs            # PageTableEntry, PageTable, PageTableWalk, MapWalk
    rights.rs                # Rights bitmask
    object.rs                # ObjectType, ObjectSize, create-info structs
    ipc_state.rs             # IPC state machine model, exhaustive verification
    notification_state.rs    # Notification timeline semaphore model
    pageset_table.rs         # PageSet table model with unit tests
    vmem.rs                  # Page table walk/map validation, index computation
    wait.rs                  # sys_wait_any readiness model, ReadinessWaiter
    fdt.rs                   # Flattened Device Tree parser
    device.rs                # Device types, compatible string hashing
    elf.rs                   # ELF64 parser
    syscall.rs               # Syscall numbers, SyscallError type
    constants.rs             # Stack canary, fill pattern, stack base address
    scheduler.rs             # Round-robin scheduling model

user/                        # Userspace binaries (separate Cargo projects)
  init/                      # Init process -- spawns and bootstraps all children
  hello/                     # Hello process -- bootstrap protocol test
  uart-driver/               # UART driver -- claims PL011 from device manager
  device-manager/            # Device manager -- DTB parsing, device claim IPC server
  lockjaw-userlib/           # Shared userspace library (syscall wrappers, print helpers)

docs/                        # Book of Lockjaw -- design documentation
  memory-model.md            # Why the kernel never allocates
  object-model.md            # PageSets, handles, the create-info pattern
  higher-half-kernel.md      # Why the kernel lives in the upper VA half
  kernel-drivers.md          # Why GIC and timer are the only kernel drivers
  threads.md                 # Context switching and preemptive scheduling
  syscalls.md                # Syscall ABI, EL0 drop, yield
  ipc.md                     # IPC design, the two ABIs, message registers
  process-creation.md        # Userspace-driven process creation
  tech-debt.md               # Known limitations and planned fixes
  yagni-parking-lot.md       # Removed code tracked for future phases

xtask/                       # Build tools
  src/main.rs                # check-stack and check-pointers commands
  stack-annotations.toml     # Indirect call targets for BLR verification

tests/
  qemu_integration.sh        # Boot QEMU, assert expected serial output (29 checks)
```

## Roadmap

| Phase | Status | Description |
|-------|--------|-------------|
| 1. Boot to UART | Done | Bare-metal boot, UART, kprintln!, panic handler |
| 2. Memory Management | Done | Page allocator, page tables, MMU, higher-half, guard page |
| 3. Exceptions and Interrupts | Done | Vector table, GICv3, timer, crash diagnostics |
| 4. Object Model | Done | Typed objects, handles, rights, PageSets |
| 5. Threads | Done | TCB, context switch, preemptive round-robin scheduler |
| 6. Syscall Interface | Done | Drop to EL0, SVC handler, typed error returns |
| 7. IPC | Done | Synchronous endpoints, call/reply, notifications, wait_any |
| 8. Userspace Processes | Done | ELF loader, per-process TTBR0, bootstrap channels |
| 9. Userspace Drivers | Done | UART driver, IRQ notifications, IPC event loop |
| 10. Device Manager | Done | DTB parsing, device claim protocol, MMIO PageSets |
| 11. Display Driver | Next | Virtio-GPU or PL110 framebuffer driver in userspace |
| 12. POSIX Compatibility | Planned | POSIX personality server, musl libc port |

## License

See [LICENSE](LICENSE).
