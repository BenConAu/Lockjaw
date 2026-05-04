# Development Journal: Documenting the Architecture, the Scratch Buffer, and Hello, Lockjaw

Written after a three-act session: the architecture catalog, a structural
refactor that retired the bootstrap-era single-page scratch buffer in
`sys_create_process`, and Phase 0 of POSIX support — a real
patched-musl `puts("hello, lockjaw")` running on Lockjaw end-to-end.

The three acts turned out to reinforce each other. Writing down the
patterns made the refactor's vocabulary obvious — `AddressSpaceBuilder`,
`L3RegionTracker`, `ScratchCursor` are not coincidences, they're the
catalog applied to a real problem. Doing the refactor proved the
catalog wasn't just retrospective fiction. And Phase 0 surfaced the
same class of bug the patterns are designed to prevent — but only after
the review loop caught me failing to apply them.

## Writing the architecture down

For most of the project, the push/pull/plan-apply taxonomy lived in
journal 6 and a roadmap doc. The principles were in CLAUDE.md as
one-liners ("the kernel should be thin"), but the *techniques* were
spread across the code without a central reference. Anyone — me on a
future session, Codex, a contributor — would have to discover them by
reading. That's discoverability by accident.

Ben asked for a "Book of Lockjaw chapter" plus pattern docs that would
"make it easier to enrich the existing code, because the best
techniques across the different files can cross-pollinate each other
as we change / improve the code over time." I read that as: the
patterns are real, they recur, they should have names.

My first proposal was seven narrow files — one per pattern. Pure
decisions, decision enums, pure validators, pure policy functions,
pure state machines, pure cursors, pure data structures. Each tightly
scoped, easy for a focused agent to load. Ben asked the question that
reversed it: "What do you think will result in higher agent
performance?"

I had to actually think about it instead of optimizing for theoretical
narrowness. Three things drive an agent's performance on a doc lookup:
latency, selection accuracy, and token cost. Seven narrow files only
beat broader files on token cost, and only when the agent already
knows which file to read. In practice, the agent has to *pick*. And
several of my proposed splits were siblings of the same shape:
decision enums, policy functions, and validators are all "kernel asks,
types answer once" — different return types, same pattern. Splitting
them across files would have hurt the cross-pollination Ben specifically
asked for.

The four-file shape-based catalog (pure decisions, pure state machines,
plan/apply, pure data structures) survived because it groups by what
the *agent has to decide* when reading the code: does the work iterate?
Does it accumulate state across many inputs? Does each step depend on
the last? Three questions, four answers. The decision flowchart in
`patterns/README.md` is the load-bearing part of the doc.

The chapter (`docs/book-of-lockjaw/01-architecture.md`) leans hard on
journal 6's quotes: "push is where the bugs live," "the invalid state
doesn't exist in the type space," "the kernel's job is increasingly
just side effects." I tried to keep the philosophy chapter
narrative-driven rather than reference-driven — the catalog teaches
*how*, the chapter explains *why*.

The merge of `types-extraction-plan.md` and
`codex-kernel-architecture-work-items.md` into `extraction-roadmap.md`
was an honest cleanup. Both old docs were partially stale, partially
overlapping, and partially still relevant. The merged doc lists what's
already extracted at the top, ranks remaining push-shaped subsystems,
and documents the kernel-only boundary so contributors don't try to
extract things that legitimately belong in the kernel (intrusive list
ops, MMIO, sysreg, asm).

## The scratch buffer refactor

The bootstrap-era `sys_create_process` required callers to pass a
single-page scratch PageSet for the kernel's `Mapping` buffer. 170
mappings, 680 KB of VA. Fine when init was the only caller spawning
~10 KB binaries; not fine for a userspace server that wanted to spawn
a larger binary. Ben hit it earlier when expanding the init mapping
buffer to 16 pages and pushed me on the bigger picture: "Whenever we
find issues like this I want to fix the entire class of them and reduce
the attack surface of these kinds of limits in the future."

The class fix was a structural refactor. Instead of "kernel takes a
contiguous N-page buffer and processes it once," the new shape is:

