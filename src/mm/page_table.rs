use crate::mm::addr::PhysAddr;
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

/// Read-only at EL1, no access at EL0.
pub const AP_RO_EL1: u8 = 0b10;

/// Read-only at EL1 and EL0.
pub const AP_RO_ALL: u8 = 0b11;

// ---------------------------------------------------------------------------
// Shareability constants (SH field, bits [9:8])
// ---------------------------------------------------------------------------

pub const SH_NON: u8 = 0b00;
pub const SH_OUTER: u8 = 0b10;
pub const SH_INNER: u8 = 0b11;

// ---------------------------------------------------------------------------
// Page table entry bit positions
// ---------------------------------------------------------------------------

const PTE_VALID: u64 = 1 << 0;
const PTE_TABLE: u64 = 1 << 1; // For L0-L2: table descriptor. For L3: page descriptor.
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
    /// An empty (invalid) entry.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw 64-bit value.
    pub const fn raw(self) -> u64 {
        self.0
    }

    // -- Readers --

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

    // -- Builders --

    /// L0/L1/L2 table descriptor pointing to the next-level page table.
    pub const fn new_table(next_table_phys: PhysAddr) -> Self {
        Self(PTE_VALID | PTE_TABLE | (next_table_phys.as_u64() & PTE_ADDR_MASK))
    }

    /// L1 block (1 GB) or L2 block (2 MB) descriptor.
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

    /// L3 page (4 KB) descriptor. Bit 1 must be set for L3 page entries.
    pub const fn new_page(phys: PhysAddr, attr: u8, ap: u8, sh: u8) -> Self {
        Self(
            PTE_VALID
                | PTE_TABLE // bit 1 = 1 for L3 page descriptors
                | ((attr as u64 & 0x7) << PTE_ATTR_SHIFT)
                | ((ap as u64 & 0x3) << PTE_AP_SHIFT)
                | ((sh as u64 & 0x3) << PTE_SH_SHIFT)
                | PTE_AF
                | (phys.as_u64() & PTE_ADDR_MASK),
        )
    }

    /// Set the Privileged Execute-Never bit.
    pub const fn with_pxn(self) -> Self {
        Self(self.0 | PTE_PXN)
    }

    /// Set the Unprivileged Execute-Never bit.
    pub const fn with_uxn(self) -> Self {
        Self(self.0 | PTE_UXN)
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
/// Used at all four levels (L0, L1, L2, L3) — structurally identical.
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    /// An empty page table (all entries invalid).
    pub const fn empty() -> Self {
        Self {
            entries: [PageTableEntry::empty(); 512],
        }
    }
}
