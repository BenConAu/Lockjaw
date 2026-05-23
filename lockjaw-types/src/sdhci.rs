/// SDHCI v3 controller types — register offsets, capability decode,
/// and SD command opcodes.
///
/// All pure types — no MMIO access, no volatile, no barriers.
/// Host-testable layout / decode logic.
///
/// Wire DTOs (`Adma2Descriptor`) live in `crate::wire::sdhci`,
/// generated from `user/wirespecs/sdhci.toml` by `cargo xtask
/// gen-wires`. This module re-exports them so consumers can use
/// `lockjaw_types::sdhci::Adma2Descriptor` directly; the
/// `ADMA2_ATTR_*` bit constants below compose into the typed
/// `attr` field at construction.
///
/// References:
///   - SD Host Controller Standard Specification v4.20 (relevant
///     parts, since the BCM2711 emmc2 controller is SDHCI v3)
///   - Linux `drivers/mmc/host/sdhci.h` (cross-reference for
///     register offsets and CAPABILITIES bit layout)
///   - SD Physical Layer Specification v6.00 (command set)

pub use crate::wire::sdhci::*;

// ---------------------------------------------------------------------------
// SDHCI register offsets (Standard Spec § 2.1.1)
// ---------------------------------------------------------------------------
//
// All offsets are byte offsets from the controller's MMIO base.
// Names match the SD Host Controller spec / Linux kernel headers
// for cross-reference.

/// `SDMA_SYS_ADDR_REG` (0x000) — also serves as the 32-bit ADMA
/// system address argument register. Read here only as part of
/// dump-the-state diagnostics.
pub const SDHCI_SYSADDR: u64 = 0x000;
/// `BLOCK_SIZE_REG` (0x004) — bits[11:0] = block size in bytes.
pub const SDHCI_BLOCK_SIZE: u64 = 0x004;
/// `BLOCK_COUNT_REG` (0x006) — number of blocks to transfer.
pub const SDHCI_BLOCK_COUNT: u64 = 0x006;
/// `ARGUMENT_REG` (0x008) — command argument.
pub const SDHCI_ARGUMENT: u64 = 0x008;
/// `TRANSFER_MODE_REG` (0x00c) + `COMMAND_REG` (0x00e) — combined
/// 32-bit write triggers command issue.
pub const SDHCI_TRANSFER_MODE: u64 = 0x00c;
pub const SDHCI_COMMAND: u64 = 0x00e;
/// `RESPONSE_REG` (0x010..0x01f) — 4 × 32-bit response slots.
pub const SDHCI_RESPONSE_0: u64 = 0x010;
pub const SDHCI_RESPONSE_1: u64 = 0x014;
pub const SDHCI_RESPONSE_2: u64 = 0x018;
pub const SDHCI_RESPONSE_3: u64 = 0x01c;
/// `BUFFER_DATA_PORT_REG` (0x020) — PIO data window.
pub const SDHCI_BUFFER_DATA_PORT: u64 = 0x020;
/// `PRESENT_STATE_REG` (0x024) — controller status snapshot.
pub const SDHCI_PRESENT_STATE: u64 = 0x024;
/// `HOST_CONTROL_1` (0x028).
pub const SDHCI_HOST_CONTROL: u64 = 0x028;
/// `POWER_CONTROL_REG` (0x029).
pub const SDHCI_POWER_CONTROL: u64 = 0x029;
/// `BLOCK_GAP_CONTROL_REG` (0x02a).
pub const SDHCI_BLOCK_GAP_CONTROL: u64 = 0x02a;
/// `WAKEUP_CONTROL_REG` (0x02b).
pub const SDHCI_WAKEUP_CONTROL: u64 = 0x02b;
/// `CLOCK_CONTROL_REG` (0x02c).
pub const SDHCI_CLOCK_CONTROL: u64 = 0x02c;
/// `TIMEOUT_CONTROL_REG` (0x02e).
pub const SDHCI_TIMEOUT_CONTROL: u64 = 0x02e;
/// `SOFTWARE_RESET_REG` (0x02f) — write `SW_RST_*` bits.
pub const SDHCI_SOFTWARE_RESET: u64 = 0x02f;
/// `NORMAL_INT_STATUS` (0x030).
pub const SDHCI_NORMAL_INT_STATUS: u64 = 0x030;
/// `ERROR_INT_STATUS` (0x032).
pub const SDHCI_ERROR_INT_STATUS: u64 = 0x032;
/// `NORMAL_INT_STATUS_ENABLE` (0x034).
pub const SDHCI_NORMAL_INT_STATUS_ENABLE: u64 = 0x034;
/// `ERROR_INT_STATUS_ENABLE` (0x036).
pub const SDHCI_ERROR_INT_STATUS_ENABLE: u64 = 0x036;
/// `NORMAL_INT_SIGNAL_ENABLE` (0x038).
pub const SDHCI_NORMAL_INT_SIGNAL_ENABLE: u64 = 0x038;
/// `ERROR_INT_SIGNAL_ENABLE` (0x03a).
pub const SDHCI_ERROR_INT_SIGNAL_ENABLE: u64 = 0x03a;
/// `CAPABILITIES` (0x040) — fixed device capabilities, low half.
pub const SDHCI_CAPABILITIES: u64 = 0x040;
/// `CAPABILITIES_HI` (0x044) — capabilities high half.
pub const SDHCI_CAPABILITIES_HI: u64 = 0x044;
/// `SLOT_INT_STATUS_VERSION` (0x0fc) — slot status (low 16) +
/// host controller version (high 16).
pub const SDHCI_HOST_VERSION: u64 = 0x0fe;

// ---------------------------------------------------------------------------
// HOST_CONTROL_1 bits (offset 0x028)
// ---------------------------------------------------------------------------

/// `DAT_XFER_WIDTH` (bit 1): 0 = 1-bit DAT line (default after
/// reset), 1 = 4-bit DAT lines. Mirror this on the host *after*
/// successfully telling the card via ACMD6.
pub const SDHCI_HOST_CTRL_DAT_4BIT: u8 = 0x02;

// ---------------------------------------------------------------------------
// CLOCK_CONTROL bits (offset 0x02c)
// ---------------------------------------------------------------------------

/// Internal Clock Enable — start the host oscillator.
pub const SDHCI_CLOCK_INT_EN: u16 = 0x0001;
/// Internal Clock Stable — set by hardware when the oscillator is stable.
pub const SDHCI_CLOCK_INT_STABLE: u16 = 0x0002;
/// SD Clock Enable — gates the oscillator through to the card slot.
pub const SDHCI_CLOCK_CARD_EN: u16 = 0x0004;

// ---------------------------------------------------------------------------
// POWER_CONTROL bits (offset 0x029)
// ---------------------------------------------------------------------------

/// Bus Power On. OR this with a voltage-select constant to enable the bus.
pub const SDHCI_POWER_ON: u8 = 0x01;
/// Bus Voltage Select = 3.3 V. bits[3:1] = 0b111 = 7, so 0x0E.
pub const SDHCI_POWER_330: u8 = 0x0E;

// ---------------------------------------------------------------------------
// NORMAL_INT_STATUS bits (offset 0x030, write-1-to-clear)
// ---------------------------------------------------------------------------

/// Command Complete (bit 0). Set when the CMD/response phase finishes.
pub const SDHCI_INT_CMD_COMPLETE: u16 = 0x0001;
/// Transfer Complete (bit 1). Set when the data transfer phase finishes.
pub const SDHCI_INT_DATA_COMPLETE: u16 = 0x0002;
/// Buffer Write Ready (bit 4). Set when controller is ready to accept PIO write data.
pub const SDHCI_INT_BUF_WR_READY: u16 = 0x0010;
/// Buffer Read Ready (bit 5). Set when PIO data is available in BUFFER_DATA_PORT.
pub const SDHCI_INT_BUF_RD_READY: u16 = 0x0020;
/// Error Interrupt (bit 15). Summary bit — consult ERROR_INT_STATUS for
/// which error fired.
pub const SDHCI_INT_ERROR: u16 = 0x8000;

