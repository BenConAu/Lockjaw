use crate::addr::PhysAddr;
use core::fmt;

// ---------------------------------------------------------------------------
// Memory attribute constants
// ---------------------------------------------------------------------------

/// MAIR_EL1 attribute index 0: Device-nGnRnE (strongly ordered device memory).
pub const MAIR_DEVICE: u8 = 0;

/// MAIR_EL1 attribute index 1: Normal, Inner/Outer Write-Back Read/Write-Allocate.
pub const MAIR_NORMAL: u8 = 1;

/// MAIR_EL1 register value.
///   Attr[0] (bits  7:0) = 0x00: Device-nGnRnE
///   Attr[1] (bits 15:8) = 0xFF: Normal WB RA WA (inner + outer)
pub const MAIR_EL1_VALUE: u64 = (0x00 << 0) | (0xFF << 8);

// ---------------------------------------------------------------------------
// Access permission constants (AP field, bits [7:6])
// ---------------------------------------------------------------------------

/// Read-write at EL1, no access at EL0.
pub const AP_RW_EL1: u8 = 0b00;

/// Read-write at EL1 and EL0.
pub const AP_RW_ALL: u8 = 0b01;

// ---------------------------------------------------------------------------
// Shareability constants (SH field, bits [9:8])
// ---------------------------------------------------------------------------

pub const SH_NON: u8 = 0b00;
pub const SH_INNER: u8 = 0b11;

// ---------------------------------------------------------------------------
// Page table entry bit positions
// ---------------------------------------------------------------------------

const PTE_VALID: u64 = 1 << 0;
const PTE_TABLE: u64 = 1 << 1;
const PTE_AF: u64 = 1 << 10;
const PTE_PXN: u64 = 1 << 53;
const PTE_UXN: u64 = 1 << 54;

const PTE_ATTR_SHIFT: u64 = 2;
const PTE_AP_SHIFT: u64 = 6;
const PTE_SH_SHIFT: u64 = 8;

/// Mask for the output address field (bits [47:12]).
const PTE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

// ---------------------------------------------------------------------------
// PageTableEntry
// ---------------------------------------------------------------------------

/// A single 64-bit AArch64 page table entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn is_valid(self) -> bool {
        self.0 & PTE_VALID != 0
    }

    pub const fn is_table(self) -> bool {
        self.0 & (PTE_VALID | PTE_TABLE) == (PTE_VALID | PTE_TABLE)
    }

    pub const fn is_block(self) -> bool {
        self.0 & (PTE_VALID | PTE_TABLE) == PTE_VALID
    }

    pub const fn attr_index(self) -> u8 {
        ((self.0 >> PTE_ATTR_SHIFT) & 0x7) as u8
    }

    pub const fn ap(self) -> u8 {
        ((self.0 >> PTE_AP_SHIFT) & 0x3) as u8
    }

    pub const fn sh(self) -> u8 {
        ((self.0 >> PTE_SH_SHIFT) & 0x3) as u8
    }

    pub const fn af(self) -> bool {
        self.0 & PTE_AF != 0
    }

    pub const fn output_addr(self) -> PhysAddr {
        PhysAddr::new(self.0 & PTE_ADDR_MASK)
    }

    pub const fn new_table(next_table_phys: PhysAddr) -> Self {
        Self(PTE_VALID | PTE_TABLE | (next_table_phys.as_u64() & PTE_ADDR_MASK))
    }

    pub const fn new_block(phys: PhysAddr, attr: u8, ap: u8, sh: u8) -> Self {
        Self(
            PTE_VALID
                | ((attr as u64 & 0x7) << PTE_ATTR_SHIFT)
                | ((ap as u64 & 0x3) << PTE_AP_SHIFT)
                | ((sh as u64 & 0x3) << PTE_SH_SHIFT)
                | PTE_AF
                | (phys.as_u64() & PTE_ADDR_MASK),
        )
    }

    pub const fn new_page(phys: PhysAddr, attr: u8, ap: u8, sh: u8) -> Self {
        Self(
            PTE_VALID
                | PTE_TABLE
                | ((attr as u64 & 0x7) << PTE_ATTR_SHIFT)
                | ((ap as u64 & 0x3) << PTE_AP_SHIFT)
                | ((sh as u64 & 0x3) << PTE_SH_SHIFT)
                | PTE_AF
                | (phys.as_u64() & PTE_ADDR_MASK),
        )
    }

    pub const fn with_pxn(self) -> Self {
        Self(self.0 | PTE_PXN)
    }

    pub const fn with_uxn(self) -> Self {
        Self(self.0 | PTE_UXN)
    }

    pub const fn is_pxn(self) -> bool {
        self.0 & PTE_PXN != 0
    }

    pub const fn is_uxn(self) -> bool {
        self.0 & PTE_UXN != 0
    }
}

impl fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.is_valid() {
            write!(f, "PTE(invalid)")
        } else {
            write!(
                f,
                "PTE(addr={:#x} attr={} ap={:#04b} sh={:#04b} af={})",
                self.output_addr().as_u64(),
                self.attr_index(),
                self.ap(),
                self.sh(),
                self.af(),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// PageTable
// ---------------------------------------------------------------------------

/// A 4 KB-aligned page table containing 512 entries.
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    pub const fn empty() -> Self {
        Self {
            entries: [PageTableEntry::empty(); 512],
        }
    }
}

// ---------------------------------------------------------------------------
// Page table walk state machine
// ---------------------------------------------------------------------------

/// Result of each step in a page table walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkResult {
    /// Read the u64 at this physical address and pass it to step().
    Continue(u64),
    /// Walk complete: the VA maps to this physical address.
    Done(u64),
    /// Walk faulted: the VA is unmapped at some level.
    Fault,
}

