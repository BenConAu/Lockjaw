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
use crate::syscall::{sys_call_ret4, sys_receive_ret4, sys_reply};
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

// ---------------------------------------------------------------------------
// Server side — used by clock-provider drivers (cprman et al.).
//
// The wire shape mirrors what the client sends through device-manager:
//   msg = [op, clock_id_raw, arg, _]
//   reply = [status, value, _, _]
//     where status is CLOCK_OK on success (value = result) or one of
//     CLOCK_ERR_* with the value slot carrying the failure-specific
//     payload (e.g., NotSupported echoes the requested clock_id).
//
// The framework owns IPC framing + error encoding. Drivers implement
// `ClockEngine::dispatch` with the per-leaf MMIO work.
// ---------------------------------------------------------------------------

/// Per-driver dispatch policy for clock ops.
///
/// `dispatch` translates one request into a `Result<u64, ClockError>`.
/// The framework encodes the result into the IPC reply: `Ok(v)`
/// produces `(CLOCK_OK, v, 0, 0)`; `Err(NotSupported(id))` produces
/// `(CLOCK_ERR_NOT_SUPPORTED, id as u64, 0, 0)`; any other `Err`
/// produces `(e.status_code(), 0, 0, 0)`.
///
/// Implementors of this trait don't see receive/reply syscalls or
/// status-code constants — the framework owns those. Driver focus
/// stays on per-clock-leaf MMIO sequencing.
pub trait ClockEngine {
    /// Dispatch one clock operation. `clock_id_raw` is the on-the-wire
    /// per-controller leaf identifier (e.g. `BCM2835_CLOCK_EMMC2 = 51`
    /// for the Pi 4B's CPRMAN). Pass to `ClockId::try_from_u32` if a
    /// typed decode is wanted; returning `Err(NotSupported(id))`
    /// surfaces unknown ids without losing the id in the log line.
    fn dispatch(&mut self, op: u64, clock_id_raw: u32, arg: u64) -> Result<u64, ClockError>;
}

/// Drive the clock-provider server loop on `server_ep`. Receives
/// `[op, clock_id_raw, arg, _]` requests, calls
/// `engine.dispatch(op, clock_id_raw, arg)`, and encodes the result
/// into a reply.
///
/// IPC-receive failures are skipped (continue the loop) rather than
/// exited — a transient receive error shouldn't take the clock
/// provider offline. `sys_reply` failures are similarly discarded;
/// the next request will reset the channel state.
///
/// Never returns: clock providers serve for the lifetime of the
/// driver process.
pub fn run_clock_server<E: ClockEngine>(engine: &mut E, server_ep: EndpointHandle) -> ! {
    loop {
        let msg = match sys_receive_ret4(server_ep) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let op = msg[0];
        let clock_id_raw = msg[1] as u32;
        let arg = msg[2];

        let _ = match engine.dispatch(op, clock_id_raw, arg) {
            Ok(value) => sys_reply(CLOCK_OK, value, 0, 0),
            Err(ClockError::NotSupported(id)) =>
                sys_reply(CLOCK_ERR_NOT_SUPPORTED, id as u64, 0, 0),
            Err(e) => sys_reply(e.status_code(), 0, 0, 0),
        };
    }
}
