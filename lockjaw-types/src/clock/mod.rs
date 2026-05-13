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

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------
//
// Two legs of the same logical clock op (see
// `docs/book-of-lockjaw/03-non-virtualizable-hardware.md` for why
// device-manager proxies):
//
// 1. driver  →  device-manager:
//      msg = [CLOCK_OP_*, handle_id, arg, 0]
//    where handle_id is the opaque u32 device-manager allocated for
//    this driver via CMD_GET_CLOCK_HANDLE. device-manager looks up
//    (caller_token, handle_id) → (controller_phandle, clock_id) in
//    its binding table.
//
// 2. device-manager  →  provider (e.g., cprman-driver):
//      msg = [CLOCK_OP_*, clock_id, arg, 0]
//    The provider trusts the message body because device-manager is
//    its only legitimate caller (no driver holds a handle to the
//    provider).
//
// In both legs the reply layout is the same:
//      msg = [status, value, 0, 0]
// where `status` is one of CLOCK_OK / CLOCK_ERR_*, and `value` is
// op-specific (actual rate for SET_RATE / GET_RATE, 0 otherwise).

/// Per-clock-op opcodes. Word 0 of the request on either leg.
///
/// Values placed in the 0x1000+ range so they cannot collide with
/// device-manager's hardware-arbitration commands (CMD_CLAIM_DEVICE
/// = 1, CMD_PROBE_DEVICE = 2, CMD_CLAIM_BY_ADDR = 3 in
/// `lockjaw_types::device`). On the driver→devmgr leg both
/// command spaces share msg word 0, so a 1..=4 collision would
/// silently misroute SET_RATE to CMD_CLAIM_DEVICE etc.
pub const CLOCK_OP_SET_RATE: u64 = 0x1001;
pub const CLOCK_OP_GET_RATE: u64 = 0x1002;
pub const CLOCK_OP_ENABLE:   u64 = 0x1003;
pub const CLOCK_OP_DISABLE:  u64 = 0x1004;

/// Acquire a clock handle from device-manager. driver-only command;
/// the provider (cprman-driver) does not see this.
///
/// Request:  msg = [CMD_GET_CLOCK_HANDLE, controller_phandle, clock_id, 0]
/// Response: msg = [status, handle_id, 0, 0]
///   status: CLOCK_OK on success, CLOCK_ERR_* otherwise.
///   handle_id: opaque u32 the driver passes to CLOCK_OP_* later.
pub const CMD_GET_CLOCK_HANDLE: u64 = 0x1000;

/// Reply status codes. Word 0 of the reply on either leg.
pub const CLOCK_OK:                  u64 = 0;
pub const CLOCK_ERR_NOT_SUPPORTED:   u64 = 1;
pub const CLOCK_ERR_OUT_OF_RANGE:    u64 = 2;
pub const CLOCK_ERR_HARDWARE:        u64 = 3;
pub const CLOCK_ERR_BAD_OP:          u64 = 4;
/// device-manager-only: the requested controller_phandle is not a
/// known clock provider in this build (no driver registered for it).
pub const CLOCK_ERR_NO_PROVIDER:     u64 = 5;
/// device-manager-only: the binding table is full (32 entries today;
/// see device-manager's CLOCK_HANDLE_TABLE_CAP).
pub const CLOCK_ERR_TABLE_FULL:      u64 = 6;
/// device-manager-only: the handle_id is not owned by the calling
/// caller_token, or the handle was never allocated.
pub const CLOCK_ERR_INVALID_HANDLE:  u64 = 7;

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
    /// device-manager-side: no provider registered for the requested
    /// controller_phandle in this build.
    NoProvider,
    /// device-manager-side: binding table exhausted.
    TableFull,
    /// device-manager-side: handle_id not owned by caller, or never
    /// allocated.
    InvalidHandle,
    /// Client wrapper: IPC call failed (typically a bad endpoint
    /// handle or rights issue).
    IpcFailed,
}

impl ClockError {
    /// Reply word 0 encoding. Word 1 carries the offending id for
    /// NotSupported (so the receiver can print it). `IpcFailed` is a
    /// client-side-only synthesized state and never crosses the
    /// IPC boundary, so it has no on-the-wire encoding.
    pub fn status_code(self) -> u64 {
        match self {
            ClockError::NotSupported(_) => CLOCK_ERR_NOT_SUPPORTED,
            ClockError::OutOfRange      => CLOCK_ERR_OUT_OF_RANGE,
            ClockError::Hardware        => CLOCK_ERR_HARDWARE,
            ClockError::BadOp           => CLOCK_ERR_BAD_OP,
            ClockError::NoProvider      => CLOCK_ERR_NO_PROVIDER,
            ClockError::TableFull       => CLOCK_ERR_TABLE_FULL,
            ClockError::InvalidHandle   => CLOCK_ERR_INVALID_HANDLE,
            // IpcFailed is client-only; if we ever try to put it on
            // the wire, route through BadOp so the receiver still
            // gets a defined status word.
            ClockError::IpcFailed       => CLOCK_ERR_BAD_OP,
        }
    }