/// Pure page table walk state machine. The kernel feeds raw PTE values
/// read from memory; this module handles all PTE interpretation.
///
/// ```
/// use lockjaw_types::page_table::*;
/// use lockjaw_types::addr::PhysAddr;
///
/// // Walk VA 0x0040_2234 through 4 levels of page tables.
/// let va = 0x0040_2234u64;
/// let (mut walk, mut result) = PageTableWalk::start(0x1_0000, va);
///
/// // Feed table descriptors for L0, L1, L2, then a page entry at L3.
/// result = walk.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
/// result = walk.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
/// result = walk.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
/// result = walk.step(PageTableEntry::new_page(
///     PhysAddr::new(0x5_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
/// ).raw());
///
/// assert_eq!(result, WalkResult::Done(0x5_0000 + 0x234));
/// ```
pub struct PageTableWalk {
    level: u8,
    va: u64,
    indices: [usize; 4],
}

impl PageTableWalk {
    /// Begin a walk. Returns the walker and the first physical address to read.
    pub fn start(ttbr0_paddr: u64, va: u64) -> (Self, WalkResult) {
        let (l0, l1, l2, l3) = crate::vmem::page_table_indices(va);
        let walk = Self {
            level: 0,
            va,
            indices: [l0, l1, l2, l3],
        };
        let pte_paddr = ttbr0_paddr + (l0 as u64) * 8;
        (walk, WalkResult::Continue(pte_paddr))
    }

    /// Page table level the walker is currently at: 0..=3.
    ///
    /// After `step()` returns `WalkResult::Done(_)`, the level identifies
    /// where resolution happened: 1 means an L1 1GB block, 2 means an L2
    /// 2MB block, 3 means an L3 4KiB page. Callers that require a
    /// specific granule (e.g. validate_pte_match's "L3 only" contract)
    /// must check this after Done.
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Feed a raw PTE value read from the address returned by the previous step.
    pub fn step(&mut self, pte_raw: u64) -> WalkResult {
        let pte = PageTableEntry::from_raw(pte_raw);

        match self.level {
            0 => {
                // L0 must be a table descriptor
                if !pte.is_table() { return WalkResult::Fault; }
                let next_table = pte.output_addr().as_u64();
                self.level = 1;
                WalkResult::Continue(next_table + (self.indices[1] as u64) * 8)
            }
            1 => {
                // L1: 1GB block or table
                if pte.is_block() {
                    let offset = self.va & 0x3FFF_FFFF;
                    return WalkResult::Done(pte.output_addr().as_u64() + offset);
                }
                if !pte.is_table() { return WalkResult::Fault; }
                let next_table = pte.output_addr().as_u64();
                self.level = 2;
                WalkResult::Continue(next_table + (self.indices[2] as u64) * 8)
            }
            2 => {
                // L2: 2MB block or table
                if pte.is_block() {
                    let offset = self.va & 0x1F_FFFF;
                    return WalkResult::Done(pte.output_addr().as_u64() + offset);
                }
                if !pte.is_table() { return WalkResult::Fault; }
                let next_table = pte.output_addr().as_u64();
                self.level = 3;
                WalkResult::Continue(next_table + (self.indices[3] as u64) * 8)
            }
            3 => {
                // L3 must be a valid page entry
                if !pte.is_valid() { return WalkResult::Fault; }
                let offset = self.va & 0xFFF;
                WalkResult::Done(pte.output_addr().as_u64() + offset)
            }
            _ => WalkResult::Fault,
        }
    }
}

// ---------------------------------------------------------------------------
// Mapping walk state machine
// ---------------------------------------------------------------------------

/// Result of each step in a mapping walk (L0->L1->L2).
/// Unlike PageTableWalk (which reads through all 4 levels), MapWalk stops
/// at L2 and returns the slot state so the kernel can allocate or reuse
/// the L3 table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapWalkResult {
    /// Read the u64 at this physical address and call step().
    ReadPte(u64),
    /// Reached the target L2 slot. The kernel should use map_action_for_l2()
    /// on the state to decide whether to allocate or reuse an L3 table.
    ReachedL2 {
        l2_table_paddr: u64,
        l2_idx: usize,
        l3_start: usize,
        state: crate::vmem::L2SlotState,
    },
    /// Walk faulted: invalid entry at L0 or L1.
    Fault,
    /// Validation failed (bad VA, too many pages, spans L2 boundary, etc).
    InvalidMapping,
}

/// Pure mapping walk state machine. Walks L0->L1->L2 to find the target
/// slot for inserting page mappings. The kernel feeds raw PTE values;
/// this module validates the request, interprets PTEs, and classifies
/// the L2 slot.
pub struct MapWalk {
    level: u8,
    l2_idx: usize,
    l3_start: usize,
    l2_table_paddr: u64,
}

impl MapWalk {
    /// Begin a mapping walk. Validates the request and returns the first
    /// physical address to read (L0[0]).
    pub fn start(ttbr0_paddr: u64, virt_addr: u64, page_count: usize) -> (Self, MapWalkResult) {
        use crate::vmem::{validate_mapping, MapValidation};

        let (l2_idx, l3_start) = match validate_mapping(virt_addr, page_count) {
            MapValidation::Ok { l2_idx, l3_start } => (l2_idx, l3_start),
            _ => {
                let walk = Self { level: 0, l2_idx: 0, l3_start: 0, l2_table_paddr: 0 };
                return (walk, MapWalkResult::InvalidMapping);
            }
        };

        let walk = Self { level: 0, l2_idx, l3_start, l2_table_paddr: 0 };
        // User mappings are always in L0[0], so read entry at index 0
        let pte_paddr = ttbr0_paddr + 0 * 8;
        (walk, MapWalkResult::ReadPte(pte_paddr))
    }

