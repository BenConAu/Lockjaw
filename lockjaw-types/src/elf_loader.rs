//! ELF segment → page-list loading plan.
//!
//! Pure plan/apply: given a parsed [`ElfInfo`](crate::elf::ElfInfo) and the
//! length of the underlying ELF byte slice, produce a sequence of
//! [`ElfLoadEntry`] page-sized work units. Each entry tells a kernel-syscall
//! caller what to do for one destination page: allocate, place a slice of
//! file data at a specific in-page offset (or zero — for BSS), and register
//! the page at a page-aligned virtual address with the segment's executable
//! flag.
//!
//! The plan owns the bounds policy. All overflow checks (`vaddr+mem_size`,
//! `vaddr+file_size`, `file_offset+file_size > elf_len`, page-count
//! explosion, vaddr in user range) happen during plan construction. By the
//! time a caller iterates entries, every byte the plan references is
//! guaranteed to exist in the ELF and every VA is in the user range.
//!
//! Used by both `user/posix-server` (loading musl binaries with
//! tightly-packed unaligned LOAD segments) and `user/init` (loading the
//! Rust user crates). Centralizing here means the unaligned-segment
//! handling and bounds checks live in one host-tested place.

use crate::addr::PAGE_SIZE;
use crate::constants::USER_VA_END;
use crate::elf::ElfInfo;

/// One page-sized work unit in an ELF load plan.
///
/// Tells the caller:
///
/// 1. Allocate a page.
/// 2. (Optionally) copy `elf_data[src_file_range.0..src_file_range.1]`
///    into that page starting at `in_page_offset`. The remainder of the
///    page should be zeroed (the caller does this once after allocating).
/// 3. Register the page at `page_va` with the `executable` permission.
///
/// `src_file_range.0 == src_file_range.1` indicates a BSS-only page
/// (no file data to copy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElfLoadEntry {
    /// Page-aligned destination VA. The kernel maps the allocated page at
    /// this address.
    pub page_va: u64,
    /// `[start, end)` indices into the ELF byte slice. Empty when this
    /// page is entirely BSS or pre-data padding.
    pub src_file_range: (usize, usize),
    /// Offset within the destination page where file data should be
    /// written. Nonzero when the source segment's `vaddr` is mid-page.
    pub in_page_offset: usize,
    /// Whether this page should be mapped executable.
    pub executable: bool,
}

impl ElfLoadEntry {
    /// Zero-initialized entry suitable for filling a buffer before
    /// passing it to [`plan_elf_load`]. The plan overwrites populated
    /// entries; trailing entries are unused and left at this value.
    pub const EMPTY: Self = Self {
        page_va: 0,
        src_file_range: (0, 0),
        in_page_offset: 0,
        executable: false,
    };
}

/// A fully validated load plan, borrowing a caller-provided slice of
/// [`ElfLoadEntry`]. Construct via [`plan_elf_load`].
///
/// Caller owns the storage. Posix-server can use a small stack array
/// (~64 entries, 2.5 KB); init's loader can use a larger buffer or a
/// donated page. The cap is the caller's choice — if the plan needs
/// more entries than the buffer holds, [`plan_elf_load`] returns
/// [`ElfLoadError::TooManyEntries`].
#[derive(Debug)]
pub struct ElfLoadPlan<'a> {
    entries: &'a [ElfLoadEntry],
}

impl<'a> ElfLoadPlan<'a> {
    /// All page-sized work units in iteration order. Caller iterates and
    /// applies each one as a side effect.
    pub fn entries(&self) -> &[ElfLoadEntry] {
        self.entries
    }

    /// Number of entries the plan produced (i.e. number of pages the
    /// caller must allocate and map).
    pub fn page_count(&self) -> usize {
        self.entries.len()
    }
}