// ---------------------------------------------------------------------------
// ERROR_INT_STATUS bits (offset 0x032, write-1-to-clear)
// ---------------------------------------------------------------------------

/// Command Timeout (bit 0). Card did not respond within the timeout window.
pub const SDHCI_INT_CMD_TIMEOUT: u16 = 0x0001;
/// Command CRC Error (bit 1). CRC mismatch detected in the response.
pub const SDHCI_INT_CMD_CRC: u16 = 0x0002;
/// Command End Bit Error (bit 2). End bit of the response was wrong.
pub const SDHCI_INT_CMD_END_BIT: u16 = 0x0004;
/// Command Index Error (bit 3). Response's command-index field
/// didn't match the issued command (only checked when CMD_INDEX
/// flag is set in the COMMAND register).
pub const SDHCI_INT_CMD_INDEX: u16 = 0x0008;
/// Data Timeout Error (bit 4). Card did not provide data within the timeout window.
pub const SDHCI_INT_DATA_TIMEOUT: u16 = 0x0010;
/// Data CRC Error (bit 5). CRC mismatch on received data.
pub const SDHCI_INT_DATA_CRC: u16 = 0x0020;
/// Data End Bit Error (bit 6). End bit of received data was wrong.
pub const SDHCI_INT_DATA_END_BIT: u16 = 0x0040;

// ---------------------------------------------------------------------------
// PRESENT_STATE bits (offset 0x024, read-only)
// ---------------------------------------------------------------------------

/// CMD Line Inhibit (bit 0). Set while a command transfer is in progress.
pub const SDHCI_CMD_INHIBIT: u32 = 0x0000_0001;
/// DAT Line Inhibit (bit 1). Set while a data transfer is in progress or
/// the card is busy on DAT0 (R1b). Must be clear before issuing commands
/// that use the DAT lines (R1b responses, data transfers).
pub const SDHCI_DAT_INHIBIT: u32 = 0x0000_0002;

// ---------------------------------------------------------------------------
// TRANSFER_MODE bits (offset 0x00c, u16)
// ---------------------------------------------------------------------------
//
// Written before the COMMAND register write that triggers the transfer.
// For single-block PIO reads: SDHCI_TRNS_READ only (no DMA, no multi-block).

/// DMA Enable (bit 0). Clear for PIO; set for SDMA/ADMA2.
pub const SDHCI_TRNS_DMA: u16 = 0x0001;
/// Block Count Enable (bit 1). Set for multi-block; clear for single-block.
pub const SDHCI_TRNS_BLK_CNT_EN: u16 = 0x0002;
/// Auto-CMD12 Enable (bits[3:2] = 01). Controller auto-issues CMD12 after data.
pub const SDHCI_TRNS_AUTO_CMD12: u16 = 0x0004;
/// Auto-CMD23 Enable (bits[3:2] = 10). Controller auto-issues CMD23 (SET_BLOCK_COUNT)
/// before the data command, using the value at SDHCI_ARGUMENT2 (offset 0x000) as the
/// CMD23 argument. Modern SDHCI v3+ feature; CAPABILITIES bit 30 advertises support.
pub const SDHCI_TRNS_AUTO_CMD23: u16 = 0x0008;
/// Data Transfer Direction (bit 4): 1 = read (card→host), 0 = write (host→card).
pub const SDHCI_TRNS_READ: u16 = 0x0010;
/// Multi Block Select (bit 5): 1 = multi-block, 0 = single block.
pub const SDHCI_TRNS_MULTI: u16 = 0x0020;

/// `ARGUMENT2_REG` (0x000) — argument register for Auto-CMD23. Aliases the
/// `SDMA_SYS_ADDR_REG` location; the register's role is determined by which
/// transfer mode is active (PIO + Auto-CMD23 here, SDMA in M6+).
pub const SDHCI_ARGUMENT2: u64 = 0x000;

// ---------------------------------------------------------------------------
// HOST_CONTROL_1 DMA_SEL (bits[4:3], inside the existing SDHCI_HOST_CONTROL
// register at offset 0x028)
// ---------------------------------------------------------------------------

/// DMA_SEL field mask. Two bits at [4:3] in HOST_CONTROL_1 selecting the
/// DMA engine the controller drives when TRANSFER_MODE.DMA_EN is set:
///   00 = SDMA          (single-buffer, legacy)
///   01 = reserved
///   10 = ADMA2 32-bit  (descriptor-table-driven, 32-bit phys)
///   11 = ADMA2 64-bit  (descriptor-table-driven, 64-bit phys)
/// Lockjaw uses ADMA2 32-bit (M6 sub-commit 2b); 64-bit ADMA2 is reserved
/// for the day Lockjaw runs on a >4 GiB PA system.
pub const SDHCI_HOST_CTRL_DMA_SEL_MASK: u8 = 0b0001_1000;
/// DMA_SEL = ADMA2 32-bit. OR into HOST_CONTROL_1.
pub const SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32: u8 = 0b0001_0000;

// ---------------------------------------------------------------------------
// ADMA2 descriptor-table address register
// ---------------------------------------------------------------------------

/// `ADMA_SYS_ADDR_REG` (0x058) — physical address of the ADMA2 descriptor
/// table. 32-bit register; for 64-bit ADMA2 a paired register at 0x05C
/// holds the high 32 bits (unused in this Lockjaw revision). DIFFERENT
/// register from SDHCI_SYSADDR (0x000), which is the SDMA source/dest
/// address (also aliased as ARGUMENT2 for Auto-CMD23 in M5).
pub const SDHCI_ADMA_ADDRESS: u64 = 0x058;

// ---------------------------------------------------------------------------
// ADMA2 descriptor — 32-bit address mode, per SDHCI v3 §1.13
// ---------------------------------------------------------------------------
//
// Each descriptor is exactly 8 bytes:
//   bits [15: 0] = Attributes
//     bit  0  VALID — 1 = descriptor is valid (always set for live entries)
//     bit  1  END   — 1 = last descriptor in the table (controller stops)
//     bit  2  INT   — 1 = raise ADMA-end interrupt when this entry finishes
//     bits[5:4] ACT — 00 = nop, 10 = tran (data transfer), 11 = link (chain)
//   bits [31:16] = Length in bytes (0 = 65536 per spec; we cap at 65535)
//   bits [63:32] = Data Address (must be 4-byte aligned)
//
// Wire layout in memory (little-endian u64):
//   `(addr << 32) | (length << 16) | attrs`
//
// Descriptor table itself must be 8-byte aligned (a page-aligned alloc
// from the DMA pool satisfies this trivially).

/// Descriptor is live. Set on every entry the controller should process.
pub const ADMA2_ATTR_VALID: u16 = 1 << 0;
/// Last descriptor in the chain. Controller raises ADMA-end and stops.
pub const ADMA2_ATTR_END: u16   = 1 << 1;
/// Generate interrupt when the controller finishes this descriptor.
/// Unused in M6 sub-commit 2b (poll-based wait).
pub const ADMA2_ATTR_INT: u16   = 1 << 2;
/// ACT field value: transfer (`tran`). Single-descriptor data move.
pub const ADMA2_ATTR_ACT_TRAN: u16 = 0b10 << 4;
/// ACT field value: link to next descriptor table. Future multi-segment
/// transfers; M6 sub-commit 2b uses one tran+end descriptor.
pub const ADMA2_ATTR_ACT_LINK: u16 = 0b11 << 4;

