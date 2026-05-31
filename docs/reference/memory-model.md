# Memory Model

Lockjaw's kernel does not dynamically allocate memory through any
heap or `alloc` crate. Every byte of kernel-resident state is either
in BSS (statically sized, decided at link time) or in pages handed
out by one of two dedicated kernel allocators: the **buddy** for
physical pages userspace will own, and the **KVM pool** for kernel
objects whose storage the kernel itself manages.

## Where physical RAM comes from

The kernel does not hardcode RAM size. At boot, the DTB is parsed
for the `memory` node, and the answer lives in
`src/mm/addr.rs::ram_start()` / `ram_size()` / `total_pages()`. On
QEMU virt the values are 1-2 GiB at `0x4000_0000`; on Pi 4B they
are typically 4 GiB at `0x0000_0000`. The buddy allocator is
parameterized by `total_pages()` at init time — no `[u8; 4096]`
bitmap, no compile-time constants for page count.

## How RAM is partitioned

`src/mm/page_alloc.rs::init_with_gap` runs once at boot and
classifies every page of RAM into one of five categories:

| Region | Lifetime | Backing |
|---|---|---|
| `[ram_start, kernel_start)` | Reserved forever | Firmware, DTB, anything pre-kernel-image. Not registered with any allocator. |
| Kernel image | Reserved forever | `.text` / `.rodata` / `.data` / `.bss` sections. |
| Per-CPU stack region | Reserved forever | 4 KiB guard page + 8 KiB stack per CPU, `MAX_CPUS=4`. Stride = 12 KiB. See [`stack-budget.md`](stack-budget.md). |
| DMA pool | Carved off the tail | Highest 2 MiB block (`DMA_POOL_PAGES = 512` from `lockjaw-types/src/dma_pool.rs:56`). Owned by `src/cap/dma_pool.rs`, not buddy. |
| Everything else | Free | Registered with `BuddyAllocator` at boot, handed out via `sys_alloc_pages` / `sys_alloc_dma_pages`. |

The two ranges that go to buddy are:
1. `[kernel_end, stacks_start)` — the alignment gap between the
   kernel image and the 2 MiB-aligned per-CPU stack region.
2. `[stacks_end, dma_pool_base)` — most of the free RAM.

The post-pool tail `[dma_pool_end, ram_end)` also goes to buddy if
there's anything there.

## The buddy allocator

`lockjaw-types/src/buddy.rs::BuddyAllocator` is a textbook
order-based buddy. `MAX_PAGES = 262144` (1 GiB at 4 KiB), `MAX_ORDER = 18`
(2^18 = 256K pages = 1 GiB). The kernel wraps it in `src/mm/page_alloc.rs`;
under SMP (landed in Phase 11), concurrent access is serialized by
the Giant Kernel Lock (`src/sched/gkl.rs`) — every syscall and IRQ
entry takes the GKL, so the page allocator's `unsafe impl Sync`
relies on "GKL held, IRQs masked" rather than the older single-core
invariant. The end-state is to break the GKL into per-subsystem
locks (a `SpinMutex` around buddy state would be one of them);
that's tracked in [`../tracking/tech-debt.md`](../tracking/tech-debt.md)
under "UnsafeCell globals serialized only by GKL".

Public API:
- `alloc_page() -> Option<PhysPage>` — single-page allocation
  (`page_alloc.rs:149`).
- `alloc_pages_contiguous(count: usize) -> Option<PhysPage>` —
  multi-page, physically contiguous (`:160`).
- `dealloc_page(page)` / `dealloc_pages_contiguous(first, count)` —
  returns to buddy (`:173`, `:184`).
- `zero_page(paddr)` — zero-fills via the linear KVA map (`:196`).
- `free_count()` — diagnostic (`:140`).

Userspace reaches buddy through `sys_alloc_pages(count, flags)`:
`ALLOC_FLAG_CONTIGUOUS=1` routes to `alloc_pages_contiguous`,
otherwise the request walks the buddy.

## The DMA pool

