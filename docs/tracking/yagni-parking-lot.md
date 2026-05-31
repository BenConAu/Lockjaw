# YAGNI Parking Lot

Code that was removed to keep the build warning-free. Each item lists what it was, when it'll be needed, and enough detail to reimplement it quickly.

We follow YAGNI: don't write it until you need it. This doc prevents knowledge loss without the cost of carrying dead code.

---

## VirtAddr newtype (removed from `src/mm/addr.rs`)

**What:** `VirtAddr(u64)` wrapper with `new()`, `as_u64()`, `page_indices()` (extracts L0/L1/L2/L3 indices for 4KB granule 4-level walk), `page_offset()`, `is_page_aligned()`, `Debug`, `LowerHex`.

**When needed:** Phase 6 (userspace) — mapping user pages requires walking the page table by virtual address indices. Also useful for any code that takes a VA and needs to decompose it into table indices.

**Key detail:** `page_indices()` extracts bits [47:39], [38:30], [29:21], [20:12] as L0–L3 indices respectively. `page_offset()` is bits [11:0].

---

## PhysPage::from_number / PhysPage::number (removed from `src/mm/addr.rs`)

**What:** Construct a `PhysPage` from its page number (not address), and retrieve the page number.

**When needed:** If direct page-number manipulation is needed beyond `PhysPage::containing()` and `start_addr()`.

**Key detail:** Page number = `phys_addr >> 12`. `from_number(n)` stores `n` directly; `start_addr()` already exists and does `n << 12`.

---

## PhysAddr::is_page_aligned (removed from `src/mm/addr.rs`)

**What:** `self.0 & (PAGE_SIZE - 1) == 0`

**When needed:** Any phase that adds assertions on page-aligned addresses (mapping, object creation).

---

## AP_RO_EL1, AP_RO_ALL (removed from `src/mm/page_table.rs`)

**What:** Access permission constants for the AP field (bits [7:6]) of page table entries.

- `AP_RO_EL1 = 0b10` — Read-only at EL1, no access at EL0
- `AP_RO_ALL = 0b11` — Read-only at EL1 and EL0

**When needed:** Read-only kernel mappings (`.rodata`, `.text`) need `AP_RO_EL1` for W^X enforcement. Shared read-only user pages need `AP_RO_ALL`.

Note: `AP_RW_ALL` was restored in Phase 6.

---

## SGI broadcast for cross-core wakeup (removed from `src/arch/aarch64/gic.rs`)

**What:** `send_sgi_broadcast()` — writes ICC_SGI1_EL1 (system register encoding `S3_0_C12_C11_5`) with IRM=1 (all other PEs), INTID=0. Plus SGI 0 dispatch in `irq_dispatch()` calling `tick()`, and SGI 0 Group 1 + enable in `init_redistributor()`.

**When needed:** Phase E (fine-grained locking). Currently kernel threads hold the GKL with IRQs masked (cooperative scheduling). An SGI from `unblock_thread()` wakes secondaries that spin on the GKL until the kernel thread releases, starving user threads on the boot CPU. Once kernel threads are preemptible or use fine-grained locks, SGI wakeup in `unblock_thread()` becomes safe and necessary for cross-core latency.

**Key details:**
- INTID 0 is reserved for this purpose — `irq_bind.rs` rejects userspace binding of INTID 0
- `init_redistributor()` must add `| (1 << 0)` to GICR_IGROUPR0 and GICR_ISENABLER0
- `irq_dispatch()` must handle INTID 0 → `scheduler::tick()`
- `unblock_thread()` calls `gic::send_sgi_broadcast()` after marking Ready

---

## SH_OUTER (removed from `src/mm/page_table.rs`)

**What:** `SH_OUTER = 0b10` — Outer Shareable memory attribute.

**When needed:** Multi-core support or specific DMA coherency requirements. Single-core QEMU virt uses Inner Shareable (`SH_INNER`) for normal memory and Non-shareable (`SH_NON`) for device.

---

## Pi 4B B0-stepping 1-GiB DMA-reachable ceiling check (never built)

**What:** BCM2711 board revisions earlier than C0 have a 1-GiB DMA-reachable address window (the original SoC stepping has a smaller bus window than the C0+ "Rev 1.4" boards). emmc2's ADMA2 descriptor takes a 32-bit physical address (so `buf_phys < 4 GiB` is enforced), but B0 boards need the stricter `buf_phys < 1 GiB`.

**Reimplementation sketch:**
- Runtime stepping detection: read board revision from DTB (`/proc/cpuinfo`-equivalent in Linux land; in Lockjaw, the device-tree property `/compatible` or `/model` plus the firmware-exposed revision register).
- If stepping is `< C0`: enforce `buf_phys < 1 << 30` instead of `< 1 << 32` in `adma2_single_block_read` (and any other ADMA2 caller).
- New error variant `Emmc2Error::BufferPhysAboveStepping1Gib`.
- `docs/tracking/tech-debt.md` note: "board-stepping detection is best-effort; if DTB doesn't expose revision, default to the stricter check (safe but rejects buffers that newer boards would accept)."

**When needed:** When (a) a B0-stepping Pi 4B is in the test fleet, OR (b) Lockjaw is shipped/distributed to users who might have older boards. Today: the user's board is C0+, no B0 board is on hand, and there's no distribution surface.

**Why parked, not in `post-c1-fix-plan.md`:** The post-C1 plan's theme is "C1's Pi-flash gate finally met" — adding speculative stepping-detection scaffolding for hardware nobody owns dilutes that theme. CLAUDE.md ("delete dead code, track deferred ideas in `docs/tracking/yagni-parking-lot.md`") makes this the right home.
