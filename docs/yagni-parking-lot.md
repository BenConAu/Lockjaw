# YAGNI Parking Lot

Code that was removed to keep the build warning-free. Each item lists what it was, when it'll be needed, and enough detail to reimplement it quickly.

We follow YAGNI: don't write it until you need it. This doc prevents knowledge loss without the cost of carrying dead code.

---

## VirtAddr newtype (removed from `src/mm/addr.rs`)

**What:** `VirtAddr(u64)` wrapper with `new()`, `as_u64()`, `page_indices()` (extracts L0/L1/L2/L3 indices for 4KB granule 4-level walk), `page_offset()`, `is_page_aligned()`, `Debug`, `LowerHex`.

**When needed:** Phase 6 (userspace) — mapping user pages requires walking the page table by virtual address indices. Also useful for any code that takes a VA and needs to decompose it into table indices.

**Key detail:** `page_indices()` extracts bits [47:39], [38:30], [29:21], [20:12] as L0–L3 indices respectively. `page_offset()` is bits [11:0].

---

## PhysFrame::from_number / PhysFrame::number (removed from `src/mm/addr.rs`)

**What:** Construct a `PhysFrame` from its frame number (not address), and retrieve the frame number.

**When needed:** Phase 4 (capabilities) — the Untyped retype system tracks frames by number for the watermark allocator.

**Key detail:** Frame number = `phys_addr >> 12`. `from_number(n)` stores `n` directly; `start_addr()` already exists and does `n << 12`.

---

## PhysAddr::is_page_aligned (removed from `src/mm/addr.rs`)

**What:** `self.0 & (PAGE_SIZE - 1) == 0`

**When needed:** Any phase that adds assertions on page-aligned addresses (mapping, retype).

---

## AP_RW_ALL, AP_RO_EL1, AP_RO_ALL (removed from `src/mm/page_table.rs`)

**What:** Access permission constants for the AP field (bits [7:6]) of page table entries.

- `AP_RW_ALL = 0b01` — Read-write at EL1 and EL0
- `AP_RO_EL1 = 0b10` — Read-only at EL1, no access at EL0
- `AP_RO_ALL = 0b11` — Read-only at EL1 and EL0

**When needed:** Phase 6 (userspace) — user pages need `AP_RW_ALL` or `AP_RO_ALL`. Read-only kernel mappings (`.rodata`, `.text`) need `AP_RO_EL1` for W^X enforcement.

---

## SH_OUTER (removed from `src/mm/page_table.rs`)

**What:** `SH_OUTER = 0b10` — Outer Shareable memory attribute.

**When needed:** Multi-core support or specific DMA coherency requirements. Single-core QEMU virt uses Inner Shareable (`SH_INNER`) for normal memory and Non-shareable (`SH_NON`) for device.

---

## PTE_PXN, PTE_UXN, with_pxn(), with_uxn() (removed from `src/mm/page_table.rs`)

**What:** Execute-never bits and builder methods.

- `PTE_PXN = 1 << 53` — Privileged Execute-Never
- `PTE_UXN = 1 << 54` — Unprivileged Execute-Never (also called XN)
- `with_pxn(self) -> Self` — set PXN bit on an entry
- `with_uxn(self) -> Self` — set UXN bit on an entry

**When needed:** Phase 6+ (security hardening) — data pages should have PXN set (no executing data), user code pages should have PXN set (no executing user code at EL1). Essential for W^X policy.
