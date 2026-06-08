/// Minimal Flattened Device Tree (FDT) parser.
///
/// Parses the binary DTB format to extract device information: compatible
/// strings, MMIO addresses (reg property), interrupt numbers, and
/// `clocks` references (resolved against per-controller `#clock-cells`).
/// Pure, no_std, testable on host with a real DTB blob.
///
/// Limitations (sufficient for QEMU virt + Pi 4B):
/// - Phandle resolution scoped to clocks (no general phandle table
///   exposed to callers; sufficient for the providers we have today).
/// - Assumes root #address-cells=2, #size-cells=2.
///
/// Two public parsers:
/// - `parse_fdt()`: full device enumeration for the userspace device
///   manager. Returns up to MAX_DEVICES entries in an FdtDevices array.
/// - `scan_platform()`: lightweight kernel boot scanner. Extracts only
///   UART, GIC, and memory into a fixed PlatformHw struct. Safe for the
///   kernel's 8KB stack budget.
///
/// Both share the same FDT walk and property extraction helpers. Only
/// `parse_fdt` resolves `clocks` references; `scan_platform` doesn't
/// need them.

use crate::device::{ClockRef, DeviceInfo, MAX_CLOCK_REFS, MAX_DEVICES, compatible_hash};

// ---------------------------------------------------------------------------
// SMP boot method types (discovered from DTB)
// ---------------------------------------------------------------------------

/// How secondary CPUs are started, as described by the DTB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmpMethod {
    /// No SMP boot method found in DTB.
    None,
    /// PSCI (Power State Coordination Interface).
    /// `hvc`: true = HVC conduit (QEMU virt), false = SMC conduit.
    Psci { hvc: bool },
    /// Spin-table: write entry point to cpu-release-addr, dsb, sev.
    SpinTable,
}

/// Per-CPU info extracted from DTB /cpus/cpu@N nodes.
#[derive(Clone, Copy, Debug)]
pub struct CpuInfo {
    /// MPIDR affinity value from the cpu node's reg property.
    /// Used as target_cpu for PSCI CPU_ON and as identity key.
    pub mpidr: u64,
    /// Physical address to write for spin-table release. 0 if N/A.
    pub release_addr: u64,
}

impl CpuInfo {
    pub const EMPTY: Self = Self { mpidr: 0, release_addr: 0 };
}

// FDT structure block tokens
const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE: u32 = 0x00000002;
const FDT_PROP: u32 = 0x00000003;
const FDT_NOP: u32 = 0x00000004;
const FDT_END: u32 = 0x00000009;

// FDT header magic
const FDT_MAGIC: u32 = 0xd00dfeed;

/// Maximum nesting depth tracked during FDT walk.
/// Nodes deeper than this are traversed but their properties aren't parsed.
/// QEMU virt: devices at depth 1. Pi 4B: devices at depth 2 (root/soc/device).
/// Reduced from 8 to limit stack usage in kernel boot (8 KB stack budget).
const MAX_DEPTH: usize = 4;

/// Maximum compatible strings tracked per node.
const MAX_COMPAT: usize = 4;

/// Maximum ranges entries tracked per bus node.
/// Pi 4B soc has 3 ranges entries (main peripherals, PCIe, GIC area).
const MAX_RANGES: usize = 3;

/// A single entry from a DTB `ranges` property.
#[derive(Clone, Copy)]
struct RangeEntry {
    child_addr: u64,
    parent_addr: u64,
    size: u64,
}

/// Per-depth address space tracking for correct reg/ranges parsing.
/// Each node's `#address-cells` and `#size-cells` describe how its
/// children encode addresses. Ranges map from this node's bus space
/// to its parent's bus space.
#[derive(Clone, Copy)]
struct DepthCells {
    address_cells: u32,
    size_cells: u32,
    ranges: [RangeEntry; MAX_RANGES],
    range_count: u8,
    has_ranges: bool,
    /// Deferred ranges property data (parsed at first child's BEGIN_NODE,
    /// when both this node's and parent's #address-cells are known).
    ranges_start: usize,
    ranges_len: usize,
}

impl DepthCells {
    const DEFAULT: Self = Self {
        address_cells: 2,
        size_cells: 2,
        ranges: [RangeEntry { child_addr: 0, parent_addr: 0, size: 0 }; 3],
        range_count: 0,
        has_ranges: false,
        ranges_start: 0,
        ranges_len: 0,
    };
}

/// Errors from FDT parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdtError {
    /// Data is too small to contain an FDT header.
    TooSmall,
    /// Magic number doesn't match 0xd00dfeed.
    BadMagic,
    /// Structure or strings block offset is out of bounds.
    InvalidOffsets,
    /// Structure block ended unexpectedly (truncated or corrupt DTB).
    Truncated,
}

// ---------------------------------------------------------------------------
// FDT header helpers
// ---------------------------------------------------------------------------

/// Validate an FDT header and return (off_dt_struct, off_dt_strings).
fn validate_header(data: &[u8]) -> Result<(usize, usize), FdtError> {
    if data.len() < 40 {
        return Err(FdtError::TooSmall);
    }
    let magic = read_u32_be(data, 0);
    if magic != FDT_MAGIC {
        return Err(FdtError::BadMagic);
    }
    let off_dt_struct = read_u32_be(data, 8) as usize;
    let off_dt_strings = read_u32_be(data, 12) as usize;
    if off_dt_struct >= data.len() || off_dt_strings >= data.len() {
        return Err(FdtError::InvalidOffsets);
    }
    Ok((off_dt_struct, off_dt_strings))
}

/// Compute the actual content size of a DTB from its header.
/// Returns `off_dt_strings + size_dt_strings` — the end of the last
/// content block. QEMU pads `totalsize` to 1MB, so this is the
/// reliable way to determine how much of the DTB is real content.
///
/// `header` must be at least 40 bytes (the FDT header size).
pub fn dtb_content_size(header: &[u8]) -> Result<usize, FdtError> {
    if header.len() < 40 {
        return Err(FdtError::TooSmall);
    }
    let magic = read_u32_be(header, 0);
    if magic != FDT_MAGIC {
        return Err(FdtError::BadMagic);
    }
    let off_dt_strings = read_u32_be(header, 12) as usize;
    let size_dt_strings = read_u32_be(header, 32) as usize;
    // Checked add: a malformed header could otherwise wrap `usize`
    // before the caller bound-checks against the mapped DTB window.
    off_dt_strings
        .checked_add(size_dt_strings)
        .ok_or(FdtError::InvalidOffsets)
}

// ---------------------------------------------------------------------------
// Property extraction helpers
// ---------------------------------------------------------------------------

/// Parse a compatible property: hash ALL null-terminated strings.
/// DTB compatible properties contain a list of strings from most-specific
/// to most-generic (e.g., "brcm,bcm2835-pl011\0arm,pl011\0arm,primecell\0").
/// Returns hashes for up to MAX_COMPAT strings so consumers can match
/// against any of them.
fn parse_all_compat(data: &[u8], start: usize, len: usize) -> ([u64; MAX_COMPAT], u8) {
    let mut hashes = [0u64; MAX_COMPAT];
    let mut count = 0u8;
    let end = start + len;
    let mut pos = start;
    while pos < end && (count as usize) < MAX_COMPAT {
        let s = read_string(data, pos);
        if s.is_empty() {
            break;
        }
        hashes[count as usize] = compatible_hash(s);
        count += 1;
        pos += s.len() + 1; // skip past null terminator
    }
    (hashes, count)
}