The 2 MiB tail carve-out (`src/cap/dma_pool.rs`) exists because DMA
allocations have different cache attributes from regular pages.
Post-C1 of the cacheable-DMA migration (see
[`../history/cacheable-dma-migration-plan.md`](../history/cacheable-dma-migration-plan.md)),
the pool participates in the kernel TTBR1 direct map as Normal
Cacheable — same MAIR slot as the rest of RAM — but it is allocated
through its own `dma_pool::alloc_pages` / `free_pages` rather than
through buddy. Two reasons:

- **Origin discipline.** A pageset returned by `sys_alloc_dma_pages`
  carries `PageSetOrigin::DmaPool`; one from `sys_alloc_pages`
  carries `PageSetOrigin::Buddy`. The DMA-sync syscalls reject
  Buddy-origin pages with `INVALID_PARAMETER` — only DmaPool pages
  are valid for `sys_dma_sync_for_{cpu,device}`. This origin tag
  is the runtime check; the type-level `SyncCapable` gate in
  `lockjaw-userlib::dma` is the compile-time enforcement (see
  [`../architecture/04-driver-substrate.md`](../architecture/04-driver-substrate.md)).
- **Allocator isolation.** Feeding pool pages into buddy would
  double-issue them. The pool is its own bitmap allocator
  (`lockjaw-types/src/dma_pool.rs::DmaPool`) over a fixed PA range.

The pool's physical base is computed at boot in `init_with_gap`:
round `ram_end` down to a 2 MiB boundary, subtract `DMA_POOL_PAGES`
× `PAGE_SIZE` to get `pool_base_phys`. If RAM is too tight to fit
the carve-out aligned, init logs a warning and the pool is empty —
`sys_alloc_dma_pages` then returns `OUT_OF_MEMORY` (cannot happen
on Pi 4B or QEMU virt today; both have ≥ 1 GiB).

## The KVM allocator

Kernel objects (TCBs, endpoints, handle tables, PageSet headers,
process pages) live in a higher-half virtually-contiguous range
managed by `src/mm/kvm.rs` + `lockjaw-types/src/kvm.rs`. The KVM
pool occupies `L0[256]` at `0xFFFF_8000_0000_0000` and stitches
together N physically-discontiguous pages into N pages of virtually
contiguous KVA. The roadmap that motivated the KVM allocator lives
at [`../tracking/kernel-vmem-roadmap.md`](../tracking/kernel-vmem-roadmap.md).

The userspace-facing entry remains the PageSet handle: userspace
doesn't allocate kernel objects directly. The kernel allocates KVM
pages for the object, then returns a handle. The PageSet underlying
the object is bookkeeping in the kernel; userspace never sees the
KVA.

## Who owns what

| Memory class | Allocator | API to userspace |
|---|---|---|
| User PageSets (Buddy origin) | `page_alloc` (buddy) | `sys_alloc_pages` |
| User PageSets (DmaPool origin) | `cap::dma_pool` | `sys_alloc_dma_pages` |
| Kernel objects (TCB, Endpoint, etc.) | `mm::kvm` | (kernel-internal; userspace gets handles) |
| Kernel image + stacks + DTB | Linker / boot | (none — reserved at init) |

## The static budget

Kernel-resident BSS state, on top of the per-CPU stack region:

| Item | Size | Notes |
|---|---|---|
| Boot page tables | ~24 KB | L0 + L1 + L2 + L3 tables (static arrays) |
| Per-CPU stacks | 32 KiB usable + 16 KiB guards | 4 CPUs × (8 KiB stack + 4 KiB guard), stride 12 KiB |
| BuddyAllocator state | ~96 KiB | ~64 KiB per-order freelist bitmap (sum across MAX_ORDER=18 orders for MAX_PAGES=262144) + ~32 KiB allocated bitmap. Source comment: `lockjaw-types/src/buddy.rs:41`. |
| DmaPool bitmap | 64 bytes | 512 bits for `DMA_POOL_PAGES = 512` |
| KVM allocator state | small (varies) | Free-list + level-page bookkeeping |
| BSS misc | varies | Counters, global state |

No kernel memory grows at runtime in a way that can fail.
The stack-budget invariant is proven by `cargo xtask check-stack`;
see [`stack-budget.md`](stack-budget.md). The buddy and KVM
allocators return `Option::None` / `Result::Err` on exhaustion —
the kernel never panics on allocation failure.