/// Errors from [`plan_elf_load`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfLoadError {
    /// `vaddr + mem_size` would overflow `u64`. Indicates a malformed or
    /// adversarial ELF.
    VaddrRangeOverflow { seg_idx: usize },
    /// `file_offset + file_size > elf_len`, or `file_offset > elf_len`.
    /// Indicates a truncated or adversarial ELF — the caller would
    /// otherwise index out of the byte slice.
    FileRangeOutOfBounds { seg_idx: usize, file_end: u64, elf_len: usize },
    /// Segment expansion would produce more page entries than the
    /// caller-provided buffer can hold. `cap` is the buffer's length.
    /// Indicates either a pathological `mem_size` (e.g. 4 GB BSS) or
    /// an under-sized buffer for the binary being loaded.
    TooManyEntries { needed: usize, cap: usize },
    /// `vaddr` falls outside the user VA range
    /// (`[0, USER_VA_END)`). Defense in depth — the kernel side validates
    /// again during `sys_create_process`, but rejecting here gives a
    /// clearer error and a clean test target.
    VaddrOutOfUserRange { seg_idx: usize, vaddr: u64 },
    /// `file_size > mem_size`. The ELF spec requires the file-backed
    /// bytes to fit within the in-memory extent of the segment; any
    /// excess in `mem_size` becomes BSS. A segment that violates this
    /// would otherwise let the loader copy file bytes past
    /// `vaddr + mem_size` into a destination page.
    FileSizeExceedsMemSize { seg_idx: usize, file_size: u64, mem_size: u64 },
}