    /// Decode a reply word pair (status, value) back into the typed
    /// error. `value` is read for status codes that carry a payload
    /// (NotSupported uses value as the offending id).
    ///
    /// Returns `Ok(value)` when status is CLOCK_OK; the meaning of
    /// `value` is op-specific (actual rate for SET/GET_RATE, 0 for
    /// ENABLE/DISABLE/CMD_GET_CLOCK_HANDLE-handle_id).
    pub fn decode(status: u64, value: u64) -> Result<u64, ClockError> {
        match status {
            CLOCK_OK                  => Ok(value),
            CLOCK_ERR_NOT_SUPPORTED   => Err(ClockError::NotSupported(value as u32)),
            CLOCK_ERR_OUT_OF_RANGE    => Err(ClockError::OutOfRange),
            CLOCK_ERR_HARDWARE        => Err(ClockError::Hardware),
            CLOCK_ERR_BAD_OP          => Err(ClockError::BadOp),
            CLOCK_ERR_NO_PROVIDER     => Err(ClockError::NoProvider),
            CLOCK_ERR_TABLE_FULL      => Err(ClockError::TableFull),
            CLOCK_ERR_INVALID_HANDLE  => Err(ClockError::InvalidHandle),
            // Unknown status: best effort, treat as protocol bug.
            _                         => Err(ClockError::BadOp),
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
            CLOCK_ERR_NO_PROVIDER,
            CLOCK_ERR_TABLE_FULL,
            CLOCK_ERR_INVALID_HANDLE,
        ];
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                assert_ne!(codes[i], codes[j]);
            }
        }
    }

    #[test]
    fn cmd_get_clock_handle_distinct_from_op_codes() {
        // CMD_GET_CLOCK_HANDLE shares word 0 with the CLOCK_OP_*
        // opcodes on the driver→devmgr leg; device-manager's dispatch
        // must distinguish them.
        for op in [CLOCK_OP_SET_RATE, CLOCK_OP_GET_RATE, CLOCK_OP_ENABLE, CLOCK_OP_DISABLE] {
            assert_ne!(op, CMD_GET_CLOCK_HANDLE);
        }
    }

    #[test]
    fn clock_opcodes_disjoint_from_device_manager_commands() {
        // Bug-by-construction lockdown. device-manager dispatches by
        // matching msg[0] against both its own CMD_* commands
        // (CMD_CLAIM_DEVICE / CMD_PROBE_DEVICE / CMD_CLAIM_BY_ADDR)
        // and the clock CMD_/CLOCK_OP_* set; a value collision would
        // silently misroute. Caught a real M0c bug where
        // CLOCK_OP_SET_RATE = 1 == CMD_CLAIM_DEVICE.
        use crate::device::{CMD_CLAIM_DEVICE, CMD_PROBE_DEVICE, CMD_CLAIM_BY_ADDR};
        for op in [
            CLOCK_OP_SET_RATE, CLOCK_OP_GET_RATE,
            CLOCK_OP_ENABLE,   CLOCK_OP_DISABLE,
            CMD_GET_CLOCK_HANDLE,
        ] {
            for cmd in [CMD_CLAIM_DEVICE, CMD_PROBE_DEVICE, CMD_CLAIM_BY_ADDR] {
                assert_ne!(op, cmd, "clock opcode {:#x} collides with device-manager cmd {:#x}", op, cmd);
            }
        }
    }

    #[test]
    fn error_to_status_round_trip() {
        // Every error variant that can cross the wire round-trips
        // via decode(status, value).
        let cases = [
            (ClockError::OutOfRange,            CLOCK_ERR_OUT_OF_RANGE,    0),
            (ClockError::Hardware,              CLOCK_ERR_HARDWARE,        0),
            (ClockError::BadOp,                 CLOCK_ERR_BAD_OP,          0),
            (ClockError::NoProvider,            CLOCK_ERR_NO_PROVIDER,     0),
            (ClockError::TableFull,             CLOCK_ERR_TABLE_FULL,      0),
            (ClockError::InvalidHandle,         CLOCK_ERR_INVALID_HANDLE,  0),
        ];
        for (err, expected_status, expected_value) in cases {
            assert_eq!(err.status_code(), expected_status);
            assert_eq!(ClockError::decode(expected_status, expected_value), Err(err));
        }
        // NotSupported carries the offending id through value.
        assert_eq!(ClockError::NotSupported(42).status_code(), CLOCK_ERR_NOT_SUPPORTED);
        assert_eq!(
            ClockError::decode(CLOCK_ERR_NOT_SUPPORTED, 42),
            Err(ClockError::NotSupported(42)),
        );
    }

    #[test]
    fn decode_ok_returns_value() {
        // CLOCK_OK passes value through (actual rate for SET/GET_RATE,
        // handle_id for CMD_GET_CLOCK_HANDLE).
        assert_eq!(ClockError::decode(CLOCK_OK, 200_000_000), Ok(200_000_000));
        assert_eq!(ClockError::decode(CLOCK_OK, 0), Ok(0));
    }

    #[test]
    fn ipc_failed_routes_through_bad_op_on_wire() {
        // IpcFailed is client-side only; if it ever crosses the wire
        // (a bug), the encoding must be a defined value rather than
        // garbage. BadOp is the chosen catch-all.
        assert_eq!(ClockError::IpcFailed.status_code(), CLOCK_ERR_BAD_OP);
    }
}
