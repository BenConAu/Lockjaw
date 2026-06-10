# The `no-kernel-alloc` principle

> The Lockjaw kernel allocates memory **only during bootstrap**. After
> the bootstrap→userspace handoff, every syscall handler runs without
> the ability to allocate fresh physical or virtual pages. Userspace
> donates memory; the kernel reinterprets the donated page as kernel-
> object backing.

This document codifies the principle, audits where the kernel
currently violates it, and proposes a structural enforcement
mechanism that makes new violations difficult to introduce once the
existing ones are fixed.

## 1. The principle

### Statement

The kernel may allocate pages, KVM mappings, and kernel-object backing
storage **only during the bootstrap phase**: from `kernel_main` entry
through the moment the first userspace process is scheduled. After
that boundary, the allocator surface that produces fresh memory is
**not callable from any code reachable through a syscall handler**.

The kernel still **owns** memory at runtime — handle tables, page-
table walks, refcounts, free-lists — but it does not **grow** memory
at runtime. The pool is sized at boot; runtime activity recycles
within the pool.

### Why

1. **Type-level invariant: "kernel cannot fail from memory"**. Every
   kernel allocation is a possible `OUT_OF_MEMORY` syscall return,
   which complicates every error path that reaches it. Removing
   the allocator from the runtime hot path removes the failure mode
   from the type system — `SyscallError` shrinks, kernel control
   flow simplifies, and the invariant "the kernel doesn't fail
   because of resource exhaustion" becomes structural.
   (`docs/tracking/tech-debt.md:43-61` already names this as the
   driving rationale.)

2. **Capability discipline**. seL4's foundational design — and
   Lockjaw's inheritance of it — is that userspace owns memory and
   donates it to the kernel for specific kernel-object purposes. The
   kernel never reaches for memory that wasn't given to it. This
   makes resource accounting strictly userspace's job, eliminates
   covert channels through kernel heap pressure, and makes
   `OUT_OF_MEMORY` a userspace-allocator concern rather than a
   kernel-syscall concern.

3. **Bounded latency**. Every runtime allocation is a potentially-
   unbounded operation (allocator walks, free-list scans, page-table
   tree growth). Pre-allocating at bootstrap means syscall paths are
   bounded by table-index lookup time, not heap pressure.

4. **Auditability**. A kernel that allocates at runtime has many
   places where a refcount bug, a leak, or a fragmentation pattern
   can manifest. A kernel that only allocates at bootstrap has one
   place to audit: the boot path.

### The canonical correct pattern (donate-and-claim)

The shape we want every kernel-object creation to take, already shipped
in `sys_create_endpoint` / `sys_create_notification` / `sys_create_reply`
(`src/syscall/handler.rs:691-717`):

```
1. Userspace calls sys_alloc_pages(1) → receives PageSet handle.
2. Userspace passes the PageSet handle to sys_create_<object>.
3. Kernel calls kvm::map_existing on the donated PageSet → reserves
   PTEs in the pre-allocated KVM range. NO fresh allocation.
4. Kernel writes the kernel-object struct in-place on the donated
   page (the page is now kernel-owned for the lifetime of the object).
5. Kernel inserts a handle into the caller's table — pre-allocated
   at process creation. No allocation.
```

The kernel's part is **map an existing page** and **construct in-place**.
Neither step grows kernel memory. Each runtime violation we need to
fix is a syscall that today does step (3) differently — it allocates
fresh — and the fix is to migrate it onto the same donate-and-claim
shape. Architectural reference: `docs/reference/object-model.md:31-59`.

## 2. Current violations

Audit conducted across `src/process.rs`, `src/syscall/handler.rs`,
`src/cap/`, `src/mm/`, and `src/arch/aarch64/`. **16 violation sites in
5 files**, all reachable through 4 syscalls.

### V1-V6 — Process and thread creation (most leverage)

