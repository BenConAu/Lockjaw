//! Cache-line range math for DMA cache-maintenance operations.
//!
//! Pure types — no asm, no MMIO, no kernel/userlib coupling. The kernel
//! side reads `CTR_EL0.DminLine` at boot to confirm the host's actual
//! data cache line matches `CACHE_LINE_BYTES`; the asm primitives that
//! actually issue `dc civac` / `dc cvac` live in `src/arch/aarch64/cache.rs`
//! kernel-side because they require EL1.
//!
//! ARMv8 architectural minimum is 16 bytes; the practical floor across
//! the parts Lockjaw targets (Cortex-A72 on BCM2711, QEMU virt's default
//! Cortex-A57) is 64 bytes. Picking 64 here lets host tests verify the
//! range-expansion math without depending on a runtime probe.
//!
//! `CTR_EL0.DminLine` reports the line size as `4 << DminLine` bytes;
//! the kernel boot path panics if the read value doesn't match
//! `CACHE_LINE_BYTES`, surfacing platform mismatch before any DMA
//! happens. Future cores with larger lines would update the constant
//! here.

/// Data cache line size in bytes for every platform Lockjaw currently
/// supports. The kernel verifies this against `CTR_EL0.DminLine` at boot.
pub const CACHE_LINE_BYTES: u64 = 64;

/// Round `addr` down to the start of its containing cache line.
/// Pure bit-mask — cannot overflow for any `u64` input.
#[inline]
pub const fn align_down_to_line(addr: u64) -> u64 {
    addr & !(CACHE_LINE_BYTES - 1)
}

/// Round `end` up to the start of the next cache line. Returns
/// `None` if `end > u64::MAX - (CACHE_LINE_BYTES - 1)` (the
/// rounding would overflow). Private — public callers go through
/// `lines_covering`, which performs the same overflow check on the
/// composite `start + len` arithmetic.
#[inline]
const fn checked_align_up_to_line(end: u64) -> Option<u64> {
    match end.checked_add(CACHE_LINE_BYTES - 1) {
        Some(padded) => Some(padded & !(CACHE_LINE_BYTES - 1)),
        None => None,
    }
}

/// Expand a `[start, start+len)` byte range to the cache-line-aligned
/// `[line_start, line_end)` range that fully covers it. Returns
/// `Some((line_start, line_count))` where `line_count` is the
/// number of cache lines the caller must iterate over.
///
/// Empty ranges (`len == 0`) return `Some((start_aligned, 0))`.
///
/// Returns `None` on overflow — either `start + len` exceeds
/// `u64::MAX`, or the line-end rounding does. The kernel-side
/// cache-maintenance syscalls treat `None` as a hard rejection
/// (the silent alternative — wrap around and invalidate the
/// wrong lines — is the worst possible failure mode for a
/// cache-maintenance primitive). Codex review pass 1 caught the
/// silent-wrap bug class on the unchecked version.
#[inline]
pub const fn lines_covering(start: u64, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return Some((align_down_to_line(start), 0));
    }
    let end = match start.checked_add(len) {
        Some(e) => e,
        None => return None,
    };
    let line_end = match checked_align_up_to_line(end) {
        Some(le) => le,
        None => return None,
    };
    let line_start = align_down_to_line(start);
    // `line_end >= line_start` is guaranteed: line_end is end
    // rounded up, line_start is start rounded down, end > start.
    // Subtraction cannot underflow.
    let line_count = (line_end - line_start) / CACHE_LINE_BYTES;
    Some((line_start, line_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_range_is_unchanged() {
        // 64-byte aligned start, length is a multiple of line size:
        // expansion does not pad either end.
        assert_eq!(lines_covering(0x1000, 128), Some((0x1000, 2)));
    }

    #[test]
    fn misaligned_start_rounds_down() {
        // Start 1 byte into a line; expansion pads back to line start
        // and covers two lines (because end at 0x1001+64 = 0x1041
        // lies in the second line).
        assert_eq!(lines_covering(0x1001, 64), Some((0x1000, 2)));
    }

    #[test]
    fn misaligned_end_rounds_up() {
        // Start aligned, end 1 byte past a line boundary.
        assert_eq!(lines_covering(0x1000, 65), Some((0x1000, 2)));
    }

    #[test]
    fn single_byte_inside_line() {
        assert_eq!(lines_covering(0x1010, 1), Some((0x1000, 1)));
    }

    #[test]
    fn zero_length_returns_zero_lines() {
        // Empty range still returns a sensible aligned start, but
        // zero line count. Zero-length sync requests are no-ops.
        assert_eq!(lines_covering(0x1234, 0), Some((0x1200, 0)));
    }

    #[test]
    fn full_page_is_one_line_loop_per_64_bytes() {
        // 4 KiB page = 64 lines.
        assert_eq!(lines_covering(0x10000, 4096), Some((0x10000, 64)));
    }

    #[test]
    fn one_line_exactly() {
        assert_eq!(lines_covering(0x2000, CACHE_LINE_BYTES), Some((0x2000, 1)));
    }

    // --- Overflow / boundary tests (codex review pass 1) ----------

    #[test]
    fn start_plus_len_overflow_returns_none() {
        // start near u64::MAX such that start + len wraps. Worst-
        // case real-world cache-maintenance request that must NOT
        // silently invalidate near-zero lines.
        assert_eq!(lines_covering(u64::MAX, 1), None);
        assert_eq!(lines_covering(u64::MAX - 10, 100), None);
        assert_eq!(lines_covering(u64::MAX / 2 + 1, u64::MAX / 2 + 1), None);
    }

    #[test]
    fn line_end_rounding_overflow_returns_none() {
        // start + len does not overflow, but end + (line - 1)
        // does. With len > 0 and end above u64::MAX - 63, the
        // line-end rounding would wrap; reject instead.
        let last_safe_end = u64::MAX - (CACHE_LINE_BYTES - 1);
        assert_eq!(lines_covering(0, last_safe_end + 1), None);
        assert_eq!(lines_covering(0, u64::MAX), None);
    }

    #[test]
    fn boundary_just_inside_max_succeeds() {
        // end exactly equals the highest value that survives the
        // padding addition (u64::MAX - 63). Must succeed; this
        // pins the boundary so a future overflow-tightening fix
        // doesn't accidentally over-reject.
        let last_safe_end = u64::MAX - (CACHE_LINE_BYTES - 1);
        assert!(lines_covering(0, last_safe_end).is_some());
    }

    #[test]
    fn zero_len_at_u64_max_does_not_overflow() {
        // Even at u64::MAX itself, zero-length is a defined no-op.
        assert_eq!(
            lines_covering(u64::MAX, 0),
            Some((align_down_to_line(u64::MAX), 0))
        );
    }
}