/// Build a load plan from parsed ELF info into a caller-provided buffer.
///
/// `elf_len` is the length of the ELF byte slice the caller will use to
/// resolve each entry's `src_file_range`. The plan only references bytes
/// that fit within `elf_len`; bounds violations are reported via
/// [`ElfLoadError::FileRangeOutOfBounds`].
///
/// `out` is the storage for entries. The plan writes into the prefix of
/// this slice and returns an [`ElfLoadPlan`] borrowing the populated
/// portion. If the binary requires more entries than `out` can hold,
/// returns [`ElfLoadError::TooManyEntries`]. The buffer's contents on
/// error are unspecified (partially populated; the borrow is released
/// when the error is returned).
///
/// The plan walks each `PT_LOAD` segment, computes the page-aligned VA
/// range it covers, and emits one [`ElfLoadEntry`] per destination page.
/// For each page it intersects the file-backed sub-range
/// `[vaddr, vaddr+file_size)` to determine whether (and from where) file
/// data should be copied; the rest of the page is zeroed (BSS, pre-data
/// padding, or trailing padding when file_size doesn't fill a page).
pub fn plan_elf_load<'a>(
    info: &ElfInfo,
    elf_len: usize,
    out: &'a mut [ElfLoadEntry],
) -> Result<ElfLoadPlan<'a>, ElfLoadError> {
    let cap = out.len();
    let mut count: usize = 0;

    for i in 0..info.segment_count {
        let seg = &info.segments[i];

        // ELF invariant: file-backed bytes must fit within the in-memory
        // extent of the segment. Without this, num_pages (derived from
        // mem_size) and the file-copy range (derived from file_size)
        // diverge, and the loader would copy file bytes past
        // vaddr + mem_size into a destination page.
        //
        // This check runs *before* the mem_size==0 skip so that the
        // degenerate `mem_size=0, file_size>0` case is rejected as
        // malformed rather than silently dropped.
        if seg.file_size > seg.mem_size {
            return Err(ElfLoadError::FileSizeExceedsMemSize {
                seg_idx: i,
                file_size: seg.file_size,
                mem_size: seg.mem_size,
            });
        }

        if seg.mem_size == 0 {
            // mem_size == 0 && file_size == 0 (the previous check
            // forces this) — the segment contributes nothing. Skip.
            continue;
        }

        // Bounds policy lives here: refuse to compute a plan over
        // arithmetic that would wrap or escape the byte slice.
        // (file_size <= mem_size enforced above, so vaddr+file_size
        // can't overflow if vaddr+mem_size doesn't.)
        let seg_end_va = seg
            .vaddr
            .checked_add(seg.mem_size)
            .ok_or(ElfLoadError::VaddrRangeOverflow { seg_idx: i })?;
        let seg_file_end_va = seg.vaddr + seg.file_size;
        let file_end = seg
            .file_offset
            .checked_add(seg.file_size)
            .ok_or(ElfLoadError::FileRangeOutOfBounds {
                seg_idx: i,
                file_end: u64::MAX,
                elf_len,
            })?;
        if file_end > elf_len as u64 || seg.file_offset > elf_len as u64 {
            return Err(ElfLoadError::FileRangeOutOfBounds {
                seg_idx: i,
                file_end,
                elf_len,
            });
        }

        // Defense in depth: don't even build a plan that targets a VA the
        // kernel will reject. seg_end_va is exclusive, so anything
        // strictly above USER_VA_END crosses into kernel space.
        if seg.vaddr >= USER_VA_END || seg_end_va > USER_VA_END {
            return Err(ElfLoadError::VaddrOutOfUserRange {
                seg_idx: i,
                vaddr: seg.vaddr,
            });
        }

        let first_page_va = seg.vaddr & !(PAGE_SIZE - 1);
        let last_page_va = (seg_end_va - 1) & !(PAGE_SIZE - 1);
        let num_pages = ((last_page_va - first_page_va) / PAGE_SIZE + 1) as usize;

        // Refuse to emit a plan that doesn't fit in the caller's buffer.
        if count
            .checked_add(num_pages)
            .map_or(true, |n| n > cap)
        {
            return Err(ElfLoadError::TooManyEntries {
                needed: count + num_pages,
                cap,
            });
        }

        for p in 0..num_pages {
            let page_va = first_page_va + (p as u64) * PAGE_SIZE;
            let page_end_va = page_va + PAGE_SIZE;

            // Intersect this page with the segment's file-backed range.
            let copy_start_va = page_va.max(seg.vaddr);
            let copy_end_va = page_end_va.min(seg_file_end_va);

            let (src_file_range, in_page_offset) = if copy_end_va > copy_start_va {
                let in_page_off = (copy_start_va - page_va) as usize;
                let src_start = (seg.file_offset + (copy_start_va - seg.vaddr)) as usize;
                let src_end = src_start + (copy_end_va - copy_start_va) as usize;
                ((src_start, src_end), in_page_off)
            } else {
                // BSS-only page (or trailing padding).
                ((0, 0), 0)
            };

            out[count] = ElfLoadEntry {
                page_va,
                src_file_range,
                in_page_offset,
                executable: seg.executable,
            };
            count += 1;
        }
    }

    Ok(ElfLoadPlan { entries: &out[..count] })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::{LoadSegment, MAX_SEGMENTS};

    /// Build an `ElfInfo` directly (skipping the parser) so tests can
    /// construct adversarial segment lists that wouldn't survive parsing.
    fn make_info(segs: &[LoadSegment]) -> ElfInfo {
        let mut segments = [LoadSegment {
            vaddr: 0,
            file_offset: 0,
            file_size: 0,
            mem_size: 0,
            executable: false,
            writable: false,
        }; MAX_SEGMENTS];
        for (i, s) in segs.iter().enumerate() {
            segments[i] = *s;
        }
        ElfInfo {
            entry_point: 0,
            segments,
            segment_count: segs.len(),
        }
    }

    fn seg(vaddr: u64, file_offset: u64, file_size: u64, mem_size: u64, executable: bool) -> LoadSegment {
        LoadSegment {
            vaddr,
            file_offset,
            file_size,
            mem_size,
            executable,
            writable: false,
        }
    }

    /// Default buffer size for happy-path tests. Generous enough that
    /// any reasonable test ELF fits without triggering TooManyEntries.
    const TEST_BUF_LEN: usize = 32;

    // ---- Happy paths ----

    #[test]
    fn page_aligned_full_page_segment() {
        let info = make_info(&[seg(0x40_0000, 0x1000, PAGE_SIZE, PAGE_SIZE, true)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x10000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 1);
        let e = plan.entries()[0];
        assert_eq!(e.page_va, 0x40_0000);
        assert_eq!(e.src_file_range, (0x1000, 0x1000 + PAGE_SIZE as usize));
        assert_eq!(e.in_page_offset, 0);
        assert!(e.executable);
    }

    #[test]
    fn bss_tail_in_same_page() {
        // file_size 0x100, mem_size 0x500 — same page, bss tail
        let info = make_info(&[seg(0x40_0000, 0x1000, 0x100, 0x500, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x10000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 1);
        let e = plan.entries()[0];
        assert_eq!(e.src_file_range, (0x1000, 0x1100));
        assert_eq!(e.in_page_offset, 0);
    }

    #[test]
    fn unaligned_vaddr_musl_case() {
        // vaddr 0x41ffa8, file_size 0x150, mem_size 0x7b8
        // (the actual musl Phase 0 case)
        let info = make_info(&[seg(0x41_ffa8, 0xffa8, 0x150, 0x7b8, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x20_0000, &mut buf).unwrap();
        // Spans pages 0x41f000 and 0x420000.
        assert_eq!(plan.page_count(), 2);

        let p0 = plan.entries()[0];
        assert_eq!(p0.page_va, 0x41_f000);
        assert_eq!(p0.in_page_offset, 0xfa8);
        // File data 0xffa8..0xffa8+0x58 fills bytes 0xfa8..0x1000 of page 0.
        assert_eq!(p0.src_file_range, (0xffa8, 0x10000));

        let p1 = plan.entries()[1];
        assert_eq!(p1.page_va, 0x42_0000);
        assert_eq!(p1.in_page_offset, 0);
        // Remaining 0xf8 bytes of file data fill bytes 0..0xf8 of page 1.
        assert_eq!(p1.src_file_range, (0x10000, 0x10000 + 0xf8));
    }

    #[test]
    fn segment_crosses_page_boundary() {
        // vaddr 0x400ff0, file_size+mem_size 0x100 — last 16 bytes of
        // page 0, first 0xf0 bytes of page 1.
        let info = make_info(&[seg(0x40_0ff0, 0x1000, 0x100, 0x100, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x10000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 2);
        assert_eq!(plan.entries()[0].in_page_offset, 0xff0);
        assert_eq!(plan.entries()[0].src_file_range, (0x1000, 0x1010));
        assert_eq!(plan.entries()[1].page_va, 0x40_1000);
        assert_eq!(plan.entries()[1].in_page_offset, 0);
        // First page took 0x10 file bytes (0x1000..0x1010), so this page
        // gets the remaining 0xf0: bytes 0x1010..0x1100.
        assert_eq!(plan.entries()[1].src_file_range, (0x1010, 0x1100));
    }

    #[test]
    fn multi_segment_text_rodata_bss() {
        let info = make_info(&[
            seg(0x40_0000, 0x1000, PAGE_SIZE, PAGE_SIZE, true),
            seg(0x40_1000, 0x2000, 0x800, 0x800, false),
            seg(0x40_2000, 0x3000, 0x100, 0x1000, false), // mostly BSS
        ]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x10000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 3);
        assert!(plan.entries()[0].executable);
        assert!(!plan.entries()[1].executable);
        assert!(!plan.entries()[2].executable);
        // BSS-mostly segment still emits one entry; file_range is the
        // 0x100-byte prefix.
        assert_eq!(plan.entries()[2].src_file_range, (0x3000, 0x3100));
    }

    #[test]
    fn empty_segment_list() {
        let info = make_info(&[]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x1000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 0);
        assert!(plan.entries().is_empty());
    }

    #[test]
    fn zero_mem_size_segment_skipped() {
        // PT_LOAD with mem_size=0 is degenerate but legal; skip it.
        let info = make_info(&[
            seg(0x40_0000, 0x1000, 0, 0, false),
            seg(0x40_1000, 0x1000, PAGE_SIZE, PAGE_SIZE, false),
        ]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x10000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 1);
        assert_eq!(plan.entries()[0].page_va, 0x40_1000);
    }

    #[test]
    fn pre_data_padding_first_page_partially_filled() {
        // vaddr 0x400800 mid-page. The page from 0x400000 onwards is
        // emitted; bytes 0..0x800 of the page are zero (no file data),
        // file_data begins at in_page_offset 0x800.
        let info = make_info(&[seg(0x40_0800, 0x1000, 0x400, 0x400, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, 0x10000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 1);
        let e = plan.entries()[0];
        assert_eq!(e.page_va, 0x40_0000);
        assert_eq!(e.in_page_offset, 0x800);
        assert_eq!(e.src_file_range, (0x1000, 0x1400));
    }

    // ---- Bounds and overflow ----

    #[test]
    fn vaddr_plus_mem_size_overflows() {
        let info = make_info(&[seg(u64::MAX - 0x100, 0, 0, 0x200, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        assert_eq!(err, ElfLoadError::VaddrRangeOverflow { seg_idx: 0 });
    }

    #[test]
    fn file_size_larger_than_mem_size_caught_before_vaddr_overflow() {
        // Constructed to look like it could overflow vaddr+file_size,
        // but the file_size > mem_size invariant violation is detected
        // first (file_size = 0x200, mem_size = 0x100).
        let info = make_info(&[seg(u64::MAX - 0x100, 0, 0x200, 0x100, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        assert_eq!(
            err,
            ElfLoadError::FileSizeExceedsMemSize {
                seg_idx: 0,
                file_size: 0x200,
                mem_size: 0x100,
            }
        );
    }

    #[test]
    fn file_range_past_end_of_elf() {
        let info = make_info(&[seg(0x40_0000, 0x900, 0x800, 0x800, false)]);
        // elf_len=0x1000; segment wants bytes [0x900, 0x1100) — past end.
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x1000, &mut buf).unwrap_err();
        match err {
            ElfLoadError::FileRangeOutOfBounds { seg_idx, file_end, elf_len } => {
                assert_eq!(seg_idx, 0);
                assert_eq!(file_end, 0x1100);
                assert_eq!(elf_len, 0x1000);
            }
            other => panic!("expected FileRangeOutOfBounds, got {:?}", other),
        }
    }

    #[test]
    fn file_offset_past_end_of_elf() {
        let info = make_info(&[seg(0x40_0000, 0x9999, 0, 0x100, false)]);
        // Empty file_size but file_offset itself is past elf_len.
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x1000, &mut buf).unwrap_err();
        assert!(matches!(err, ElfLoadError::FileRangeOutOfBounds { seg_idx: 0, .. }));
    }

    #[test]
    fn file_offset_plus_file_size_overflow_u64() {
        let info = make_info(&[seg(0x40_0000, u64::MAX - 0x100, 0x200, 0x200, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x1000, &mut buf).unwrap_err();
        assert!(matches!(err, ElfLoadError::FileRangeOutOfBounds { seg_idx: 0, .. }));
    }

    #[test]
    fn pathological_bss_too_many_entries() {
        // mem_size requires more pages than the caller's buffer holds.
        // Caller buffer is 4 entries; segment expands to 5 pages.
        let huge_mem = 5u64 * PAGE_SIZE;
        let info = make_info(&[seg(0x40_0000, 0x1000, 0x100, huge_mem, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; 4];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        match err {
            ElfLoadError::TooManyEntries { needed, cap } => {
                assert_eq!(needed, 5);
                assert_eq!(cap, 4);
            }
            other => panic!("expected TooManyEntries, got {:?}", other),
        }
    }

    #[test]
    fn zero_mem_size_with_nonzero_file_size_rejected() {
        // Degenerate case: mem_size=0 but file_size>0. Without the
        // invariant check running before the mem_size==0 skip, this
        // would be silently dropped — a malformed ELF passing through.
        let info = make_info(&[seg(0x40_0000, 0x1000, 0x100, 0, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        assert_eq!(
            err,
            ElfLoadError::FileSizeExceedsMemSize {
                seg_idx: 0,
                file_size: 0x100,
                mem_size: 0,
            }
        );
    }

    #[test]
    fn file_size_larger_than_mem_size_rejected() {
        // ELF invariant violation: file-backed bytes wouldn't fit in
        // the segment's in-memory extent. Without this rejection, the
        // loader would copy file bytes past seg.vaddr + mem_size into a
        // destination page (because num_pages comes from mem_size and
        // the copy range comes from file_size).
        let info = make_info(&[seg(0x40_0000, 0x1000, 0x500, 0x100, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        assert_eq!(
            err,
            ElfLoadError::FileSizeExceedsMemSize {
                seg_idx: 0,
                file_size: 0x500,
                mem_size: 0x100,
            }
        );
    }

    #[test]
    fn vaddr_in_kernel_range_rejected() {
        // 0x40000000 = USER_VA_END; anything >= rejected.
        let info = make_info(&[seg(0x4000_0000, 0x1000, PAGE_SIZE, PAGE_SIZE, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        assert_eq!(
            err,
            ElfLoadError::VaddrOutOfUserRange {
                seg_idx: 0,
                vaddr: 0x4000_0000
            }
        );
    }

    #[test]
    fn vaddr_just_below_kernel_with_mem_size_crossing() {
        // vaddr fine, but vaddr+mem_size crosses USER_VA_END.
        let info = make_info(&[seg(0x3FFF_F000, 0x1000, PAGE_SIZE, 2 * PAGE_SIZE, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        // seg_end_va = 0x4000_1000 > USER_VA_END
        assert!(matches!(err, ElfLoadError::VaddrOutOfUserRange { seg_idx: 0, .. }));
    }

    #[test]
    fn buffer_exactly_fits_at_capacity() {
        // Edge case: caller buffer = exactly the page count needed.
        // A 5-page segment fills a 5-entry buffer with no error.
        let big = 5u64 * PAGE_SIZE;
        let info = make_info(&[seg(0x40_0000, 0, 0, big, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; 5];
        let plan = plan_elf_load(&info, 0x10000, &mut buf).unwrap();
        assert_eq!(plan.page_count(), 5);
    }

    #[test]
    fn buffer_one_short_returns_too_many() {
        // Same 5-page segment, 4-entry buffer → TooManyEntries.
        let big = 5u64 * PAGE_SIZE;
        let info = make_info(&[seg(0x40_0000, 0, 0, big, false)]);
        let mut buf = [ElfLoadEntry::EMPTY; 4];
        let err = plan_elf_load(&info, 0x10000, &mut buf).unwrap_err();
        assert_eq!(err, ElfLoadError::TooManyEntries { needed: 5, cap: 4 });
    }

    // ---- Round-trip apply ----

    #[test]
    fn roundtrip_apply_recovers_segment_bytes() {
        // Build a fake ELF byte slice with known content; build a plan;
        // simulate a caller that allocates pages, copies file data, and
        // verify the resulting page contents match the segment.
        extern crate std;
        use std::vec::Vec;

        let mut elf = Vec::new();
        elf.resize(0x4000, 0u8); // padding to file_offset
        // segment at file offset 0x2000, 0x300 bytes "ABCD..."
        for i in 0..0x300 {
            elf[0x2000 + i] = (i & 0xFF) as u8;
        }
        // segment at vaddr 0x40_0040 (mid-page), file 0x300 bytes,
        // mem_size 0x500 (BSS tail of 0x200 zeros).
        let info = make_info(&[seg(0x40_0040, 0x2000, 0x300, 0x500, false)]);
        let mut planbuf = [ElfLoadEntry::EMPTY; TEST_BUF_LEN];
        let plan = plan_elf_load(&info, elf.len(), &mut planbuf).unwrap();
        assert_eq!(plan.page_count(), 1);

        // Apply: zero a page, copy file bytes per the plan.
        let mut page = [0u8; PAGE_SIZE as usize];
        let e = plan.entries()[0];
        let (s, t) = e.src_file_range;
        page[e.in_page_offset..e.in_page_offset + (t - s)].copy_from_slice(&elf[s..t]);

        // Bytes [0..0x40) before segment vaddr remain zero.
        assert!(page[..0x40].iter().all(|b| *b == 0));
        // Bytes [0x40..0x340) match the file content.
        for i in 0..0x300 {
            assert_eq!(page[0x40 + i], (i & 0xFF) as u8, "mismatch at {}", i);
        }
        // Bytes [0x340..PAGE_SIZE) are BSS — remain zero.
        assert!(page[0x340..].iter().all(|b| *b == 0));
    }
}
