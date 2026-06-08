/// Kernel print module — zero vtables, zero indirect calls.
///
/// All formatting uses concrete dispatch via the `KPrint` trait.
/// No `core::fmt`, no `dyn Write`, no function pointers. Every
/// call resolves to a direct `bl` instruction at compile time.
///
/// Supported wrapper types:
///   Hex(u64)        — 0x prefix, minimal width: `0x1a2b`
///   Addr(u64)       — 0x + 16 hex digits: `0x000000004020c558`
///   Hex32(u64)      — 0x + 8 hex digits: `0x00000042`
///   PaddedHex8(u64) — 8 hex digits, no prefix: `0008c558`
///   HexByte(u64)    — 0x + 2 hex digits: `0x1f`
///   Dec02(usize)    — 2-digit zero-padded decimal: `05`
///
/// Built-in impls: &str, u64, u32, u8, usize, i64, bool
use crate::arch::aarch64::pl011::Pl011;

// ── trait ──────────────────────────────────────────────────────

/// Kernel-print trait. Each impl writes directly to the UART
/// with no intermediate buffering or trait-object dispatch.
pub trait KPrint {
    fn kprint(&self);
}

// ── wrapper types ──────────────────────────────────────────────

/// Hex with `0x` prefix, minimal width. `Hex(0x1a2b)` → `0x1a2b`
pub struct Hex(pub u64);

/// 64-bit address: `0x` + 16 hex digits. `Addr(0x42)` → `0x0000000000000042`
pub struct Addr(pub u64);

/// 32-bit hex: `0x` + 8 hex digits. `Hex32(0x42)` → `0x00000042`
pub struct Hex32(pub u64);

/// 8 hex digits, no prefix. `PaddedHex8(0x42)` → `00000042`
#[allow(dead_code)]
pub struct PaddedHex8(pub u64);

/// Byte hex: `0x` + 2 hex digits. `HexByte(0x1f)` → `0x1f`
pub struct HexByte(pub u64);

/// 2-digit zero-padded decimal. `Dec02(5)` → `05`
pub struct Dec02(pub usize);

// ── formatters ─────────────────────────────────────────────────

/// Emit a single hex nibble (0–15) as an ASCII character.
#[inline(always)]
fn hex_nibble(val: u64, shift: u32) -> u8 {
    let n = ((val >> shift) & 0xF) as u8;
    if n < 10 { b'0' + n } else { b'a' + n - 10 }
}

/// Unsigned decimal to UART. Max 20 digits for u64::MAX.
fn put_decimal(mut val: u64) {
    let uart = Pl011::new();
    if val == 0 {
        uart.putc(b'0');
        return;
    }
    // 20 digits max for u64::MAX = 18446744073709551615
    let mut buf = [0u8; 20];
    let mut i = 20;
    while val > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    while i < 20 {
        uart.putc(buf[i]);
        i += 1;
    }
}

// ── KPrint impls ───────────────────────────────────────────────

impl KPrint for str {
    fn kprint(&self) {
        Pl011::new().puts(self);
    }
}

impl KPrint for &str {
    fn kprint(&self) {
        Pl011::new().puts(self);
    }
}

impl KPrint for u64 {
    fn kprint(&self) {
        put_decimal(*self);
    }
}

impl KPrint for u32 {
    fn kprint(&self) {
        put_decimal(*self as u64);
    }
}

impl KPrint for u8 {
    fn kprint(&self) {
        put_decimal(*self as u64);
    }
}

impl KPrint for usize {
    fn kprint(&self) {
        put_decimal(*self as u64);
    }
}

impl KPrint for i64 {
    fn kprint(&self) {
        if *self < 0 {
            Pl011::new().putc(b'-');
            // i64::MIN cannot be negated within i64
            let abs = if *self == i64::MIN {
                9_223_372_036_854_775_808u64
            } else {
                (-*self) as u64
            };
            put_decimal(abs);
        } else {
            put_decimal(*self as u64);
        }
    }
}

impl KPrint for bool {
    fn kprint(&self) {
        Pl011::new().puts(if *self { "true" } else { "false" });
    }
}

impl KPrint for Hex {
    fn kprint(&self) {
        let uart = Pl011::new();
        uart.puts("0x");
        let val = self.0;
        if val == 0 {
            uart.putc(b'0');
            return;
        }
        // Find highest non-zero nibble
        let mut started = false;
        let mut shift = 60u32;
        loop {
            let nibble = hex_nibble(val, shift);
            if nibble != b'0' || started {
                started = true;
                uart.putc(nibble);
            }
            if shift == 0 { break; }
            shift -= 4;
        }
    }
}

impl KPrint for Addr {
    fn kprint(&self) {
        let uart = Pl011::new();
        uart.puts("0x");
        let mut shift = 60u32;
        loop {
            uart.putc(hex_nibble(self.0, shift));
            if shift == 0 { break; }
            shift -= 4;
        }
    }
}

impl KPrint for Hex32 {
    fn kprint(&self) {
        let uart = Pl011::new();
        uart.puts("0x");
        let mut shift = 28u32;
        loop {
            uart.putc(hex_nibble(self.0, shift));
            if shift == 0 { break; }
            shift -= 4;
        }
    }
}

impl KPrint for PaddedHex8 {
    fn kprint(&self) {
        let uart = Pl011::new();
        let mut shift = 28u32;
        loop {
            uart.putc(hex_nibble(self.0, shift));
            if shift == 0 { break; }
            shift -= 4;
        }
    }
}

impl KPrint for HexByte {
    fn kprint(&self) {
        let uart = Pl011::new();
        uart.puts("0x");
        uart.putc(hex_nibble(self.0, 4));
        uart.putc(hex_nibble(self.0, 0));
    }
}

impl KPrint for Dec02 {
    fn kprint(&self) {
        let uart = Pl011::new();
        let n = self.0 % 100;
        uart.putc(b'0' + (n / 10) as u8);
        uart.putc(b'0' + (n % 10) as u8);
    }
}

// ── macros ─────────────────────────────────────────────────────

#[macro_export]
macro_rules! kprint {
    ($($arg:expr),+ $(,)?) => {{
        $( $crate::print::KPrint::kprint(&$arg); )+
    }};
}

#[macro_export]
macro_rules! kprintln {
    () => {
        $crate::arch::aarch64::pl011::Pl011::new().puts("\n");
    };
    ($($arg:expr),+ $(,)?) => {{
        $( $crate::print::KPrint::kprint(&$arg); )+
        $crate::arch::aarch64::pl011::Pl011::new().puts("\n");
    }};
}