/// Walk the DTB structure block looking for ANY node whose
/// `compatible` property hashes to `target`. Returns true on
/// first match. Allocation-free; suitable for init's "are we on
/// Pi 4B?" probe (DTB blob is already mapped, no need to build
/// the full FdtDevices summary just to answer a yes/no question).
///
/// Used by init to choose between virtio-blk and emmc2 as the
/// block backend wired to fat32-server. Future callers that need
/// the same "device present?" yes/no without the full device
/// table can reuse this directly.
pub fn has_compatible_hash(data: &[u8], target: u64) -> bool {
    if data.len() < 40 {
        return false;
    }
    let magic = read_u32_be(data, 0);
    if magic != FDT_MAGIC {
        return false;
    }
    let off_dt_struct = read_u32_be(data, 8) as usize;
    let off_dt_strings = read_u32_be(data, 12) as usize;
    let size_dt_struct = read_u32_be(data, 36) as usize;
    let size_dt_strings = read_u32_be(data, 32) as usize;
    // Every header-derived range must fit within `data`. Checked
    // adds guard against malformed DTBs that could overflow `usize`
    // before the inequality compare.
    let struct_end = match off_dt_struct.checked_add(size_dt_struct) {
        Some(v) => v,
        None => return false,
    };
    let strings_end = match off_dt_strings.checked_add(size_dt_strings) {
        Some(v) => v,
        None => return false,
    };
    if struct_end > data.len() || strings_end > data.len() {
        return false;
    }
    let mut pos = off_dt_struct;
    while pos + 4 <= struct_end {
        let token = read_u32_be(data, pos);
        pos += 4;
        match token {
            FDT_BEGIN_NODE => {
                // skip null-terminated unit name, then 4-byte align
                let mut name_end = pos;
                while name_end < struct_end && data[name_end] != 0 {
                    name_end += 1;
                }
                pos = match name_end.checked_add(4) {
                    Some(v) => v & !3,
                    None => return false,
                };
            }
            FDT_END_NODE | FDT_NOP => {}
            FDT_END => break,
            FDT_PROP => {
                if pos + 8 > struct_end { break; }
                let prop_len = read_u32_be(data, pos) as usize;
                let name_off = read_u32_be(data, pos + 4) as usize;
                pos += 8;
                // Property name lives in the strings block; bound the
                // read to that block to keep mis-decode local. Without
                // these checks a corrupt name_off could read past the
                // DTB end into adjacent memory.
                let name_start = match off_dt_strings.checked_add(name_off) {
                    Some(v) => v,
                    None => break,
                };
                if name_start >= strings_end { break; }
                let name = read_string_bounded(data, name_start, strings_end);
                if name == b"compatible" {
                    // Hash each null-terminated string; match any.
                    let val_end = match pos.checked_add(prop_len) {
                        Some(v) => v,
                        None => break,
                    };
                    if val_end > struct_end { break; }
                    let mut s_off = pos;
                    while s_off < val_end {
                        let s = read_string_bounded(data, s_off, val_end);
                        if s.is_empty() {
                            break;
                        }
                        if compatible_hash(s) == target {
                            return true;
                        }
                        match s_off.checked_add(s.len()).and_then(|v| v.checked_add(1)) {
                            Some(v) => s_off = v,
                            None => break,
                        }
                    }
                }
                // 4-byte-align the next position; saturating because a
                // pathological prop_len could otherwise overflow.
                pos = match pos.checked_add(prop_len)
                    .and_then(|v| v.checked_add(3))
                {
                    Some(v) => v & !3,
                    None => break,
                };
            }
            _ => break, // unknown token; bail rather than mis-decode
        }
    }
    false
}

/// Bounded variant of `read_string`: stops at `end` even if no NUL
/// is encountered. Used by `has_compatible_hash` to prevent a
/// malformed DTB from reading past the strings or property region.
fn read_string_bounded(data: &[u8], start: usize, end: usize) -> &[u8] {
    let end = end.min(data.len());
    let mut i = start;
    while i < end && data[i] != 0 {
        i += 1;
    }
    &data[start..i]
}

/// Read 1 or 2 big-endian u32 cells as a u64 value.
/// DTB properties encode addresses and sizes using a variable number
/// of 32-bit cells specified by the parent's #address-cells / #size-cells.
fn read_cells(data: &[u8], offset: usize, cells: u32) -> u64 {
    match cells {
        1 => read_u32_be(data, offset) as u64,
        2 => {
            let hi = read_u32_be(data, offset) as u64;
            let lo = read_u32_be(data, offset + 4) as u64;
            (hi << 32) | lo
        }
        _ => 0, // unsupported
    }
}

/// Parse an interrupts property (GICv3 format: <type spi_number flags>).
/// Returns the INTID (spi_number + 32) if type == 0 (SPI), else 0.
fn parse_interrupt(data: &[u8], prop_data_start: usize) -> u32 {
    let int_type = read_u32_be(data, prop_data_start);
    let spi_num = read_u32_be(data, prop_data_start + 4);
    if int_type == 0 { spi_num + 32 } else { 0 }
}

// ---------------------------------------------------------------------------
// FDT structure walk
// ---------------------------------------------------------------------------

/// Per-node state accumulated during an FDT walk.
/// CPU enable-method property from DTB cpu@N nodes.
#[derive(Clone, Copy, PartialEq)]
enum EnableMethod {
    None,
    Psci,
    SpinTable,
}

/// PSCI conduit from DTB /psci node's "method" property.
#[derive(Clone, Copy, PartialEq)]
enum PsciConduit {
    /// No method property, or unrecognized value.
    Unknown,
    Hvc,
    Smc,
}

/// Reset at each BEGIN_NODE, populated by PROP tokens, consumed at END_NODE.
#[derive(Clone, Copy)]
struct NodeState {
    /// FNV-1a hashes of all compatible strings (up to MAX_COMPAT).
    /// DTB lists them most-specific to most-generic; consumers can match
    /// against any entry to find a known device type.
    compat_hashes: [u64; MAX_COMPAT],
    /// Number of valid entries in compat_hashes.
    compat_count: u8,
    /// First reg entry: address (translated to root physical space).
    reg_addr: u64,
    /// First reg entry: size.
    reg_size: u64,
    /// Second reg entry: address (for multi-reg devices like GIC). 0 = absent.
    reg_addr2: u64,
    /// Interrupt ID (SPI + 32). 0 = no interrupts property.
    intid: u32,
    /// True if this node had a compatible property.
    has_compat: bool,
    /// Deferred reg property position and length.
    /// Parsed at END_NODE when parent's #address-cells is known.
    reg_start: usize,
    reg_len: usize,
    enable_method: EnableMethod,
    /// cpu-release-addr for spin-table CPUs (always 2 cells per binding spec)
    cpu_release_addr: u64,
    /// /psci node's "method" property conduit.
    psci_conduit: PsciConduit,
    /// `phandle` (or `linux,phandle`) of this node, if present.
    /// 0 = no phandle (DTB convention; phandle 0 is reserved).
    phandle: u32,
    /// `#clock-cells` value for nodes that act as clock controllers.
    /// `u32::MAX` = property absent (we use sentinel rather than 0
    /// because 0 is a valid value for fixed-rate clocks with no id).
    clock_cells: u32,
    /// Raw bytes of the `clocks` property (deferred resolution —
    /// each referenced controller's `#clock-cells` may not have been
    /// seen yet during the walk). 0 = no clocks property.
    clocks_start: usize,
    clocks_len: usize,
}

impl NodeState {
    const EMPTY: Self = Self {
        compat_hashes: [0; MAX_COMPAT], compat_count: 0,
        reg_addr: 0, reg_size: 0, reg_addr2: 0,
        intid: 0, has_compat: false,
        reg_start: 0, reg_len: 0,
        enable_method: EnableMethod::None,
        cpu_release_addr: 0,
        psci_conduit: PsciConduit::Unknown,
        phandle: 0,
        clock_cells: u32::MAX,
        clocks_start: 0,
        clocks_len: 0,
    };

    /// Check if any compatible hash matches the given hash.
    fn has_compat_hash(&self, hash: u64) -> bool {
        let mut i = 0;
        while i < self.compat_count as usize {
            if self.compat_hashes[i] == hash {
                return true;
            }
            i += 1;
        }
        false
    }
}

