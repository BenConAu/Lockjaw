/// Display DDI (Device Driver Interface) types.
///
/// Defines the OS-owned protocol between display clients and display
/// engine drivers. Request/response enums encode to 4 u64 IPC words.
/// Hardware drivers implement the DisplayEngine trait; clients use
/// DisplayClient. Both share these types.

// ---------------------------------------------------------------------------
// Pixel formats
// ---------------------------------------------------------------------------

/// DRM fourcc for XRGB8888 = "XR24" = 0x34325258
pub const PIXEL_FORMAT_XRGB8888: u32 = 0x34325258;

// ---------------------------------------------------------------------------
// ModeInfo
// ---------------------------------------------------------------------------

/// Display mode descriptor. Modes are ordered by preference (first = preferred).
/// The index in the mode list is the mode identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeInfo {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub refresh_millihz: u32,
}

// ---------------------------------------------------------------------------
// DisplayError
// ---------------------------------------------------------------------------

/// Display DDI error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayError {
    /// Mode index out of range.
    InvalidMode,
    /// Session ID does not match the active session.
    InvalidSession,
    /// Buffer handle not recognized.
    InvalidBuffer,
    /// Buffer or resource allocation failed.
    AllocFailed,
    /// Operation requires a mode to be set first.
    NotConfigured,
    /// Another session is already active.
    SessionBusy,
    /// Unrecognized command tag.
    UnknownCommand,
    /// IPC transport failure (bad endpoint, server unreachable, etc.)
    IpcFailed,
}

impl DisplayError {
    /// Encode as a nonzero error code for IPC msg[0].
    pub fn code(self) -> u64 {
        match self {
            Self::InvalidMode => 1,
            Self::InvalidSession => 2,
            Self::InvalidBuffer => 3,
            Self::AllocFailed => 4,
            Self::NotConfigured => 5,
            Self::SessionBusy => 6,
            Self::UnknownCommand => 7,
            Self::IpcFailed => 8,
        }
    }

    /// Decode from an IPC error code. Returns None for 0 (success) or
    /// unknown codes.
    pub fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::InvalidMode),
            2 => Some(Self::InvalidSession),
            3 => Some(Self::InvalidBuffer),
            4 => Some(Self::AllocFailed),
            5 => Some(Self::NotConfigured),
            6 => Some(Self::SessionBusy),
            7 => Some(Self::UnknownCommand),
            8 => Some(Self::IpcFailed),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Dimension packing helpers
// ---------------------------------------------------------------------------

/// Pack two u32 values into one u64: high 32 bits = a, low 32 bits = b.
pub fn pack_u32_pair(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | (b as u64)
}

/// Unpack one u64 into two u32 values: (high, low).
pub fn unpack_u32_pair(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

// ---------------------------------------------------------------------------
// DisplayRequest
// ---------------------------------------------------------------------------

/// Convert a u64 IPC word to u32, returning None if upper bits are set.
/// Rejects out-of-range values instead of silently truncating.
fn to_u32(v: u64) -> Option<u32> {
    if v > u32::MAX as u64 { None } else { Some(v as u32) }
}

// Command tags (msg[0])
const CMD_LIST_MODES: u64 = 1;
const CMD_GET_MODE: u64 = 2;
const CMD_CREATE_SESSION: u64 = 3;
const CMD_ALLOC_BUFFER: u64 = 4;
const CMD_SET_MODE: u64 = 5;
const CMD_SET_SCANOUT: u64 = 6;
const CMD_RELEASE_SESSION: u64 = 7;

/// Display DDI request (client -> driver).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayRequest {
    /// Query the number of supported display modes.
    ListModes,
    /// Get mode details by index (0 = preferred).
    GetMode { index: u32 },
    /// Create a display session.
    CreateSession,
    /// Allocate a scanout-compatible buffer.
    AllocBuffer { session: u32, width: u32, height: u32, format: u32 },
    /// Full modeset: set resolution and start scanning a buffer.
    SetMode { session: u32, mode_index: u32, buffer: u32 },
    /// Page flip: change the displayed buffer without a modeset.
    SetScanout { session: u32, buffer: u32 },
    /// Release the display session.
    ReleaseSession { session: u32 },
}

