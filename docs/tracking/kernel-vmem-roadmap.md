# Kernel virtual memory — roadmap

## Context

Kernel objects historically reached memory through TTBR1's linear
higher-half map at `KERNEL_VA_OFFSET = 0xFFFF_0000_0000_0000`
(`src/mm/addr.rs`). Adjacent paddrs map to adjacent vaddrs there, so
"virtually contiguous" was synonymous with "physically contiguous".

`BackedHeader` (in `lockjaw-types/src/pageset_table.rs`) walks past
the first backing page via raw `base.add(byte_offset)` arithmetic —
its safety contract requires *virtual* contiguity. Under the linear
map, the kernel could only satisfy that by calling
`page_alloc::alloc_pages_contiguous(header_pages)`, which contends
with userspace large allocations for order-6 buddy blocks
(`MAX_PRACTICAL_PAGES_PER_SET = 16384` headers need 33 pages → buddy
rounds to 64 = 256 KiB).

The KVM allocator (`src/mm/kvm.rs` + `lockjaw-types/src/kvm.rs`)
hands out N-page virtually-contiguous ranges in a dedicated
higher-half pool at `0xFFFF_8000_0000_0000` (`KVM_L0_INDEX = 256`),
backed by N independently-allocated physical frames stitched into
TTBR1. The `BackedHeader` contract is unchanged; it's now satisfied
by the KVM allocator instead of by the linear map.

## What lives in KVM today

- PageSet headers (allocation in `src/cap/pageset_table.rs:
  alloc_pages`, `alloc_pages_contiguous`, `alloc_and_insert_header`).
  The `PageSetEntry.header_kva: KernelVa` field carries the regime
  at the type level.
- Boot self-test (`src/mm/kvm.rs::boot_self_test`) — a one-shot
  diagnostic that allocates a 33-page range, pre-fragments
  `page_alloc` first, asserts the backing frames are scattered
  (proving the stitched path is exercised), and round-trips
  sentinel reads through every page.

Type-level lockdown:

- `KernelVa` (`lockjaw-types/src/addr.rs`) is a newtype distinct
  from `PhysAddr` — `compile_fail` doctests prove they cannot be
  assigned to each other.
- `HandleKind`'s non-empty variants now carry their own typed
  address per regime: `Endpoint { paddr: PhysAddr, ... }`,
  `Notification { paddr: PhysAddr }`, …, `PageSet { kva: KernelVa,
  mapped_va_page: u32 }`. There is no longer a polymorphic
  `HandleEntry.object_paddr: u64` field that takes its meaning from
  `kind` — the type system rules out crossing the regimes.

## What this unblocks

- Migrating other large kernel objects (handle tables that exceed
  one page, mapping scratch buffers, future objects) onto the same
  allocator. Each migration shrinks the surface that depends on the
  linear map.
- Relinking the kernel binary into the higher-half. **Done**
  (commits `17baed3` + `c70c417`): the kernel image now lives in a
  dedicated L0[1] VA region; `linker.ld` no longer pins the kernel
  to physical addresses. See
  `docs/history/relink-notes.md` for the Phase 0 diagnostic
  notebook + the `KernelImageVa` / `KernelStackBase` audit
  scaffolding.
- Removing the kernel identity map from user TTBR0
  (`AddressSpaceBuilder::new` at `src/arch/aarch64/vmem.rs`).
  Precondition (the kernel relink) is met; remaining work is to
  zero TTBR0 on kernel-thread context switch and audit that no
  kernel code path still depends on the identity map. Tracked
  in `docs/tracking/tech-debt.md` "Kernel threads leave stale user
  TTBR0 in hardware".

## What's deferred

- L3 page-table-page reclamation on `free_kernel_pages`. The current
  allocator keeps L3 (and L2) tables forever once a 2 MiB region has
  had its L3 allocated. Worst-case footprint is bounded by the pool
  size; reclamation is a closed-form optimization for later.
- ASID-based TLB management. Single-core today, no ASIDs allocated.
- Per-CPU KVM pools. Will matter when GKL breaks up.
- Freelist overflow buffer (`docs/tracking/tech-debt.md` — KVM free path: 64-
  page deferred-dealloc buffer). Safe under today's call patterns;
  will become unsafe when more migrations land.

## Cross-references

- `docs/tracking/yagni-parking-lot.md` — features built ahead of the call
  sites that need them, to be removed if they don't.
- `docs/tracking/tech-debt.md` — features needed but not yet built.
- `docs/architecture/patterns/` — push/pull/plan-apply pattern catalog the KVM
  allocator was built against.