/// Parse deferred ranges property data for the node at depth `d`.
/// Called when all properties at depth `d` are complete (triggered by
/// the first child's BEGIN_NODE or at END_NODE).
///
/// Uses this node's #address-cells + parent's #address-cells to decode
/// (child_addr, parent_addr, length) triples from the raw ranges data.
fn parse_ranges_at(data: &[u8], d: usize, cells: &mut [DepthCells; MAX_DEPTH]) {
    let child_ac = cells[d].address_cells;
    let parent_ac = if d > 0 { cells[d - 1].address_cells } else { 2 };
    let child_sc = cells[d].size_cells;
    let entry_bytes = (child_ac + parent_ac + child_sc) as usize * 4;

    if entry_bytes == 0 {
        return;
    }

    let start = cells[d].ranges_start;
    let end = start + cells[d].ranges_len;
    let mut off = start;

    while off + entry_bytes <= end && (cells[d].range_count as usize) < MAX_RANGES {
        let ca = read_cells(data, off, child_ac);
        let pa = read_cells(data, off + child_ac as usize * 4, parent_ac);
        let sz = read_cells(data, off + (child_ac + parent_ac) as usize * 4, child_sc);
        cells[d].ranges[cells[d].range_count as usize] = RangeEntry {
            child_addr: ca,
            parent_addr: pa,
            size: sz,
        };
        cells[d].range_count += 1;
        off += entry_bytes;
    }
}

/// Translate an address from a node's parent bus space to root physical space.
///
/// A node at depth `d` has its reg encoded in the parent's (d-1) address
/// space. This function walks up the tree, applying each bus node's ranges
/// to translate from child space to parent space, until reaching the root
/// (depth 0 = physical addresses).
fn translate_address(addr: u64, node_depth: usize, cells: &[DepthCells; MAX_DEPTH]) -> u64 {
    let mut a = addr;
    // Walk from the parent level down to depth 1 (root has no parent).
    // At each level, apply ranges to translate from that bus space
    // to the parent's bus space.
    let mut level = if node_depth > 0 { node_depth - 1 } else { return a };
    while level > 0 {
        let dc = &cells[level];
        if dc.range_count > 0 {
            let mut i = 0;
            while i < dc.range_count as usize {
                let r = &dc.ranges[i];
                if a >= r.child_addr && a.wrapping_sub(r.child_addr) < r.size {
                    a = r.parent_addr.wrapping_add(a.wrapping_sub(r.child_addr));
                    break;
                }
                i += 1;
            }
        }
        level -= 1;
    }
    a
}

/// Walk the FDT structure block, calling `on_node` for each completed node.
///
/// The walk handles token dispatch, node name parsing, property extraction
/// (compatible, reg, interrupts), and depth tracking. The caller-provided
/// `on_node` closure receives the node name and accumulated NodeState for
/// each END_NODE, and can store whatever it needs.
///
/// Returns `Err(FdtError::Truncated)` if the structure block ends
/// unexpectedly (missing FDT_END, property data overflows, etc.).
/// A clean walk always reaches FDT_END or the root END_NODE.
fn walk_fdt(
    data: &[u8],
    off_dt_struct: usize,
    off_dt_strings: usize,
    mut on_node: impl FnMut(&[u8], &NodeState),
) -> Result<(), FdtError> {
    let mut pos = off_dt_struct;
    let mut depth: usize = 0;

    // Per-depth node state
    let mut state: [NodeState; MAX_DEPTH] = [NodeState::EMPTY; MAX_DEPTH];
    // Per-depth node name start/end for passing to callback
    let mut name_start: [usize; MAX_DEPTH] = [0; MAX_DEPTH];
    let mut name_end: [usize; MAX_DEPTH] = [0; MAX_DEPTH];
    // Per-depth address space tracking (#address-cells, #size-cells, ranges)
    let mut cells: [DepthCells; MAX_DEPTH] = [DepthCells::DEFAULT; MAX_DEPTH];

    loop {
        if pos + 4 > data.len() {
            return Err(FdtError::Truncated);
        }
        let token = read_u32_be(data, pos);
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                // Finalize parent's deferred ranges before processing children.
                // By this point, all properties of the parent node are complete.
                if depth > 0 && depth <= MAX_DEPTH {
                    let pd = depth - 1;
                    if pd < MAX_DEPTH && cells[pd].has_ranges && cells[pd].range_count == 0
                       && cells[pd].ranges_len > 0 {
                        parse_ranges_at(data, pd, &mut cells);
                    }
                }

                // Read node name (null-terminated, padded to 4 bytes)
                let ns = pos;
                while pos < data.len() && data[pos] != 0 {
                    pos += 1;
                }
                let ne = pos;
                pos += 1; // skip null terminator
                pos = align4(pos);
                // Reset state for this depth level
                if depth < MAX_DEPTH {
                    state[depth] = NodeState::EMPTY;
                    cells[depth] = DepthCells::DEFAULT;
                    name_start[depth] = ns;
                    name_end[depth] = ne;
                }
                depth += 1;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    // Unmatched END_NODE — corrupt structure block
                    return Err(FdtError::Truncated);
                }
                depth -= 1;
                // Deliver completed node to the callback
                if depth < MAX_DEPTH {
                    // Parse deferred reg using parent's #address-cells / #size-cells
                    if state[depth].reg_len > 0 {
                        let (ac, sc) = if depth > 0 {
                            (cells[depth - 1].address_cells, cells[depth - 1].size_cells)
                        } else {
                            (2, 2) // DTB spec defaults
                        };
                        let entry_size = (ac + sc) as usize * 4;
                        let start = state[depth].reg_start;
                        if state[depth].reg_len >= entry_size {
                            state[depth].reg_addr = read_cells(data, start, ac);
                            state[depth].reg_size = read_cells(
                                data, start + ac as usize * 4, sc);
                            if state[depth].reg_len >= entry_size * 2 {
                                state[depth].reg_addr2 = read_cells(
                                    data, start + entry_size, ac);
                            }
                        }
                    }

                    // Translate addresses through ranges chain to root physical
                    if state[depth].reg_addr != 0 {
                        state[depth].reg_addr = translate_address(
                            state[depth].reg_addr, depth, &cells);
                    }
                    if state[depth].reg_addr2 != 0 {
                        state[depth].reg_addr2 = translate_address(
                            state[depth].reg_addr2, depth, &cells);
                    }

                    let name = &data[name_start[depth]..name_end[depth]];
                    on_node(name, &state[depth]);
                }
            }
            FDT_PROP => {
                if pos + 8 > data.len() {
                    return Err(FdtError::Truncated);
                }
                let prop_len = read_u32_be(data, pos) as usize;
                let name_off = read_u32_be(data, pos + 4) as usize;
                pos += 8;

                let prop_data_start = pos;
                let prop_data_end = pos + prop_len;
                if prop_data_end > data.len() {
                    return Err(FdtError::Truncated);
                }

                // Read property name from strings block.
                // Properties are stored at (depth - 1) since depth was
                // incremented on BEGIN_NODE before properties are read.
                let prop_name = read_string(data, off_dt_strings + name_off);
                let d = if depth > 0 { depth - 1 } else {
                    pos = align4(prop_data_end);
                    continue;
                };

                if d < MAX_DEPTH {
                    if str_eq(prop_name, b"compatible") {
                        // Hash all compatible strings (most-specific first)
                        let (hashes, count) = parse_all_compat(
                            data, prop_data_start, prop_len);
                        state[d].compat_hashes = hashes;
                        state[d].compat_count = count;
                        state[d].has_compat = true;
                    } else if str_eq(prop_name, b"#address-cells") && prop_len >= 4 {
                        cells[d].address_cells = read_u32_be(data, prop_data_start);
                    } else if str_eq(prop_name, b"#size-cells") && prop_len >= 4 {
                        cells[d].size_cells = read_u32_be(data, prop_data_start);
                    } else if str_eq(prop_name, b"reg") {
                        // Defer parsing until END_NODE — parent's #address-cells
                        // may not have been seen yet in property order
                        state[d].reg_start = prop_data_start;
                        state[d].reg_len = prop_len;
                    } else if str_eq(prop_name, b"interrupts") && prop_len >= 12 {
                        state[d].intid = parse_interrupt(data, prop_data_start);
                    } else if str_eq(prop_name, b"ranges") {
                        // Defer parsing — need both this node's and parent's
                        // #address-cells, which may appear later in property order
                        cells[d].has_ranges = true;
                        cells[d].ranges_start = prop_data_start;
                        cells[d].ranges_len = prop_len;
                    } else if str_eq(prop_name, b"enable-method") {
                        let val = read_string(data, prop_data_start);
                        if str_eq(val, b"psci") {
                            state[d].enable_method = EnableMethod::Psci;
                        } else if str_eq(val, b"spin-table") {
                            state[d].enable_method = EnableMethod::SpinTable;
                        }
                    } else if str_eq(prop_name, b"cpu-release-addr") && prop_len >= 8 {
                        // Per DTB binding spec, cpu-release-addr is always a
                        // 64-bit physical address (2 × 32-bit cells), regardless
                        // of parent #address-cells.
                        state[d].cpu_release_addr = read_cells(data, prop_data_start, 2);
                    } else if str_eq(prop_name, b"method") {
                        let val = read_string(data, prop_data_start);
                        if str_eq(val, b"hvc") {
                            state[d].psci_conduit = PsciConduit::Hvc;
                        } else if str_eq(val, b"smc") {
                            state[d].psci_conduit = PsciConduit::Smc;
                        }
                        // Unrecognized values stay PsciConduit::Unknown
                    } else if (str_eq(prop_name, b"phandle")
                              || str_eq(prop_name, b"linux,phandle"))
                              && prop_len >= 4 {
                        // Some toolchains emit `linux,phandle` (deprecated)
                        // alongside or instead of `phandle`. Either form
                        // serves as this node's phandle for cross-references.
                        state[d].phandle = read_u32_be(data, prop_data_start);
                    } else if str_eq(prop_name, b"#clock-cells") && prop_len >= 4 {
                        // This node is a clock controller. Records how
                        // many cells per clock-reference its consumers
                        // must supply. 0 cells = single anonymous clock
                        // (e.g., fixed-clock); 1+ cells = id + extras.
                        state[d].clock_cells = read_u32_be(data, prop_data_start);
                    } else if str_eq(prop_name, b"clocks") {
                        // Defer parsing — the referenced controller's
                        // #clock-cells may not have been walked yet.
                        // Resolution happens after the full walk
                        // completes (see resolve_clocks).
                        state[d].clocks_start = prop_data_start;
                        state[d].clocks_len = prop_len;
                    }
                }

                pos = align4(prop_data_end);
            }
            FDT_NOP => {}
            FDT_END => return Ok(()),
            _ => return Err(FdtError::Truncated), // unknown token
        }
    }
}

