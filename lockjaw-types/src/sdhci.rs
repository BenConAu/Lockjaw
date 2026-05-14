/// SDHCI v3 controller types — register offsets, capability decode,
/// and SD command opcodes.
///
/// All pure types — no MMIO access, no volatile, no barriers.
/// Host-testable layout / decode logic.
///
/// References:
///   - SD Host Controller Standard Specification v4.20 (relevant
///     parts, since the BCM2711 emmc2 controller is SDHCI v3)
///   - Linux `drivers/mmc/host/sdhci.h` (cross-reference for
///     register offsets and CAPABILITIES bit layout)
///   - SD Physical Layer Specification v6.00 (command set)

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
pub const SDHCI_SYSADDR: usize = 0x000;
/// `BLOCK_SIZE_REG` (0x004) — bits[11:0] = block size in bytes.
pub const SDHCI_BLOCK_SIZE: usize = 0x004;
/// `BLOCK_COUNT_REG` (0x006) — number of blocks to transfer.
pub const SDHCI_BLOCK_COUNT: usize = 0x006;
/// `ARGUMENT_REG` (0x008) — command argument.
pub const SDHCI_ARGUMENT: usize = 0x008;
/// `TRANSFER_MODE_REG` (0x00c) + `COMMAND_REG` (0x00e) — combined
/// 32-bit write triggers command issue.
pub const SDHCI_TRANSFER_MODE: usize = 0x00c;
pub const SDHCI_COMMAND: usize = 0x00e;
/// `RESPONSE_REG` (0x010..0x01f) — 4 × 32-bit response slots.
pub const SDHCI_RESPONSE_0: usize = 0x010;
pub const SDHCI_RESPONSE_1: usize = 0x014;
pub const SDHCI_RESPONSE_2: usize = 0x018;
pub const SDHCI_RESPONSE_3: usize = 0x01c;
/// `BUFFER_DATA_PORT_REG` (0x020) — PIO data window.
pub const SDHCI_BUFFER_DATA_PORT: usize = 0x020;
/// `PRESENT_STATE_REG` (0x024) — controller status snapshot.
pub const SDHCI_PRESENT_STATE: usize = 0x024;
/// `HOST_CONTROL_1` (0x028).
pub const SDHCI_HOST_CONTROL: usize = 0x028;
/// `POWER_CONTROL_REG` (0x029).
pub const SDHCI_POWER_CONTROL: usize = 0x029;
/// `BLOCK_GAP_CONTROL_REG` (0x02a).
pub const SDHCI_BLOCK_GAP_CONTROL: usize = 0x02a;
/// `WAKEUP_CONTROL_REG` (0x02b).
pub const SDHCI_WAKEUP_CONTROL: usize = 0x02b;
/// `CLOCK_CONTROL_REG` (0x02c).
pub const SDHCI_CLOCK_CONTROL: usize = 0x02c;
/// `TIMEOUT_CONTROL_REG` (0x02e).
pub const SDHCI_TIMEOUT_CONTROL: usize = 0x02e;
/// `SOFTWARE_RESET_REG` (0x02f) — write `SW_RST_*` bits.
pub const SDHCI_SOFTWARE_RESET: usize = 0x02f;
/// `NORMAL_INT_STATUS` (0x030).
pub const SDHCI_NORMAL_INT_STATUS: usize = 0x030;
/// `ERROR_INT_STATUS` (0x032).
pub const SDHCI_ERROR_INT_STATUS: usize = 0x032;
/// `NORMAL_INT_STATUS_ENABLE` (0x034).
pub const SDHCI_NORMAL_INT_STATUS_ENABLE: usize = 0x034;
/// `ERROR_INT_STATUS_ENABLE` (0x036).
pub const SDHCI_ERROR_INT_STATUS_ENABLE: usize = 0x036;
/// `NORMAL_INT_SIGNAL_ENABLE` (0x038).
pub const SDHCI_NORMAL_INT_SIGNAL_ENABLE: usize = 0x038;
/// `ERROR_INT_SIGNAL_ENABLE` (0x03a).
pub const SDHCI_ERROR_INT_SIGNAL_ENABLE: usize = 0x03a;
/// `CAPABILITIES` (0x040) — fixed device capabilities, low half.
pub const SDHCI_CAPABILITIES: usize = 0x040;
/// `CAPABILITIES_HI` (0x044) — capabilities high half.
pub const SDHCI_CAPABILITIES_HI: usize = 0x044;
/// `SLOT_INT_STATUS_VERSION` (0x0fc) — slot status (low 16) +
/// host controller version (high 16).
pub const SDHCI_HOST_VERSION: usize = 0x0fe;

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

// ---------------------------------------------------------------------------
// PRESENT_STATE bits (offset 0x024, read-only)
// ---------------------------------------------------------------------------

/// CMD Line Inhibit (bit 0). Set while a command transfer is in progress.
pub const SDHCI_CMD_INHIBIT: u32 = 0x0000_0001;

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
/// No response type (CMD0, broadcast commands with no reply).
pub const SDHCI_CMD_RESP_NONE: u8 = 0x00;
/// Short response — 48-bit (R1, R3, R4, R5, R6, R7).
pub const SDHCI_CMD_RESP_SHORT: u8 = 0x02;

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
}