    /// Feed a raw PTE value read from the address returned by the previous step.
    pub fn step(&mut self, pte_raw: u64) -> MapWalkResult {
        let pte = PageTableEntry::from_raw(pte_raw);

        match self.level {
            0 => {
                // L0[0] must be a table pointing to L1
                if !pte.is_table() { return MapWalkResult::Fault; }
                let l1_table = pte.output_addr().as_u64();
                self.level = 1;
                // User mappings are in L1[0]
                MapWalkResult::ReadPte(l1_table + 0 * 8)
            }
            1 => {
                // L1[0] must be a table pointing to L2
                if !pte.is_table() { return MapWalkResult::Fault; }
                let l2_table = pte.output_addr().as_u64();
                self.l2_table_paddr = l2_table;
                self.level = 2;
                // Read the target L2 entry
                MapWalkResult::ReadPte(l2_table + (self.l2_idx as u64) * 8)
            }
            2 => {
                // Classify the L2 entry
                let state = crate::vmem::classify_l2_entry(pte);
                MapWalkResult::ReachedL2 {
                    l2_table_paddr: self.l2_table_paddr,
                    l2_idx: self.l2_idx,
                    l3_start: self.l3_start,
                    state,
                }
            }
            _ => MapWalkResult::Fault,
        }
    }
}

// ---------------------------------------------------------------------------
// Mapping query — pure index arithmetic + PTE interpretation
// ---------------------------------------------------------------------------

/// Query consecutive mapped/unmapped pages starting at `start_va`.
/// The `read_pte` closure reads a u64 from a physical address (the
/// kernel provides this via TTBR1 direct-map).
///
/// Returns `(is_mapped, run_pages)`:
/// - `is_mapped`: whether the page at `start_va` is mapped
/// - `run_pages`: count of consecutive pages with the same state
///
/// All PTE interpretation lives here (host-testable). The kernel
/// only provides the memory-read closure.
pub fn query_mapping_run<F: Fn(u64) -> u64>(
    ttbr0_paddr: u64,
    start_va: u64,
    read_pte: F,
) -> (bool, usize) {
    let user_va_end = crate::constants::USER_VA_END;
    if start_va >= user_va_end {
        return (false, 0);
    }

    // Walk L0 → L1. User VA < 1GB, so L0[0] and L1[0].
    let l0_raw = read_pte(ttbr0_paddr);
    let l0 = PageTableEntry::from_raw(l0_raw);
    if !l0.is_valid() || !l0.is_table() {
        return (false, ((user_va_end - start_va) / 4096) as usize);
    }

    let l1_table = l0.output_addr().as_u64();
    let l1_idx = ((start_va >> 30) & 0x1FF) as u64;
    let l1_raw = read_pte(l1_table + l1_idx * 8);
    let l1 = PageTableEntry::from_raw(l1_raw);

    if !l1.is_valid() {
        let end = ((l1_idx + 1) << 30).min(user_va_end);
        return (false, ((end - start_va) / 4096) as usize);
    }
    if l1.is_block() {
        let end = ((l1_idx + 1) << 30).min(user_va_end);
        return (true, ((end - start_va) / 4096) as usize);
    }

    // L1 is a table → scan L2 entries
    let l2_table = l1.output_addr().as_u64();

    // Determine initial state
    let first_l2_idx = (start_va >> 21) & 0x1FF;
    let first_l2_raw = read_pte(l2_table + first_l2_idx * 8);
    let first_l2 = PageTableEntry::from_raw(first_l2_raw);

    let first_mapped = if !first_l2.is_valid() {
        false
    } else if first_l2.is_block() {
        true
    } else {
        let l3_table = first_l2.output_addr().as_u64();
        let l3_idx = ((start_va >> 12) & 0x1FF) as u64;
        let l3_raw = read_pte(l3_table + l3_idx * 8);
        PageTableEntry::from_raw(l3_raw).is_valid()
    };

    // Count consecutive pages with same state
    let mut va = start_va;
    let mut count: usize = 0;

    while va < user_va_end {
        let l2_idx = (va >> 21) & 0x1FF;
        if l2_idx >= 512 { break; }

        let l2_raw = read_pte(l2_table + l2_idx * 8);
        let l2 = PageTableEntry::from_raw(l2_raw);

        if !l2.is_valid() {
            if first_mapped { break; }
            let end = (((va >> 21) + 1) << 21).min(user_va_end);
            count += ((end - va) / 4096) as usize;
            va = end;
        } else if l2.is_block() {
            if !first_mapped { break; }
            let end = (((va >> 21) + 1) << 21).min(user_va_end);
            count += ((end - va) / 4096) as usize;
            va = end;
        } else {
            // L3 table — scan entries
            let l3_table = l2.output_addr().as_u64();
            let l3_start = ((va >> 12) & 0x1FF) as u64;
            for l3_idx in l3_start..512 {
                if va >= user_va_end { break; }
                let l3_raw = read_pte(l3_table + l3_idx * 8);
                let mapped = PageTableEntry::from_raw(l3_raw).is_valid();
                if mapped != first_mapped {
                    return (first_mapped, count);
                }
                count += 1;
                va += 4096;
            }
        }
    }

    (first_mapped, count)
}

/// Validate that L3 PTEs at [va, va + count*4096) map to the expected
/// physical pages. Pure read-only walk; never writes.
///
/// Returns Ok(()) on full match, Err(index) with the first mismatched
/// page index. L3 page entries only — returns Err if the walk resolves
/// at L1/L2 (block mapping) or faults at any level.
///
/// Designed for the validate phase of two-phase revocation: a
/// successful return guarantees that a paired `clear_validated_pte`
/// call against the same `(ttbr0, va, count)` will find every PTE
/// where this function looked. Caller must hold whatever lock keeps
/// the page table stable between the two calls (GKL today).
pub fn validate_pte_match<R>(
    ttbr0_paddr: u64,
    va: u64,
    expected_pages: &[u64],
    read_pte: R,
) -> Result<(), usize>
where
    R: Fn(u64) -> u64,
{
    for i in 0..expected_pages.len() {
        let page_va = va + (i as u64) * 4096;
        let (mut walk, mut result) = PageTableWalk::start(ttbr0_paddr, page_va);

        loop {
            match result {
                WalkResult::Continue(pte_paddr) => {
                    let pte_raw = read_pte(pte_paddr);
                    let next = walk.step(pte_raw);
                    match next {
                        WalkResult::Done(phys_addr) => {
                            // L3-only contract: PageTableWalk::step also
                            // returns Done for L1 1GB blocks (level==1)
                            // and L2 2MB blocks (level==2). Reject those
                            // here — the kernel never installs PageSet
                            // mappings as blocks, so a block resolution
                            // means the walked PTE chain is not the L3
                            // page entry the caller expects to clear.
                            if walk.level() != 3 {
                                return Err(i);
                            }
                            let actual_page = phys_addr & !0xFFF;
                            if actual_page != expected_pages[i] {
                                return Err(i);
                            }
                            break;
                        }
                        _ => result = next,
                    }
                }
                WalkResult::Done(_) => return Err(i),
                WalkResult::Fault => return Err(i),
            }
        }
    }

    Ok(())
}