// ---------------------------------------------------------------------------
// Full device enumeration (for userspace device manager)
// ---------------------------------------------------------------------------

/// Maximum number of clock-controller nodes the resolver tracks.
/// One entry per phandle that declared `#clock-cells`. Pi 4B has CPRMAN
/// + a few fixed-clock nodes; QEMU virt has none. 16 is plenty.
const MAX_CLOCK_PROVIDERS: usize = 16;

/// Per-controller record: maps a phandle to its `#clock-cells` value.
/// Built during `parse_fdt`, consumed by `resolve_clocks` after the
/// full walk finishes.
#[derive(Clone, Copy)]
struct ClockProvider {
    phandle: u32,
    clock_cells: u32,
}

/// Captured raw `clocks` property bytes for one device, awaiting
/// resolution against the phandle → #clock-cells table.
#[derive(Clone, Copy)]
struct PendingClocks {
    /// Index into FdtDevices.devices for the consumer.
    device_idx: u16,
    /// File offset + length of the raw clocks property bytes.
    bytes_start: u32,
    bytes_len: u32,
}

/// Result of parsing an FDT: a list of discovered devices.
pub struct FdtDevices {
    pub devices: [DeviceInfo; MAX_DEVICES],
    pub count: usize,
}

/// Empty `FdtDevices` builder for callers that pre-allocate the
/// device array (typically in a static or page-backed buffer to avoid
/// the ~18 KB stack copy a by-value return would produce).
impl FdtDevices {
    pub const fn empty() -> Self {
        Self {
            devices: [DeviceInfo {
                compat_hashes: [0; MAX_COMPAT],
                compat_count: 0,
                mmio_addr: 0,
                mmio_size: 0,
                intid: 0,
                claimed: false,
                claim_token: 0,
                clocks: [ClockRef { controller_phandle: 0, clock_id: 0 }; MAX_CLOCK_REFS],
                clock_count: 0,
                phandle: 0,
            }; MAX_DEVICES],
            count: 0,
        }
    }
}

/// Parse an FDT blob into a caller-provided `FdtDevices` buffer.
/// Resets the buffer's `count` to 0 and fills in devices with
/// compatible strings, MMIO addresses, interrupt IDs, and resolved
/// clock references.
///
/// Takes the output by `&mut` rather than returning by value because
/// `FdtDevices` is large (~18 KB at MAX_DEVICES = 192) — a by-value
/// return materializes the struct on both the callee's and caller's
/// stack momentarily, blowing past typical userspace stack budgets.
pub fn parse_fdt_into(data: &[u8], result: &mut FdtDevices) -> Result<(), FdtError> {
    let (off_dt_struct, off_dt_strings) = validate_header(data)?;

    result.count = 0;

    // Phandle → #clock-cells table, populated as we encounter clock-
    // controller nodes during the walk. Used after the walk to resolve
    // each device's deferred `clocks` property bytes.
    let mut providers: [ClockProvider; MAX_CLOCK_PROVIDERS] =
        [ClockProvider { phandle: 0, clock_cells: 0 }; MAX_CLOCK_PROVIDERS];
    let mut provider_count: usize = 0;

    // Devices with `clocks` properties to resolve after the walk.
    let mut pending: [PendingClocks; MAX_DEVICES] =
        [PendingClocks { device_idx: 0, bytes_start: 0, bytes_len: 0 }; MAX_DEVICES];
    let mut pending_count: usize = 0;

    walk_fdt(data, off_dt_struct, off_dt_strings, |_name, node| {
        // Record clock providers regardless of whether they have a
        // compatible string (clocks-controller nodes sometimes don't,
        // e.g., bare "fixed-clock" nodes).
        if node.clock_cells != u32::MAX
            && node.phandle != 0
            && provider_count < MAX_CLOCK_PROVIDERS
        {
            providers[provider_count] = ClockProvider {
                phandle: node.phandle,
                clock_cells: node.clock_cells,
            };
            provider_count += 1;
        }

        // Save every node that had a compatible string.
        // Store all compat hashes so consumers can match any of them
        // (e.g., Pi 4B UART has "arm,pl011-axi" first, "arm,pl011" second).
        if node.has_compat && result.count < MAX_DEVICES {
            let idx = result.count;
            result.devices[idx] = DeviceInfo {
                compat_hashes: node.compat_hashes,
                compat_count: node.compat_count,
                mmio_addr: node.reg_addr,
                mmio_size: node.reg_size,
                intid: node.intid,
                claimed: false,
                claim_token: 0,
                clocks: [ClockRef { controller_phandle: 0, clock_id: 0 }; MAX_CLOCK_REFS],
                clock_count: 0,
                phandle: node.phandle,
            };

            // Defer clocks resolution — controller's #clock-cells may
            // not have been seen yet. Capture the raw property bytes.
            if node.clocks_len > 0 && pending_count < MAX_DEVICES {
                pending[pending_count] = PendingClocks {
                    device_idx: idx as u16,
                    bytes_start: node.clocks_start as u32,
                    bytes_len: node.clocks_len as u32,
                };
                pending_count += 1;
            }

            result.count += 1;
        }
    })?;

    // Walk complete: every clock controller has been recorded.
    // Resolve each pending device's `clocks` property bytes against
    // the provider table.
    let providers_slice = &providers[..provider_count];
    let mut i = 0;
    while i < pending_count {
        let p = pending[i];
        resolve_clocks_for(
            &mut result.devices[p.device_idx as usize],
            data,
            p.bytes_start as usize,
            p.bytes_len as usize,
            providers_slice,
        );
        i += 1;
    }

    Ok(())
}

