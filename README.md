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

## What works today

Lockjaw boots on QEMU, sets up virtual memory, handles interrupts, and runs preemptively scheduled kernel threads. Here is the boot output:

```
=== Lockjaw Microkernel v0.1.0 ===
Target: AArch64 (ARMv8-A), QEMU virt

Memory layout:
  Kernel load:  0x40080000
  BSS:          0x40087000 - 0x4008f000 (32768 bytes)
  Kernel end:   0x4008f000
  Stack:        0x40090000 - 0x40091000 (4096 bytes)

Physical memory: 0x40000000 - 0x48000000 (32768 pages)
  Page allocator: 145 reserved, 32623 free

Enabling MMU (identity map)...
MMU enabled
Enabling higher-half kernel mapping...
Higher-half active
Guard page active (unmapped).
Stack canary intact.
Exception vectors installed.
  GIC distributor: 288 IRQ lines
  GIC initialized, timer PPI 27 enabled
  Timer frequency: 62500000 Hz
IRQs unmasked.
Scheduler started. Entering idle loop.

[A] count=100
[B] count=150
[A] count=200
[B] count=300
...
```

### Completed phases

**Phase 1 -- Boot to UART.** Bare-metal Rust binary boots on QEMU `virt`, prints to PL011 UART via MMIO, has a formatted `kprintln!` macro and a panic handler that prints file/line/message.

**Phase 2 -- Memory Management.** Bitmap page allocator over 128 MB of RAM (32,768 pages). AArch64 4-level page tables with identity mapping, then higher-half kernel mapping via TTBR1 (kernel at `0xFFFF_0000_xxxx_xxxx`). Unmapped guard page below the kernel stack with a canary value checked on every context switch.

**Phase 3 -- Exceptions and Interrupts.** Exception vector table with full register save/restore (31 GPRs + ELR/SPSR/ESR). GICv3 interrupt controller initialization. Virtual timer firing every 10ms for preemptive scheduling.

**Phase 4 -- Kernel Object Model.** Typed kernel objects created in user-donated pages via the Vulkan-style create-info pattern (query size, allocate PageSet, donate, create). Handle tables with insert/lookup/remove and rights checking (Read, Write, Grant).

**Phase 5 -- Threads and Context Switching.** Thread Control Blocks with per-thread 4 KB stacks. Assembly `context_switch` saves/restores callee-saved registers and swaps SP. Round-robin scheduler driven by the timer interrupt. Preemptive multithreading verified with interleaved output from concurrent threads.

### Build tool: stack depth verification

`cargo xtask check-stack` runs on every build and verifies:

- **No recursion** -- detects cycles in the call graph (DFS on disassembly)
- **Stack depth within budget** -- sums per-function frame sizes along the worst-case path (normal path budget: 3072 bytes, interrupt path: 1024 bytes)
- **All indirect calls annotated** -- every `BLR` instruction must be listed in `xtask/stack-annotations.toml` with its known targets, or the build fails

## Building and running

### Prerequisites

```
rustup target add aarch64-unknown-none
cargo install cargo-binutils rustfilt
rustup component add llvm-tools
brew install qemu  # or apt install qemu-system-aarch64
```

### Build and run

```sh
make build          # Build the kernel (debug)
make run            # Build and run in QEMU
make check-stack    # Verify stack depth and no recursion
make objdump        # Disassemble the kernel
```

QEMU is invoked with `-machine virt,gic-version=3 -cpu cortex-a53 -nographic`. Press Ctrl-A then X to exit.

## Project structure

```
src/
  main.rs                    # kmain, panic handler, boot banner, test threads
  print.rs                   # kprintln! macro
  arch/aarch64/
    boot.rs                  # _start entry point (EL2 to EL1, stack, BSS)
    uart.rs                  # PL011 UART driver
    mmu.rs                   # Page tables, MMU enable, higher-half, guard page
    exceptions.rs            # Exception vector table, register save/restore
    gic.rs                   # GICv3 interrupt controller
    timer.rs                 # Virtual timer (10ms periodic ticks)
  mm/
    addr.rs                  # PhysAddr, PhysPage newtypes
    page_alloc.rs            # Bitmap page allocator
    page_table.rs            # PageTableEntry, PageTable types
    stack.rs                 # Stack canary init/check
  cap/
    object.rs                # ObjectType, create-info pattern, query/create
    handle_table.rs          # Handle insert/lookup/remove with rights
    rights.rs                # Rights bitmask (Read, Write, Grant)
    pageset.rs               # PageSet allocation and donation
  sched/
    tcb.rs                   # Thread Control Block
    context.rs               # context_switch assembly, thread_entry trampoline
    scheduler.rs             # Round-robin scheduler

docs/                        # Book of Lockjaw -- design documentation
  memory-model.md            # Why the kernel never allocates
  object-model.md            # PageSets, handles, the create-info pattern
  higher-half-kernel.md      # Why the kernel lives in the upper VA half
  kernel-drivers.md          # Why GIC and timer are the only kernel drivers
  threads.md                 # Context switching and preemptive scheduling
  yagni-parking-lot.md       # Removed code tracked for future phases
```

## Roadmap

| Phase | Status | Description |
|-------|--------|-------------|
| 1. Boot to UART | Done | Bare-metal boot, UART, kprintln!, panic handler |
| 2. Memory Management | Done | Page allocator, page tables, MMU, higher-half, guard page |
| 3. Exceptions and Interrupts | Done | Vector table, GICv3, timer |
| 4. Object Model | Done | Typed objects, handles, rights, PageSets |
| 5. Threads | Done | TCB, context switch, preemptive round-robin scheduler |
| 6. Syscall Interface | Next | Drop to EL0, SVC handler, core syscalls |
| 7. IPC | Planned | Synchronous endpoint-based message passing |
| 8. Userspace Processes | Planned | ELF loader, init process, isolated address spaces |
| 9. Userspace Drivers | Planned | UART driver in userspace, IRQ notifications |
| 10. POSIX Compatibility | Stretch | POSIX personality server, musl libc port |

## License

See [LICENSE](LICENSE).
