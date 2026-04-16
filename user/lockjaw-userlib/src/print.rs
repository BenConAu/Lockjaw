use crate::syscall::{putc, sys_call};

pub fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

/// Print via IPC to a UART server endpoint. Each character is a sys_call,
/// which binds/releases the given Reply object once per byte.
pub fn ipc_puts(ep: u64, reply: u64, s: &str) {
    for b in s.bytes() {
        let _ = sys_call(ep, reply, b as u64, 0, 0, 0);
    }
}
