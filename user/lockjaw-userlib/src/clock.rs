/// Typed client wrapper for clock operations brokered through the
/// device-manager.
///
/// Per `docs/book-of-lockjaw/03-non-virtualizable-hardware.md`,
/// drivers do **not** receive a direct handle to the clock provider
/// (cprman-driver et al.). Instead they call device-manager:
/// `acquire` exchanges (controller_phandle, clock_id) for an opaque
/// `handle_id`, and subsequent `set_rate` / `get_rate` / `enable` /
/// `disable` calls present the `handle_id` so device-manager can
/// look up the binding (scoped by the driver's caller_token) and
/// forward to the provider.
///
/// The wrapper packs IPC messages and decodes the typed
/// `ClockError` from the reply word; callers `match` instead of
/// touching opcodes or status constants.

pub use lockjaw_types::clock::*;
use crate::syscall::sys_call_ret4;
use crate::handle::{EndpointHandle, ReplyHandle};

/// Client-side handle for one clock leaf, brokered through
/// device-manager. Hold one of these per (controller, clock_id) the
/// driver needs.
///
/// Construction goes through `acquire` so the device-manager binding
/// is established before any op can be issued.
pub struct ClockClient {
    devmgr: EndpointHandle,
    reply: ReplyHandle,
    handle_id: u32,
}

impl ClockClient {
    /// Acquire a clock binding from device-manager. The DTB phandle
    /// names the controller node; the clock_id is the per-controller
    /// leaf identifier (e.g., `BCM2835_CLOCK_EMMC2 = 51` for the
    /// Pi 4B's CPRMAN).
    ///
    /// Returns `NoProvider` if device-manager has no driver
    /// registered for `controller_phandle`, `TableFull` if the
    /// binding table is exhausted, or other `ClockError` per the
    /// IPC reply.
    pub fn acquire(
        devmgr: EndpointHandle,
        reply: ReplyHandle,
        controller_phandle: u32,
        clock_id: u32,
    ) -> Result<Self, ClockError> {
        let msg = sys_call_ret4(
            devmgr,
            reply,
            CMD_GET_CLOCK_HANDLE,
            controller_phandle as u64,
            clock_id as u64,
            0,
        ).map_err(|_| ClockError::IpcFailed)?;
        let handle_id = ClockError::decode(msg[0], msg[1])? as u32;
        Ok(Self { devmgr, reply, handle_id })
    }

    /// Program the clock to `target_hz`. Returns the actual rate the
    /// hardware will produce (may differ from target due to divider
    /// quantization — see `compute_divider` for CPRMAN semantics).
    pub fn set_rate(&self, target_hz: u64) -> Result<u64, ClockError> {
        self.op(CLOCK_OP_SET_RATE, target_hz)
    }

    /// Read the current rate the clock is producing.
    pub fn get_rate(&self) -> Result<u64, ClockError> {
        self.op(CLOCK_OP_GET_RATE, 0)
    }

    /// Enable (ungate) the clock.
    pub fn enable(&self) -> Result<(), ClockError> {
        self.op(CLOCK_OP_ENABLE, 0).map(|_| ())
    }

    /// Disable (gate) the clock.
    pub fn disable(&self) -> Result<(), ClockError> {
        self.op(CLOCK_OP_DISABLE, 0).map(|_| ())
    }

    fn op(&self, op: u64, arg: u64) -> Result<u64, ClockError> {
        let msg = sys_call_ret4(
            self.devmgr,
            self.reply,
            op,
            self.handle_id as u64,
            arg,
            0,
        ).map_err(|_| ClockError::IpcFailed)?;
        ClockError::decode(msg[0], msg[1])
    }
}
