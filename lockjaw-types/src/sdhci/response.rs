//! Typed SDHCI command response shapes.
//!
//! Each `ResponseShape` impl carries the COMMAND-register flag bits the
//! controller expects for that response shape, plus the pure decoder from
//! the four 32-bit RESPONSE registers to a typed result. Used by the
//! future `SdhciCommandInit::issue_no_data::<R>` envelope in
//! `lockjaw-userlib` to pick the correct flags + decode in one type-level
//! decision — picking the wrong shape becomes a compile error instead of
//! a silent register-misread.
//!
//! Response shape coverage (SD Physical Layer Spec § 4.9):
//!   R0  — no response (CMD0, broadcast)
//!   R1  — short, status word (most commands)
//!   R1b — short with busy on DAT0 (CMD7 SELECT_CARD, R1b variants)
//!   R2  — long 128-bit (CMD2 CID, CMD9 CSD); caller decodes via
//!         `CsdV2::decode` (or a future CID helper) since the same wire
//!         shape carries either
//!   R3  — short, OCR (ACMD41); no CRC/index (OCR doesn't carry them)
//!   R6  — short, RCA + status (CMD3 SEND_RELATIVE_ADDR)
//!   R7  — short, echo of argument (CMD8 SEND_IF_COND)

use super::{
    r6_rca, OcrRegister, SDHCI_CMD_CRC, SDHCI_CMD_INDEX, SDHCI_CMD_RESP_LONG,
    SDHCI_CMD_RESP_NONE, SDHCI_CMD_RESP_SHORT, SDHCI_CMD_RESP_SHORT_BUSY,
};

/// SDHCI command response shape — the COMMAND-register flag bits to use
/// and how to decode the resulting RESPONSE registers.
///
/// Sealed: the seven response shapes (R0/R1/R1b/R2/R3/R6/R7) are the
/// complete set per SD Physical Layer Spec § 4.9.
pub trait ResponseShape: sealed::Sealed {
    /// The decoded result type for this response shape.
    type Decoded;
    /// OR of `SDHCI_CMD_RESP_*` / `SDHCI_CMD_CRC` / `SDHCI_CMD_INDEX`
    /// bits to write into the low byte of `COMMAND_REG`.
    const FLAGS: u8;
    /// Pure decode from the four RESPONSE register reads, passed in
    /// register-address order (`r[0]` = RESPONSE_0 at 0x010,
    /// `r[3]` = RESPONSE_3 at 0x01c).
    fn decode(r: [u32; 4]) -> Self::Decoded;
}

mod sealed {
    pub trait Sealed {}
}

/// No-response shape (CMD0, broadcast commands with no reply).
pub struct R0;
impl sealed::Sealed for R0 {}
impl ResponseShape for R0 {
    type Decoded = ();
    const FLAGS: u8 = SDHCI_CMD_RESP_NONE;
    fn decode(_r: [u32; 4]) {}
}

/// Short response with status word (R1). Most commands. The status word
/// is the standard SD card status (errors, current state, ready-for-data
/// bit). `RESPONSE_0` holds it.
pub struct R1;
impl sealed::Sealed for R1 {}
impl ResponseShape for R1 {
    type Decoded = u32;
    const FLAGS: u8 = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX;
    fn decode(r: [u32; 4]) -> u32 {
        r[0]
    }
}

/// Short response with busy on DAT0 (R1b — CMD7 SELECT_CARD,
/// CMD12 STOP_TRANSMISSION). The controller delays `CMD_COMPLETE`
/// until the card releases DAT0; the driver must additionally poll
/// `PRESENT_STATE.DAT_INHIBIT` to confirm the busy phase ended.
pub struct R1b;
impl sealed::Sealed for R1b {}
impl ResponseShape for R1b {
    type Decoded = u32;
    const FLAGS: u8 = SDHCI_CMD_RESP_SHORT_BUSY | SDHCI_CMD_CRC | SDHCI_CMD_INDEX;
    fn decode(r: [u32; 4]) -> u32 {
        r[0]
    }
}

/// Long 128-bit response (R2 — CMD2 CID, CMD9 CSD). Returns the raw
/// four-word view in register-address order (`r[0]` = RESPONSE_0,
/// `r[3]` = RESPONSE_3); callers decode CID via a future helper or CSD
/// via `CsdV2::decode` from this same module's parent.
///
/// Index check is NOT set in `FLAGS`: R2 doesn't carry the command-index
/// echo because the CRC7 byte is stripped from the bottom and there's no
/// dedicated slot for the index.
pub struct R2;
impl sealed::Sealed for R2 {}
impl ResponseShape for R2 {
    type Decoded = [u32; 4];
    const FLAGS: u8 = SDHCI_CMD_RESP_LONG | SDHCI_CMD_CRC;
    fn decode(r: [u32; 4]) -> [u32; 4] {
        r
    }
}

/// Short response carrying OCR (R3 — ACMD41). No CRC or index check
/// because the OCR doesn't carry either per SD spec; the controller
/// must not validate them or the response will be flagged as
/// CMD_CRC / CMD_INDEX error.
pub struct R3;
impl sealed::Sealed for R3 {}
impl ResponseShape for R3 {
    type Decoded = OcrRegister;
    const FLAGS: u8 = SDHCI_CMD_RESP_SHORT;
    fn decode(r: [u32; 4]) -> OcrRegister {
        OcrRegister::decode(r[0])
    }
}

/// Decoded R6 response (CMD3 SEND_RELATIVE_ADDR): RCA + 16-bit card
/// status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct R6Response {
    /// New Published RCA (RESPONSE_0[31:16]).
    pub rca: u16,
    /// Card status bits (RESPONSE_0[15:0]).
    pub status: u16,
}

