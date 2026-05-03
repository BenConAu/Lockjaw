// Re-export from lockjaw-types — single source of truth.
pub use lockjaw_types::process::{ProcessMapping, PROCESS_MAP_FLAG_EXECUTABLE};

/// Backwards-compatible alias.
pub const FLAG_EXECUTABLE: u64 = PROCESS_MAP_FLAG_EXECUTABLE;