/// Convenience wrapper around `parse_fdt_into` that allocates the
/// `FdtDevices` buffer on the caller's stack and returns it by value.
/// Suitable for host tests where stack space is plentiful; production
/// userspace (device-manager) should use `parse_fdt_into` with a
/// pre-allocated static or page-backed buffer to avoid the ~18 KB
/// per-call-frame stack hit at MAX_DEVICES = 192.
pub fn parse_fdt(data: &[u8]) -> Result<FdtDevices, FdtError> {
    let mut devs = FdtDevices::empty();
    parse_fdt_into(data, &mut devs)?;
    Ok(devs)
}

/// Resolve one device's `clocks = <&phandle id ...>` property bytes
/// into typed `ClockRef` entries. Walks the bytes one reference at a
/// time, looking up the controller's `#clock-cells` to know how many
/// cells follow each phandle. Unknown controllers (phandle not in the
/// table) are silently skipped — they correspond to controllers we
/// don't model, and the consumer's view should reflect what it can
/// actually use.
fn resolve_clocks_for(
    device: &mut DeviceInfo,
    data: &[u8],
    bytes_start: usize,
    bytes_len: usize,
    providers: &[ClockProvider],
) {
    let end = bytes_start + bytes_len;
    let mut off = bytes_start;
    while off + 4 <= end && (device.clock_count as usize) < MAX_CLOCK_REFS {
        let phandle = read_u32_be(data, off);
        off += 4;
        // Look up #clock-cells for this phandle.
        let cells = match providers.iter().find(|p| p.phandle == phandle) {
            Some(p) => p.clock_cells,
            None => {
                // Unknown controller — we can't decode further bytes
                // because we don't know how many cells to skip.
                // Stop processing this property to avoid mis-aligned
                // decode of later (phandle, id) tuples.
                break;
            }
        };
        // Per binding spec: #clock-cells = N means N u32 cells follow
        // the phandle. By convention the first cell is the clock_id;
        // any further cells are controller-specific (we ignore them
        // for now — emmc2/CPRMAN uses #clock-cells = 1).
        let cells_bytes = (cells as usize) * 4;
        if off + cells_bytes > end {
            break;
        }
        let clock_id = if cells >= 1 {
            read_u32_be(data, off)
        } else {
            0 // fixed-clock with no id
        };
        off += cells_bytes;

        let idx = device.clock_count as usize;
        device.clocks[idx] = ClockRef {
            controller_phandle: phandle,
            clock_id,
        };
        device.clock_count += 1;
    }
}

// ---------------------------------------------------------------------------
// Lightweight kernel boot scanner
// ---------------------------------------------------------------------------

/// Platform hardware discovered from DTB. Fixed-size, no arrays.
/// Used by the kernel's early boot to find UART, GIC, and RAM without
/// the stack cost of the full FdtDevices array (which holds 64 entries
/// and exceeds the kernel's 8KB stack budget).
#[derive(Clone, Copy, Debug)]
pub struct PlatformHw {
    /// PL011 UART physical base address. 0 = not found.
    pub pl011_base: u64,
    /// GIC distributor physical base address. 0 = not found.
    pub gicd_base: u64,
    /// GIC secondary base: redistributor (v3) or CPU interface (v2).
    /// Parsed from the second reg entry of the GIC node. 0 = not found.
    pub gic_secondary_base: u64,
    /// True if GIC is v2 (arm,cortex-a15-gic or arm,gic-400).
    pub gic_v2: bool,
    /// RAM base physical address from /memory node.
    /// Can legitimately be 0 (Pi 4B). Use ram_size to detect "not found".
    pub ram_base: u64,
    /// RAM size in bytes from /memory node. 0 = not found.
    pub ram_size: u64,
    /// SMP boot method detected from /psci node and cpu enable-method.
    pub smp_method: SmpMethod,
    /// Per-CPU info (MPIDR + release address). Indexed by discovery
    /// order; use mpidr field for identity, not array index.
    pub cpus: [CpuInfo; 4],
    /// Number of valid entries in cpus[].
    pub cpu_count: u8,
}

/// Scan a DTB for kernel-essential hardware: UART, GIC, memory.
///
/// Walks the FDT structure block once, matching only the compatible
/// strings and node names the kernel needs at boot. No device array
/// allocation — just fills in the fixed PlatformHw struct.
///
/// `#[inline(never)]` prevents LTO from merging this function's stack
/// frame (which includes the walk_fdt local arrays) into kmain's frame,
/// keeping both under the per-function 1536-byte stack cap.
#[inline(never)]
pub fn scan_platform(data: &[u8]) -> Result<PlatformHw, FdtError> {
    let (off_dt_struct, off_dt_strings) = validate_header(data)?;

    let pl011_hash = compatible_hash(b"arm,pl011");
    let gicv3_hash = compatible_hash(b"arm,gic-v3");
    let gicv2_hash = compatible_hash(b"arm,cortex-a15-gic");
    let gic400_hash = compatible_hash(b"arm,gic-400");

    let mut hw = PlatformHw {
        pl011_base: 0, gicd_base: 0, gic_secondary_base: 0,
        gic_v2: false, ram_base: 0, ram_size: 0,
        smp_method: SmpMethod::None, cpus: [CpuInfo::EMPTY; 4], cpu_count: 0,
    };

    walk_fdt(data, off_dt_struct, off_dt_strings, |name, node| {
        // Detect /memory and /memory@XXXX nodes by name
        let is_memory = str_eq(name, b"memory") ||
            (name.len() > 7 && str_eq(&name[..7], b"memory@"));

        // Guard on reg_size, not reg_addr — Pi 4B has RAM at physical 0x0,
        // so reg_addr == 0 is valid. reg_size == 0 means "not populated".
        if is_memory && node.reg_size != 0 && hw.ram_size == 0 {
            hw.ram_base = node.reg_addr;
            hw.ram_size = node.reg_size;
        } else if node.has_compat_hash(pl011_hash) && hw.pl011_base == 0 {
            // Match any compatible string (e.g., "arm,pl011-axi" first,
            // "arm,pl011" second on Pi 4B; "arm,pl011" first on QEMU).
            hw.pl011_base = node.reg_addr;
        } else if node.has_compat_hash(gicv3_hash) && hw.gicd_base == 0 {
            hw.gicd_base = node.reg_addr;
            hw.gic_secondary_base = node.reg_addr2;
            hw.gic_v2 = false;
        } else if (node.has_compat_hash(gicv2_hash) || node.has_compat_hash(gic400_hash))
                  && hw.gicd_base == 0 {
            hw.gicd_base = node.reg_addr;
            hw.gic_secondary_base = node.reg_addr2;
            hw.gic_v2 = true;
        }

        // /psci node: only set PSCI method if conduit is recognized.
        // A /psci node with missing or unrecognized method property is
        // treated as no valid boot method — don't guess SMC vs HVC.
        if str_eq(name, b"psci") {
            match node.psci_conduit {
                PsciConduit::Hvc => hw.smp_method = SmpMethod::Psci { hvc: true },
                PsciConduit::Smc => hw.smp_method = SmpMethod::Psci { hvc: false },
                PsciConduit::Unknown => {} // malformed — leave as None
            }
        }

        // cpu@N nodes: extract MPIDR identity and spin-table release address
        let is_cpu = name.len() >= 4 && str_eq(&name[..4], b"cpu@");
        if is_cpu && (hw.cpu_count as usize) < 4 {
            let idx = hw.cpu_count as usize;
            hw.cpus[idx] = CpuInfo {
                mpidr: node.reg_addr,
                release_addr: node.cpu_release_addr,
            };
            hw.cpu_count += 1;

            // Derive SpinTable from cpu enable-method only if no /psci node
            // overrides it. PSCI requires an explicit /psci node.
            if node.enable_method == EnableMethod::SpinTable
                && !matches!(hw.smp_method, SmpMethod::Psci { .. })
            {
                hw.smp_method = SmpMethod::SpinTable;
            }
        }
    })?;

    Ok(hw)
}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
    ])
}

