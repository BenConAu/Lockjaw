/// Clock controller IPC + error types shared by every clock provider
/// driver and its clients.
///
/// Per-controller specifics (BCM2711 CPRMAN's register layout, divider
/// math, ClockId enum) live in submodules — `cprman` for the BCM2711
/// today; future providers (a board with a different SoC, an I2C
/// clock chip, etc.) get their own submodule alongside.
///
/// The ClockOp / ClockReplyStatus shape here is the cross-controller
/// IPC contract: every provider speaks the same opcodes and returns
/// the same status codes; controller-specific things (which clock
/// ids exist, what rates are achievable) flow through the `id` and
/// `actual_hz` fields and through `ClockError::NotSupported(id)` for
/// any leaf the provider doesn't model.

pub mod cprman;

/// IPC operation codes for ClockOp messages. Caller passes the opcode
/// in word 0; reply status occupies word 0 of the reply, with
/// op-specific values in words 1..=3.
pub const CLOCK_OP_SET_RATE: u64 = 1;
pub const CLOCK_OP_GET_RATE: u64 = 2;
pub const CLOCK_OP_ENABLE:   u64 = 3;
pub const CLOCK_OP_DISABLE:  u64 = 4;

/// Reply status codes. Word 0 of the reply.
pub const CLOCK_OK:                u64 = 0;
pub const CLOCK_ERR_NOT_SUPPORTED: u64 = 1;
pub const CLOCK_ERR_OUT_OF_RANGE:  u64 = 2;
pub const CLOCK_ERR_HARDWARE:      u64 = 3;
pub const CLOCK_ERR_BAD_OP:        u64 = 4;

/// Typed error returned by the userspace `ClockClient` wrapper.
/// Mirrors the on-the-wire status codes; the wrapper decodes the
/// raw u64 reply word into this enum so callers can `match` rather
/// than compare against constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockError {
    /// The requested clock_id is not modeled by this provider.
    /// Carries the id so log messages can be specific.
    NotSupported(u32),
    /// `set_rate` target is outside what the divider can express, or
    /// `get_rate` of an unconfigured clock.
    OutOfRange,
    /// MMIO write succeeded but the hardware did not reach the
    /// expected post-condition (e.g., BUSY bit didn't clear).
    Hardware,
    /// Caller sent an unknown opcode word — protocol bug on the
    /// client side.
    BadOp,
}

impl ClockError {
    /// Reply word 0 encoding. Word 1 carries the offending id for
    /// NotSupported (so the receiver can print it).
    pub fn status_code(self) -> u64 {
        match self {
            ClockError::NotSupported(_) => CLOCK_ERR_NOT_SUPPORTED,
            ClockError::OutOfRange      => CLOCK_ERR_OUT_OF_RANGE,
            ClockError::Hardware        => CLOCK_ERR_HARDWARE,
            ClockError::BadOp           => CLOCK_ERR_BAD_OP,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_codes_distinct() {
        let codes = [
            CLOCK_OP_SET_RATE,
            CLOCK_OP_GET_RATE,
            CLOCK_OP_ENABLE,
            CLOCK_OP_DISABLE,
        ];
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                assert_ne!(codes[i], codes[j]);
            }
        }
    }

    #[test]
    fn status_codes_distinct() {
        let codes = [
            CLOCK_OK,
            CLOCK_ERR_NOT_SUPPORTED,
            CLOCK_ERR_OUT_OF_RANGE,
            CLOCK_ERR_HARDWARE,
            CLOCK_ERR_BAD_OP,
        ];
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                assert_ne!(codes[i], codes[j]);
            }
        }
    }

    #[test]
    fn error_to_status_round_trip() {
        assert_eq!(ClockError::NotSupported(7).status_code(), CLOCK_ERR_NOT_SUPPORTED);
        assert_eq!(ClockError::OutOfRange.status_code(),    CLOCK_ERR_OUT_OF_RANGE);
        assert_eq!(ClockError::Hardware.status_code(),      CLOCK_ERR_HARDWARE);
        assert_eq!(ClockError::BadOp.status_code(),         CLOCK_ERR_BAD_OP);
    }
}