impl DisplayRequest {
    /// Encode to 4-word IPC message.
    pub fn encode(self) -> [u64; 4] {
        match self {
            Self::ListModes => [CMD_LIST_MODES, 0, 0, 0],
            Self::GetMode { index } => [CMD_GET_MODE, index as u64, 0, 0],
            Self::CreateSession => [CMD_CREATE_SESSION, 0, 0, 0],
            Self::AllocBuffer { session, width, height, format } => [
                CMD_ALLOC_BUFFER,
                session as u64,
                pack_u32_pair(width, height),
                format as u64,
            ],
            Self::SetMode { session, mode_index, buffer } => [
                CMD_SET_MODE,
                session as u64,
                mode_index as u64,
                buffer as u64,
            ],
            Self::SetScanout { session, buffer } => [
                CMD_SET_SCANOUT,
                session as u64,
                buffer as u64,
                0,
            ],
            Self::ReleaseSession { session } => [
                CMD_RELEASE_SESSION,
                session as u64,
                0,
                0,
            ],
        }
    }

    /// Decode from 4-word IPC message. Returns None if the command tag
    /// is unrecognized or any u32 field has out-of-range upper bits set.
    pub fn decode(msg: [u64; 4]) -> Option<Self> {
        match msg[0] {
            CMD_LIST_MODES => Some(Self::ListModes),
            CMD_GET_MODE => Some(Self::GetMode { index: to_u32(msg[1])? }),
            CMD_CREATE_SESSION => Some(Self::CreateSession),
            CMD_ALLOC_BUFFER => {
                let (width, height) = unpack_u32_pair(msg[2]);
                Some(Self::AllocBuffer {
                    session: to_u32(msg[1])?,
                    width,
                    height,
                    format: to_u32(msg[3])?,
                })
            }
            CMD_SET_MODE => Some(Self::SetMode {
                session: to_u32(msg[1])?,
                mode_index: to_u32(msg[2])?,
                buffer: to_u32(msg[3])?,
            }),
            CMD_SET_SCANOUT => Some(Self::SetScanout {
                session: to_u32(msg[1])?,
                buffer: to_u32(msg[2])?,
            }),
            CMD_RELEASE_SESSION => Some(Self::ReleaseSession {
                session: to_u32(msg[1])?,
            }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// DisplayResponse
// ---------------------------------------------------------------------------

/// Display DDI response (driver -> client).
///
/// Response type is implied by the request -- the client knows what it asked.
/// msg[0] = 0 on success, nonzero error code on failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayResponse {
    /// Reply to ListModes: number of supported modes.
    ModeCount(u32),
    /// Reply to GetMode: mode descriptor.
    Mode(ModeInfo),
    /// Reply to CreateSession: session ID.
    Session { id: u32 },
    /// Reply to AllocBuffer: exported handle, stride, and size.
    Buffer { handle: u32, stride: u32, size: u32 },
    /// Generic success (SetMode, SetScanout, ReleaseSession).
    Ok,
    /// Error response for any command.
    Err(DisplayError),
}

impl DisplayResponse {
    /// Encode to 4-word IPC message.
    pub fn encode(self) -> [u64; 4] {
        match self {
            Self::ModeCount(count) => [0, count as u64, 0, 0],
            Self::Mode(m) => [
                0,
                pack_u32_pair(m.width, m.height),
                pack_u32_pair(m.refresh_millihz, m.format),
                0,
            ],
            Self::Session { id } => [0, id as u64, 0, 0],
            Self::Buffer { handle, stride, size } => [
                0,
                handle as u64,
                stride as u64,
                size as u64,
            ],
            Self::Ok => [0, 0, 0, 0],
            Self::Err(e) => [e.code(), 0, 0, 0],
        }
    }

    /// Decode a ModeCount response. Returns Err if msg[0] is nonzero.
    pub fn decode_mode_count(msg: [u64; 4]) -> Result<u32, DisplayError> {
        check_status(msg[0])?;
        Ok(msg[1] as u32)
    }

    /// Decode a Mode response. Returns Err if msg[0] is nonzero.
    pub fn decode_mode(msg: [u64; 4]) -> Result<ModeInfo, DisplayError> {
        check_status(msg[0])?;
        let (width, height) = unpack_u32_pair(msg[1]);
        let (refresh_millihz, format) = unpack_u32_pair(msg[2]);
        Ok(ModeInfo { width, height, format, refresh_millihz })
    }

    /// Decode a Session response. Returns Err if msg[0] is nonzero.
    pub fn decode_session(msg: [u64; 4]) -> Result<u32, DisplayError> {
        check_status(msg[0])?;
        Ok(msg[1] as u32)
    }

    /// Decode a Buffer response. Returns Err if msg[0] is nonzero.
    /// Returns (handle, stride, size).
    pub fn decode_buffer(msg: [u64; 4]) -> Result<(u32, u32, u32), DisplayError> {
        check_status(msg[0])?;
        Ok((msg[1] as u32, msg[2] as u32, msg[3] as u32))
    }

    /// Decode an Ok response. Returns Err if msg[0] is nonzero.
    pub fn decode_ok(msg: [u64; 4]) -> Result<(), DisplayError> {
        check_status(msg[0])
    }
}

/// Check the status word (msg[0]). 0 = success, nonzero = error.
fn check_status(status: u64) -> Result<(), DisplayError> {
    if status == 0 {
        Ok(())
    } else {
        Err(DisplayError::from_code(status).unwrap_or(DisplayError::UnknownCommand))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- pack/unpack ---

    #[test]
    fn pack_unpack_roundtrip() {
        assert_eq!(unpack_u32_pair(pack_u32_pair(320, 240)), (320, 240));
        assert_eq!(unpack_u32_pair(pack_u32_pair(0, 0)), (0, 0));
        assert_eq!(unpack_u32_pair(pack_u32_pair(u32::MAX, u32::MAX)), (u32::MAX, u32::MAX));
        assert_eq!(unpack_u32_pair(pack_u32_pair(1920, 1080)), (1920, 1080));
    }

    #[test]
    fn pack_no_overlap() {
        let packed = pack_u32_pair(0xAAAA_BBBB, 0xCCCC_DDDD);
        assert_eq!(packed, 0xAAAA_BBBB_CCCC_DDDD);
    }

    // --- ModeInfo ---

    #[test]
    fn mode_info_size() {
        assert_eq!(core::mem::size_of::<ModeInfo>(), 16);
    }

    // --- DisplayError ---

    #[test]
    fn error_code_roundtrip() {
        let errors = [
            DisplayError::InvalidMode,
            DisplayError::InvalidSession,
            DisplayError::InvalidBuffer,
            DisplayError::AllocFailed,
            DisplayError::NotConfigured,
            DisplayError::SessionBusy,
            DisplayError::UnknownCommand,
            DisplayError::IpcFailed,
        ];
        for e in errors {
            let code = e.code();
            assert_ne!(code, 0, "error code must be nonzero");
            assert_eq!(DisplayError::from_code(code), Some(e));
        }
    }

    #[test]
    fn error_from_code_zero_is_none() {
        assert_eq!(DisplayError::from_code(0), None);
    }

    #[test]
    fn error_from_code_unknown_is_none() {
        assert_eq!(DisplayError::from_code(999), None);
    }

    #[test]
    fn error_codes_are_unique() {
        let errors = [
            DisplayError::InvalidMode,
            DisplayError::InvalidSession,
            DisplayError::InvalidBuffer,
            DisplayError::AllocFailed,
            DisplayError::NotConfigured,
            DisplayError::SessionBusy,
            DisplayError::UnknownCommand,
            DisplayError::IpcFailed,
        ];
        for i in 0..errors.len() {
            for j in (i + 1)..errors.len() {
                assert_ne!(errors[i].code(), errors[j].code(),
                    "{:?} and {:?} have same code", errors[i], errors[j]);
            }
        }
    }

    // --- DisplayRequest encode/decode ---

    #[test]
    fn request_list_modes_roundtrip() {
        let req = DisplayRequest::ListModes;
        assert_eq!(DisplayRequest::decode(req.encode()), Some(req));
    }

    #[test]
    fn request_get_mode_roundtrip() {
        let req = DisplayRequest::GetMode { index: 5 };
        assert_eq!(DisplayRequest::decode(req.encode()), Some(req));
    }

    #[test]
    fn request_create_session_roundtrip() {
        let req = DisplayRequest::CreateSession;
        assert_eq!(DisplayRequest::decode(req.encode()), Some(req));
    }

    #[test]
    fn request_alloc_buffer_roundtrip() {
        let req = DisplayRequest::AllocBuffer {
            session: 0,
            width: 640,
            height: 480,
            format: PIXEL_FORMAT_XRGB8888,
        };
        assert_eq!(DisplayRequest::decode(req.encode()), Some(req));
    }

    #[test]
    fn request_set_mode_roundtrip() {
        let req = DisplayRequest::SetMode {
            session: 1,
            mode_index: 0,
            buffer: 3,
        };
        assert_eq!(DisplayRequest::decode(req.encode()), Some(req));
    }

    #[test]
    fn request_set_scanout_roundtrip() {
        let req = DisplayRequest::SetScanout {
            session: 0,
            buffer: 7,
        };
        assert_eq!(DisplayRequest::decode(req.encode()), Some(req));
    }

    #[test]
    fn request_release_session_roundtrip() {
        let req = DisplayRequest::ReleaseSession { session: 2 };
        assert_eq!(DisplayRequest::decode(req.encode()), Some(req));
    }

    #[test]
    fn request_decode_unknown_command() {
        assert_eq!(DisplayRequest::decode([99, 0, 0, 0]), None);
        assert_eq!(DisplayRequest::decode([0, 0, 0, 0]), None);
    }

    #[test]
    fn request_decode_rejects_out_of_range_u32() {
        let overflow = 0x1_0000_0000u64; // u32::MAX + 1
        // GetMode: index overflow
        assert_eq!(DisplayRequest::decode([CMD_GET_MODE, overflow, 0, 0]), None);
        // SetMode: session overflow
        assert_eq!(DisplayRequest::decode([CMD_SET_MODE, overflow, 0, 0]), None);
        // SetMode: mode_index overflow
        assert_eq!(DisplayRequest::decode([CMD_SET_MODE, 0, overflow, 0]), None);
        // SetMode: buffer overflow
        assert_eq!(DisplayRequest::decode([CMD_SET_MODE, 0, 0, overflow]), None);
        // AllocBuffer: session overflow
        assert_eq!(DisplayRequest::decode([CMD_ALLOC_BUFFER, overflow, 0, 0]), None);
        // AllocBuffer: format overflow
        assert_eq!(DisplayRequest::decode([CMD_ALLOC_BUFFER, 0, 0, overflow]), None);
        // SetScanout: session overflow
        assert_eq!(DisplayRequest::decode([CMD_SET_SCANOUT, overflow, 0, 0]), None);
        // SetScanout: buffer overflow
        assert_eq!(DisplayRequest::decode([CMD_SET_SCANOUT, 0, overflow, 0]), None);
        // ReleaseSession: session overflow
        assert_eq!(DisplayRequest::decode([CMD_RELEASE_SESSION, overflow, 0, 0]), None);
    }

    // --- DisplayResponse encode/decode ---

    #[test]
    fn response_mode_count_roundtrip() {
        let msg = DisplayResponse::ModeCount(3).encode();
        assert_eq!(DisplayResponse::decode_mode_count(msg), Ok(3));
    }

    #[test]
    fn response_mode_roundtrip() {
        let mode = ModeInfo {
            width: 1024,
            height: 768,
            format: PIXEL_FORMAT_XRGB8888,
            refresh_millihz: 60000,
        };
        let msg = DisplayResponse::Mode(mode).encode();
        assert_eq!(DisplayResponse::decode_mode(msg), Ok(mode));
    }

    #[test]
    fn response_session_roundtrip() {
        let msg = DisplayResponse::Session { id: 42 }.encode();
        assert_eq!(DisplayResponse::decode_session(msg), Ok(42));
    }

    #[test]
    fn response_buffer_roundtrip() {
        let msg = DisplayResponse::Buffer {
            handle: 5,
            stride: 2560,
            size: 1228800,
        }.encode();
        assert_eq!(DisplayResponse::decode_buffer(msg), Ok((5, 2560, 1228800)));
    }

    #[test]
    fn response_ok_roundtrip() {
        let msg = DisplayResponse::Ok.encode();
        assert_eq!(DisplayResponse::decode_ok(msg), Ok(()));
    }

    #[test]
    fn response_err_roundtrip() {
        for e in [
            DisplayError::InvalidMode,
            DisplayError::AllocFailed,
            DisplayError::SessionBusy,
        ] {
            let msg = DisplayResponse::Err(e).encode();
            assert_eq!(DisplayResponse::decode_ok(msg), Err(e));
            assert_eq!(DisplayResponse::decode_mode_count(msg), Err(e));
        }
    }

    #[test]
    fn response_err_unknown_code_becomes_unknown_command() {
        // An unrecognized error code falls back to UnknownCommand
        let msg = [255, 0, 0, 0];
        assert_eq!(DisplayResponse::decode_ok(msg), Err(DisplayError::UnknownCommand));
    }

    // --- Wire format spot checks ---

    #[test]
    fn alloc_buffer_wire_format() {
        let req = DisplayRequest::AllocBuffer {
            session: 1,
            width: 320,
            height: 240,
            format: PIXEL_FORMAT_XRGB8888,
        };
        let msg = req.encode();
        assert_eq!(msg[0], CMD_ALLOC_BUFFER);
        assert_eq!(msg[1], 1); // session
        assert_eq!(msg[2], pack_u32_pair(320, 240)); // packed dimensions
        assert_eq!(msg[3], PIXEL_FORMAT_XRGB8888 as u64); // format
    }

    #[test]
    fn mode_response_wire_format() {
        let mode = ModeInfo {
            width: 640,
            height: 480,
            format: PIXEL_FORMAT_XRGB8888,
            refresh_millihz: 60000,
        };
        let msg = DisplayResponse::Mode(mode).encode();
        assert_eq!(msg[0], 0); // success
        assert_eq!(unpack_u32_pair(msg[1]), (640, 480));
        assert_eq!(unpack_u32_pair(msg[2]), (60000, PIXEL_FORMAT_XRGB8888));
    }
}