/// Short response with RCA (R6 — CMD3 SEND_RELATIVE_ADDR).
pub struct R6;
impl sealed::Sealed for R6 {}
impl ResponseShape for R6 {
    type Decoded = R6Response;
    const FLAGS: u8 = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX;
    fn decode(r: [u32; 4]) -> R6Response {
        R6Response {
            rca: r6_rca(r[0]),
            status: (r[0] & 0xFFFF) as u16,
        }
    }
}

/// Short response echoing the command argument (R7 — CMD8 SEND_IF_COND).
/// The caller verifies the echo matches the issued argument (typically
/// `CMD8_IF_COND_ARG = 0x000001AA`).
pub struct R7;
impl sealed::Sealed for R7 {}
impl ResponseShape for R7 {
    type Decoded = u32;
    const FLAGS: u8 = SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX;
    fn decode(r: [u32; 4]) -> u32 {
        r[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r0_flags_is_none() {
        // CMD0 carries no response; FLAGS must encode RESP_NONE (0).
        assert_eq!(R0::FLAGS, SDHCI_CMD_RESP_NONE);
        assert_eq!(R0::FLAGS, 0x00);
    }

    #[test]
    fn r1_flags_short_crc_index() {
        // RESP_SHORT (0x02) | CRC (0x08) | INDEX (0x10) = 0x1A.
        assert_eq!(R1::FLAGS, 0x1A);
    }

    #[test]
    fn r1b_flags_short_busy_crc_index() {
        // RESP_SHORT_BUSY (0x03) | CRC | INDEX = 0x1B.
        assert_eq!(R1b::FLAGS, 0x1B);
    }

    #[test]
    fn r2_flags_long_crc_no_index() {
        // R2 has no index check (CRC7 stripped only): RESP_LONG (0x01) | CRC (0x08) = 0x09.
        assert_eq!(R2::FLAGS, 0x09);
        assert_eq!(R2::FLAGS & SDHCI_CMD_INDEX, 0);
    }

    #[test]
    fn r3_flags_short_no_crc_no_index() {
        // OCR doesn't carry CRC or index: RESP_SHORT (0x02) alone.
        assert_eq!(R3::FLAGS, 0x02);
        assert_eq!(R3::FLAGS & SDHCI_CMD_CRC, 0);
        assert_eq!(R3::FLAGS & SDHCI_CMD_INDEX, 0);
    }

    #[test]
    fn r6_flags_short_crc_index() {
        assert_eq!(R6::FLAGS, 0x1A);
    }

    #[test]
    fn r7_flags_short_crc_index() {
        assert_eq!(R7::FLAGS, 0x1A);
    }

    #[test]
    fn r1_decode_is_response_0() {
        let r = [0xCAFE_BABE, 0xDEAD_BEEF, 0, 0];
        assert_eq!(R1::decode(r), 0xCAFE_BABE);
    }

    #[test]
    fn r1b_decode_is_response_0() {
        let r = [0xAAAA_5555, 0, 0, 0];
        assert_eq!(R1b::decode(r), 0xAAAA_5555);
    }

    #[test]
    fn r2_decode_passes_through_all_four() {
        // R2 returns the raw register quartet; callers decode CSD/CID.
        let r = [1, 2, 3, 4];
        assert_eq!(R2::decode(r), [1, 2, 3, 4]);
    }

    #[test]
    fn r3_decode_to_ocr_powered_up_sdhc() {
        // bit 31 set (power_up_done), bit 30 set (CCS=SDHC/SDXC).
        let ocr = R3::decode([0xC000_0000, 0, 0, 0]);
        assert!(ocr.power_up_done);
        assert!(ocr.ccs);
    }

    #[test]
    fn r3_decode_to_ocr_busy() {
        // bit 31 clear (still initializing); ACMD41 must retry.
        let ocr = R3::decode([0x00FF_8000, 0, 0, 0]);
        assert!(!ocr.power_up_done);
        assert!(!ocr.ccs);
    }

    #[test]
    fn r6_decode_splits_rca_and_status() {
        let r6 = R6::decode([0xAAAA_5555, 0, 0, 0]);
        assert_eq!(r6.rca, 0xAAAA);
        assert_eq!(r6.status, 0x5555);
    }

    #[test]
    fn r6_decode_ignores_higher_response_words() {
        // CMD3 returns short; only RESPONSE_0 is meaningful.
        let r6 = R6::decode([0x1234_FFFF, 0xDEAD, 0xBEEF, 0xCAFE]);
        assert_eq!(r6.rca, 0x1234);
        assert_eq!(r6.status, 0xFFFF);
    }

    #[test]
    fn r7_decode_is_response_0() {
        // CMD8 echo: argument 0x000001AA echoed in low 12 bits.
        assert_eq!(R7::decode([0x0000_01AA, 0, 0, 0]), 0x0000_01AA);
    }

    #[test]
    fn all_flags_fit_in_low_byte() {
        // FLAGS is OR'd into bits[7:0] of COMMAND_REG; must not exceed 8 bits.
        assert!(R0::FLAGS as u16 <= 0xFF);
        assert!(R1::FLAGS as u16 <= 0xFF);
        assert!(R1b::FLAGS as u16 <= 0xFF);
        assert!(R2::FLAGS as u16 <= 0xFF);
        assert!(R3::FLAGS as u16 <= 0xFF);
        assert!(R6::FLAGS as u16 <= 0xFF);
        assert!(R7::FLAGS as u16 <= 0xFF);
    }
}