/// Build the 8-byte (one-`u64` LE) wire encoding for a single ADMA2 32-bit
/// `tran+end` descriptor. Pure; no MMIO.
///
/// `buf_phys`: physical address of the data buffer. Must be 4-byte aligned
/// and fit in 32 bits (asserted at debug; release silently truncates upper
/// bits — caller's responsibility).
///
/// `length`: transfer size in bytes. Max 65535 per descriptor; the SDHCI
/// spec quirk where length=0 means 65536 is not exercised here.
pub const fn adma2_tran_end_descriptor(buf_phys: u32, length: u16) -> u64 {
    let attrs: u16 = ADMA2_ATTR_VALID | ADMA2_ATTR_END | ADMA2_ATTR_ACT_TRAN;
    ((buf_phys as u64) << 32) | ((length as u64) << 16) | (attrs as u64)
}

// ---------------------------------------------------------------------------
// Adma2Descriptor convenience constructors.
//
// The generated `Adma2Descriptor::new(attr, length, address)` is the
// raw constructor. SDHCI v3 §1.13.4 defines exactly four descriptor
// shapes — NOP, TRAN, TRAN+END, LINK — and the bit combinations are
// load-bearing: forgetting VALID makes the controller silently skip
// the entry; ORing both ACT_TRAN and ACT_LINK is undefined behavior;
// omitting END on the final TRAN of a chain hangs the controller
// waiting for a next descriptor that never arrives. The named
// constructors below produce exactly the four legal shapes, so a
// driver author cannot construct an illegal combination by hand-
// ORing `ADMA2_ATTR_*` constants into a single u16 argument.
//
// INT is orthogonal to the action — `with_int` is the one chainable
// modifier, mirroring how driver IRQ-vs-poll choice is independent
// of descriptor shape.
// ---------------------------------------------------------------------------

impl Adma2Descriptor {
    /// VALID | ACT_TRAN — mid-chain data transfer. Use `tran_end`
    /// for the last descriptor in a chain (sets END too); a
    /// chain that ends on `tran` without END hangs the controller.
    ///
    /// Argument order is (address, length) — matches the consumer
    /// mental model "buffer at X, Y bytes long" and the legacy
    /// `adma2_tran_end_descriptor(buf_phys, length)` helper, NOT
    /// the raw `Adma2Descriptor::new(attr, length, address)` wire-
    /// field order. The wire constructor follows the byte layout
    /// (codegen-determined); the named constructors are hand-written
    /// and follow caller ergonomics.
    pub fn tran(address: u32, length: u16) -> Self {
        Self::new(ADMA2_ATTR_VALID | ADMA2_ATTR_ACT_TRAN, length, address)
    }

    /// VALID | END | ACT_TRAN — final data transfer in a chain.
    /// Single-descriptor transfers use this exclusively (no preceding
    /// `tran` entries). Argument order (address, length) — see `tran`.
    pub fn tran_end(address: u32, length: u16) -> Self {
        Self::new(
            ADMA2_ATTR_VALID | ADMA2_ATTR_END | ADMA2_ATTR_ACT_TRAN,
            length,
            address,
        )
    }

    /// VALID | ACT_LINK — chain to another descriptor table at
    /// `address`. Length is don't-care for LINK per spec; emitted
    /// as 0 by convention.
    pub fn link(address: u32) -> Self {
        Self::new(ADMA2_ATTR_VALID | ADMA2_ATTR_ACT_LINK, 0, address)
    }

    /// VALID alone — no-op descriptor (ACT field zero). Controller
    /// advances past it without transferring data. Rarely needed
    /// outside descriptor-table alignment padding.
    pub fn nop() -> Self {
        Self::new(ADMA2_ATTR_VALID, 0, 0)
    }

    /// Set the INT bit so the controller raises the descriptor-done
    /// interrupt on completion of this entry. Orthogonal to the
    /// action; chainable onto any of `tran` / `tran_end` / `link` /
    /// `nop`. Emmc2 polls today (no driver wires this yet); kept
    /// available for IRQ-driven transfers.
    pub fn with_int(self) -> Self {
        let attr = self.attr() | ADMA2_ATTR_INT;
        Self::new(attr, self.length(), self.address())
    }
}

// ---------------------------------------------------------------------------
// COMMAND_REG flag constants (offset 0x00e, low byte)
// ---------------------------------------------------------------------------
//
// Assemble with `sd_command_word(index, flags)`. Bits[13:8] = command
// index, bits[7:0] = flags. See SDHCI Spec § 2.2.5.

/// CRC check enable for the response field.
pub const SDHCI_CMD_CRC: u8 = 0x08;
/// Index check enable for the response field.
pub const SDHCI_CMD_INDEX: u8 = 0x10;
/// Data Present Select (bit 5). Set for commands that transfer data via the
/// DAT lines (CMD17, CMD18, CMD24, CMD25). The TRANSFER_MODE register must
/// be programmed before the COMMAND write that triggers the transfer.
pub const SDHCI_CMD_DATA: u8 = 0x20;
/// No response type (CMD0, broadcast commands with no reply).
pub const SDHCI_CMD_RESP_NONE: u8 = 0x00;
/// Long response — 136-bit (R2: CID, CSD).
/// Response payload spans RESPONSE_3..RESPONSE_0.
pub const SDHCI_CMD_RESP_LONG: u8 = 0x01;
/// Short response — 48-bit (R1, R3, R4, R5, R6, R7).
pub const SDHCI_CMD_RESP_SHORT: u8 = 0x02;
/// Short response with busy on DAT0 (R1b, R5b — e.g. CMD7
/// SELECT_CARD). The controller delays CMD_COMPLETE until the
/// card releases DAT0.
pub const SDHCI_CMD_RESP_SHORT_BUSY: u8 = 0x03;

// ---------------------------------------------------------------------------
// SOFTWARE_RESET bits (offset 0x02f)
// ---------------------------------------------------------------------------

/// `SW_RST_DAT` — reset data circuit only.
pub const SW_RST_DATA: u8 = 1 << 2;
/// `SW_RST_CMD` — reset command circuit only.
pub const SW_RST_CMD: u8 = 1 << 1;
/// `SW_RST_ALL` — full controller reset. Spec recommends polling
/// until this bit clears (max ~100 ms in practice).
pub const SW_RST_ALL: u8 = 1 << 0;

// ---------------------------------------------------------------------------
// HOST_VERSION (offset 0x0fe, high half of 0x0fc reg)
// ---------------------------------------------------------------------------

/// SDHCI specification version. Encoded in the low byte of
/// HOST_VERSION. Historical: 0 = v1.0, 1 = v2.0, 2 = v3.0,
/// 3 = v4.0, ... — Lockjaw expects v3 (BCM2711 emmc2).
pub const SDHCI_SPEC_300: u8 = 2;

// ---------------------------------------------------------------------------
// CAPABILITIES decode (offsets 0x040 / 0x044, treated as one 64-bit
// value with the LSB at 0x040)
// ---------------------------------------------------------------------------
//
// Per SD Host Controller Standard Spec, Table 2-9 (CAPABILITIES
// register fields). Layout differs by spec version; the fields
// below are SDHCI v3 (which is what BCM2711 emmc2 advertises).

/// Decoded view of the 64-bit CAPABILITIES register pair.
/// All numerically-meaningful fields are decoded; raw 64-bit
/// `bits` is preserved for dumping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Raw 64-bit view (CAPABILITIES_HI << 32 | CAPABILITIES).
    pub bits: u64,
    /// Base SD clock frequency in MHz, decoded from bits[14:8].
    /// Zero means "indicated by capability not present" — driver
    /// must source the value from elsewhere (the clock controller,
    /// for Lockjaw).
    pub base_clock_mhz: u8,
    /// Maximum block length in bytes (512, 1024, 2048, or
    /// reserved). Decoded from bits[17:16].
    pub max_block_len_bytes: u16,
    /// True if the controller advertises ADMA2 support
    /// (bits[19] = 1).
    pub adma2_supported: bool,
    /// True if 64-bit system addressing (bits[28] in the v4
    /// layout / bits[63] in v3+ embedded scheme) is supported.
    /// Lockjaw uses 32-bit ADMA2 today; this is informational.
    pub addr_64_v3: bool,
    /// True if voltage support 3.3 V (bits[24]) is advertised.
    pub voltage_3v3: bool,
    /// True if voltage support 3.0 V (bits[25]) is advertised.
    pub voltage_3v0: bool,
    /// True if voltage support 1.8 V (bits[26]) is advertised
    /// (only meaningful for embedded/UHS use; Pi 4B emmc2 is
    /// fixed-3.3 V to the microSD slot).
    pub voltage_1v8: bool,
}

