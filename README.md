# Lockjaw

A capability-based microkernel written in Rust, targeting AArch64 (ARMv8-A). Runs on QEMU `virt` machine. Inspired by seL4 and Zircon, but with its own object model.

## What is this?

Lockjaw is a from-scratch microkernel that explores a middle ground between seL4's rigorous user-controlled memory model and Zircon's pragmatic handle-based API. The kernel never dynamically allocates memory. Userspace requests physical pages, then either maps them for its own use or donates them to the kernel to create objects like threads, IPC endpoints, and handle tables.

The design follows a few core principles:

- **Kernel never allocates.** All object memory comes from user-donated pages (PageSets). The kernel has only a fixed-size boot region in BSS.
- **Handle-based access control.** Every kernel object is accessed through an integer handle with an associated rights bitmask. No handle, no access.
- **Vulkan-inspired create-info pattern.** Each object type has its own create-info struct used for both size queries and creation. Same struct, no mismatch.
- **Proven stack safety.** A custom build tool analyzes the call graph and per-function stack sizes on every build. Indirect calls must be annotated or the build fails.
- **Map or donate, never both.** A PageSet is either mapped into userspace (MappedPages) or donated for a kernel object. This prevents userspace from reading kernel object internals.
- **Verified IPC state machine.** The IPC endpoint logic is driven by a pure state machine model that is exhaustively explored at test time — all reachable states, all transitions, all effect orderings verified. The kernel executes effects mechanically; the model makes all decisions.

## What works today

Lockjaw boots on QEMU, manages virtual memory, handles interrupts, runs preemptively scheduled threads, serves syscalls from EL0 userspace, passes messages between threads via synchronous IPC, and runs multiple isolated userspace processes loaded from ELF binaries.

```
=== Lockjaw Microkernel v0.1.0 ===
Physical memory: 0x40000000 - 0x48000000 (32768 pages)
MMU enabled
Higher-half active
Exception vectors installed.
  GIC initialized, timer PPI 27 enabled
  Endpoint created
Loading init process...
  Entry point: 0x400000
Dropping to EL0...
Hello from userspace init!
init: alloc_pages(1) OK, id=0
init: map_pages OK
init: mapped memory read/write OK
init: parsing child ELF...
init: spawning child process...
init: child spawned successfully!
init: alive
Hello from child process!
child: alive
init: alive
child: alive
```

### Completed phases

**Phase 1 -- Boot to UART.** Bare-metal Rust binary boots on QEMU `virt`, prints to PL011 UART via MMIO, has a formatted `kprintln!` macro and a panic handler that prints file/line/message.

**Phase 2 -- Memory Management.** Bitmap page allocator over 128 MB of RAM (32,768 pages). AArch64 4-level page tables with identity mapping, then higher-half kernel mapping via TTBR1 (kernel at `0xFFFF_0000_xxxx_xxxx`). Unmapped guard page below the kernel stack with a canary value checked on every context switch.

**Phase 3 -- Exceptions and Interrupts.** Exception vector table with full register save/restore (31 GPRs + ELR/SPSR/ESR). GICv3 interrupt controller initialization. Virtual timer firing every 10ms for preemptive scheduling.

**Phase 4 -- Kernel Object Model.** Typed kernel objects created in user-donated pages via the Vulkan-style create-info pattern (query size, allocate PageSet, donate, create). Handle tables with insert/lookup/remove and rights checking (Read, Write, Grant).

**Phase 5 -- Threads and Context Switching.** Thread Control Blocks with per-thread 4 KB stacks. Assembly `context_switch` saves/restores callee-saved registers and swaps SP. Round-robin scheduler driven by the timer interrupt. Preemptive multithreading verified with interleaved output from concurrent threads.

**Phase 6 -- Syscall Interface.** Userspace code runs at EL0 (unprivileged). SVC traps to kernel via separate lower-EL exception vector. Syscall dispatch on x8 register. `sys_debug_putc` for UART output, `sys_yield` for voluntary rescheduling. User page tables in TTBR0 with PXN/UXN security bits.

**Phase 7 -- IPC.** Synchronous rendezvous message passing through Endpoint objects. Four message registers (x0-x3) transferred between threads. Send/receive with blocking, call/reply for client/server patterns. IPC state machine exhaustively verified: 20 reachable states, 36 transitions, all invariants checked. The kernel's IPC is driven entirely by the verified model — it executes derived effects mechanically without making decisions. 10,000 call/reply round-trips in 16 ticks on QEMU.

**Phase 8 -- Userspace Processes.** Per-process TTBR0 page tables swapped by the scheduler on context switch. ELF64 parser loads the init process from an embedded binary. Init runs at EL0 and spawns a child process entirely from userspace: parses the child ELF, allocates pages via `sys_alloc_pages`, maps them via `sys_map_pages`, copies segment data, and calls `sys_create_process` with a mapping list. The kernel reads the list from init's memory and builds the child's address space. Two processes running at the same VA (0x400000) with separate page tables, printing different messages — isolation proven by construction.

### Testing

Three layers of automated testing run on every build:

| Layer | Count | What it tests |
|-------|-------|---------------|
| Unit tests (host) | 59 | Address types, PTE bitfields, rights, object sizes, IPC state machine, PageSet table, page table walk |
| Integration tests (QEMU) | 29 | Full boot through all 8 phases, expected serial output |
| Stack analysis | 3 | No recursion, depth within budget, all indirect calls annotated |
| **Total** | **91** | `make test` runs everything |