fn align4(pos: usize) -> usize {
    (pos + 3) & !3
}

/// Read a null-terminated string starting at `offset`. Returns the byte slice
/// up to (not including) the null terminator.
fn read_string(data: &[u8], offset: usize) -> &[u8] {
    if offset >= data.len() {
        return &[];
    }
    let mut end = offset;
    while end < data.len() && data[end] != 0 {
        end += 1;
    }
    &data[offset..end]
}

/// Compare a byte slice to a known string.
fn str_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::PL011_HASH;

    // Load the real QEMU virt DTB for testing
    static QEMU_DTB: &[u8] = include_bytes!("../test-data/qemu-virt.dtb");

    // --- header + helper tests ---

    #[test]
    fn validate_header_succeeds() {
        assert!(validate_header(QEMU_DTB).is_ok());
    }

    #[test]
    fn validate_header_too_small() {
        assert_eq!(validate_header(&[0; 10]).unwrap_err(), FdtError::TooSmall);
    }

    #[test]
    fn validate_header_bad_magic() {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&[0xBA, 0xAD, 0xF0, 0x0D]);
        assert_eq!(validate_header(&data).unwrap_err(), FdtError::BadMagic);
    }

    #[test]
    fn read_u32_be_test() {
        assert_eq!(read_u32_be(&[0xDE, 0xAD, 0xBE, 0xEF], 0), 0xDEADBEEF);
        assert_eq!(read_u32_be(&[0x00, 0x00, 0x00, 0x01], 0), 1);
    }

    #[test]
    fn read_cells_one() {
        // #address-cells=1: single 32-bit value
        let data = [0x09, 0x00, 0x00, 0x00];
        assert_eq!(read_cells(&data, 0, 1), 0x0900_0000);
    }

    #[test]
    fn read_cells_two() {
        // #address-cells=2: two 32-bit values → 64-bit
        let data = [
            0x00, 0x00, 0x00, 0x00,  // hi
            0x09, 0x00, 0x00, 0x00,  // lo
        ];
        assert_eq!(read_cells(&data, 0, 2), 0x0900_0000);
    }

    #[test]
    fn read_cells_two_high() {
        // High bits set
        let data = [
            0x00, 0x00, 0x00, 0x01,  // hi = 1
            0x00, 0x00, 0x00, 0x00,  // lo = 0
        ];
        assert_eq!(read_cells(&data, 0, 2), 0x1_0000_0000);
    }

    #[test]
    fn parse_interrupt_spi() {
        // <0x00000000 0x00000001 0x00000004> = SPI 1, INTID = 33
        let data = [
            0x00, 0x00, 0x00, 0x00,  // type = 0 (SPI)
            0x00, 0x00, 0x00, 0x01,  // spi_num = 1
            0x00, 0x00, 0x00, 0x04,  // flags
        ];
        assert_eq!(parse_interrupt(&data, 0), 33);
    }

    #[test]
    fn parse_interrupt_non_spi() {
        // type = 1 (PPI), should return 0
        let data = [
            0x00, 0x00, 0x00, 0x01,  // type = 1 (PPI)
            0x00, 0x00, 0x00, 0x0B,  // number
            0x00, 0x00, 0x00, 0x04,  // flags
        ];
        assert_eq!(parse_interrupt(&data, 0), 0);
    }

    // --- parse_fdt tests ---

    #[test]
    fn parse_qemu_dtb_succeeds() {
        let result = parse_fdt(QEMU_DTB);
        assert!(result.is_ok());
    }

    #[test]
    fn finds_pl011_devices() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let pl011_count = devs.devices[..devs.count]
            .iter()
            .filter(|d| d.has_compat(PL011_HASH))
            .count();
        assert_eq!(pl011_count, 2, "QEMU virt should have 2 PL011 UARTs");
    }

    #[test]
    fn uart0_has_correct_address() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let uart0 = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(PL011_HASH) && d.mmio_addr == 0x0900_0000);
        assert!(uart0.is_some(), "UART0 should be at 0x09000000");
        let uart0 = uart0.unwrap();
        assert_eq!(uart0.mmio_size, 0x1000);
        assert_eq!(uart0.intid, 33); // SPI 1 + 32
    }

    #[test]
    fn uart1_has_correct_address() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let uart1 = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(PL011_HASH) && d.mmio_addr == 0x0904_0000);
        assert!(uart1.is_some(), "UART1 should be at 0x09040000");
        let uart1 = uart1.unwrap();
        assert_eq!(uart1.mmio_size, 0x1000);
        assert_eq!(uart1.intid, 40); // SPI 8 + 32
    }

    #[test]
    fn finds_gicv3() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let gic_hash = compatible_hash(b"arm,gic-v3");
        let gic = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(gic_hash));
        assert!(gic.is_some(), "Should find GICv3");
        let gic = gic.unwrap();
        assert_eq!(gic.mmio_addr, 0x0800_0000);
    }

    #[test]
    fn devices_not_claimed_initially() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        for d in &devs.devices[..devs.count] {
            assert!(!d.claimed);
        }
    }

    #[test]
    fn finds_multiple_device_types() {
        let devs = parse_fdt(QEMU_DTB).unwrap();
        // Should find more than just PL011s — GIC, virtio, etc.
        assert!(devs.count >= 3, "Should find at least 3 devices, found {}", devs.count);
    }

    #[test]
    fn too_small_input() {
        match parse_fdt(&[0; 10]) {
            Err(FdtError::TooSmall) => {}
            _ => panic!("expected TooSmall"),
        }
    }

    // --- clocks-reference resolution tests ---

    // PI4B_DTB is declared further down (in the scan_platform tests
    // section). Reuse that constant here.

    /// CM_EMMC2 clock id from the upstream BCM2711 DT binding
    /// (`include/dt-bindings/clock/bcm2835.h::BCM2835_CLOCK_EMMC2`).
    /// Stable enum value; used by Linux, U-Boot, Circle, and now us.
    const BCM2711_CLOCK_EMMC2: u32 = 51;

    #[test]
    fn pi4b_dtb_parses() {
        assert!(parse_fdt(PI4B_DTB).is_ok());
    }

    #[test]
    fn pi4b_cprman_phandle_populated_in_device_info() {
        // device-manager's clock-provider registry validates incoming
        // CMD_GET_CLOCK_HANDLE requests against this phandle. If the
        // FDT parser stops plumbing it into DeviceInfo, the registry
        // would silently fail (no provider would ever match) — lock
        // it down here.
        let devs = parse_fdt(PI4B_DTB).unwrap();
        let cprman_hash = compatible_hash(b"brcm,bcm2711-cprman");
        let cprman = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(cprman_hash))
            .expect("Pi 4B DTB should have brcm,bcm2711-cprman");
        assert_ne!(cprman.phandle, 0,
                   "cprman node declares a phandle in the Pi 4B DTB");

        // And the consumer's clocks-reference must point to that
        // exact phandle — both sides of the binding agree.
        let emmc2_hash = compatible_hash(b"brcm,bcm2711-emmc2");
        let emmc2 = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(emmc2_hash))
            .expect("Pi 4B DTB should have brcm,bcm2711-emmc2");
        assert_eq!(emmc2.clocks[0].controller_phandle, cprman.phandle,
                   "emmc2's clocks ref must point at cprman's phandle");
    }

    #[test]
    fn pi4b_emmc2_has_clocks_resolved() {
        let devs = parse_fdt(PI4B_DTB).unwrap();
        let emmc2_hash = compatible_hash(b"brcm,bcm2711-emmc2");
        let emmc2 = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(emmc2_hash))
            .expect("Pi 4B DTB should have brcm,bcm2711-emmc2");
        assert_eq!(emmc2.clock_count, 1, "emmc2 has exactly one clock");
        let cref = emmc2.clocks[0];
        assert_ne!(cref.controller_phandle, 0,
                   "controller phandle should be resolved (CPRMAN)");
        assert_eq!(cref.clock_id, BCM2711_CLOCK_EMMC2,
                   "clock_id should be CM_EMMC2 (51) per BCM2711 binding");
    }

    #[test]
    fn qemu_clocks_resolve_to_apb_pclk() {
        // QEMU virt's PL011 UARTs reference an apb-pclk fixed-clock
        // (#clock-cells = 0). The resolver should record the
        // controller's phandle with clock_id = 0 (no id cells).
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let pl011_hash = compatible_hash(b"arm,pl011");
        let uart = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(pl011_hash))
            .expect("QEMU virt has PL011 UARTs");
        assert!(uart.clock_count >= 1,
                "PL011 should resolve at least one clock reference");
        // apb-pclk has #clock-cells = 0, so clock_id is 0.
        assert_eq!(uart.clocks[0].clock_id, 0);
        assert_ne!(uart.clocks[0].controller_phandle, 0,
                   "apb-pclk's phandle is non-zero");
    }

    #[test]
    fn resolve_clocks_skips_unknown_phandle() {
        // Synthetic check: a clocks property whose first phandle is
        // not in the providers table must stop processing (we can't
        // know how many cells to skip). The device's clock_count
        // should remain 0.
        let mut device = DeviceInfo {
            compat_hashes: [0; MAX_COMPAT],
            compat_count: 0,
            mmio_addr: 0,
            mmio_size: 0,
            intid: 0,
            claimed: false,
            claim_token: 0,
            clocks: [ClockRef { controller_phandle: 0, clock_id: 0 }; MAX_CLOCK_REFS],
            clock_count: 0,
            phandle: 0,
        };
        let bytes: [u8; 8] = [
            0, 0, 0, 0xCC,  // phandle 0xCC (not in providers)
            0, 0, 0, 0x07,  // would-be clock_id
        ];
        let providers: [ClockProvider; 1] = [
            ClockProvider { phandle: 0xAA, clock_cells: 1 },
        ];
        resolve_clocks_for(&mut device, &bytes, 0, bytes.len(), &providers);
        assert_eq!(device.clock_count, 0,
                   "unknown phandle yields no resolved entries");
    }

    #[test]
    fn resolve_clocks_decodes_one_ref() {
        let mut device = DeviceInfo {
            compat_hashes: [0; MAX_COMPAT],
            compat_count: 0,
            mmio_addr: 0,
            mmio_size: 0,
            intid: 0,
            claimed: false,
            claim_token: 0,
            clocks: [ClockRef { controller_phandle: 0, clock_id: 0 }; MAX_CLOCK_REFS],
            clock_count: 0,
            phandle: 0,
        };
        let bytes: [u8; 8] = [
            0, 0, 0, 0xAA,  // phandle 0xAA (in providers, #clock-cells = 1)
            0, 0, 0, 0x33,  // clock_id 0x33
        ];
        let providers: [ClockProvider; 1] = [
            ClockProvider { phandle: 0xAA, clock_cells: 1 },
        ];
        resolve_clocks_for(&mut device, &bytes, 0, bytes.len(), &providers);
        assert_eq!(device.clock_count, 1);
        assert_eq!(device.clocks[0].controller_phandle, 0xAA);
        assert_eq!(device.clocks[0].clock_id, 0x33);
    }

    #[test]
    fn resolve_clocks_decodes_multiple_refs() {
        let mut device = DeviceInfo {
            compat_hashes: [0; MAX_COMPAT],
            compat_count: 0,
            mmio_addr: 0,
            mmio_size: 0,
            intid: 0,
            claimed: false,
            claim_token: 0,
            clocks: [ClockRef { controller_phandle: 0, clock_id: 0 }; MAX_CLOCK_REFS],
            clock_count: 0,
            phandle: 0,
        };
        let bytes: [u8; 16] = [
            0, 0, 0, 0xAA,  // phandle AA (1 cell)
            0, 0, 0, 0x10,  // clock_id 0x10
            0, 0, 0, 0xBB,  // phandle BB (2 cells)
            0, 0, 0, 0x20,  // clock_id 0x20
            // Note: BB has 2 cells per ref, but only 1 cell of data
            // remains; we'd stop early. Adjust if needed.
        ];
        let providers: [ClockProvider; 2] = [
            ClockProvider { phandle: 0xAA, clock_cells: 1 },
            ClockProvider { phandle: 0xBB, clock_cells: 1 }, // simplify
        ];
        resolve_clocks_for(&mut device, &bytes, 0, bytes.len(), &providers);
        assert_eq!(device.clock_count, 2);
        assert_eq!(device.clocks[0].controller_phandle, 0xAA);
        assert_eq!(device.clocks[0].clock_id, 0x10);
        assert_eq!(device.clocks[1].controller_phandle, 0xBB);
        assert_eq!(device.clocks[1].clock_id, 0x20);
    }

    #[test]
    fn bad_magic() {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&[0xBA, 0xAD, 0xF0, 0x0D]);
        match parse_fdt(&data) {
            Err(FdtError::BadMagic) => {}
            _ => panic!("expected BadMagic"),
        }
    }

    // --- scan_platform tests ---

    #[test]
    fn scan_finds_pl011() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        assert_eq!(hw.pl011_base, 0x0900_0000, "PL011 UART0 base");
    }

    #[test]
    fn scan_finds_gicv3() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        assert_ne!(hw.gicd_base, 0, "GICD base should be nonzero");
        assert!(!hw.gic_v2, "QEMU virt DTB should be GICv3");
    }

    #[test]
    fn scan_finds_gic_secondary() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        assert_ne!(hw.gic_secondary_base, 0,
            "GICv3 redistributor base should be nonzero");
    }

    #[test]
    fn scan_finds_memory() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        assert_eq!(hw.ram_base, 0x4000_0000, "RAM base");
        assert!(hw.ram_size > 0, "RAM size should be nonzero");
    }

    #[test]
    fn scan_bad_magic() {
        let bad = [0u8; 64];
        assert!(scan_platform(&bad).is_err());
    }

    #[test]
    fn scan_too_small() {
        let tiny = [0u8; 10];
        assert!(scan_platform(&tiny).is_err());
    }

    // --- truncation detection ---

    #[test]
    fn truncated_before_strings_returns_invalid_offsets() {
        // Cut the DTB so the strings block offset points past the end.
        // validate_header catches this before the walk even starts.
        let (off_dt_struct, _) = validate_header(QEMU_DTB).unwrap();
        let truncated = &QEMU_DTB[..off_dt_struct + 200];
        match parse_fdt(truncated) {
            Err(FdtError::InvalidOffsets) => {}
            _ => panic!("expected InvalidOffsets"),
        }
        match scan_platform(truncated) {
            Err(FdtError::InvalidOffsets) => {}
            _ => panic!("expected InvalidOffsets"),
        }
    }

    #[test]
    fn unmatched_end_node_returns_truncated() {
        // Construct a DTB whose structure block starts with FDT_END_NODE
        // (depth 0 — no matching BEGIN_NODE). Should be rejected.
        let mut dtb = [0u8; 80];
        dtb[0..4].copy_from_slice(&0xd00dfeedu32.to_be_bytes());
        dtb[4..8].copy_from_slice(&80u32.to_be_bytes());
        dtb[8..12].copy_from_slice(&48u32.to_be_bytes());  // off_dt_struct
        dtb[12..16].copy_from_slice(&44u32.to_be_bytes()); // off_dt_strings
        dtb[32..36].copy_from_slice(&4u32.to_be_bytes());  // size_dt_strings
        // Structure block: bare FDT_END_NODE at depth 0
        dtb[48..52].copy_from_slice(&FDT_END_NODE.to_be_bytes());
        match parse_fdt(&dtb) {
            Err(FdtError::Truncated) => {}
            _ => panic!("expected Truncated for unmatched END_NODE"),
        }
        match scan_platform(&dtb) {
            Err(FdtError::Truncated) => {}
            _ => panic!("expected Truncated for unmatched END_NODE"),
        }
    }

    #[test]
    fn truncated_during_walk_returns_truncated() {
        // Construct a minimal DTB with valid header offsets but a
        // structure block that ends mid-node (no FDT_END reachable).
        let mut dtb = [0u8; 128];
        // Header: magic
        dtb[0..4].copy_from_slice(&0xd00dfeedu32.to_be_bytes());
        // totalsize (not checked by our parser)
        dtb[4..8].copy_from_slice(&128u32.to_be_bytes());
        // off_dt_struct = 48 (just past the 40-byte header + padding)
        dtb[8..12].copy_from_slice(&48u32.to_be_bytes());
        // off_dt_strings = 44 (4 bytes of empty strings block before struct)
        dtb[12..16].copy_from_slice(&44u32.to_be_bytes());
        // size_dt_strings = 4
        dtb[32..36].copy_from_slice(&4u32.to_be_bytes());

        // Structure block at offset 48:
        // FDT_BEGIN_NODE + node name "test\0" + padding
        let s = 48;
        dtb[s..s+4].copy_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        dtb[s+4] = b't'; dtb[s+5] = b'e'; dtb[s+6] = b's'; dtb[s+7] = b't';
        dtb[s+8] = 0; // null terminator
        // align to 4: next pos = s+12

        // FDT_PROP with prop_len that overflows past end of dtb
        dtb[s+12..s+16].copy_from_slice(&FDT_PROP.to_be_bytes());
        dtb[s+16..s+20].copy_from_slice(&9999u32.to_be_bytes()); // prop_len = 9999 (way past end)
        dtb[s+20..s+24].copy_from_slice(&0u32.to_be_bytes()); // name_off

        // No FDT_END — the walker should hit the overflowing prop_len
        // and return Truncated.
        match parse_fdt(&dtb) {
            Err(FdtError::Truncated) => {}
            _ => panic!("expected Truncated from parse_fdt"),
        }
        match scan_platform(&dtb) {
            Err(FdtError::Truncated) => {}
            _ => panic!("expected Truncated from scan_platform"),
        }
    }

    // --- consistency: both parsers agree ---

    #[test]
    fn scan_and_parse_agree_on_pl011() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let uart0 = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(PL011_HASH));
        assert_eq!(hw.pl011_base, uart0.unwrap().mmio_addr);
    }

    #[test]
    fn scan_and_parse_agree_on_gic() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        let devs = parse_fdt(QEMU_DTB).unwrap();
        let gic_hash = compatible_hash(b"arm,gic-v3");
        let gic = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(gic_hash));
        assert_eq!(hw.gicd_base, gic.unwrap().mmio_addr);
    }

    // --- Pi 4B DTB tests (address translation + multi-compatible) ---

    static PI4B_DTB: &[u8] = include_bytes!("../test-data/pi4b.dtb");

    #[test]
    fn pi4b_header_valid() {
        assert!(validate_header(PI4B_DTB).is_ok());
    }

    #[test]
    fn pi4b_scan_finds_pl011_translated() {
        // Pi 4B UART: compatible = "arm,pl011-axi", "arm,pl011", ...
        // DTB reg = 0x7e201000 (bus address), ranges translate to 0xfe201000
        let hw = scan_platform(PI4B_DTB).unwrap();
        assert_eq!(hw.pl011_base, 0xfe20_1000,
            "UART should be translated from bus 0x7e201000 to physical 0xfe201000");
    }

    #[test]
    fn pi4b_scan_finds_gic400() {
        // Pi 4B GIC-400: compatible = "arm,gic-400"
        // DTB reg = 0x40041000 (bus), ranges translate to 0xff841000
        let hw = scan_platform(PI4B_DTB).unwrap();
        assert!(hw.gic_v2, "Pi 4B should have GICv2 (gic-400)");
        assert_eq!(hw.gicd_base, 0xff84_1000,
            "GICD should be translated from bus 0x40041000 to physical 0xff841000");
        assert_eq!(hw.gic_secondary_base, 0xff84_2000,
            "GIC CPU interface should be at 0xff842000");
    }

    #[test]
    fn pi4b_multi_compat_matches() {
        // Pi 4B UART has compatible = "arm,pl011-axi", "arm,pl011".
        // parse_fdt now stores all compat hashes. Both should match.
        let devs = parse_fdt(PI4B_DTB).unwrap();
        let uart = devs.devices[..devs.count]
            .iter()
            .find(|d| d.has_compat(PL011_HASH) && d.mmio_addr == 0xfe20_1000);
        assert!(uart.is_some(),
            "parse_fdt should find Pi UART via arm,pl011 (second compat string)");
        let pl011_axi_hash = compatible_hash(b"arm,pl011-axi");
        assert!(uart.unwrap().has_compat(pl011_axi_hash),
            "Pi UART should also match arm,pl011-axi (first compat string)");
    }

    #[test]
    fn pi4b_parse_succeeds() {
        let devs = parse_fdt(PI4B_DTB).unwrap();
        assert!(devs.count > 0, "Pi 4B DTB should have devices");
    }

    // -----------------------------------------------------------------------
    // SMP boot method detection
    // -----------------------------------------------------------------------

    #[test]
    fn qemu_smp_detects_psci_hvc() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        assert_eq!(hw.smp_method, SmpMethod::Psci { hvc: true });
    }

    #[test]
    fn qemu_smp_finds_four_cpus() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        assert_eq!(hw.cpu_count, 4);
        // MPIDR values 0..3 from cpu reg property
        for i in 0..4 {
            assert_eq!(hw.cpus[i].mpidr, i as u64);
        }
    }

    #[test]
    fn pi4b_smp_detects_spin_table() {
        let hw = scan_platform(PI4B_DTB).unwrap();
        assert_eq!(hw.smp_method, SmpMethod::SpinTable);
    }

    #[test]
    fn pi4b_smp_finds_four_cpus_with_release_addrs() {
        let hw = scan_platform(PI4B_DTB).unwrap();
        assert_eq!(hw.cpu_count, 4);
        // MPIDR values 0..3
        for i in 0..4 {
            assert_eq!(hw.cpus[i].mpidr, i as u64);
        }
        // cpu-release-addr: 0xd8, 0xe0, 0xe8, 0xf0
        assert_eq!(hw.cpus[0].release_addr, 0xd8);
        assert_eq!(hw.cpus[1].release_addr, 0xe0);
        assert_eq!(hw.cpus[2].release_addr, 0xe8);
        assert_eq!(hw.cpus[3].release_addr, 0xf0);
    }

    #[test]
    fn qemu_cpus_have_no_release_addr() {
        let hw = scan_platform(QEMU_DTB).unwrap();
        for i in 0..4 {
            assert_eq!(hw.cpus[i].release_addr, 0);
        }
    }
}
