# Higher-Half Kernel

## What is it?

On AArch64, the 64-bit virtual address space is split in half:

```
0x0000_0000_0000_0000  ┐
                        │  Lower half (TTBR0) — userspace
0x0000_FFFF_FFFF_FFFF  ┘
         ...              (non-canonical hole)
0xFFFF_0000_0000_0000  ┐
                        │  Upper half (TTBR1) — kernel
0xFFFF_FFFF_FFFF_FFFF  ┘
```

The hardware enforces this split: addresses starting with `0x0000` are translated through TTBR0, addresses starting with `0xFFFF` through TTBR1. Each half has its own page table root register.

A "higher-half kernel" means the kernel lives in the upper half. Its code, data, stack, and device MMIO are all accessed through addresses like `0xFFFF_0000_4008_0000` rather than their physical addresses like `0x4008_0000`.

## Why do we care?

The entire reason is **process isolation**. Here's the problem we'd have without it:

When a userspace process runs, the CPU uses TTBR0 to translate its virtual addresses. The process sees its own code at `0x0000_0000_0040_0000` (or wherever it's loaded), its own stack, its own heap — all in the lower half. It cannot see another process's memory because each process has its own TTBR0 page table.

But when that process makes a syscall, the CPU traps into the kernel. The kernel needs to access its own code, its page tables, the UART, the process's TCB — all of its data structures. If the kernel lived in the lower half too, we'd have a conflict: the kernel's addresses would overlap with the process's addresses, and we'd need to swap page tables on every syscall and every interrupt. That's expensive and fragile.

With a higher-half kernel, the kernel lives in the upper half (TTBR1) and userspace lives in the lower half (TTBR0). On a syscall or interrupt:

- **TTBR1 (kernel) stays the same** — the kernel is always mapped, always reachable.
- **TTBR0 (userspace) stays the same too** — the kernel can read/write the calling process's memory directly through the lower-half addresses.

Context switching between processes only needs to swap TTBR0 (the userspace page table). TTBR1 never changes. This is fast and clean.

## Why do it now (Phase 2) instead of later?

Three reasons:

1. **It's easier to set up early.** Right now the kernel is tiny and we can identity-map everything. The transition from identity-mapped to higher-half is straightforward because both mappings coexist during the switch — no address ever becomes invalid. If we waited until Phase 6 (userspace) to do this, we'd have a much larger kernel with more pointers to fix up.

2. **It shapes everything that follows.** Every kernel pointer from Phase 3 onward (exception vectors, GIC addresses, page table walks) will use higher-half addresses. If we set up the convention now, all future code is written correctly from the start. Retrofitting it later would mean touching every file.

3. **The guard page (Milestone 2.6) needs real page tables.** The identity map uses 1 GB block descriptors — every address in the block is mapped. To create an unmapped guard page below the stack, we need to break the mapping into 4 KB pages around the stack region. We're already doing fine-grained page table work, so establishing the higher-half mapping at the same time is natural.

## How the transition works

The key insight is that both mappings are active simultaneously:

```
Step 1: Identity map only (TTBR0)
  PA 0x4008_0000  ←→  VA 0x4008_0000     ✓ (identity)
  PA 0x4008_0000  ←→  VA 0xFFFF_...      ✗ (TTBR1 not set)

Step 2: Install TTBR1 (both active)
  PA 0x4008_0000  ←→  VA 0x4008_0000     ✓ (identity, TTBR0)
  PA 0x4008_0000  ←→  VA 0xFFFF_...      ✓ (higher-half, TTBR1)

Step 3: Switch SP and UART to high addresses
  Still using TTBR0 for code execution
  Stack and UART now accessed via TTBR1 addresses
```

At no point does any currently-in-use address become unmapped. The identity map (TTBR0) stays active and will be repurposed for userspace page tables in Phase 6.
