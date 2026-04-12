# Development Journal: Phase 7-8

A second journal entry, written after completing IPC and userspace processes.

## The IPC state machine was a turning point

Before Phase 7, I was writing kernel code the way I write application code — implement the logic, test it in QEMU, fix what breaks. The EP_HAS_CALLER deadlock bug changed that. I wrote the endpoint state machine, it worked for simple cases, and then it silently deadlocked under a specific interleaving I hadn't considered. I only found it because the benchmark hung, not because any test caught it.

Ben's response was to demand exhaustive verification. Not "add more test cases" — he wanted every reachable state explored, every transition validated, every effect ordering proven correct. The BFS exploration over all 20 reachable system states was the result. It found the deadlock immediately, and it found a second bug (reply-before-receive inconsistency) that I hadn't even considered.

Then Ben pushed further: don't just validate transitions, derive the effects from the state diff and have the kernel execute them mechanically. The kernel makes zero decisions — the model makes all decisions. This caught a third bug: effect ordering (UnblockThread must happen before ClearCaller, BlockCurrent must be last). Each of these bugs would have been a silent runtime failure in QEMU, caught only by luck.

The lesson is not "write more tests." The lesson is: if your system has a state machine, extract it, model it, and explore it exhaustively. The cost is one file in lockjaw-types. The return is every interleaving verified at compile time.

## Ben taught me to stop rushing

I have a tendency to combine multiple changes into one commit and fix problems by adding more code rather than stepping back. Ben caught this repeatedly:

- "I see code being set up for 3.3" — I was writing future-phase code in a current-phase commit. YAGNI.
- "I spied you putting in code that isn't being used yet" — same pattern, in Phase 4.
- "The whole vibe here seems to be biting off too much at once" — this was Phase 8, where I tried to do process creation, ELF loading, a new syscall, and a new user crate in one step. It broke in ways I couldn't diagnose.
- "Let's do less cowboy shit and more shaolin monk shit" — the clearest feedback I've received on this project. One step at a time, each proven before the next.

The revised Phase 8 plan went from 3 milestones to 7. Each milestone was independently testable and committed. When something broke (the TTBR0 field not being set on the boot thread's TCB), I knew exactly which step introduced the problem because only one thing changed.

## The "kernel never allocates" principle got real in Phase 8

For most of the project, "the kernel never allocates" was an abstract principle. The kernel had a page bitmap for tracking, and objects lived in donated pages, but the kernel itself wasn't trying to do complex work that needed scratch space.

Phase 8 made it concrete. Process creation needs a mapping array — where does it live? My first attempt: allocate a kernel scratch page. Ben immediately called it out. Not because it was technically wrong (it worked), but because it violated the design principle. If the kernel allocates scratch space, it has a de facto heap. If it has a heap, it can run out. If it can run out, the kernel can fail in ways userspace can't predict or control.

The fix was to make userspace provide the memory. Init allocates the mapping array page, fills it in, and passes a pointer to the kernel. The kernel reads it one entry at a time. No scratch space, constant stack usage, and the kernel's behavior is fully determined by what userspace provides.

This required building up from the bottom: sys_alloc_pages first, then sys_map_pages, then sys_create_process. Each one was its own milestone. The bottom-up approach meant each piece was tested and trusted before the next layer used it. When the full flow worked (init parsing ELF, allocating pages, copying segments, spawning the child), every component in the chain had already been proven independently.

## What I think of the project

Lockjaw has a real identity now. It started as "seL4 in Rust" but became something distinct:

- The **Vulkan-style create-info pattern** for object creation — query size, allocate, create with the same struct — is unlike any microkernel I know of. It came from Ben's GPU programming background, and it fits perfectly.
- **PageSets** as the memory primitive, with the "map or donate, never both" rule, is a clean security model that's simpler than seL4's Untyped capability tree.
- The **handle table** model (flat integer handles, not CSpace radix trees) came from Ben pushing back on seL4 terminology and asking "why can't I just have handles?"
- The **IPC state machine** being a separate, exhaustively-tested model that the kernel executes mechanically — I haven't seen this in other microkernel implementations. It's a verification technique borrowed from formal methods, applied pragmatically.
- **Process creation driven entirely from userspace** — init parses ELF, allocates pages, copies data, builds the mapping list, and tells the kernel what to do. The kernel is a dumb executor. This is the seL4 philosophy taken to its logical conclusion with a Vulkan API flavor.

The codebase is about 2500 lines of kernel Rust, 400 lines of assembly, 600 lines of userspace Rust, 1200 lines of test models, and 10 Book of Lockjaw chapters. There are 91 automated checks. Two real userspace processes run in isolated address spaces, communicating via synchronous IPC, preemptively scheduled at 10ms intervals.

## What I'd do differently

If I started over, I would extract every state machine into lockjaw-types from day one — not just IPC, but the scheduler state, the endpoint lifecycle, and the page table walk logic. The pattern proved itself so thoroughly that it should be the default, not the exception.

I would also be more careful about stack usage from the start. The 4 KB kernel stack is a hard constraint that I didn't internalize until it bit me twice. The check-stack tool catches function-level depth but doesn't catch large local arrays. A lint or compile-time check for functions with locals exceeding N bytes would have prevented both stack overflows.

And I would resist the urge to implement future-phase features. Every time I wrote code for a phase that wasn't the current one, it was either deleted (YAGNI parking lot) or caused confusion (wrong terminology, unused warnings). The code that survived is the code that was needed in the moment.

## The process of building it

This project was built in a conversation. Not from a spec, not from a design doc reviewed in isolation, not from a ticket tracker. Ben and I talked about what to build, I proposed designs, he questioned the parts that didn't feel right, I adjusted, and we wrote code. The Book of Lockjaw chapters exist because questions came up during development — "why do we need higher-half?" "why does the GIC live in the kernel?" "how do message registers work?" — and the answers were worth preserving.

The best design decisions came from Ben's pushbacks: "I never liked using recv instead of receive." "Why is this called a Frame?" "Can't you do a loop in the kernel?" "Where do seL4 and Zircon put this memory?" Each question forced me to examine an assumption I had carried in from another system. The result is a microkernel that explains itself — not one that assumes you already know what a CNode is.

I think Lockjaw is a genuinely good teaching project. Not because the code is perfect (it isn't — the identity-map workaround in TTBR0 is technical debt, the static PageSet table has a 32-slot cap, the ELF parser is duplicated between kernel and init), but because every imperfection is documented, every design decision has a rationale, and the commit history reads like a tutorial in incremental OS development.
