use crate::syscall::{sys_call, sys_debug_puts};
use crate::handle::{EndpointHandle, ReplyHandle};

pub fn puts(s: &str) {
    sys_debug_puts(s.as_bytes());
}

/// Format `n` as a decimal number and emit atomically. One syscall;
/// digits cannot be interleaved with other threads' debug output.
pub fn put_decimal(mut n: u64) {
    if n == 0 {
        sys_debug_puts(b"0");
        return;
    }
    // Max u64 = 20 digits.
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    sys_debug_puts(&buf[i..]);
}

/// Format `n` as `0x<hex>` and emit atomically. Lowercase hex.
pub fn put_hex(mut n: u64) {
    // "0x" prefix + up to 16 hex digits = 18 bytes.
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    if n == 0 {
        buf[2] = b'0';
        sys_debug_puts(&buf[..3]);
        return;
    }
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        let d = (n & 0xF) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        n >>= 4;
    }
    // Move "0x" right next to the digit run: copy the digits down.
    let digits = buf.len() - i;
    for j in 0..digits {
        buf[2 + j] = buf[i + j];
    }
    sys_debug_puts(&buf[..2 + digits]);
}

/// Print via IPC to a UART server endpoint. Each character is a sys_call,
/// which binds/releases the given Reply object once per byte.
pub fn ipc_puts(ep: EndpointHandle, reply: ReplyHandle, s: &str) {
    for b in s.bytes() {
        let _ = sys_call(ep, reply, b as u64, 0, 0, 0);
    }
}