impl Capabilities {
    /// Decode a CAPABILITIES register pair. `lo` is the value at
    /// offset 0x040 (CAPABILITIES), `hi` is at offset 0x044
    /// (CAPABILITIES_HI). The function does not enforce a spec
    /// version — fields decode per SDHCI v3/v4, which matches the
    /// BCM2711 emmc2 controller.
    pub const fn decode(lo: u32, hi: u32) -> Self {
        let bits = ((hi as u64) << 32) | (lo as u64);
        Self {
            bits,
            base_clock_mhz: ((lo >> 8) & 0xFF) as u8,
            max_block_len_bytes: match (lo >> 16) & 0x3 {
                0 => 512,
                1 => 1024,
                2 => 2048,
                _ => 0, // reserved
            },
            adma2_supported: (lo & (1 << 19)) != 0,
            addr_64_v3: (lo & (1 << 28)) != 0,
            voltage_3v3: (lo & (1 << 24)) != 0,
            voltage_3v0: (lo & (1 << 25)) != 0,
            voltage_1v8: (lo & (1 << 26)) != 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Clock divisor math (SDHCI v3 10-bit divisor mode)
// ---------------------------------------------------------------------------
//
// SDHCI v3 uses a 10-bit programmable divisor N where the SD clock output
// is: SD_CLK = base_clock / (2 × N).  N = 0 is pass-through (bypass).
//
// N is packed into CLOCK_CONTROL as:
//   bits[15:8] = lower 8 bits of N
//   bits[7:6]  = upper 2 bits of N  (SDHCI v3 only; v1/v2 ignore these)
//
// Example: 200 MHz base → 400 kHz target
//   N = 200_000_000 / (2 × 400_000) = 250 = 0xFA
//   lo = 0xFA  (CLOCK_CONTROL[15:8])
//   hi = 0x00  (CLOCK_CONTROL[7:6])
//
// The caller assembles CLOCK_CONTROL as:
//   (lo as u16) << 8 | (hi as u16) << 6 | enable_bits

/// Compute the `(lo, hi)` CLOCK_CONTROL divisor pair for SDHCI v3.
/// Produces the closest output ≤ `target_hz` from `base_hz`.
/// `lo` → CLOCK_CONTROL\[15:8\], `hi` (2-bit) → CLOCK_CONTROL\[7:6\].
pub const fn compute_clock_divisor(base_hz: u64, target_hz: u64) -> (u8, u8) {
    if target_hz == 0 || base_hz <= target_hz {
        return (0, 0); // N = 0 → bypass
    }
    // N = ceil(base_hz / (2 × target_hz)), clamped to [1, 1023].
    let double_target = 2 * target_hz;
    let n_raw = (base_hz + double_target - 1) / double_target;
    let n = if n_raw > 1023 { 1023 } else { n_raw };
    ((n & 0xFF) as u8, ((n >> 8) & 0x3) as u8)
}

// ---------------------------------------------------------------------------
// Command register word builder
// ---------------------------------------------------------------------------

/// Build the 16-bit value for `COMMAND_REG` (offset 0x00e).
///
/// `index` is the SD command number (0–63); `flags` is the OR of
/// `SDHCI_CMD_*` flag constants. bits\[13:8\] = index, bits\[7:0\] = flags.
pub const fn sd_command_word(index: u8, flags: u8) -> u16 {
    ((index as u16) << 8) | (flags as u16)
}

// ---------------------------------------------------------------------------
// SD command opcodes (SD Physical Layer Spec § 4.7.4)
// ---------------------------------------------------------------------------
//
// Numbered to match the spec; the SDHCI controller takes `cmd_index` in
// bits[13:8] of COMMAND_REG. Use `sd_command_word(cmd.index(), flags)` to
// build the register value.

/// Standard SD commands. Exhaustive match forces a compile error when
/// new commands are added without handling them in the dispatch site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SdCommand {
    /// CMD0 — GO_IDLE_STATE. No response. Resets card to idle.
    GoIdleState = 0,
    /// CMD2 — ALL_SEND_CID. R2 (long) response.
    AllSendCid = 2,
    /// CMD3 — SEND_RELATIVE_ADDR. R6 response (RCA).
    SendRelativeAddr = 3,
    /// CMD6 — SWITCH_FUNC (M3 uses the ACMD form: ACMD6 SET_BUS_WIDTH).
    /// Same opcode; the difference is the CMD55 prefix that turns
    /// it into an application-specific command.
    SetBusWidth = 6,
    /// CMD7 — SELECT/DESELECT_CARD. R1b response.
    SelectCard = 7,
    /// CMD8 — SEND_IF_COND. Required for SDv2+ identification.
    /// Argument: (VHS=1, check_pattern=0xAA). R7 response echoes
    /// the argument.
    SendIfCond = 8,
    /// CMD9 — SEND_CSD. R2 (long) response.
    SendCsd = 9,
    /// CMD12 — STOP_TRANSMISSION. R1b response. Used to terminate
    /// open-ended multi-block transfers (we use AUTO_CMD23
    /// instead, per the M5 plan).
    StopTransmission = 12,
    /// CMD17 — READ_SINGLE_BLOCK. R1 response.
    ReadSingleBlock = 17,
    /// CMD18 — READ_MULTIPLE_BLOCK. R1 response.
    ReadMultipleBlock = 18,
    /// CMD23 — SET_BLOCK_COUNT. R1 response. Used as a prefix to
    /// CMD18/CMD25 in Auto-CMD23 mode.
    SetBlockCount = 23,
    /// CMD24 — WRITE_BLOCK. R1 response.
    WriteBlock = 24,
    /// CMD25 — WRITE_MULTIPLE_BLOCK. R1 response. M7 BlockEngine
    /// writes use this with Auto-CMD23 (same shape as CMD18).
    WriteMultipleBlock = 25,
    /// ACMD41 — SD_SEND_OP_COND. R3 response (OCR). Sent after a
    /// CMD55 prefix. Argument carries the host-supported voltage
    /// window plus HCS=1 (Host Capacity Support — required to
    /// negotiate SDHC/SDXC). The card returns OCR with the busy
    /// bit cleared once it has finished its power-up sequence;
    /// driver loops on this until ready.
    SdSendOpCond = 41,
    /// CMD55 — APP_CMD. Prefixes any ACMD; R1 response.
    AppCmd = 55,
}

impl SdCommand {
    /// Numeric command index for OR'ing into COMMAND_REG bits[13:8].
    pub const fn index(self) -> u8 {
        self as u8
    }
}

// ---------------------------------------------------------------------------
// SD command arguments (named constants for common values)
// ---------------------------------------------------------------------------

/// CMD8 (SEND_IF_COND) argument: VHS = 1 (2.7–3.6 V) + check pattern 0xAA.
/// A card that echoes this argument in its R7 response is an SDv2+ card
/// capable of SDHC/SDXC negotiation.
pub const CMD8_IF_COND_ARG: u32 = 0x0000_01AA;

/// ACMD41 (SD_SEND_OP_COND) argument with HCS=1 (bit 30) and the full
/// 2.7–3.6 V voltage window (bits[23:15] all set). HCS=1 is required to
/// negotiate SDHC/SDXC block-addressing; without it the card always reports
/// CCS=0 regardless of its internal capacity.
pub const ACMD41_ARG_HCS: u32 = 0x40FF_8000;

// ---------------------------------------------------------------------------
// R3 response (OCR register) decode  — ACMD41
// ---------------------------------------------------------------------------
//
// ACMD41 returns an R3 response.  SDHCI stores the 32-bit OCR value directly
// in RESPONSE_0 (same short-response path as R1/R7).
//
// SD Physical Layer Spec § 5.1 (OCR Register).

/// Decoded view of the OCR register returned in an R3 (ACMD41) response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OcrRegister {
    /// Power-up Status Bit (OCR bit 31).  `false` while the card is still
    /// initializing (busy); `true` once it is ready to accept commands.
    /// ACMD41 must be retried until `power_up_done` is `true`.
    pub power_up_done: bool,
    /// Card Capacity Status (OCR bit 30).  Only meaningful after
    /// `power_up_done` is `true`.  `true` = SDHC/SDXC (block-addressed);
    /// `false` = SDSC (byte-addressed, legacy).  Requires HCS=1 in the
    /// ACMD41 argument to elicit an honest answer from the card.
    pub ccs: bool,
}

impl OcrRegister {
    /// Decode the 32-bit OCR value stored in RESPONSE_0 after ACMD41.
    pub const fn decode(response_0: u32) -> Self {
        Self {
            power_up_done: (response_0 & (1 << 31)) != 0,
            ccs: (response_0 & (1 << 30)) != 0,
        }
    }
}

// ---------------------------------------------------------------------------
// R6 response (CMD3 SEND_RELATIVE_ADDR) — RCA extraction
// ---------------------------------------------------------------------------
//
// CMD3 returns an R6 response.  The 32-bit payload in RESPONSE_0:
//   bits[31:16] — New Published RCA (Relative Card Address)
//   bits[15:0]  — card status bits  (ignored during normal ID-mode flow)
//
// SD Physical Layer Spec § 4.9.5.

/// Extract the Relative Card Address from a CMD3 R6 response.
/// `response_0` is read directly from SDHCI RESPONSE_0.
pub const fn r6_rca(response_0: u32) -> u16 {
    (response_0 >> 16) as u16
}

// ---------------------------------------------------------------------------
// R2 CSD-v2 decode — CMD9 SEND_CSD
// ---------------------------------------------------------------------------
//
// CMD9 returns a 136-bit R2 response.  SDHCI strips the CRC7 byte from the
// bottom of the R2 frame and stores the remaining 120 bits of the 128-bit
// CSD register across four 32-bit words, MSB in the HIGHEST-address register:
//
//   RESPONSE_3 (0x01c) = CSD[127:104]  (high bits; bits[31:24] = 0)
//   RESPONSE_2 (0x018) = CSD[103: 72]
//   RESPONSE_1 (0x014) = CSD[ 71: 40]
//   RESPONSE_0 (0x010) = CSD[ 39:  8]  (low bits; CSD[7:1] CRC stripped)
//
// CSD v2 (SDHC/SDXC, CSD_STRUCTURE = 0b01) capacity fields:
//   CSD[127:126] — CSD_STRUCTURE (must be 1)
//   CSD[ 69: 48] — C_SIZE (22-bit unsigned)
//   Capacity      = (C_SIZE + 1) × 512 KiB
//
// Field extraction (all single-register):
//   CSD_STRUCTURE = RESPONSE_3[23:22]
//   C_SIZE        = RESPONSE_1[29:8]    (22 bits spanning CSD[69:48])
//
// SD Physical Layer Spec § 5.3.3; SDHCI Spec § 2.2.8.
// Confirmed against Linux drivers/mmc/host/sdhci.c::sdhci_finish_command,
// which reconstructs resp[0] = (RESPONSE_3 << 8) | RESPONSE_2[7:0] (i.e.
// RESPONSE_3 is the high word with an 8-bit shift applied).

/// Capacity information decoded from a CSD v2 (SDHC/SDXC) register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsdV2 {
    /// Total card capacity in bytes.
    pub capacity_bytes: u64,
    /// Total card capacity in 512-byte blocks (`capacity_bytes / 512`).
    pub capacity_blocks: u64,
}