| # | File:line | Trigger | Backing |
|---|-----------|---------|---------|
| V1 | `src/process.rs:410` | `sys_create_process` | ProcessObject page (1 KVM page) |
| V2 | `src/process.rs:535` | `sys_create_process` | HandleTable backing page |
| V3 | `src/process.rs:561` | `sys_create_process` | Initial-thread stack page |
| V4 | `src/process.rs:564` | `sys_create_process` | Initial-thread TCB page |
| V5 | `src/syscall/handler.rs:1139` | `sys_create_thread` | Thread stack page |
| V6 | `src/syscall/handler.rs:1144` | `sys_create_thread` | TCB page |

All six are `kvm::alloc_kernel_pages(1)` calls in the runtime syscall
path. **Fix shape**: same as the canonical pattern — the parent
process allocates PageSets (4 for process creation, 2 for thread
creation) and donates handles to the syscall. The HandleTable's
fixed-size array fits in a 4-KiB page by design; same for the TCB
and the ProcessObject.

### V7-V10 — PageSet header allocations

| # | File:line | Trigger | Backing |
|---|-----------|---------|---------|
| V7 | `src/cap/pageset_table.rs:75` | `sys_alloc_pages` | PageSet header (KVM page) |
| V8 | `src/cap/pageset_table.rs:131` | `sys_alloc_pages_contiguous` | PageSet header |
| V9 | `src/cap/pageset_table.rs:206` | `sys_alloc_dma_pages` | PageSet header |
| V10 | `src/cap/pageset_table.rs:279` | `sys_register_device_page` | PageSet header |

