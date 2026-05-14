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
// for cross-reference. Only the registers M1 actually touches are
// listed today; M2+ adds the rest as it grows the protocol surface.

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
// SD command opcodes (SD Physical Layer Spec § 4.7.4)
// ---------------------------------------------------------------------------
//
// Typed for M2+ to dispatch on. Numbering matches the spec; the
// SDHCI controller takes `cmd_index` in bits[13:8] of
// COMMAND_REG, so `as u16` produces the value to OR in.

/// Standard SD commands. M1 doesn't issue any of these; the enum
/// is in place so M2+ can match exhaustively rather than passing
/// raw integers.
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
}
