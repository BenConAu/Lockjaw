/// Display DDI server loop, engine trait, and client wrapper.
///
/// Hardware display drivers implement `DisplayEngine`. The reusable
/// `run_display_server()` handles IPC dispatch, session state, and
/// buffer export. Clients use `DisplayClient` for typed access.

// Re-export DDI types so drivers and clients can import from one place.
pub use lockjaw_types::display::*;
use crate::syscall::*;

// ---------------------------------------------------------------------------
// BufferInfo (client-side result from alloc_buffer)
// ---------------------------------------------------------------------------

/// Buffer allocation result returned to clients.
#[derive(Clone, Copy, Debug)]
pub struct BufferInfo {
    /// Exported handle in the client's handle table (for sys_map_pages
    /// and for referencing in set_mode/set_scanout).
    pub handle: u32,
    /// Bytes per row.
    pub stride: u32,
    /// Total buffer size in bytes.
    pub size: u32,
}

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// Tracks the single active display session (v1: max 1).
struct SessionState {
    active: Option<u32>,
    next_id: u32,
}

impl SessionState {
    const fn new() -> Self {
        Self { active: None, next_id: 0 }
    }

    fn create(&mut self) -> Result<u32, DisplayError> {
        if self.active.is_some() {
            return Err(DisplayError::SessionBusy);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.active = Some(id);
        Ok(id)
    }

    fn validate(&self, session: u32) -> Result<(), DisplayError> {
        match self.active {
            Some(id) if id == session => Ok(()),
            _ => Err(DisplayError::InvalidSession),
        }
    }

    fn release(&mut self, session: u32) -> Result<(), DisplayError> {
        self.validate(session)?;
        self.active = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BufferTracker
// ---------------------------------------------------------------------------

const MAX_BUFFERS: usize = 8;

/// Maps client-space handles to engine-space handles.
/// Populated on AllocBuffer, looked up on SetMode/SetScanout.
struct BufferTracker {
    slots: [Option<BufferSlot>; MAX_BUFFERS],
}

#[derive(Clone, Copy)]
struct BufferSlot {
    client_handle: u32,
    engine_handle: u64,
}

impl BufferTracker {
    const fn new() -> Self {
        Self { slots: [None; MAX_BUFFERS] }
    }

    /// Record a new buffer mapping. Returns false if full.
    fn track(&mut self, client_handle: u32, engine_handle: u64) -> bool {
        for slot in self.slots.iter_mut() {
            if slot.is_none() {
                *slot = Some(BufferSlot { client_handle, engine_handle });
                return true;
            }
        }
        false
    }

    /// Check if there is room for another buffer.
    fn has_capacity(&self) -> bool {
        self.slots.iter().any(|s| s.is_none())
    }

    /// Free all tracked buffers through the engine, then clear the
    /// tracker. Prevents cross-client handle aliasing and avoids
    /// orphaning engine-side allocations.
    fn release_all(&mut self, engine: &mut impl DisplayEngine) {
        for slot in self.slots.iter_mut() {
            if let Some(s) = slot.take() {
                engine.free_buffer(s.engine_handle);
            }
        }
    }

    /// Translate a client handle to the engine handle.
    fn translate(&self, client_handle: u32) -> Option<u64> {
        for slot in self.slots.iter() {
            if let Some(s) = slot {
                if s.client_handle == client_handle {
                    return Some(s.engine_handle);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// DisplayEngine trait
// ---------------------------------------------------------------------------

/// Display engine trait -- implemented by hardware-specific drivers.
/// The OS provides the server loop (`run_display_server`); the driver
/// provides the hardware logic via this trait.
///
/// Engine methods receive engine-space handles (translated by the
/// server loop's BufferTracker). The engine does not call
/// sys_export_handle -- the server loop handles the export chain.
pub trait DisplayEngine {
    /// Number of display modes supported. Modes are ordered by
    /// preference (index 0 = preferred).
    fn mode_count(&self) -> u32;

    /// Get mode descriptor by index. Returns None if out of range.
    fn get_mode(&self, index: u32) -> Option<ModeInfo>;

    /// Prepare the engine for a new session. The server loop owns
    /// session identity — the engine just validates readiness.
    fn create_session(&mut self) -> Result<(), DisplayError>;

    /// Allocate a scanout-compatible buffer.
    /// Returns (pageset_handle, stride, size_bytes).
    /// The pageset_handle is the driver's kernel handle (u64).
    fn alloc_buffer(&mut self, session: u32, width: u32, height: u32, format: u32)
        -> Result<(u64, u32, u32), DisplayError>;

    /// Full modeset: set display timing/resolution and start scanning
    /// the given buffer. buffer_handle is in engine-space (translated
    /// by the server loop).
    fn set_mode(&mut self, session: u32, mode_index: u32, buffer_handle: u64)
        -> Result<(), DisplayError>;

    /// Page flip: change which buffer is displayed at the current mode.
    /// No modeset. Returns NotConfigured if no mode has been set.
    fn set_scanout(&mut self, session: u32, buffer_handle: u64)
        -> Result<(), DisplayError>;

    /// Free a previously allocated buffer. Called by the server loop
    /// on export failure or session teardown. Implementations should
    /// release internal tracking. Physical page deallocation depends
    /// on kernel support (sys_free_pages does not yet exist).
    fn free_buffer(&mut self, buffer_handle: u64);

    /// Release the session. Display keeps showing the last buffer.
    fn release_session(&mut self, session: u32) -> Result<(), DisplayError>;
}

// ---------------------------------------------------------------------------
// run_display_server
// ---------------------------------------------------------------------------

/// Run the display server loop. Receives on `client_ep`, decodes
/// DisplayRequest, dispatches to the engine trait, handles session
/// state and buffer export chain. Never returns.
///
/// **sys_export_handle timing**: the export happens after sys_receive
/// and before sys_reply, which is the valid window where the caller
/// is bound to this server thread.
pub fn run_display_server(
    engine: &mut impl DisplayEngine,
    client_ep: u64,
) -> ! {
    let mut session = SessionState::new();
    let mut buffers = BufferTracker::new();

    loop {
        let msg = match sys_receive_ret4(client_ep) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let req = match DisplayRequest::decode(msg) {
            Some(r) => r,
            None => {
                let resp = DisplayResponse::Err(DisplayError::UnknownCommand);
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
                continue;
            }
        };

        match req {
            DisplayRequest::ListModes => {
                let resp = DisplayResponse::ModeCount(engine.mode_count());
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
            }

            DisplayRequest::GetMode { index } => {
                let resp = match engine.get_mode(index) {
                    Some(mode) => DisplayResponse::Mode(mode),
                    None => DisplayResponse::Err(DisplayError::InvalidMode),
                };
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
            }

            DisplayRequest::CreateSession => {
                let resp = match session.create() {
                    Ok(id) => {
                        match engine.create_session() {
                            Ok(()) => DisplayResponse::Session { id },
                            Err(e) => {
                                session.active = None;
                                DisplayResponse::Err(e)
                            }
                        }
                    }
                    Err(e) => DisplayResponse::Err(e),
                };
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
            }

            DisplayRequest::AllocBuffer { session: sid, width, height, format } => {
                let resp = if session.validate(sid).is_err() {
                    DisplayResponse::Err(DisplayError::InvalidSession)
                } else if !buffers.has_capacity() {
                    // Check capacity before allocating to avoid leaking
                    // engine resources on a tracking failure.
                    DisplayResponse::Err(DisplayError::AllocFailed)
                } else {
                    match engine.alloc_buffer(sid, width, height, format) {
                        Ok((ps_handle, stride, size)) => {
                            // Export pageset into the bound caller's handle table.
                            // Valid window: after sys_receive, before sys_reply.
                            match sys_export_handle(ps_handle) {
                                Ok(client_idx) => {
                                    let client_handle = client_idx as u32;
                                    // Capacity was checked above, so this cannot fail.
                                    buffers.track(client_handle, ps_handle);
                                    DisplayResponse::Buffer {
                                        handle: client_handle,
                                        stride,
                                        size,
                                    }
                                }
                                Err(_) => {
                                    engine.free_buffer(ps_handle);
                                    DisplayResponse::Err(DisplayError::AllocFailed)
                                }
                            }
                        }
                        Err(e) => DisplayResponse::Err(e),
                    }
                };
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
            }

            DisplayRequest::SetMode { session: sid, mode_index, buffer } => {
                let resp = if session.validate(sid).is_err() {
                    DisplayResponse::Err(DisplayError::InvalidSession)
                } else {
                    match buffers.translate(buffer) {
                        Some(engine_handle) => {
                            match engine.set_mode(sid, mode_index, engine_handle) {
                                Ok(()) => DisplayResponse::Ok,
                                Err(e) => DisplayResponse::Err(e),
                            }
                        }
                        None => DisplayResponse::Err(DisplayError::InvalidBuffer),
                    }
                };
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
            }

            DisplayRequest::SetScanout { session: sid, buffer } => {
                let resp = if session.validate(sid).is_err() {
                    DisplayResponse::Err(DisplayError::InvalidSession)
                } else {
                    match buffers.translate(buffer) {
                        Some(engine_handle) => {
                            match engine.set_scanout(sid, engine_handle) {
                                Ok(()) => DisplayResponse::Ok,
                                Err(e) => DisplayResponse::Err(e),
                            }
                        }
                        None => DisplayResponse::Err(DisplayError::InvalidBuffer),
                    }
                };
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
            }

            DisplayRequest::ReleaseSession { session: sid } => {
                let resp = if session.validate(sid).is_err() {
                    DisplayResponse::Err(DisplayError::InvalidSession)
                } else {
                    match engine.release_session(sid) {
                        Ok(()) => {
                            session.release(sid).ok();
                            buffers.release_all(engine);
                            DisplayResponse::Ok
                        }
                        Err(e) => DisplayResponse::Err(e),
                    }
                };
                let r = resp.encode();
                sys_reply(r[0], r[1], r[2], r[3]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DisplayClient
// ---------------------------------------------------------------------------

/// Client-side display wrapper. Hides IPC message packing behind
/// typed methods. Each method does one synchronous IPC call.
pub struct DisplayClient {
    endpoint: u64,
    reply: u64,
}

impl DisplayClient {
    /// Create a new display client targeting the given endpoint.
    /// `reply` is a Reply handle allocated by the client thread.
    pub fn new(endpoint: u64, reply: u64) -> Self {
        Self { endpoint, reply }
    }

    /// Query the number of supported display modes.
    pub fn list_modes(&self) -> Result<u32, DisplayError> {
        let req = DisplayRequest::ListModes.encode();
        let msg = self.call(req)?;
        DisplayResponse::decode_mode_count(msg)
    }

    /// Get mode details by index. Index 0 = preferred mode.
    pub fn get_mode(&self, index: u32) -> Result<ModeInfo, DisplayError> {
        let req = DisplayRequest::GetMode { index }.encode();
        let msg = self.call(req)?;
        DisplayResponse::decode_mode(msg)
    }

    /// Create a display session. Returns session ID.
    pub fn create_session(&self) -> Result<u32, DisplayError> {
        let req = DisplayRequest::CreateSession.encode();
        let msg = self.call(req)?;
        DisplayResponse::decode_session(msg)
    }

    /// Allocate a scanout-compatible buffer from the driver.
    pub fn alloc_buffer(&self, session: u32, width: u32, height: u32, format: u32)
        -> Result<BufferInfo, DisplayError>
    {
        let req = DisplayRequest::AllocBuffer { session, width, height, format }.encode();
        let msg = self.call(req)?;
        let (handle, stride, size) = DisplayResponse::decode_buffer(msg)?;
        Ok(BufferInfo { handle, stride, size })
    }

    /// Full modeset: set display timing/resolution and start scanning
    /// the given buffer.
    pub fn set_mode(&self, session: u32, mode_index: u32, buffer: u32)
        -> Result<(), DisplayError>
    {
        let req = DisplayRequest::SetMode { session, mode_index, buffer }.encode();
        let msg = self.call(req)?;
        DisplayResponse::decode_ok(msg)
    }

    /// Page flip: change which buffer is displayed at the current mode.
    pub fn set_scanout(&self, session: u32, buffer: u32) -> Result<(), DisplayError> {
        let req = DisplayRequest::SetScanout { session, buffer }.encode();
        let msg = self.call(req)?;
        DisplayResponse::decode_ok(msg)
    }

    /// Release the display session. Display keeps showing last buffer.
    pub fn release_session(&self, session: u32) -> Result<(), DisplayError> {
        let req = DisplayRequest::ReleaseSession { session }.encode();
        let msg = self.call(req)?;
        DisplayResponse::decode_ok(msg)
    }

    /// Send request and receive reply via IPC.
    fn call(&self, req: [u64; 4]) -> Result<[u64; 4], DisplayError> {
        sys_call_ret4(self.endpoint, self.reply, req[0], req[1], req[2], req[3])
            .map_err(|_| DisplayError::IpcFailed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;
    use core::cell::RefCell;

    // --- Mock engine ---

    struct MockEngine {
        freed: RefCell<Vec<u64>>,
        session_active: bool,
        alloc_fail: bool,
        release_fail: bool,
    }

    impl MockEngine {
        fn new() -> Self {
            Self {
                freed: RefCell::new(Vec::new()),
                session_active: false,
                alloc_fail: false,
                release_fail: false,
            }
        }

        fn freed_handles(&self) -> Vec<u64> {
            self.freed.borrow().clone()
        }
    }

    impl DisplayEngine for MockEngine {
        fn mode_count(&self) -> u32 { 2 }
        fn get_mode(&self, index: u32) -> Option<ModeInfo> {
            if index < 2 {
                Some(ModeInfo { width: 320, height: 240, format: PIXEL_FORMAT_XRGB8888, refresh_millihz: 60000 })
            } else {
                None
            }
        }
        fn create_session(&mut self) -> Result<(), DisplayError> {
            self.session_active = true;
            Ok(())
        }
        fn alloc_buffer(&mut self, _session: u32, _w: u32, _h: u32, _fmt: u32)
            -> Result<(u64, u32, u32), DisplayError>
        {
            if self.alloc_fail { return Err(DisplayError::AllocFailed); }
            Ok((0x1000, 1280, 307200))
        }
        fn set_mode(&mut self, _session: u32, _mode: u32, _buf: u64) -> Result<(), DisplayError> {
            Ok(())
        }
        fn set_scanout(&mut self, _session: u32, _buf: u64) -> Result<(), DisplayError> {
            Ok(())
        }
        fn free_buffer(&mut self, handle: u64) {
            self.freed.borrow_mut().push(handle);
        }
        fn release_session(&mut self, _session: u32) -> Result<(), DisplayError> {
            if self.release_fail { return Err(DisplayError::InvalidSession); }
            self.session_active = false;
            Ok(())
        }
    }

    // --- SessionState tests ---

    #[test]
    fn session_create_and_validate() {
        let mut s = SessionState::new();
        let id = s.create().unwrap();
        assert!(s.validate(id).is_ok());
    }

    #[test]
    fn session_busy_rejects_second() {
        let mut s = SessionState::new();
        s.create().unwrap();
        assert_eq!(s.create(), Err(DisplayError::SessionBusy));
    }

    #[test]
    fn session_validate_wrong_id() {
        let mut s = SessionState::new();
        let id = s.create().unwrap();
        assert_eq!(s.validate(id + 1), Err(DisplayError::InvalidSession));
    }

    #[test]
    fn session_validate_none_active() {
        let s = SessionState::new();
        assert_eq!(s.validate(0), Err(DisplayError::InvalidSession));
    }

    #[test]
    fn session_release_allows_new() {
        let mut s = SessionState::new();
        let id = s.create().unwrap();
        s.release(id).unwrap();
        // Can create a new session after release
        let id2 = s.create().unwrap();
        assert_ne!(id, id2);
        assert!(s.validate(id2).is_ok());
    }

    #[test]
    fn session_release_wrong_id() {
        let mut s = SessionState::new();
        let id = s.create().unwrap();
        assert_eq!(s.release(id + 1), Err(DisplayError::InvalidSession));
        // Original session still active
        assert!(s.validate(id).is_ok());
    }

    // --- BufferTracker tests ---

    #[test]
    fn tracker_track_and_translate() {
        let mut t = BufferTracker::new();
        assert!(t.track(3, 0xA000));
        assert_eq!(t.translate(3), Some(0xA000));
    }

    #[test]
    fn tracker_translate_unknown() {
        let t = BufferTracker::new();
        assert_eq!(t.translate(99), None);
    }

    #[test]
    fn tracker_capacity() {
        let mut t = BufferTracker::new();
        for i in 0..MAX_BUFFERS {
            assert!(t.has_capacity());
            assert!(t.track(i as u32, i as u64 * 0x1000));
        }
        assert!(!t.has_capacity());
        assert!(!t.track(99, 0xFFFF));
    }

    #[test]
    fn tracker_release_all_calls_free_buffer() {
        let mut t = BufferTracker::new();
        let mut engine = MockEngine::new();
        t.track(0, 0xA000);
        t.track(1, 0xB000);
        t.track(2, 0xC000);

        t.release_all(&mut engine);

        let mut freed = engine.freed_handles();
        freed.sort();
        assert_eq!(freed, std::vec![0xA000, 0xB000, 0xC000]);
        // Tracker is now empty
        assert_eq!(t.translate(0), None);
        assert_eq!(t.translate(1), None);
        assert_eq!(t.translate(2), None);
        assert!(t.has_capacity());
    }

    #[test]
    fn tracker_release_all_empty_is_noop() {
        let mut t = BufferTracker::new();
        let mut engine = MockEngine::new();
        t.release_all(&mut engine);
        assert!(engine.freed_handles().is_empty());
    }
}