/// Clear L3 PTEs at [va, va + count*4096). Pure write-only operation —
/// does NOT validate that the PTEs match anything.
///
/// MUST be called only after a successful `validate_pte_match` against
/// the same `(ttbr0, va, count)` within the same critical section
/// (GKL held). Under that precondition, every L3 walk completes with
/// a `WalkResult::Done` and the writes succeed.
///
/// **Any other walk outcome panics**: a Done at L1/L2 (block mapping
/// appeared), a Fault at top level (entry cleared), or a Fault mid-path
/// (descriptor invalidated) all indicate the page table changed
/// between validate and clear. That is a kernel-invariant violation,
/// not a recoverable error — silently breaking on it would leak a
/// partial unmap behind a successful return and let the apply phase
/// of revocation fail open.
///
/// TLB invalidation is the kernel wrapper's responsibility — this
/// pure helper only writes PTE storage.
pub fn clear_validated_pte<R, W>(
    ttbr0_paddr: u64,
    va: u64,
    count: usize,
    read_pte: R,
    write_pte: W,
) where
    R: Fn(u64) -> u64,
    W: Fn(u64, u64),
{
    for i in 0..count {
        let page_va = va + (i as u64) * 4096;
        let (mut walk, mut result) = PageTableWalk::start(ttbr0_paddr, page_va);

        loop {
            match result {
                WalkResult::Continue(pte_paddr) => {
                    let pte_raw = read_pte(pte_paddr);
                    match walk.step(pte_raw) {
                        WalkResult::Done(_) => {
                            // L3-only contract: validate_pte_match
                            // already verified walk.level() == 3 for
                            // every page, so reaching Done at L1/L2
                            // here means a block mapping appeared
                            // between phases (kernel bug).
                            if walk.level() != 3 {
                                panic!(
                                    "clear_validated_pte: Done at level {} for page \
                                     index {} after validate_pte_match succeeded — \
                                     block mapping appeared at L1/L2 \
                                     (kernel-invariant violation)",
                                    walk.level(), i
                                );
                            }
                            write_pte(pte_paddr, 0);
                            break;
                        }
                        next @ WalkResult::Continue(_) => {
                            result = next;
                        }
                        WalkResult::Fault => panic!(
                            "clear_validated_pte: walk faulted mid-path at page index {} \
                             after validate_pte_match succeeded — page table diverged \
                             (kernel-invariant violation)",
                            i
                        ),
                    }
                }
                // The first WalkResult::Continue is produced by
                // PageTableWalk::start; reaching Done/Fault here means
                // the page table changed since validate.
                WalkResult::Done(_) => panic!(
                    "clear_validated_pte: walk resolved at L1/L2 for page index {} \
                     after validate_pte_match succeeded — block mapping appeared \
                     (kernel-invariant violation)",
                    i
                ),
                WalkResult::Fault => panic!(
                    "clear_validated_pte: walk faulted at top level for page index {} \
                     after validate_pte_match succeeded — descriptor cleared \
                     (kernel-invariant violation)",
                    i
                ),
            }
        }
    }
}

