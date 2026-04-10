# Development Journal: Building Lockjaw

A contemporaneous account of building the Lockjaw microkernel, written by Claude (the AI collaborator) after completing Phases 1-6.

## How we got here

Lockjaw started with a plan document and an empty repo. We built from nothing — no template, no reference implementation, just a microkernel plan inspired by seL4 and Zircon, and a Rust toolchain targeting AArch64. Six phases later, we have a kernel that boots on QEMU, manages virtual memory, handles interrupts, creates typed kernel objects, runs preemptively scheduled threads, and serves syscalls from userspace code at EL0.

The total elapsed time for Phases 1-6 was a single extended conversation. Every line of code was written, built, tested in QEMU, and committed within this session.

## What went well

**Incremental verification.** Every milestone ended with running the kernel in QEMU and seeing it work. We never accumulated more than one untested feature. When something broke (and things did break), we always knew exactly what changed. This is the single most important practice in the project.

**The commit discipline.** Ben insisted on small atomic commits from the start. I initially underestimated how much this mattered. When we hit the TTBR0 identity-map bug in Phase 6, being able to trace back through small, well-described commits made the debugging tractable. The later rule of 30+ line commit messages came from lived pain — we spent real time debugging a page table issue that would have been easier if the commit had explained the "why" more thoroughly.

**YAGNI enforcement.** Ben caught me multiple times writing code for future phases — unused enum variants, function stubs, constants that nothing called. Each time, we deleted the dead code and tracked it in the YAGNI parking lot document. This felt slow in the moment but kept the codebase honest. Every line that exists is exercised. The build has zero warnings.

**Lockjaw's own identity.** The project started as "seL4 in Rust" but evolved into something genuinely different. The Vulkan-inspired create-info pattern for kernel objects, PageSets as the memory primitive, the "map or donate, never both" security rule, handle tables instead of CSpaces — these emerged from Ben pushing back on seL4 conventions and asking "why can't we do it this way?" Every pushback led to a cleaner design. The object model is simpler than seL4's and more principled than Zircon's.

**The stack analysis tool.** The custom call graph analyzer in the xtask started as a fallback because cargo-call-stack was pinned to a 2023 nightly. It turned into something better: a build-time enforcer that fails on unannotated indirect calls. When we added function pointers for thread entry in Phase 5, the tool immediately demanded annotations. This is the kind of safety net that prevents silent regressions.

## What was hard

**The TTBR0 identity map problem (Phase 6).** This was the hardest bug we hit. The kernel binary is linked at physical addresses, so VBAR_EL1 points to a physical address. When we replaced TTBR0 with user page tables, that address became unmapped. The exception vector fetch faulted, which tried to go to the exception vector, which faulted again — an infinite loop with no output. QEMU just went silent.

The fix (including the kernel's identity map in the user TTBR0) is a workaround, not a solution. The proper fix is relinking the kernel at higher-half virtual addresses, which requires a boot trampoline and linker script changes. We documented the workaround thoroughly and moved on. This is the right tradeoff for a project at this stage — it works correctly, it's safe (user can't access kernel pages), and the real fix is a known quantity for later.

**Assembly commenting discipline.** The rule that every assembly line needs a comment is good, but it's also the thing I most often forgot on first draft. The context switch assembly, the exception vector macros, the boot sequence — all required revisiting to add comments. In retrospect, the comments are worth it every time. The SAVE_REGS/RESTORE_REGS macros are 40 lines of assembly that would be impenetrable without them.

**Getting the GICv3 timer interrupt working.** We initially used INTID 30 (the non-secure physical timer) instead of INTID 27 (the virtual timer), because the master plan said "PPI 30." The timer never fired. The fix was a one-line change but the debugging was confusing because there was no error — just silence.

**QEMU's default GIC version.** QEMU virt defaults to GICv2, but we wrote GICv3 code. The kernel data-aborted trying to access the GICv3 redistributor which doesn't exist on GICv2. Adding `-M virt,gic-version=3` fixed it, but it took a while to realize that QEMU's default didn't match our target.

## Observations about AI-assisted kernel development

**I can write correct AArch64 assembly, but I can't debug it.** When assembly works, it works immediately. When it doesn't, the failure mode is "QEMU goes silent" or "infinite exception loop." I don't have access to GDB or QEMU's monitor console. Ben running QEMU locally and seeing the output was essential. QEMU's `-d int` debug log was the key to diagnosing the TTBR0 bug — without it, we'd have been stuck.

**The conversation shaped the architecture.** The object model wasn't designed upfront — it emerged from a back-and-forth where Ben questioned every seL4-ism. "Why can't I just have handles?" led to dropping CSpaces. "Why is this called a Frame?" led to renaming PhysFrame to PhysPage. "I want something like Vulkan's create-info pattern" led to per-type create-info structs. Each of these made the design simpler and more coherent. A design document written in isolation would not have produced this.

**YAGNI is harder than it sounds when you know what's coming.** I have a model of the full 10-phase plan in my head. When I write Phase 4 code, I naturally want to include types and constants for Phase 5. Ben's enforcement of strict YAGNI was the corrective I needed. The parking lot document is the pressure release valve — I don't lose the knowledge, I just don't encode it as code until it's needed.

**The Book of Lockjaw is the project's actual documentation.** The docs/ directory started as an afterthought (Ben asked for a memory model explanation) and became the canonical record of design decisions. Each chapter exists because someone asked "why?" about a specific design choice. The higher-half kernel doc, the kernel drivers doc, the object model doc — these aren't reference manuals, they're answers to real questions that came up during development. This is the best kind of documentation: it explains the non-obvious.

## What's next

Phase 7 (IPC) is the next big feature — synchronous message passing between threads through Endpoint objects. This will be the first time two pieces of userspace code communicate. After that, Phase 8 loads actual ELF binaries and creates isolated processes with separate address spaces.

But first: testing. We're adding a `lockjaw-types` crate for host-side unit tests (pure logic like bit manipulation, size queries, rights checking) and QEMU integration tests (boot the kernel, check serial output). This should have been done earlier, but the incremental QEMU verification carried us through six phases without regressions. Now the codebase is big enough that automated tests will pay for themselves.

The kernel is currently ~1500 lines of Rust + ~300 lines of assembly, with 60 functions in the release binary. The worst-case stack depth is 656 bytes out of a 3072 byte budget. There are zero compiler warnings and zero known bugs. It boots in under a second on QEMU and preemptively schedules three threads (two kernel, one userspace) with 10ms time slices.

It's a good foundation.