/// Returned when the CSD register is not CSD version 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotCsdV2 {
    /// The `CSD_STRUCTURE` value actually seen (1 = v2 expected).
    pub csd_structure: u8,
}

impl CsdV2 {
    /// Decode a CSD v2 register from four SDHCI RESPONSE words.
    ///
    /// Pass the words in register-address order (lowest address first):
    /// `r[0]` = RESPONSE_0 (0x010) = CSD[ 39:  8] (least significant, CRC stripped),
    /// `r[1]` = RESPONSE_1 (0x014) = CSD[ 71: 40],
    /// `r[2]` = RESPONSE_2 (0x018) = CSD[103: 72],
    /// `r[3]` = RESPONSE_3 (0x01c) = CSD[127:104] (most significant, bits[31:24]=0).
    ///
    /// Returns `Err(NotCsdV2)` if `CSD_STRUCTURE != 1`.
    pub const fn decode(r: [u32; 4]) -> Result<Self, NotCsdV2> {
        // CSD_STRUCTURE = CSD[127:126] → RESPONSE_3[23:22].
        let csd_structure = ((r[3] >> 22) & 0x3) as u8;
        if csd_structure != 1 {
            return Err(NotCsdV2 { csd_structure });
        }
        // C_SIZE = CSD[69:48] → RESPONSE_1[29:8] (22 bits, single register).
        let c_size = ((r[1] >> 8) & 0x3FFFFF) as u64;
        let capacity_bytes = (c_size + 1) * 512 * 1024;
        Ok(Self {
            capacity_bytes,
            capacity_blocks: capacity_bytes / 512,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Capabilities decode -----

    #[test]
    fn caps_decode_zero_is_safe() {
        let c = Capabilities::decode(0, 0);
        assert_eq!(c.bits, 0);
        assert_eq!(c.base_clock_mhz, 0);
        assert_eq!(c.max_block_len_bytes, 512);
        assert!(!c.adma2_supported);
    }

    #[test]
    fn caps_base_clock_decode() {
        // bits[14:8] of CAPABILITIES (lo) = base clock MHz.
        // 200 MHz = 0xC8 → lo = 0xC8 << 8 = 0xC800.
        let c = Capabilities::decode(0xC800, 0);
        assert_eq!(c.base_clock_mhz, 0xC8);
    }

    #[test]
    fn caps_max_block_len_decode() {
        // bits[17:16] of CAPABILITIES (lo): 0=512, 1=1024, 2=2048.
        for (raw, expected) in [(0u32, 512u16), (1, 1024), (2, 2048), (3, 0)] {
            let lo = raw << 16;
            let c = Capabilities::decode(lo, 0);
            assert_eq!(
                c.max_block_len_bytes, expected,
                "raw={} should decode to {} bytes",
                raw, expected
            );
        }
    }

    #[test]
    fn caps_adma2_supported_bit() {
        let c = Capabilities::decode(1 << 19, 0);
        assert!(c.adma2_supported);
        let c = Capabilities::decode(0, 0);
        assert!(!c.adma2_supported);
    }

    #[test]
    fn caps_voltage_bits() {
        let c = Capabilities::decode((1 << 24) | (1 << 26), 0);
        assert!(c.voltage_3v3);
        assert!(!c.voltage_3v0);
        assert!(c.voltage_1v8);
    }

    #[test]
    fn caps_realistic_pi4b_emmc2() {
        // Indicative Pi 4B emmc2 CAPABILITIES values — base clock
        // 200 MHz (programmed by CPRMAN), ADMA2 supported, 1024
        // max block length, 3.3V supported. The real silicon
        // value will be checked at M1 boot; this test just pins
        // the decode of a representative bit pattern.
        let lo = (200u32 << 8)        // base clock 200 MHz in bits[14:8]
               | (1u32 << 16)         // max block len = 1024 in bits[17:16]
               | (1u32 << 19)         // ADMA2 supported in bits[19]
               | (1u32 << 24);        // 3.3V supported in bits[24]
        let c = Capabilities::decode(lo, 0);
        assert_eq!(c.base_clock_mhz, 200);
        assert_eq!(c.max_block_len_bytes, 1024);
        assert!(c.adma2_supported);
        assert!(c.voltage_3v3);
        assert!(!c.voltage_1v8);
    }

    #[test]
    fn caps_bits_round_trip() {
        // The raw bits field should be hi << 32 | lo, regardless
        // of decode semantics — useful for unconditional dumping.
        let c = Capabilities::decode(0xDEAD_BEEF, 0xCAFE_BABE);
        assert_eq!(c.bits, 0xCAFE_BABE_DEAD_BEEF);
    }

    // ----- Register offsets -----

    #[test]
    fn capabilities_offsets_are_canonical() {
        // The lo/hi pair must be 4 bytes apart; the M1 driver
        // reads them as two 32-bit registers at these offsets.
        assert_eq!(SDHCI_CAPABILITIES, 0x040);
        assert_eq!(SDHCI_CAPABILITIES_HI, 0x044);
    }

    #[test]
    fn software_reset_bits_distinct() {
        assert_ne!(SW_RST_ALL, SW_RST_CMD);
        assert_ne!(SW_RST_CMD, SW_RST_DATA);
        assert_ne!(SW_RST_ALL, SW_RST_DATA);
    }

    #[test]
    fn present_state_inhibit_bits_distinct() {
        assert_ne!(SDHCI_CMD_INHIBIT, SDHCI_DAT_INHIBIT);
    }

    // ----- SD command opcodes -----

    #[test]
    fn sd_command_indices_match_spec() {
        // Cross-check against the SD Physical Layer Spec command
        // numbers. If any of these drift the controller will
        // dispatch to the wrong slot.
        assert_eq!(SdCommand::GoIdleState.index(), 0);
        assert_eq!(SdCommand::SendIfCond.index(), 8);
        assert_eq!(SdCommand::ReadSingleBlock.index(), 17);
        assert_eq!(SdCommand::ReadMultipleBlock.index(), 18);
        assert_eq!(SdCommand::SetBlockCount.index(), 23);
        assert_eq!(SdCommand::WriteBlock.index(), 24);
        assert_eq!(SdCommand::WriteMultipleBlock.index(), 25);
        assert_eq!(SdCommand::AppCmd.index(), 55);
    }

    // ----- Clock divisor math -----

    #[test]
    fn clock_divisor_400khz_from_200mhz() {
        // Pi 4B emmc2 base clock = 200 MHz; ID-mode = 400 kHz.
        // N = 200_000_000 / (2 × 400_000) = 250 = 0xFA.
        let (lo, hi) = compute_clock_divisor(200_000_000, 400_000);
        assert_eq!(lo, 0xFA, "lower 8 bits of N should be 0xFA (=250)");
        assert_eq!(hi, 0x00, "upper 2 bits of N should be 0 (250 fits in 8 bits)");
    }

    #[test]
    fn clock_divisor_bypass_when_base_lte_target() {
        // If base ≤ target the divisor is 0 (bypass).
        let (lo, hi) = compute_clock_divisor(400_000, 400_000);
        assert_eq!((lo, hi), (0, 0));
        let (lo2, hi2) = compute_clock_divisor(1, 400_000);
        assert_eq!((lo2, hi2), (0, 0));
    }

    #[test]
    fn clock_divisor_target_zero_returns_bypass() {
        let (lo, hi) = compute_clock_divisor(200_000_000, 0);
        assert_eq!((lo, hi), (0, 0));
    }

    #[test]
    fn clock_divisor_assembled_clock_control_word() {
        // Verify the assembled CLOCK_CONTROL value for 200 MHz → 400 kHz.
        // (lo=0xFA, hi=0x00) → (0xFA << 8) | (0x00 << 6) = 0xFA00.
        let (lo, hi) = compute_clock_divisor(200_000_000, 400_000);
        let ctrl = (lo as u16) << 8 | (hi as u16) << 6;
        assert_eq!(ctrl, 0xFA00);
        // With INT_EN set:
        assert_eq!(ctrl | SDHCI_CLOCK_INT_EN, 0xFA01);
        // With INT_EN + CARD_EN set:
        assert_eq!(ctrl | SDHCI_CLOCK_INT_EN | SDHCI_CLOCK_CARD_EN, 0xFA05);
    }

    // ----- M3 command indices -----

    #[test]
    fn sd_command_m3_indices_match_spec() {
        assert_eq!(SdCommand::AllSendCid.index(), 2);
        assert_eq!(SdCommand::SendRelativeAddr.index(), 3);
        assert_eq!(SdCommand::SetBusWidth.index(), 6);
        assert_eq!(SdCommand::SelectCard.index(), 7);
        assert_eq!(SdCommand::SendCsd.index(), 9);
        assert_eq!(SdCommand::SdSendOpCond.index(), 41);
    }

    // ----- OCR decode -----

    #[test]
    fn ocr_card_ready_and_sdhc() {
        // Bit 31 set (power up done), bit 30 set (CCS = SDHC/SDXC).
        let ocr = OcrRegister::decode(0xC000_0000);
        assert!(ocr.power_up_done);
        assert!(ocr.ccs);
    }

    #[test]
    fn ocr_card_busy() {
        // Bit 31 clear → still initializing; loop must retry.
        let ocr = OcrRegister::decode(0x00FF_8000);
        assert!(!ocr.power_up_done);
        assert!(!ocr.ccs);
    }

    #[test]
    fn ocr_sdsc_card() {
        // Bit 31 set (ready), bit 30 clear (legacy SDSC, byte-addressed).
        let ocr = OcrRegister::decode(0x8000_0000);
        assert!(ocr.power_up_done);
        assert!(!ocr.ccs);
    }

    // ----- R6 RCA extraction -----

    #[test]
    fn r6_rca_upper_16_bits() {
        // CMD3 response: RCA = 0xAAAA, status bits = 0x5555.
        let rca = r6_rca(0xAAAA_5555);
        assert_eq!(rca, 0xAAAA);
    }

    #[test]
    fn r6_rca_ignores_status_bits() {
        // Varying the lower 16 bits must not affect the RCA.
        assert_eq!(r6_rca(0x1234_0000), 0x1234);
        assert_eq!(r6_rca(0x1234_FFFF), 0x1234);
    }

    // ----- CSD v2 decode -----

    #[test]
    fn csd_v2_rejects_v1_structure() {
        // CSD_STRUCTURE = 0 (v1) in RESPONSE_3[23:22] → r[3] bits[23:22] = 0.
        let r = [0u32, 0, 0, 0x0000_0000]; // r[3] bits[23:22] = 00 → structure 0
        let err = CsdV2::decode(r).unwrap_err();
        assert_eq!(err.csd_structure, 0);
    }

    #[test]
    fn csd_v2_rejects_reserved_structure() {
        // CSD_STRUCTURE = 3 (reserved) in RESPONSE_3[23:22] → r[3] bits[23:22] = 11.
        let r = [0u32, 0, 0, 0x00C0_0000]; // r[3] bits[23:22] = 11 → structure 3
        let err = CsdV2::decode(r).unwrap_err();
        assert_eq!(err.csd_structure, 3);
    }

    #[test]
    fn csd_v2_capacity_32gib() {
        // 32 GiB: C_SIZE = 65535 (0xFFFF).
        // CSD_STRUCTURE = 1 in RESPONSE_3[23:22] → r[3] = 0x0040_0000.
        // C_SIZE in RESPONSE_1[29:8] → r[1] = 0xFFFF << 8 = 0x00FFFF00.
        let r = [0u32, 0x00FFFF00, 0, 0x00400000];
        let csd = CsdV2::decode(r).unwrap();
        let expected_bytes = 65536u64 * 512 * 1024; // 34_359_738_368
        assert_eq!(csd.capacity_bytes, expected_bytes);
        assert_eq!(csd.capacity_blocks, expected_bytes / 512);
    }

    #[test]
    fn csd_v2_capacity_64gib() {
        // 64 GiB: C_SIZE = 131071 (0x1FFFF).
        // CSD_STRUCTURE = 1 in RESPONSE_3[23:22] → r[3] = 0x0040_0000.
        // C_SIZE in RESPONSE_1[29:8] → r[1] = 0x1FFFF << 8 = 0x01FFFF00.
        let r = [0u32, 0x01FFFF00, 0, 0x00400000];
        let csd = CsdV2::decode(r).unwrap();
        let expected_bytes = 131072u64 * 512 * 1024; // 68_719_476_736
        assert_eq!(csd.capacity_bytes, expected_bytes);
    }

    #[test]
    fn cmd8_if_cond_arg_vhs_and_pattern() {
        // VHS = 0x1 (bits[11:8]), check pattern = 0xAA (bits[7:0]).
        assert_eq!(CMD8_IF_COND_ARG & 0xF00, 0x100, "VHS must be 1");
        assert_eq!(CMD8_IF_COND_ARG & 0xFF, 0xAA, "check pattern must be 0xAA");
    }

    #[test]
    fn acmd41_arg_hcs_bit_set() {
        // HCS must be bit 30.
        assert_ne!(ACMD41_ARG_HCS & (1 << 30), 0, "HCS bit must be set");
    }

    // ----- Command word builder -----

    #[test]
    fn cmd0_word_is_zero() {
        // CMD0: index=0, no response, no CRC/index check → all zeros.
        let w = sd_command_word(SdCommand::GoIdleState.index(), SDHCI_CMD_RESP_NONE);
        assert_eq!(w, 0x0000);
    }

    #[test]
    fn cmd8_word_is_081a() {
        // CMD8 (SendIfCond): index=8, short response (0x02), CRC (0x08),
        // index check (0x10). flags = 0x02 | 0x08 | 0x10 = 0x1A.
        // word = (8 << 8) | 0x1A = 0x081A.
        let flags = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX;
        let w = sd_command_word(SdCommand::SendIfCond.index(), flags);
        assert_eq!(w, 0x081A);
    }

    // ----- M4 data-transfer constants -----

    #[test]
    fn normal_int_data_bits_match_spec() {
        // TRANSFER_COMPLETE is bit 1 of NORMAL_INT_STATUS.
        assert_eq!(SDHCI_INT_DATA_COMPLETE, 0x0002);
        // BUF_READ_READY is bit 5 of NORMAL_INT_STATUS.
        assert_eq!(SDHCI_INT_BUF_RD_READY, 0x0020);
        // Neither aliases CMD_COMPLETE (bit 0).
        assert_eq!(SDHCI_INT_DATA_COMPLETE & SDHCI_INT_CMD_COMPLETE, 0);
        assert_eq!(SDHCI_INT_BUF_RD_READY & SDHCI_INT_CMD_COMPLETE, 0);
        assert_eq!(SDHCI_INT_BUF_RD_READY & SDHCI_INT_DATA_COMPLETE, 0);
    }

    #[test]
    fn error_int_data_bits_match_spec() {
        // Data timeout = bit 4, data CRC = bit 5, data end-bit = bit 6.
        assert_eq!(SDHCI_INT_DATA_TIMEOUT, 0x0010);
        assert_eq!(SDHCI_INT_DATA_CRC, 0x0020);
        assert_eq!(SDHCI_INT_DATA_END_BIT, 0x0040);
        // Data error bits must not overlap the command error bits (bits 0-3).
        let cmd_mask = SDHCI_INT_CMD_TIMEOUT | SDHCI_INT_CMD_CRC
            | SDHCI_INT_CMD_END_BIT | SDHCI_INT_CMD_INDEX;
        assert_eq!(SDHCI_INT_DATA_TIMEOUT & cmd_mask, 0);
        assert_eq!(SDHCI_INT_DATA_CRC     & cmd_mask, 0);
        assert_eq!(SDHCI_INT_DATA_END_BIT & cmd_mask, 0);
    }

    #[test]
    fn cmd_data_present_bit_matches_spec() {
        // DATA_PRESENT is bit 5 of the flags byte.
        assert_eq!(SDHCI_CMD_DATA, 0x20);
        // Must not overlap response type, CRC, or index bits.
        assert_eq!(
            SDHCI_CMD_DATA & (SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX),
            0,
        );
    }

    #[test]
    fn trns_read_bit_matches_spec() {
        // DATA_XFER_DIR is bit 4 of TRANSFER_MODE.
        assert_eq!(SDHCI_TRNS_READ, 0x0010);
        // All TRNS_ bits are distinct.
        assert_ne!(SDHCI_TRNS_DMA, SDHCI_TRNS_BLK_CNT_EN);
        assert_ne!(SDHCI_TRNS_BLK_CNT_EN, SDHCI_TRNS_READ);
        assert_ne!(SDHCI_TRNS_READ, SDHCI_TRNS_MULTI);
    }

    #[test]
    fn cmd17_word_has_data_present() {
        // CMD17: index=17, SHORT(0x02)|CRC(0x08)|INDEX(0x10)|DATA(0x20) = 0x3A.
        let flags = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA;
        let w = sd_command_word(SdCommand::ReadSingleBlock.index(), flags);
        assert_eq!(w, (17u16 << 8) | 0x3A);
    }

    // ----- M5 multi-block + write constants -----

    #[test]
    fn buf_wr_ready_bit_matches_spec() {
        // BUF_WRITE_READY is bit 4 of NORMAL_INT_STATUS.
        assert_eq!(SDHCI_INT_BUF_WR_READY, 0x0010);
        // Must not alias BUF_RD_READY (bit 5) or DATA_COMPLETE (bit 1).
        assert_eq!(SDHCI_INT_BUF_WR_READY & SDHCI_INT_BUF_RD_READY, 0);
        assert_eq!(SDHCI_INT_BUF_WR_READY & SDHCI_INT_DATA_COMPLETE, 0);
    }

    #[test]
    fn auto_cmd23_bits_match_spec() {
        // AUTO_CMD select is TRANSFER_MODE bits[3:2]: 01 = CMD12, 10 = CMD23.
        assert_eq!(SDHCI_TRNS_AUTO_CMD12, 0x0004);
        assert_eq!(SDHCI_TRNS_AUTO_CMD23, 0x0008);
        // Mutually exclusive — setting both is undefined.
        assert_eq!(SDHCI_TRNS_AUTO_CMD12 & SDHCI_TRNS_AUTO_CMD23, 0);
        // Must not collide with the other TRANSFER_MODE bits we use.
        let other = SDHCI_TRNS_DMA | SDHCI_TRNS_BLK_CNT_EN | SDHCI_TRNS_READ | SDHCI_TRNS_MULTI;
        assert_eq!(SDHCI_TRNS_AUTO_CMD23 & other, 0);
    }

    #[test]
    fn argument2_aliases_sysaddr() {
        // SDHCI_ARGUMENT2 (Auto-CMD23 argument) and SDHCI_SYSADDR
        // (SDMA system address) share offset 0x000 — the register's role
        // is determined by which transfer mode is active.
        assert_eq!(SDHCI_ARGUMENT2, SDHCI_SYSADDR);
        assert_eq!(SDHCI_ARGUMENT2, 0x000);
    }

    #[test]
    fn cmd18_word_has_data_present() {
        // CMD18 (READ_MULTIPLE_BLOCK): same flags as CMD17.
        let flags = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA;
        let w = sd_command_word(SdCommand::ReadMultipleBlock.index(), flags);
        assert_eq!(w, (18u16 << 8) | 0x3A);
    }

    #[test]
    fn cmd24_word_has_data_present() {
        // CMD24 (WRITE_BLOCK): R1 short, CRC, INDEX, DATA. Same flag word as CMD17/CMD18.
        let flags = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX | SDHCI_CMD_DATA;
        let w = sd_command_word(SdCommand::WriteBlock.index(), flags);
        assert_eq!(w, (24u16 << 8) | 0x3A);
    }

    // ----- M6 sub-commit 2b — ADMA2 descriptor + DMA_SEL -----

    #[test]
    fn host_ctrl_dma_sel_encoding() {
        // DMA_SEL field is bits [4:3] of HOST_CONTROL_1. ADMA2 32-bit = 0b10
        // shifted to position 3 = 0b0001_0000 = 0x10. Mask covers both bits.
        assert_eq!(SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32, 0x10);
        assert_eq!(SDHCI_HOST_CTRL_DMA_SEL_MASK, 0x18);
        // Setting ADMA2_32 doesn't bleed outside the mask.
        assert_eq!(SDHCI_HOST_CTRL_DMA_SEL_ADMA2_32 & !SDHCI_HOST_CTRL_DMA_SEL_MASK, 0);
        // DMA_SEL doesn't collide with the 4-bit bus width bit (bit 1).
        assert_eq!(SDHCI_HOST_CTRL_DMA_SEL_MASK & SDHCI_HOST_CTRL_DAT_4BIT, 0);
    }

    #[test]
    fn adma_address_register_offset_pinned() {
        // ADMA2 descriptor table address at 0x058. Distinct from
        // SDHCI_SYSADDR / SDHCI_ARGUMENT2 (both at 0x000).
        assert_eq!(SDHCI_ADMA_ADDRESS, 0x058);
        assert_ne!(SDHCI_ADMA_ADDRESS, SDHCI_SYSADDR);
        assert_ne!(SDHCI_ADMA_ADDRESS, SDHCI_ARGUMENT2);
    }

    #[test]
    fn adma2_attr_bits_pinned() {
        // Discrete-bit attributes occupy distinct bit positions.
        assert_eq!(ADMA2_ATTR_VALID, 0x0001);
        assert_eq!(ADMA2_ATTR_END,   0x0002);
        assert_eq!(ADMA2_ATTR_INT,   0x0004);
        // ACT field bits[5:4]: tran=10, link=11.
        assert_eq!(ADMA2_ATTR_ACT_TRAN, 0b10_0000);
        assert_eq!(ADMA2_ATTR_ACT_LINK, 0b11_0000);
        // Discrete bits and ACT field don't overlap.
        let discrete = ADMA2_ATTR_VALID | ADMA2_ATTR_END | ADMA2_ATTR_INT;
        let act = ADMA2_ATTR_ACT_TRAN | ADMA2_ATTR_ACT_LINK;
        assert_eq!(discrete & act, 0);
    }

    #[test]
    fn adma2_tran_end_descriptor_layout() {
        // Spec wire layout: bits[63:32]=addr, [31:16]=length, [15:0]=attrs.
        // Single-block 512-byte read at buf_phys=0xDEAD_B000 →
        //   attrs = VALID|END|ACT_TRAN = 0x01 | 0x02 | 0x20 = 0x23
        //   length = 0x0200
        //   addr   = 0xDEAD_B000
        // word = (0xDEAD_B000 << 32) | (0x0200 << 16) | 0x23
        //      = 0xDEAD_B000_0200_0023
        let d = adma2_tran_end_descriptor(0xDEAD_B000, 512);
        assert_eq!(d, 0xDEAD_B000_0200_0023);
        // Round-trip extraction.
        assert_eq!((d & 0xFFFF) as u16, 0x0023);
        assert_eq!(((d >> 16) & 0xFFFF) as u16, 512);
        assert_eq!((d >> 32) as u32, 0xDEAD_B000);
    }

    #[test]
    fn adma2_tran_end_descriptor_max_length() {
        // Length field is 16-bit; max non-zero descriptor length is 65535.
        // (Spec quirk: length=0 means 65536 — not used here.)
        let d = adma2_tran_end_descriptor(0x1000, 65535);
        assert_eq!(((d >> 16) & 0xFFFF) as u16, 65535);
    }

    #[test]
    fn adma2_descriptor_tran_constructor() {
        // tran: VALID | ACT_TRAN, no END (mid-chain shape).
        // Argument order (address, length) — matches legacy helper.
        let d = Adma2Descriptor::tran(0xDEAD_B000, 512);
        assert_eq!(d.attr(), ADMA2_ATTR_VALID | ADMA2_ATTR_ACT_TRAN);
        assert_eq!(d.length(), 512);
        assert_eq!(d.address(), 0xDEAD_B000);
        // END must not be set on a non-terminating tran.
        assert_eq!(d.attr() & ADMA2_ATTR_END, 0);
    }

    #[test]
    fn adma2_descriptor_tran_end_constructor() {
        // tran_end: VALID | END | ACT_TRAN — single-block path's shape.
        // Argument order (address, length) — matches legacy helper.
        let d = Adma2Descriptor::tran_end(0xDEAD_B000, 512);
        assert_eq!(
            d.attr(),
            ADMA2_ATTR_VALID | ADMA2_ATTR_END | ADMA2_ATTR_ACT_TRAN,
        );
        assert_eq!(d.length(), 512);
        assert_eq!(d.address(), 0xDEAD_B000);
        // Wire bytes must match the legacy `adma2_tran_end_descriptor`
        // u64 layout (LE on wire): both helpers produce the same 8
        // bytes for the same inputs in the same order. Pins the P9.9
        // migration target — driver call sites swap one-for-one with
        // no argument transposition.
        let legacy = adma2_tran_end_descriptor(0xDEAD_B000, 512);
        assert_eq!(d.attr() as u64, legacy & 0xFFFF);
        assert_eq!(d.length() as u64, (legacy >> 16) & 0xFFFF);
        assert_eq!(d.address() as u64, legacy >> 32);
    }

    #[test]
    fn adma2_descriptor_link_constructor() {
        // link: VALID | ACT_LINK; length is don't-care, emitted as 0.
        // The constructors-produce-exactly-one-action invariant is
        // enforced by the equality check — `link` cannot return a
        // descriptor whose attr has both ACT_TRAN and ACT_LINK ORed.
        let d = Adma2Descriptor::link(0x1000_0000);
        assert_eq!(d.attr(), ADMA2_ATTR_VALID | ADMA2_ATTR_ACT_LINK);
        assert_eq!(d.length(), 0);
        assert_eq!(d.address(), 0x1000_0000);
    }

    #[test]
    fn adma2_descriptor_nop_constructor() {
        // nop: VALID alone (ACT field zero). Length and address zero.
        // ACT field check via the masked 2-bit value at bits 5:4 —
        // ACT_TRAN (0b10) and ACT_LINK (0b11) share bit 5, so masking
        // the union doesn't distinguish "no action" from "either
        // action"; the bit-shift form is correct.
        let d = Adma2Descriptor::nop();
        assert_eq!(d.attr(), ADMA2_ATTR_VALID);
        assert_eq!(d.length(), 0);
        assert_eq!(d.address(), 0);
        assert_eq!((d.attr() >> 4) & 0b11, 0);
    }

    #[test]
    fn adma2_descriptor_with_int_chains() {
        // with_int ORs INT onto any base shape's attr; length and
        // address pass through unchanged.
        let base = Adma2Descriptor::tran(0x4000, 256);
        let withi = base.with_int();
        assert_eq!(
            withi.attr(),
            ADMA2_ATTR_VALID | ADMA2_ATTR_ACT_TRAN | ADMA2_ATTR_INT,
        );
        assert_eq!(withi.length(), 256);
        assert_eq!(withi.address(), 0x4000);
        // Idempotent — chaining twice keeps the same INT bit set.
        assert_eq!(withi.with_int().attr(), withi.attr());
    }
}