These don't fit donate-and-claim — `sys_alloc_pages` is the userspace
mechanism to *acquire* a new PageSet, so the headers can't come from
a user-supplied page (we'd need a PageSet to make the PageSet).

**Fix shape**: pre-allocate a bounded `PageSetHeader` pool at
bootstrap, sized to a documented `MAX_PAGESETS`. Free list discipline
recycles headers on PageSet close. This is one of the bootstrap-sized
pools, not a user-donated path.

### V11-V14 — Page-table allocations during process creation

| # | File:line | Trigger | Backing |
|---|-----------|---------|---------|
| V11 | `src/arch/aarch64/vmem.rs:52` | `sys_create_process` → `AddressSpaceBuilder::new` | L0 page table |
| V12 | `src/arch/aarch64/vmem.rs:69` | same | L1 page table |
| V13 | `src/arch/aarch64/vmem.rs:78` | same | L2 page table |
| V14 | `src/arch/aarch64/vmem.rs:116` | `AddressSpaceBuilder::map_batch` | L3 page tables (≤8 per region) |

V11-V13 are bounded (one each per address space) — fixable by a
per-process page-table pool donated by the parent at
`sys_create_process` time (4 pages: L0, L1, L2, plus the
`AddressSpaceBuilder` workspace).

V14 is harder: L3 tables are sized by the number of mapped regions,
which scales with the userspace program. The pragmatic compromise
named in `docs/tracking/tech-debt.md:56`: parent process donates a
generous PageSet (e.g. 16 pages) sized to the worst observed L3 count
across the existing test corpus, and `AddressSpaceBuilder` claims
from it. If the userspace program needs more, it fails the syscall
with a typed error rather than the kernel growing its own pool.

### V15-V16 — KVM allocator's own page-table growth

| # | File:line | Trigger | Backing |
|---|-----------|---------|---------|
| V15 | `src/mm/kvm.rs:387` | any caller of `kvm::alloc_kernel_pages` | KVM L2 (allocator metadata) |
| V16 | `src/mm/kvm.rs:407` | same | KVM L3 (allocator metadata) |

The KVM tree itself grows when a caller asks for an L3 it hasn't
seen before. **Fix shape**: at bootstrap, pre-allocate the KVM
page-table tree sized to cover the **working portion** of the KVM
pool (`KVM_POOL_USABLE_SIZE`), distinct from the L0/L1 carve-out
(`KVM_POOL_VA_SPAN`). Working-size is driven by a cap-anchored
formula over `MAX_PAGESETS`, `MAX_THREADS`, and the audited
per-process kernel-object footprint, with explicit headroom for
kernel objects not yet enumerated. Pre-allocating the full VA span
would cost ~1 GiB of metadata — unworkable on Pi 4B; the
working-pool split is the workable shape. After this,
`kvm::alloc_kernel_pages` never grows its own metadata at runtime;
it only walks pre-existing PTEs and binds physical frames.

Delivered in NK1-A (commit reference: post-NK1-A `git log`); the
walker variants for the unreachable growth path get deleted in
NK1-B's pure-types cleanup.

### Already-correct surfaces

- `src/syscall/handler.rs:691-717` (`sys_create_endpoint`,
  `sys_create_notification`, `sys_create_reply`) — donate-and-claim,
  the canonical pattern.
- `src/syscall/handler.rs:941-999` (`sys_export_handle`) — pre-
  allocated handle table slot.
- `src/arch/aarch64/irq_bind.rs:66` — static `[Option<Binding>; 256]`.
- `src/cap/dma_pool.rs` — 2 MiB pre-reserved region carved at
  bootstrap, recycled on free.
- Scheduler run queue — static array indexed by thread id
  (`docs/tracking/tech-debt.md:37` flags this as the design choice
  that avoided dynamic run queues for exactly this reason).
- `page_alloc::alloc_page` itself for **userspace-requested**
  pages via `sys_alloc_pages` (line `pageset_table.rs:162-178`): this
  is the legitimate path — userspace asks for a physical page, the
  kernel carves one from the bootstrap-initialized buddy heap and
  hands ownership back. The buddy heap is sized at boot from the
  device tree; runtime activity carves within that fixed pool.

## 3. Structural enforcement proposal

Once the violations are fixed, we need a mechanism that prevents
new ones from being introduced. Reviewer discipline alone has not
held — the `tech-debt.md` entry has been open since process creation
landed, and new allocation sites have been added inside it (e.g.
`sys_create_thread`). The fix is to make new violations **structurally
impossible to write**, not just visible in review.

### Mechanism A: consumed-at-handoff allocator (typestate)

The kernel allocator is currently a set of free functions
(`page_alloc::alloc_page`, `kvm::alloc_kernel_pages`). Restructure
into a `BootstrapAllocator` struct that owns the allocator state and
exposes `alloc_*` methods; at the bootstrap→userspace handoff, the
struct is **consumed** by `into_runtime_view()`, which returns a
`RuntimeAllocator` exposing only the methods legal at runtime
(`free`, `lookup`, refcount ops). The `alloc_*` methods do not exist
on `RuntimeAllocator`.

Sketch (illustrative; exact names TBD):

```rust
// src/mm/allocator.rs

pub struct BootstrapAllocator {
    buddy: BuddyAllocator,
    kvm: KvmAllocator,
    // ... other allocation-capable state
}

impl BootstrapAllocator {
    /// Construct from the device-tree memory map. Called once,
    /// at kernel_main bootstrap.
    pub fn from_dtb(memory_map: &MemoryMap) -> Self { ... }

    /// Allocate a fresh physical page. Available only during bootstrap.
    pub fn alloc_page(&mut self) -> PhysPage { ... }

    /// Allocate a fresh kernel virtual range backed by physical pages.
    pub fn alloc_kernel_pages(&mut self, n: usize) -> KernelVa { ... }

    // ... other alloc_* methods.

    /// Consume the bootstrap allocator. Returns the runtime view
    /// — which has NO alloc_* methods. Called at the bootstrap→
    /// userspace handoff in main.rs.
    pub fn into_runtime_view(self) -> RuntimeAllocator {
        RuntimeAllocator {
            buddy: self.buddy,
            kvm: self.kvm,
        }
    }
}

pub struct RuntimeAllocator {
    buddy: BuddyAllocator,
    kvm: KvmAllocator,
}

impl RuntimeAllocator {
    /// Free a previously-allocated page. Available at runtime.
    pub fn free_page(&mut self, page: PhysPage) { ... }

    /// Look up the owner of a physical page. Available at runtime.
    pub fn lookup_owner(&self, page: PhysPage) -> Option<ProcessId> { ... }

    /// Adjust refcount.
    pub fn refcount_inc(&mut self, page: PhysPage) { ... }
    pub fn refcount_dec(&mut self, page: PhysPage) -> bool { ... }

    // NO alloc_* METHODS.
}
```

The kernel's syscall handlers receive `&mut RuntimeAllocator`. They
cannot call `alloc_*` because those methods don't exist on the type
they're holding. This is a **compile-time** guarantee: the violation
class is unrepresentable in any syscall handler that doesn't reach
through unsafe to a global, and the latter is independently caught by
existing `unsafe`-discipline gates.

The free-function shims at `src/mm/page_alloc.rs` and
`src/mm/kvm.rs` (today: `alloc_page()`, `alloc_kernel_pages()`)
become bootstrap-phase methods only; the symbols disappear after
handoff. Any caller that referenced them by free-function name needs
to be rewritten to take `&mut BootstrapAllocator` — and runtime
callers can't get one because there is none after handoff.

Cost: every bootstrap call site that constructs kernel objects
threads a `&mut BootstrapAllocator` through its signatures. This is
a one-time refactor; the threading dies at the handoff. After that,
the runtime call sites see only `&mut RuntimeAllocator` and the
question "could I sneak in an alloc?" is closed by the type checker.

### Mechanism B: `cargo xtask check-kernel-alloc` AST backstop

Parallel to the planned `check-driver-unsafe` and the proposed
`check-server-handwritten`, an AST scanner that runs in `make build`
and rejects calls to allocation functions from any kernel source
file not on a small allowlist (the allocator itself, the bootstrap
path).

Scope:
- Scan all `src/**.rs` (excluding `src/mm/allocator.rs`,
  `src/main.rs`, and explicitly allowlisted bootstrap files).
- Reject any token reference to `BootstrapAllocator::alloc_*`,
  `alloc_page`, `alloc_kernel_pages`, `alloc_pages_contiguous`,
  `Buddy::alloc`, `PageAlloc::alloc`, `Vec::with_capacity`,
  `Box::new`, `BTreeMap::new`, or any other function name we
  enumerate as "produces fresh memory".
- Reject macro-token-stream forms as well (raw-ident
  normalization, like the driver-unsafe scanner does).
- Allowlist a small set of bootstrap files by path; if a new file
  needs to be on the allowlist, it requires an explicit
  `docs/tracking/tech-debt.md` entry and a reviewer sign-off in the
  commit.

This catches: someone re-introducing free-function `alloc_*` shims
that bypass the typestate; someone adding a new `Vec::with_capacity`
in a syscall path; someone reaching into `alloc::*` from a runtime
file. The typestate (Mechanism A) is the primary line of defence; the
xtask is the explicit-init backstop that catches any escape valve.

This is the same shape as the planned `check-driver-unsafe`
(`CLAUDE.md` "Engineering Principles" — user-mode drivers consume
`lockjaw-userlib`, period) and `check-server-handwritten` (proposed
in `docs/ipc-wirespec-plan.md`). Three structural gates, three
different correctness invariants, same enforcement shape.

### Mechanism C: codify the principle in `ben_principles.md`

Today the principle exists as oral tradition + a
`docs/tracking/tech-debt.md:43-61` entry. New code reaches for the
allocator unprompted because the rule isn't in the canonical
principles file. Add it explicitly:

```markdown
## Tier 2 — kernel architecture

### Nº — The kernel allocates memory only during bootstrap

After the bootstrap→userspace handoff, no syscall handler may call
into the page allocator, the KVM allocator, or any heap construct
that produces fresh memory. Kernel-object creation follows the
donate-and-claim pattern: userspace passes a PageSet handle, the
kernel calls `kvm::map_existing` and constructs the object in-place.

Why: type-level invariant "kernel cannot fail from memory";
capability discipline; bounded syscall latency. See
`docs/architecture/no-kernel-alloc.md`.
```

Codifying the principle is the lowest-cost change (one-paragraph
edit) and the highest-leverage change for guiding new development.
Mechanism A makes it impossible to write a violation; Mechanism B
catches escape valves; Mechanism C tells the next contributor what
the rules are before they write the violation.

### Why all three, not just one

- **A alone** (typestate) is bypassable: a contributor frustrated by
  threading `&mut BootstrapAllocator` may declare a static
  `static mut FALLBACK_ALLOCATOR: BootstrapAllocator` (with
  `unsafe`). This is structurally hard but socially possible.
- **B alone** (AST gate) is heuristic: a renamed function, a
  macro-generated call, or a new allocation primitive escapes the
  scanner until someone updates it. The scanner is also unfriendly
  to read in review; finding the rule it enforces requires reading
  `xtask/src/check_kernel_alloc.rs`.
- **C alone** (documented principle) is reviewer-dependent: this is
  exactly the regime we have today, and it is leaking.

Together: the type system makes violations structurally hard, the
xtask catches escape valves, and the principle file tells anyone
reading the codebase why the constraint exists. Each layer covers
the others' weak points.

## 4. Migration ordering

Direct prescription for a follow-on plan; each phase is a small
number of commits, soundness over speed.

### Phase NK0 — Codify the principle

Add the Tier-2 entry to `docs/process/ben_principles.md` referencing
this document. Add the reference from `CLAUDE.md` "Engineering
Principles". This is the discipline gate while the fixes ship.

No code change; one-commit deliverable.

### Phase NK1 — Pre-allocate KVM page-table tree (V15-V16)

The KVM allocator's own L2/L3 growth is the lowest-blast-radius fix:
pre-allocate the full KVM page-table tree at bootstrap, sized to
cover the entire KVM pool. After this, `kvm::alloc_kernel_pages`
never grows its own metadata at runtime; it only walks pre-existing
PTEs and binds physical frames.

Acceptance: `make test` 104/104 + `make pi` 0xAA55 unchanged;
runtime `alloc_page` calls inside `kvm.rs:387`/`:407` removed.

### Phase NK2 — Bounded PageSet header pool (V7-V10)

Pre-allocate a `[PageSetHeader; MAX_PAGESETS]` array at bootstrap
with a free-list. The four `kvm::alloc_kernel_pages` calls in
`pageset_table.rs:75/131/206/279` become `pageset_header_pool.claim()`.

Acceptance: `sys_alloc_pages`, `sys_alloc_pages_contiguous`,
`sys_alloc_dma_pages`, `sys_register_device_page` retain wire-
compatible behaviour; on header-pool exhaustion they return
`SyscallError::OutOfPageSets` (a new typed error that names the
bounded resource — distinct from the existing
`OutOfMemory`).

### Phase NK3 — Donate-and-claim for thread creation (V5-V6)

Smaller than process creation, exercises the pattern for the new
shape. `sys_create_thread` adds two parameters: a stack PageSet
handle and a TCB PageSet handle. Caller allocates them, donates them,
kernel `map_existing` + in-place init.

Acceptance: existing `make test` thread-creation paths pass;
hand-crafted unit test for the donate path; engines that previously
called `sys_create_thread` migrated to allocate-then-donate.

### Phase NK4 — Donate-and-claim for process creation (V1-V4)

Largest scope; mirrors NK3 but with 4 donated PageSets instead of 2,
plus the per-process page-table pool (NK5). Parent process is the
init server in practice; this means modifying `init` and
`posix-server` (the only services that today spawn processes).

Acceptance: same as NK3 plus a successful POSIX `fork` /
`exec` integration path.

### Phase NK5 — Pre-allocated per-process page table pool (V11-V14)

Bundled with NK4 because they share the donate path. Parent donates
an additional PageSet (e.g. 16 pages) for the page-table workspace;
`AddressSpaceBuilder::new` and `map_batch` claim from it. On pool
exhaustion (V14 case), the syscall returns
`SyscallError::OutOfPageTables` — userspace's call to react, not the
kernel's.

Acceptance: same as NK4 plus a process with many mapped regions
(test fixture forces the L3-table growth case) succeeds while staying
within the donated pool, and exceeding it returns the typed error.

### Phase NK6 — Typestate refactor (Mechanism A)

Once all 16 violation sites are fixed, refactor the allocator surface
into `BootstrapAllocator` + `RuntimeAllocator` per Mechanism A. Every
runtime call site is rewritten to receive `&mut RuntimeAllocator`;
every bootstrap call site receives `&mut BootstrapAllocator`. The
handoff in `main.rs` calls `into_runtime_view()` and stores the
runtime view in the global scheduler state.

Acceptance: `make test` 104/104; the symbols
`BootstrapAllocator::alloc_*` are not referenced from any file in
`src/syscall/`, `src/cap/`, `src/ipc/`, or `src/sched/` (grep gate
in CI).

### Phase NK7 — `check-kernel-alloc` xtask (Mechanism B)

AST scanner parallel to `check-driver-unsafe`. Reject calls to
allocation functions from non-allowlisted files. Add to `make build`.

Acceptance: scanner unit tests cover each rejection class
(`alloc_page` direct call, `Box::new` in a runtime file,
`Vec::with_capacity` in a runtime file, macro-token alloc); injected
violation in one runtime file → build fails with the expected
finding; revert leaves the tree clean.

### Phase NK8 — Documentation closure

Remove the `docs/tracking/tech-debt.md:43-61` entry (the violations
are gone). Update `docs/architecture/01-architecture.md` to
cross-reference this document. Update the book chapter index if
applicable.

## 5. Open questions for the follow-on plan

1. **`SyscallError` shape**: today `OutOfMemory` is a generic error
   returned from many paths. After the migration, the bounded-pool
   errors (`OutOfPageSets`, `OutOfPageTables`, etc.) are
   userspace-visible signals about specific resources. Do we drop
   `OutOfMemory` from the ABI entirely on day 1 (forcing every
   migrated path to use a typed error), or keep it as a deprecated
   value until the migration completes? Drop it on day 1: that way
   partial conversions are compile errors, not silently-wrong runtime
   behaviour.

2. **MAX_PAGESETS sizing**: what's the actual upper bound across the
   test corpus today? The bounded pool size needs to be large enough
   that no current test fails, with a documented margin. Measure via
   instrumentation in NK0 prep.

3. **Per-address-space vs global page-table pool**: V11-V14 suggests
   per-process pools (donated at process creation). An alternative
   is a global pool managed by a kernel-level "page-table allocator
   server" running in userspace — the kernel never allocates page
   tables; a userspace policy server does. Out of scope for this
   document; tracked as future work after NK6.

4. **Bootstrap allocator state visibility from runtime**: after
   `into_runtime_view()`, who owns the buddy free-list and the KVM
   refcount tables? They have to be reachable for `free`,
   `refcount_inc/dec`, and `lookup_owner` at runtime — but not for
   `alloc_*`. The typestate places them on `RuntimeAllocator`; the
   detail is which fields are split and which are shared. Likely a
   one-day refactor; design surfaces in NK6.

5. **What counts as "bootstrap"**: the bootstrap phase ends when the
   first userspace thread is dispatched. But there are
   userspace-bringup activities (init's first few syscalls) that
   today are treated specially in tests. Are those still
   "bootstrap" for the allocator? Recommendation: no — once any
   userspace runs, allocator is in runtime view. The early-userspace
   bringup syscalls that need fresh memory must use donate-and-claim
   from the start. This means init donates pages back to the kernel
   for its own purposes, which is the exact dependency seL4
   established and which keeps the rule clean.

## 6. References

- `docs/tracking/tech-debt.md:37` — "no-kernel-alloc principle"
  named as an explicit constraint.
- `docs/tracking/tech-debt.md:43-61` — current violation inventory
  with rationale and per-site fix sketches.
- `docs/reference/object-model.md:31-59` — donate-and-claim
  canonical architectural reference.
- `src/syscall/handler.rs:691-717` — `sys_create_endpoint` /
  `sys_create_notification` / `sys_create_reply`: the canonical
  correct pattern, in code.
- `src/syscall/handler.rs:122-225` — `create_kernel_object_kvm`:
  the helper implementing donate-and-claim.
- `CLAUDE.md` "Engineering Principles" — host of the proposed
  Tier-2 numbered principle.
- `docs/process/ben_principles.md` — canonical principles file; the
  target of NK0.
- Parallel structural enforcement examples:
  - `check-driver-unsafe` (in progress) — same shape, different
    invariant (user-mode drivers consume `lockjaw-userlib`).
  - `check-server-handwritten` (proposed in
    `docs/ipc-wirespec-plan.md`) — same shape, different invariant
    (services consume `gen-ipc` output).