The IPC state machine test exhaustively explores all reachable system states (endpoint state x thread states) via BFS and verifies: no kernel-caused deadlocks, all invariants hold, all effect orderings correct (BlockCurrent always last, UnblockThread before ClearCaller).

### Build tool: stack depth verification

`cargo xtask check-stack` runs automatically before every `make build` and verifies:

- **No recursion** -- detects cycles in the call graph (DFS on disassembly)
- **Stack depth within budget** -- sums per-function frame sizes along the worst-case path (normal path budget: 3072 bytes, interrupt path: 1024 bytes)
- **All indirect calls annotated** -- every `BLR` instruction must be listed in `xtask/stack-annotations.toml` with its known targets, or the build fails. No silent underestimation.

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
make build          # Build (runs stack check first)
make run            # Build and run in QEMU
make test           # Run all tests (unit + integration + stack)
make test-unit      # Host-side unit tests only
make test-qemu      # QEMU integration tests only
make check-stack    # Stack depth and call graph analysis
make objdump        # Disassemble the kernel
```

QEMU is invoked with `-machine virt,gic-version=3 -cpu cortex-a53 -nographic`. Press Ctrl-A then X to exit.

## Project structure

```
src/
  main.rs                    # kmain, panic handler, boot banner, init ELF loading
  print.rs                   # kprintln! macro
  elf.rs                     # ELF64 parser for loading init at boot
  process.rs                 # sys_create_process kernel-side implementation
  arch/aarch64/
    boot.rs                  # _start entry point (EL2 to EL1, FP enable, stack, BSS)
    uart.rs                  # PL011 UART driver
    mmu.rs                   # Boot page tables, MMU enable, higher-half, guard page
    vmem.rs                  # Dynamic per-process page table allocation
    exceptions.rs            # Exception vector table, register save/restore
    gic.rs                   # GICv3 interrupt controller
    timer.rs                 # Virtual timer (10ms periodic ticks)
  mm/
    addr.rs                  # PhysAddr, PhysPage newtypes (re-exports from lockjaw-types)
    page_alloc.rs            # Bitmap page allocator
    page_table.rs            # PageTableEntry, PageTable types (re-exports from lockjaw-types)
    stack.rs                 # Stack canary init/check
  cap/
    object.rs                # ObjectType, create-info pattern, query/create
    handle_table.rs          # Handle insert/lookup/remove with rights
    rights.rs                # Rights bitmask (re-exports from lockjaw-types)
    pageset.rs               # PageSet allocation and donation
    pageset_table.rs         # PageSet tracking table (wraps lockjaw-types model)
  sched/
    tcb.rs                   # Thread Control Block with per-thread handle tables and TTBR0
    context.rs               # context_switch assembly, thread_entry trampoline
    scheduler.rs             # Round-robin scheduler with block/unblock and TTBR0 swap
  ipc/
    endpoint.rs              # Endpoint object, effect-driven send/receive/call/reply
  syscall/
    handler.rs               # Syscall dispatch (10 syscalls: debug_putc through create_process)

lockjaw-types/               # Pure-logic library crate, testable on host (x86_64)
  src/
    addr.rs                  # PhysAddr, PhysPage, PAGE_SIZE
    page_table.rs            # PageTableEntry, PageTable, MAIR/AP/SH constants
    rights.rs                # Rights bitmask
    object.rs                # ObjectType, ObjectSize, create-info structs
    ipc_state.rs             # IPC state machine model, exhaustive verification
    pageset_table.rs         # PageSet table model with unit tests
    vmem.rs                  # Page table walk validation and index computation

user/                        # Userspace binaries (separate Cargo projects)
  init/                      # Init process — first userspace program, spawns children
  hello/                     # Hello process — child spawned by init

docs/                        # Book of Lockjaw -- design documentation
  memory-model.md            # Why the kernel never allocates
  object-model.md            # PageSets, handles, the create-info pattern
  higher-half-kernel.md      # Why the kernel lives in the upper VA half
  kernel-drivers.md          # Why GIC and timer are the only kernel drivers
  threads.md                 # Context switching and preemptive scheduling
  syscalls.md                # Syscall ABI, EL0 drop, yield
  ipc.md                     # IPC design, the two ABIs, message registers
  process-creation.md        # Userspace-driven process creation, the Vulkan parallel
  development-journal.md     # Journal entry 1: Phases 1-6
  development-journal-2.md   # Journal entry 2: Phases 7-8
  yagni-parking-lot.md       # Removed code tracked for future phases

tests/
  qemu_integration.sh        # Boot QEMU, assert expected serial output (29 checks)
```

## Roadmap

| Phase | Status | Description |
|-------|--------|-------------|
| 1. Boot to UART | Done | Bare-metal boot, UART, kprintln!, panic handler |
| 2. Memory Management | Done | Page allocator, page tables, MMU, higher-half, guard page |
| 3. Exceptions and Interrupts | Done | Vector table, GICv3, timer |
| 4. Object Model | Done | Typed objects, handles, rights, PageSets |
| 5. Threads | Done | TCB, context switch, preemptive round-robin scheduler |
| 6. Syscall Interface | Done | Drop to EL0, SVC handler, debug_putc, yield |
| 7. IPC | Done | Synchronous endpoints, call/reply, verified state machine |
| 8. Userspace Processes | Done | ELF loader, per-process TTBR0, init spawns child, isolation |
| 9. Userspace Drivers | Next | UART driver in userspace, IRQ notifications |
| 10. POSIX Compatibility | Stretch | POSIX personality server, musl libc port |

## License

See [LICENSE](LICENSE).