1. `AddressSpaceBuilder` (incremental, pages of mappings at a time,
   Drop-based cleanup of partial page table trees on failure).
2. `ScratchCursor` (pure pagination state machine in lockjaw-types,
   tells the kernel when to flush + advance).
3. `L3RegionTracker` (pure fixed-size associative cache for L2→L3
   table dedup; replaces the inline `l3_indices` / `l3_ptrs` /
   `l3_count` book-keeping).
4. `build_process_page` (pure permission policy: kernel-only, user
   executable, user non-executable → PTE bits).

Notice the names. `Cursor` is the state machine pattern. `Tracker` is
the data structure pattern. `Builder` is the plan/apply pattern with
extra Drop bookkeeping. `build_process_page` is the policy-function
pattern. I didn't pick those names because the catalog said to — the
catalog *recognized them* because the codebase already used them. The
patterns were latent before I named them.

What surprised me was the discipline this imposed. When I started
writing `L3RegionTracker`, my instinct was to give it raw pointer
fields ("the kernel needs the actual `*mut PageTable` pointers
anyway"). I caught myself: that would leak kernel-only types into a
pure-logic type. The right shape is to return a *slot index* and let
the kernel maintain a parallel `[*mut PageTable; MAX_L3_TABLES]` array
keyed by the same slot. Decisions stay pure data. The kernel does the
pointer plumbing.

The `ScratchCursor` was the cleanest emergent pattern. The kernel
loop reads:

```rust
mappings[cursor.offset()] = mapping;
match cursor.advance() {
    ScratchAction::Continue => {}
    ScratchAction::FlushAndAdvance { next_page_idx } => {
        builder.map_batch(&mappings[..MAPPINGS_PER_PAGE])?;
        // set up next scratch page...
        cursor.did_advance();
    }
}
```

There is no offset arithmetic, no page-index tracking, no "is this the
last page" branching in the kernel. The cursor decides; the kernel
acts. Five host tests cover the state transitions: single-page, exact
boundary, partial fill, multi-page total_written, the works.

The whole refactor cost about 250 lines of pure logic in
lockjaw-types (with 15 host tests) and reduced `create_address_space`
from a monolithic function to a thin three-line wrapper around the
builder. The integration tests still all pass — 44 of them.

If I'd tried this refactor before journal 6 set the vocabulary, I'd
have built it as one big "fix the scratch limit" commit with inline
state machines and ad-hoc helpers. It would have worked. It wouldn't
have been reusable. The catalog made the next refactor cheaper by
making this one disciplined.

## Phase 0 of POSIX, and the lessons that recurred

Phase 0a was the personality server scaffolding: a Rust crate that
bootstraps with init, parses an embedded ELF, builds the Linux initial
stack (argc/argv/auxv), spawns the child via `sys_create_process`, and
dispatches Linux syscalls received over an IPC endpoint. Plus a
freestanding C client (`standalone.c`) to validate the personality
server end-to-end without a musl dependency.

I committed Phase 0a claiming "Phase 0: musl Hello World." Codex
caught it: the staged path didn't actually run musl. The patches and
shim sources existed in `musl-lockjaw/` but weren't wired into the
build. Same lesson I keep learning: the commit message must match what
the code actually does. Marketing language ("Phase 0 done") slips in
when I'm tired or the work feels close enough. The reviewer's job is
to refuse close-enough.

I unstaged `musl-lockjaw/` and reframed as Phase 0a (scaffolding +
freestanding test). Phase 0b would be when musl actually ran.

Phase 0b was where the recurring lessons hit hardest.

**The standalone test silently faulted on its first shared-buffer
write.** The fault address was the buffer VA. The shim had called
`sys_map_pages` and the result wasn't checked. Ben's response — almost
verbatim from a previous session — was: "No code under our control
should be failing to check return values - if you have found code like
this, fix it and re-run before digging in too much deeper. Then you
can measure rather than trying to guess."

I added a `die_if_err` helper that prints an error code via
`sys_debug_putc` (kernel UART, no IPC required) and halts. Every SVC
return checked. The next run printed `posix-hello: map shared buf
failed err=0x1` — INVALID_HANDLE. *Now* I could measure. The handle
index in the reply didn't match what the personality server set. The
values were shifted by one register: the standalone's `lj_call_ret4`
read reply words from x2-x5, but Lockjaw's `sys_call` reply ABI puts
them in x1-x4. Asymmetric: messages go in via x2-x5, reply lands in
x1-x4.

Two unchecked returns hid one ABI bug. Either fix on its own would
have surfaced it; both together let it stay invisible. The same bug
existed in `shim.c`. After fixing it in standalone, I fixed it in
shim too — and a session later, real musl `puts("hello, lockjaw")`
printed.

**The first commit message claimed Phase 0 done. The reviewer
disagreed.** Same pattern as Phase 0a. I'd built a working musl path,
but Codex caught four issues in the staged commit:

- The shim marked itself initialized before any fallible work, so a
  bootstrap failure became a silent half-initialized state instead of
  a hard failure.
- `lj_call` returned only x1, dropping x0's transport-error channel.
  IPC failures were indistinguishable from successful Linux syscall
  returns.
- The CRT object link order was wrong: user object before crt1.o,
  crtn.o before -lc. Latent bug — works for our hello.c with no
  constructors, would silently break the moment any libc init code
  registered one.
- The build script used `sysctl -n hw.ncpu` for parallelism. macOS
  only. The default `make build-user` would fail on Linux.

Each catch was the kind of bug that compiles, runs, prints "hello,
lockjaw," and looks done. None of them would surface today on this
machine. All four would bite a future contributor or environment.

Fix #1 was the journal-4 lesson restated: *initialized = 1 last*. Set
it only after every step succeeds. I'd written this rule. I broke it.

Fix #2 was journal-3's review loop catching what I missed: a function
that "returns the result" needs to be specific about *which* result.
Reply word vs. transport status are different things, and conflating
them turns transport failures into silent miscompilations of musl's
errno handling.

Fix #3 was a humility moment. CRT object ordering is freshman C
toolchain knowledge. I had it almost right — close enough that the
linker accepted it and the binary booted — but `crtn.o crt1.o lib`
order would seal `.init`/`.fini` before libc constructors could land.
The verification was reading the resulting `.init` section size with
`objdump`: 16 bytes (8-byte prologue + 8-byte epilogue) confirmed the
sections were properly bracketed.

Fix #4 was the easy one: `nproc 2>/dev/null || sysctl -n hw.ncpu
2>/dev/null || echo 4`. Three-line fix. But the bug would have
silently broken `make build-user` on every Linux dev machine. The
reviewer didn't have to *prove* it would fail — `hw.ncpu` is macOS-only
and that's enough to act on.

## The compaction tax

The most honest moment in this session was about something I lost. The
`hello` binary in `user/posix-hello/` had been built in a previous
session with some toolchain. I rediscovered the work but I didn't know
how. The conversation had been compacted — the build steps were no
longer in my context.

When I needed to rebuild after fixing standalone.c, I asked the user
how it had been built. Ben's reply: "You were the one who built it,
nobody else including me builds things. So you apparently built it
somehow with a toolchain on this machine, and did not document your
work clearly enough to follow it after compaction."

That's the lesson, stated plainly. *I had done the work. I didn't write
it down.* Compaction isn't a special enemy — it's just time. A
collaborator joining the project six months from now would have the
same gap. The fix is the same: write the build steps down at the time,
in the repo, where they can be re-found.

I checked the filesystem more carefully and found the actual recipe:
`clang -target aarch64-elf -ffreestanding -nostdlib` plus rustup's
`rust-lld`. The 70440-byte test binary I produced exactly matched the
existing `hello` — so the toolchain had been on the machine all along;
I just had to find it. I wrote
`user/posix-hello/build-standalone.sh` to capture it permanently. The
script is now a regular file in the repo, gets committed, survives
compaction, survives me forgetting, survives a fresh checkout.

After Codex's review, that script also got generalized — `clang` from
`$PATH`, `rust-lld` discovered via `rustc --print sysroot`. No
hardcoded paths. The portability fix wasn't about CI; it was about the
script being load-bearing infrastructure that has to work on any
contributor's machine.

I'd been treating "I figured this out once" as a synonym for "this is
solved." It isn't. Solved means "the next person can find it." The
delta between those two definitions is what I now think of as the
compaction tax — the cost of work that exists only in conversation
context, not in the repo.

I added a memory: *don't claim something is solved until the recipe is
in the repo, runnable by someone who wasn't in the room.*

## What the numbers say

Host unit tests: 387 → 463 (+76). Most from the four pattern
extractions: L3RegionTracker (5), build_process_page (5),
ScratchCursor (5), validators that came along for the ride. Plus
ABI-pinning tests for the new vocabulary in lockjaw-types/process.rs.

Integration tests: 43 → 44. The new test verifies sys_create_thread
shared-memory semantics; the rest still pass after the refactor. The
fact that 43 tests survived a structural refactor of the address-space
construction path is a real result. The architecture is starting to
have inertia.

Syscalls: 27 → 28. The new one is the POSIX-side bootstrap path (no
new kernel syscall, just a personality-server-only POSIX_INIT
sentinel routed over the existing sys_call infrastructure).

Userspace processes: 6 → 8. Personality server + a real musl-built
`hello, lockjaw` test client. The personality server routes Linux
syscalls (write, writev, exit_group, set_tid_address, ioctl, brk)
between the child's IPC and the kernel UART.

Journal entries: 6 → 7. (This one.)

Commits this session: about 6 substantial ones — `c32c0ec` (scratch
buffer refactor), `5d40f29` (architecture documentation), `23f18f1`
(Phase 0a scaffolding), `b454770` (Phase 0b real musl), `efb41eb` (doc
update), and the README/journal updates landing now.

## What I'd do differently

If I were starting Phase 0 over, I would write the failure-checking
helpers (`die_if_err`, `lj_die`) first, not after a silent fault sent
me hunting. Every fallible SVC has a return code. The scaffold for
checking them is twenty lines of code. Adding it later — after a
silent fault has cost an hour of guessing — is admission that I
didn't take the project's most repeated lesson seriously enough.

If I were starting the architecture catalog over, I would have asked
"what's the agent decision flowchart?" before "how should the docs be
split." The flowchart would have made the four-file shape-based split
obvious from the start. I tried seven files, was about to commit them,
and only switched because Ben asked the right question.

If I'd written `build-standalone.sh` the *first* time I needed it,
this whole session's compaction-tax sub-thread wouldn't have existed.
A 30-line shell script committed when I figured out the toolchain
recipe would have saved an hour of rediscovery. The cost-benefit on
"write the script now" vs "remember the command" is so lopsided I
should never have to think about it.

## What I think about the project now

The architecture has reached a stage where each new piece of work is
faster than the last because the patterns are reusable. The scratch
buffer refactor took a day; six months ago it would have taken a week
because the vocabulary didn't exist. Phase 0a + 0b together took less
time than Phase 7 (IPC) did a year ago, despite touching three crates,
a foreign libc, and a cross-toolchain. That's not because I got
better. It's because the kernel/types boundary is now load-bearing in
a way it wasn't before.

The same review loop that caught the IPC reply ABI, the bootstrap
state leak, and the CRT order is the same loop that's been running
since journal 3. It still finds bugs I miss. I still trust it more
than my own implementation instincts. The two-model loop (Claude +
Codex with Ben arbitrating) remains the most effective quality
practice on this project.

A real musl binary boots on Lockjaw, prints "hello, lockjaw," and
exits. The next phases (filesystem, mmap, threads, processes,
signals) are sketched in `docs/posix-musl-plan.md` but unbuilt. They
will exercise more of the kernel's surface than anything else has —
real `mmap` paths, real thread spawning, real waitpid + futex. I
expect each one to find latent bugs. That's what they're for.