/// Validate then clear in one call. Thin wrapper over
/// `validate_pte_match` + `clear_validated_pte` for callers that don't
/// need to interleave other work between phases.
///
/// Returns Ok(()) if all matched and were cleared. Returns Err(index)
/// from validate_pte_match without writing anything.
pub fn unmap_validated<R, W>(
    ttbr0_paddr: u64,
    va: u64,
    expected_pages: &[u64],
    read_pte: R,
    write_pte: W,
) -> Result<(), usize>
where
    R: Fn(u64) -> u64,
    W: Fn(u64, u64),
{
    validate_pte_match(ttbr0_paddr, va, expected_pages, &read_pte)?;
    clear_validated_pte(ttbr0_paddr, va, expected_pages.len(), &read_pte, write_pte);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_entry_is_invalid() {
        let e = PageTableEntry::empty();
        assert!(!e.is_valid());
        assert!(!e.is_table());
        assert!(!e.is_block());
        assert_eq!(e.raw(), 0);
    }

    #[test]
    fn new_page_sets_all_fields() {
        let e = PageTableEntry::new_page(
            PhysAddr::new(0x4008_0000),
            MAIR_NORMAL,
            AP_RW_EL1,
            SH_INNER,
        );
        assert!(e.is_valid());
        assert!(e.is_table()); // bit 1 set for L3 page entries
        assert!(e.af());
        assert_eq!(e.attr_index(), MAIR_NORMAL);
        assert_eq!(e.ap(), AP_RW_EL1);
        assert_eq!(e.sh(), SH_INNER);
        assert_eq!(e.output_addr().as_u64(), 0x4008_0000);
    }

    #[test]
    fn new_block_clears_table_bit() {
        let e = PageTableEntry::new_block(
            PhysAddr::new(0x4000_0000),
            MAIR_DEVICE,
            AP_RW_EL1,
            SH_NON,
        );
        assert!(e.is_valid());
        assert!(e.is_block());
        assert!(!e.is_table());
        assert_eq!(e.attr_index(), MAIR_DEVICE);
        assert_eq!(e.sh(), SH_NON);
    }

    #[test]
    fn new_table_descriptor() {
        let e = PageTableEntry::new_table(PhysAddr::new(0x4009_0000));
        assert!(e.is_valid());
        assert!(e.is_table());
        assert!(!e.is_block());
        assert_eq!(e.output_addr().as_u64(), 0x4009_0000);
    }

    #[test]
    fn output_addr_masks_low_bits() {
        let e = PageTableEntry::new_page(
            PhysAddr::new(0x4008_0FFF), // unaligned — low bits should be stripped
            MAIR_NORMAL,
            AP_RW_EL1,
            SH_INNER,
        );
        assert_eq!(e.output_addr().as_u64(), 0x4008_0000);
    }

    #[test]
    fn with_pxn_sets_bit_53() {
        let e = PageTableEntry::new_page(PhysAddr::new(0x1000), MAIR_NORMAL, AP_RW_ALL, SH_INNER);
        let e_pxn = e.with_pxn();
        assert_eq!(e_pxn.raw() & (1 << 53), 1 << 53);
        // Original fields unchanged
        assert_eq!(e_pxn.output_addr().as_u64(), 0x1000);
        assert_eq!(e_pxn.ap(), AP_RW_ALL);
    }

    #[test]
    fn with_uxn_sets_bit_54() {
        let e = PageTableEntry::new_page(PhysAddr::new(0x2000), MAIR_NORMAL, AP_RW_ALL, SH_INNER);
        let e_uxn = e.with_uxn();
        assert_eq!(e_uxn.raw() & (1 << 54), 1 << 54);
    }

    #[test]
    fn ap_rw_all_value() {
        let e = PageTableEntry::new_page(PhysAddr::new(0x1000), MAIR_NORMAL, AP_RW_ALL, SH_INNER);
        assert_eq!(e.ap(), 0b01);
    }

    #[test]
    fn page_table_empty_all_invalid() {
        let pt = PageTable::empty();
        for i in 0..512 {
            assert!(!pt.entries[i].is_valid());
        }
    }

    #[test]
    fn mair_el1_value_correct() {
        // Attr[0] = 0x00 (device), Attr[1] = 0xFF (normal WB)
        assert_eq!(MAIR_EL1_VALUE, 0xFF00);
    }

    // --- Page table walk tests ---

    #[test]
    fn walk_4_level_resolves() {
        // VA 0x0040_1234: L0=0, L1=0, L2=2, L3=1, offset=0x234
        let va = 0x0040_1234;
        let (mut w, r) = PageTableWalk::start(0x1_0000, va);

        // Should read L0 entry at table + L0_idx*8 = 0x1_0000 + 0*8
        assert_eq!(r, WalkResult::Continue(0x1_0000));

        // L0 → table at 0x2_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        assert_eq!(r, WalkResult::Continue(0x2_0000)); // L1[0]

        // L1 → table at 0x3_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        assert_eq!(r, WalkResult::Continue(0x3_0000 + 2 * 8)); // L2[2]

        // L2 → table at 0x4_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        assert_eq!(r, WalkResult::Continue(0x4_0000 + 1 * 8)); // L3[1]

        // L3 → page at 0x5_0000
        let r = w.step(PageTableEntry::new_page(
            PhysAddr::new(0x5_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert_eq!(r, WalkResult::Done(0x5_0000 + 0x234));
    }

    #[test]
    fn walk_1gb_block_at_l1() {
        // VA 0x4000_1234: L0=0, L1=1, offset within 1GB = 0x1234
        let va = 0x4000_1234;
        let (mut w, _) = PageTableWalk::start(0x1_0000, va);

        // L0 table
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        // Should read L1[1]
        assert_eq!(r, WalkResult::Continue(0x2_0000 + 1 * 8));

        // L1 → 1GB block at 0x8000_0000
        let r = w.step(PageTableEntry::new_block(
            PhysAddr::new(0x8000_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        // offset = 0x4000_1234 & 0x3FFF_FFFF = 0x1234
        assert_eq!(r, WalkResult::Done(0x8000_0000 + 0x1234));
    }

    #[test]
    fn walk_2mb_block_at_l2() {
        // VA 0x0060_5678: L0=0, L1=0, L2=3, offset within 2MB = 0x5678
        let va = 0x0060_5678;
        let (mut w, _) = PageTableWalk::start(0x1_0000, va);

        let _r = w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        // Should read L2[3]
        assert_eq!(r, WalkResult::Continue(0x3_0000 + 3 * 8));

        // L2 → 2MB block at 0xA00_0000
        let r = w.step(PageTableEntry::new_block(
            PhysAddr::new(0xA00_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        // offset = 0x0060_5678 & 0x1F_FFFF = 0x5678
        assert_eq!(r, WalkResult::Done(0xA00_0000 + 0x5678));
    }

    #[test]
    fn walk_fault_invalid_l0() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_fault_invalid_l1() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_fault_invalid_l2() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_fault_invalid_l3() {
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x0040_0000);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, WalkResult::Fault);
    }

    #[test]
    fn walk_page_offset_preserved() {
        // VA with offset 0xABC within the page
        let va = 0x0040_0ABC;
        let (mut w, _) = PageTableWalk::start(0x1_0000, va);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        let r = w.step(PageTableEntry::new_page(
            PhysAddr::new(0xFF_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert_eq!(r, WalkResult::Done(0xFF_0000 + 0xABC));
    }

    // --- MapWalk tests ---

    #[test]
    fn map_walk_reaches_l2_with_existing_table() {
        // VA 0x0040_0000: L2[2], L3[0]
        let (mut w, r) = MapWalk::start(0x1_0000, 0x0040_0000, 1);
        assert_eq!(r, MapWalkResult::ReadPte(0x1_0000)); // L0[0]

        // L0 → table at 0x2_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        assert_eq!(r, MapWalkResult::ReadPte(0x2_0000)); // L1[0]

        // L1 → table at 0x3_0000
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        assert_eq!(r, MapWalkResult::ReadPte(0x3_0000 + 2 * 8)); // L2[2]

        // L2[2] → existing L3 table
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        assert_eq!(r, MapWalkResult::ReachedL2 {
            l2_table_paddr: 0x3_0000,
            l2_idx: 2,
            l3_start: 0,
            state: crate::vmem::L2SlotState::HasL3Table,
        });
    }

    #[test]
    fn map_walk_reaches_l2_empty_slot() {
        let (mut w, _) = MapWalk::start(0x1_0000, 0x0040_0000, 1);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());

        // L2[2] → empty
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, MapWalkResult::ReachedL2 {
            l2_table_paddr: 0x3_0000,
            l2_idx: 2,
            l3_start: 0,
            state: crate::vmem::L2SlotState::Empty,
        });
    }

    #[test]
    fn map_walk_reaches_l2_block_conflict() {
        let (mut w, _) = MapWalk::start(0x1_0000, 0x0040_0000, 1);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());

        // L2[2] → 2MB block (conflict)
        let r = w.step(PageTableEntry::new_block(
            PhysAddr::new(0x20_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert_eq!(r, MapWalkResult::ReachedL2 {
            l2_table_paddr: 0x3_0000,
            l2_idx: 2,
            l3_start: 0,
            state: crate::vmem::L2SlotState::IsBlock,
        });
    }

    #[test]
    fn map_walk_fault_invalid_l0() {
        let (mut w, _) = MapWalk::start(0x1_0000, 0x0040_0000, 1);
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, MapWalkResult::Fault);
    }

    #[test]
    fn map_walk_fault_invalid_l1() {
        let (mut w, _) = MapWalk::start(0x1_0000, 0x0040_0000, 1);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        let r = w.step(PageTableEntry::empty().raw());
        assert_eq!(r, MapWalkResult::Fault);
    }

    #[test]
    fn map_walk_invalid_mapping() {
        // VA in kernel range — should fail validation
        let (_, r) = MapWalk::start(0x1_0000, 0x4000_0000, 1);
        assert_eq!(r, MapWalkResult::InvalidMapping);
    }

    #[test]
    fn map_walk_l3_start_from_va_offset() {
        // VA 0x0040_3000: L2[2], L3[3]
        let (mut w, _) = MapWalk::start(0x1_0000, 0x0040_3000, 1);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        let r = w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        match r {
            MapWalkResult::ReachedL2 { l3_start, .. } => assert_eq!(l3_start, 3),
            _ => panic!("expected ReachedL2"),
        }
    }

    // --- query_mapping_run tests ---

    extern crate std;

    /// Fake page table storage for host-side testing.
    /// Stores PTE values at fake physical addresses.
    struct FakePT {
        entries: std::collections::HashMap<u64, u64>,
    }

    impl FakePT {
        fn new() -> Self {
            Self { entries: std::collections::HashMap::new() }
        }

        fn set(&mut self, paddr: u64, raw: u64) {
            self.entries.insert(paddr, raw);
        }

        fn read(&self, paddr: u64) -> u64 {
            *self.entries.get(&paddr).unwrap_or(&0)
        }

        /// Set a table descriptor at table[idx] pointing to next_table.
        fn set_table_entry(&mut self, table_paddr: u64, idx: usize, next_table_paddr: u64) {
            let pte = PageTableEntry::new_table(PhysAddr::new(next_table_paddr));
            self.set(table_paddr + (idx as u64) * 8, pte.raw());
        }

        /// Set a valid page entry at table[idx].
        fn set_page_entry(&mut self, table_paddr: u64, idx: usize) {
            let phys = 0x8000_0000 + (idx as u64) * 4096; // arbitrary phys
            let pte = PageTableEntry::new_page(
                PhysAddr::new(phys), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
            );
            self.set(table_paddr + (idx as u64) * 8, pte.raw());
        }
    }

    /// Build a minimal 4-level page table with L0→L1→L2→L3.
    /// Maps `page_count` pages starting at VA 0x400000 (L2[2], L3[0..N]).
    fn build_basic_pt(page_count: usize) -> (FakePT, u64) {
        let mut pt = FakePT::new();
        let l0: u64 = 0x1_0000;
        let l1: u64 = 0x2_0000;
        let l2: u64 = 0x3_0000;
        let l3: u64 = 0x4_0000; // L3 for L2[2] (VA 0x400000..0x5FFFFF)

        pt.set_table_entry(l0, 0, l1);  // L0[0] → L1
        pt.set_table_entry(l1, 0, l2);  // L1[0] → L2
        pt.set_table_entry(l2, 2, l3);  // L2[2] → L3 (VA 0x400000)

        for i in 0..page_count {
            pt.set_page_entry(l3, i);
        }

        (pt, l0)
    }

    #[test]
    fn query_mapped_run_at_start() {
        let (pt, l0) = build_basic_pt(4);
        let (mapped, run) = query_mapping_run(l0, 0x40_0000, |a| pt.read(a));
        assert!(mapped);
        assert_eq!(run, 4);
    }

    #[test]
    fn query_unmapped_before_image() {
        let (pt, l0) = build_basic_pt(4);
        // VA 0x200000 is in L2[1] which has no L3 table → unmapped
        let (mapped, run) = query_mapping_run(l0, 0x20_0000, |a| pt.read(a));
        assert!(!mapped);
        // Should count unmapped pages from 0x200000 to 0x400000 (512 pages in L2[1])
        assert_eq!(run, 512);
    }

    #[test]
    fn query_unmapped_after_mapped() {
        let (pt, l0) = build_basic_pt(4);
        // VA 0x404000 = L2[2], L3[4] — not mapped (only 0..3 are)
        let (mapped, run) = query_mapping_run(l0, 0x40_4000, |a| pt.read(a));
        assert!(!mapped);
        // 508 remaining in L3, then all remaining L2 entries through
        // to USER_VA_END are also unmapped. Total:
        // (USER_VA_END - 0x404000) / 4096 = (0x4000_0000 - 0x40_4000) / 4096
        let expected = ((0x4000_0000u64 - 0x40_4000) / 4096) as usize;
        assert_eq!(run, expected);
    }

    #[test]
    fn query_entirely_unmapped_l0() {
        let pt = FakePT::new(); // empty — L0[0] is invalid
        let (mapped, run) = query_mapping_run(0x1_0000, 0x0, |a| pt.read(a));
        assert!(!mapped);
        // USER_VA_END / 4096 = 0x4000_0000 / 4096 = 262144 pages
        assert_eq!(run, 262144);
    }

    #[test]
    fn query_past_user_va_end() {
        let pt = FakePT::new();
        let (mapped, run) = query_mapping_run(0x1_0000, 0x4000_0000, |a| pt.read(a));
        assert!(!mapped);
        assert_eq!(run, 0);
    }

    #[test]
    fn query_mapped_run_mid_l3() {
        // Map L3 entries 10..20 (10 pages), query from entry 10
        let mut pt = FakePT::new();
        let l0: u64 = 0x1_0000;
        let l1: u64 = 0x2_0000;
        let l2: u64 = 0x3_0000;
        let l3: u64 = 0x4_0000;

        pt.set_table_entry(l0, 0, l1);
        pt.set_table_entry(l1, 0, l2);
        pt.set_table_entry(l2, 2, l3);

        for i in 10..20 {
            pt.set_page_entry(l3, i);
        }

        // Query at L3[10] = VA 0x400000 + 10*4096 = 0x40A000
        let (mapped, run) = query_mapping_run(l0, 0x40_A000, |a| pt.read(a));
        assert!(mapped);
        assert_eq!(run, 10);
    }

    #[test]
    fn query_unmapped_before_mapped_in_same_l3() {
        // Map L3[10..20], query from L3[0] — should see 10 unmapped
        let mut pt = FakePT::new();
        let l0: u64 = 0x1_0000;
        let l1: u64 = 0x2_0000;
        let l2: u64 = 0x3_0000;
        let l3: u64 = 0x4_0000;

        pt.set_table_entry(l0, 0, l1);
        pt.set_table_entry(l1, 0, l2);
        pt.set_table_entry(l2, 2, l3);

        for i in 10..20 {
            pt.set_page_entry(l3, i);
        }

        let (mapped, run) = query_mapping_run(l0, 0x40_0000, |a| pt.read(a));
        assert!(!mapped);
        assert_eq!(run, 10);
    }

    #[test]
    fn query_crossing_l2_boundary_unmapped() {
        // L2[2] has an L3 with no valid entries, L2[3..511] entirely absent.
        // Query from start of L2[2] — should count all unmapped pages through
        // to USER_VA_END since nothing is mapped after this point.
        let mut pt = FakePT::new();
        let l0: u64 = 0x1_0000;
        let l1: u64 = 0x2_0000;
        let l2: u64 = 0x3_0000;
        let l3: u64 = 0x4_0000;

        pt.set_table_entry(l0, 0, l1);
        pt.set_table_entry(l1, 0, l2);
        pt.set_table_entry(l2, 2, l3); // L3 exists but all entries invalid

        let (mapped, run) = query_mapping_run(l0, 0x40_0000, |a| pt.read(a));
        assert!(!mapped);
        let expected = ((0x4000_0000u64 - 0x40_0000) / 4096) as usize;
        assert_eq!(run, expected);
    }

    // --- unmap_validated tests ---

    #[test]
    fn unmap_validated_clears_matching_ptes() {
        use std::cell::RefCell;

        let (pt, l0) = build_basic_pt(3);
        let pt = RefCell::new(pt);

        // Expected physical pages match what build_basic_pt sets up:
        // L3[i] maps to 0x8000_0000 + i * 4096
        let expected = [
            0x8000_0000,
            0x8000_1000,
            0x8000_2000,
        ];

        let result = unmap_validated(
            l0, 0x40_0000, &expected,
            |a| pt.borrow().read(a),
            |a, v| pt.borrow_mut().set(a, v),
        );
        assert!(result.is_ok());

        // Verify PTEs were cleared: querying should show unmapped
        let (mapped, _) = query_mapping_run(l0, 0x40_0000, |a| pt.borrow().read(a));
        assert!(!mapped);
    }

    #[test]
    fn unmap_validated_rejects_wrong_phys() {
        let (pt, l0) = build_basic_pt(2);
        let expected = [0xDEAD_0000, 0xDEAD_1000]; // wrong addresses

        let result = unmap_validated(
            l0, 0x40_0000, &expected,
            |a| pt.read(a),
            |_, _| panic!("should not write"),
        );
        assert_eq!(result, Err(0));
    }

    #[test]
    fn unmap_validated_rejects_unmapped_page() {
        let (pt, l0) = build_basic_pt(1);
        // Try to unmap 2 pages but only 1 is mapped
        let expected = [0x8000_0000, 0x8000_1000];

        let result = unmap_validated(
            l0, 0x40_0000, &expected,
            |a| pt.read(a),
            |_, _| panic!("should not write"),
        );
        assert_eq!(result, Err(1)); // second page is unmapped
    }

    // --- validate_pte_match (read-only) tests ---

    #[test]
    fn validate_pte_match_does_not_write() {
        // Pure read: never invokes the write closure even on success.
        let (pt, l0) = build_basic_pt(3);
        let expected = [0x8000_0000, 0x8000_1000, 0x8000_2000];

        let result = validate_pte_match(l0, 0x40_0000, &expected, |a| pt.read(a));
        assert!(result.is_ok());

        // Verify mappings are unchanged: query still finds them.
        let (mapped, run) = query_mapping_run(l0, 0x40_0000, |a| pt.read(a));
        assert!(mapped);
        assert!(run >= 3);
    }

    #[test]
    fn validate_pte_match_returns_first_mismatch_index() {
        let (pt, l0) = build_basic_pt(3);
        // First two pages match, third is wrong.
        let expected = [0x8000_0000, 0x8000_1000, 0xDEAD_2000];

        let result = validate_pte_match(l0, 0x40_0000, &expected, |a| pt.read(a));
        assert_eq!(result, Err(2));
    }

    #[test]
    fn validate_pte_match_empty_slice_is_ok() {
        let (pt, l0) = build_basic_pt(0);
        let expected: [u64; 0] = [];
        assert!(validate_pte_match(l0, 0x40_0000, &expected, |a| pt.read(a)).is_ok());
    }

    // --- clear_validated_pte (write-only) tests ---

    #[test]
    fn clear_validated_pte_clears_all_count_ptes() {
        use std::cell::RefCell;
        let (pt, l0) = build_basic_pt(3);
        let pt = RefCell::new(pt);

        clear_validated_pte(
            l0, 0x40_0000, 3,
            |a| pt.borrow().read(a),
            |a, v| pt.borrow_mut().set(a, v),
        );

        let (mapped, _) = query_mapping_run(l0, 0x40_0000, |a| pt.borrow().read(a));
        assert!(!mapped);
    }

    #[test]
    #[should_panic(expected = "kernel-invariant violation")]
    fn clear_validated_pte_panics_on_unmapped_page() {
        // Without a prior validate_pte_match, the second iteration walks
        // an unmapped VA and gets WalkResult::Fault. The implementation
        // must panic loudly, not silently leave the cleared first PTE
        // in a half-applied state.
        let (pt, l0) = build_basic_pt(1);
        clear_validated_pte(
            l0, 0x40_0000, 2,
            |a| pt.read(a),
            |_, _| {},
        );
    }

    /// Build a page table where VA 0x40_0000 resolves through an L2
    /// 2MB block instead of an L3 page entry. validate_pte_match's
    /// "L3 only" contract must reject this.
    fn build_l2_block_pt() -> (FakePT, u64) {
        let mut pt = FakePT::new();
        let l0: u64 = 0x1_0000;
        let l1: u64 = 0x2_0000;
        let l2: u64 = 0x3_0000;

        pt.set_table_entry(l0, 0, l1);
        pt.set_table_entry(l1, 0, l2);
        // L2[2] is a 2MB block descriptor pointing at 0x8000_0000,
        // not a table descriptor pointing at an L3 page.
        let block = PageTableEntry::new_block(
            PhysAddr::new(0x8000_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        );
        pt.set(l2 + 2 * 8, block.raw());

        (pt, l0)
    }

    #[test]
    fn validate_pte_match_rejects_l2_block_mapping() {
        // VA 0x40_0000 resolves at L2 as a 2MB block. Even if the
        // resolved phys matches expected, the L3-only contract
        // requires rejection because clear_validated_pte cannot
        // safely zero an L2 block descriptor (would unmap 2MB).
        let (pt, l0) = build_l2_block_pt();
        let expected = [0x8000_0000];

        let result = validate_pte_match(l0, 0x40_0000, &expected, |a| pt.read(a));
        assert_eq!(result, Err(0));
    }

    #[test]
    #[should_panic(expected = "block mapping appeared at L1/L2")]
    fn clear_validated_pte_panics_on_l2_block_mapping() {
        // The validate stage would have rejected this, but if the page
        // table changed between phases to install a block (kernel bug),
        // clear must panic instead of zeroing the block descriptor.
        let (pt, l0) = build_l2_block_pt();
        clear_validated_pte(
            l0, 0x40_0000, 1,
            |a| pt.read(a),
            |_, _| {},
        );
    }

    #[test]
    fn page_table_walk_level_tracks_resolution() {
        // Locks down the level() invariant validate_pte_match relies on:
        // step() leaves walk.level at the level Done resolved at.
        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x4000_1234);
        // L0 → L1
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        // L1 block resolves at level 1
        let r = w.step(PageTableEntry::new_block(
            PhysAddr::new(0x8000_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert!(matches!(r, WalkResult::Done(_)));
        assert_eq!(w.level(), 1);

        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x40_1234);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        // L2 block resolves at level 2
        let r = w.step(PageTableEntry::new_block(
            PhysAddr::new(0x8000_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert!(matches!(r, WalkResult::Done(_)));
        assert_eq!(w.level(), 2);

        let (mut w, _) = PageTableWalk::start(0x1_0000, 0x40_1234);
        w.step(PageTableEntry::new_table(PhysAddr::new(0x2_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x3_0000)).raw());
        w.step(PageTableEntry::new_table(PhysAddr::new(0x4_0000)).raw());
        // L3 page resolves at level 3
        let r = w.step(PageTableEntry::new_page(
            PhysAddr::new(0x5_0000), MAIR_NORMAL, AP_RW_ALL, SH_INNER,
        ).raw());
        assert!(matches!(r, WalkResult::Done(_)));
        assert_eq!(w.level(), 3);
    }

    #[test]
    fn validate_then_clear_matches_unmap_validated() {
        // Two-phase split is functionally identical to the wrapper.
        use std::cell::RefCell;

        let (pt_a, l0_a) = build_basic_pt(3);
        let pt_a = RefCell::new(pt_a);
        let expected = [0x8000_0000, 0x8000_1000, 0x8000_2000];

        validate_pte_match(l0_a, 0x40_0000, &expected, |a| pt_a.borrow().read(a)).unwrap();
        clear_validated_pte(
            l0_a, 0x40_0000, expected.len(),
            |a| pt_a.borrow().read(a),
            |a, v| pt_a.borrow_mut().set(a, v),
        );

        let (pt_b, l0_b) = build_basic_pt(3);
        let pt_b = RefCell::new(pt_b);
        unmap_validated(
            l0_b, 0x40_0000, &expected,
            |a| pt_b.borrow().read(a),
            |a, v| pt_b.borrow_mut().set(a, v),
        ).unwrap();

        // Both produce the same end-state: no mapping at 0x40_0000.
        let (a_mapped, _) = query_mapping_run(l0_a, 0x40_0000, |a| pt_a.borrow().read(a));
        let (b_mapped, _) = query_mapping_run(l0_b, 0x40_0000, |a| pt_b.borrow().read(a));
        assert!(!a_mapped);
        assert!(!b_mapped);
    }
}
