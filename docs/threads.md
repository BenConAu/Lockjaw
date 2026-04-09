# Threads and Context Switching

## What is a context switch?

A context switch saves one thread's CPU state and restores another's. On AArch64, this means swapping the stack pointer and a set of registers. The thread that was running is suspended mid-execution, and another thread picks up exactly where it left off.

Lockjaw's `context_switch` assembly function is minimal: it saves only the callee-saved registers (x19-x30) because those are the ones the C/Rust calling convention requires a function to preserve. The caller-saved registers (x0-x18) are either already on the stack (saved by the compiler as part of normal function calls) or saved by the exception vector SAVE_REGS macro (for preemptive switches triggered by interrupts).

## How preemptive switching works

Every 10ms, the virtual timer fires and the CPU traps into the exception vector:

```
Thread A running
  → Timer IRQ
    → SAVE_REGS (saves ALL registers: x0-x30, ELR, SPSR onto A's stack)
      → irq_dispatch() → handle_tick() → scheduler::tick() → schedule()
        → context_switch(A, B)
          [save A's callee-saved regs, store A's SP in its TCB]
          [load B's SP from its TCB, restore B's callee-saved regs]
          [ret — "returns" into B's call chain]
        ← schedule() ← handle_tick() ← irq_dispatch()
      → RESTORE_REGS (from B's stack — restores B's x0-x30, ELR, SPSR)
    → eret (resumes B at B's saved PC)
  → Thread B continues running
```

The key insight: both threads went through the same IRQ handler chain. Thread B was previously preempted at the same point (inside schedule/context_switch). Swapping SP in the middle makes the return path unwind on B's stack instead of A's.

## How new threads start

When a thread is created, its stack gets a synthetic frame — fake saved registers that look like the thread was previously context-switched out. The saved LR (link register) points to a `thread_entry` trampoline, and x19 holds the thread's entry function pointer.

When the scheduler first switches to a new thread:
1. `context_switch` restores the synthetic callee-saved regs (x19 = entry fn, LR = trampoline)
2. `ret` jumps to the trampoline
3. Trampoline unmasks IRQs (new threads start with interrupts disabled)
4. Trampoline calls the entry function via `blr x19`
5. The entry function runs until the next timer interrupt preempts it

## Thread entry and function pointers

Each TCB stores a `fn() -> !` entry function pointer. The trampoline calls it via an indirect branch (`blr` instruction). This is the one place in the kernel where we use an indirect call.

Our stack depth analysis tool (`cargo xtask check-stack`) can't trace through indirect calls — it doesn't know which function `blr x19` resolves to. To keep the analysis accurate, every indirect call site must be annotated in `xtask/stack-annotations.toml` with its known targets. **The build fails if any indirect call is unannotated.** No silent underestimation.

For kernel threads, the set of entry functions is small and fixed. The annotation is a manual sync point — if you add a kernel thread, you must add its entry function to the annotation file.

## Scheduler

Round-robin with a static 8-slot array. The timer tick triggers `schedule()`, which finds the next Ready thread and context-switches to it. If no other thread is ready, the current thread keeps running.

Thread 0 is the idle thread — it's the boot thread (kmain), and its stack is the boot stack from the linker script. When all other threads are blocked, the idle thread runs `wfi` (wait for interrupt) in a loop.

## Stack safety

Each thread's stack gets a canary value (`0xDEAD_BEEF_DEAD_BEEF`) written at its base during thread creation. The scheduler checks the canary of the outgoing thread on every context switch. If the canary is corrupted (stack overflow), the kernel panics immediately.

Thread stacks are 4 KB pages allocated from the page allocator. The boot stack additionally has an unmapped guard page below it (set up in Phase 2).
