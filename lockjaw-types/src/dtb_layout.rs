/// DTB pageset layout — figuring out which physical pages cover the
/// boot DTB and where, within the first page, the DTB header starts.
///
/// **Why this module exists:** the boot firmware places the DTB at a
/// physical address it picks. On QEMU virt that address is page-
/// aligned by accident; on Pi 4B's VC firmware it is not (typical
/// `dtb_paddr = 0x2eff1e00`, low 12 bits = `0xe00`). The kernel
/// can't reach into the firmware to ask for an aligned address; it
/// has to handle both cases. The bug this module fixes was: the
/// kernel built a `&[PhysAddr]` of "page bases" by stepping
/// `dtb_paddr + i * PAGE_SIZE`, which produces unaligned values on
/// Pi. `register_existing` accepted the unaligned slice (the loose
/// `&[PhysAddr]` signature), userspace's `sys_map_pages` mapped the
/// page-aligned-down frames at a chosen VA, and reading from
/// `va + 0` returned bytes from `0xe00` *before* the DTB header —
/// "BAD magic" at userspace.
///
/// **The fix is type-driven:** `register_existing` now takes
/// `&[PhysPage]`, and `PhysPage` cannot be constructed from an
/// unaligned `PhysAddr` without explicitly opting in (via
/// `PhysPage::containing`, which rounds down). The boot path uses
/// `compute_layout` here to derive both the aligned page list AND
/// the in-page offset of the DTB start, then surfaces the offset to
/// userspace via `sys_get_boot_info` so consumers can apply it.
///
/// All the arithmetic lives in this pure module so it's host-
/// testable without booting a kernel — including an end-to-end test
/// that takes a real DTB blob, places it at an unaligned offset in
/// a buffer, recovers it through the same `compute_layout` ->
/// "userspace mapping" -> offset-application path the kernel will
/// use, and parses the recovered DTB. If the host test passes, the
/// kernel-side logic is correct by construction.

use crate::addr::{PAGE_SIZE, PhysAddr, PhysPage};

/// Layout of a DTB across physical pages, computed from the
/// firmware-supplied address. The kernel registers `page_count`
/// pages starting at `first_page` as a PageSet, surfaces
/// `in_page_offset` to userspace via `sys_get_boot_info`, and
/// userspace applies the offset when reading DTB bytes from the
/// mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbLayout {
    /// First page of the contiguous physical span that contains the
    /// DTB (page-aligned by construction).
    pub first_page: PhysPage,
    /// Bytes from the start of `first_page` to the DTB header. Always
    /// `< PAGE_SIZE`. Zero when the firmware happened to align the
    /// DTB on a page boundary (typical on QEMU virt).
    pub in_page_offset: u32,
    /// Number of contiguous pages spanned. Accounts for the in-page
    /// offset — a 4096-byte DTB starting at offset 1 spans 2 pages,
    /// not 1.
    pub page_count: usize,
}

/// Compute the page layout for a DTB at `dtb_paddr` containing
/// `content_size` bytes.
///
/// `content_size` is the value reported by `dtb_content_size`
/// (offset of the strings block + size of the strings block, per
/// FDT spec); it does NOT include any in-page offset.
pub const fn compute_layout(dtb_paddr: PhysAddr, content_size: usize) -> DtbLayout {
    let raw = dtb_paddr.as_u64();
    let in_page_offset = (raw & (PAGE_SIZE - 1)) as u32;
    // first_page = dtb_paddr rounded down to PAGE_SIZE, in page-number form.
    let first_page = PhysAddr::new(raw & !(PAGE_SIZE - 1)).containing_page();
    let total_bytes = in_page_offset as usize + content_size;
    let page_size = PAGE_SIZE as usize;
    let page_count = (total_bytes + page_size - 1) / page_size;
    DtbLayout { first_page, in_page_offset, page_count }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;

    // ----- page count math -----

    #[test]
    fn aligned_dtb_one_byte_spans_one_page() {
        let l = compute_layout(PhysAddr::new(0x4000_0000), 1);
        assert_eq!(l.in_page_offset, 0);
        assert_eq!(l.page_count, 1);
        assert_eq!(l.first_page.start_addr().as_u64(), 0x4000_0000);
    }

    #[test]
    fn aligned_dtb_exactly_one_page_spans_one_page() {
        let l = compute_layout(PhysAddr::new(0x4000_0000), 4096);
        assert_eq!(l.in_page_offset, 0);
        assert_eq!(l.page_count, 1);
    }

    #[test]
    fn aligned_dtb_one_byte_into_second_page_spans_two_pages() {
        let l = compute_layout(PhysAddr::new(0x4000_0000), 4097);
        assert_eq!(l.in_page_offset, 0);
        assert_eq!(l.page_count, 2);
    }

    #[test]
    fn unaligned_dtb_within_first_page_spans_one_page() {
        // start at 0xe00 in the first page; remaining = 0x200 (512).
        let l = compute_layout(PhysAddr::new(0x2eff_1e00), 512);
        assert_eq!(l.in_page_offset, 0xe00);
        assert_eq!(l.page_count, 1);
        assert_eq!(l.first_page.start_addr().as_u64(), 0x2eff_1000);
    }

    #[test]
    fn unaligned_dtb_one_byte_past_first_page_spans_two_pages() {
        let l = compute_layout(PhysAddr::new(0x2eff_1e00), 513);
        assert_eq!(l.in_page_offset, 0xe00);
        assert_eq!(l.page_count, 2);
    }

    #[test]
    fn pi_4b_realistic_case() {
        // Numbers from a real Pi 4B boot: dtb_paddr = 0x2eff_1e00,
        // dtb_content_size = 0xe112 (57618 bytes). The naive
        // (non-offset) page-count would have been
        // ceil(0xe112 / 4096) = 15; the correct count is also 15
        // here because the 0xe00 offset doesn't push past where
        // 0xe112 alone would have ended. So this case alone
        // wouldn't have caught the count bug — but it pins the
        // first_page and in_page_offset correctness.
        let l = compute_layout(PhysAddr::new(0x2eff_1e00), 0xe112);
        assert_eq!(l.first_page.start_addr().as_u64(), 0x2eff_1000);
        assert_eq!(l.in_page_offset, 0xe00);
        assert_eq!(l.page_count, 15);
    }

    #[test]
    fn case_where_offset_pushes_count_up_by_one() {
        // A DTB ending exactly at a page boundary if aligned would
        // span the same page count as content_size suggests. Add an
        // offset that pushes the end past the boundary -> +1 page.
        // content_size = 4096 -> 1 page if aligned.
        // offset 1 -> 4097 total bytes -> 2 pages.
        let l = compute_layout(PhysAddr::new(0x4000_0001), 4096);
        assert_eq!(l.in_page_offset, 1);
        assert_eq!(l.page_count, 2);
    }

    // ----- end-to-end: real DTB at unaligned offset -----

    /// QEMU virt DTB blob (the same one fdt.rs's parser tests use).
    /// Reusing it lets us prove the layout + offset-application
    /// path correctly recovers a DTB the parser already validates
    /// against.
    static QEMU_DTB: &[u8] = include_bytes!("../test-data/qemu-virt.dtb");

    /// Simulate a firmware-placed DTB at `paddr` inside a host
    /// buffer that represents physical memory. Build the layout,
    /// then act as userspace would: gather bytes from the
    /// `page_count` pages starting at `first_page` (i.e., from a
    /// virtually-contiguous "mapping"), apply the in-page offset,
    /// and parse. If the result matches the original DTB the
    /// pipeline is end-to-end correct.
    fn round_trip_at_offset(in_page_offset: usize) {
        // Build a synthetic "physical memory" buffer with PAGE_SIZE
        // padding on each side so we can safely place the DTB at
        // any offset and still read its containing pages.
        let mut mem: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let lead_padding = 0x1000;
        mem.resize(lead_padding + in_page_offset + QEMU_DTB.len() + 0x1000, 0xCD);
        // "Place" the DTB at lead_padding + in_page_offset.
        let dtb_paddr_in_mem = lead_padding + in_page_offset;
        mem[dtb_paddr_in_mem..dtb_paddr_in_mem + QEMU_DTB.len()]
            .copy_from_slice(QEMU_DTB);

        // Read the FDT header to get the content size, just like the
        // kernel boot path does.
        let header_window = &mem[dtb_paddr_in_mem..dtb_paddr_in_mem + 40];
        let content_size = crate::fdt::dtb_content_size(header_window).unwrap();

        // Compute the layout. We use a fictitious "physical address"
        // — for the test, pretend `dtb_paddr_in_mem` *is* the
        // physical address. The layout math doesn't care that the
        // address is small; only the alignment matters.
        let dtb_paddr = PhysAddr::new(dtb_paddr_in_mem as u64);
        let layout = compute_layout(dtb_paddr, content_size);

        // Sanity: the offset we put the DTB at should match what the
        // layout reports.
        assert_eq!(layout.in_page_offset as usize,
                   in_page_offset & (PAGE_SIZE as usize - 1));

        // Simulate "userspace mapping": gather the bytes for the
        // page_count pages starting at first_page, in order. This
        // is what `sys_map_pages` produces in the userspace VA — a
        // contiguous view of the physical pages.
        let first_page_paddr = layout.first_page.start_addr().as_u64() as usize;
        let mapped: alloc::vec::Vec<u8> =
            mem[first_page_paddr..first_page_paddr + layout.page_count * PAGE_SIZE as usize]
                .to_vec();

        // Apply the in-page offset to find the DTB start, then read
        // exactly content_size bytes — matches what userspace will
        // do after `sys_get_boot_info` returns the offset.
        let dtb_start = layout.in_page_offset as usize;
        let recovered = &mapped[dtb_start..dtb_start + content_size];

        // The recovered slice must parse as a valid FDT.
        let parsed = crate::fdt::parse_fdt(recovered).expect("recovered DTB parses");
        assert!(parsed.count > 0, "recovered DTB should have devices");

        // And the bytes must match the original.
        assert_eq!(recovered, &QEMU_DTB[..content_size]);
    }

    #[test]
    fn end_to_end_aligned_offset_zero() {
        round_trip_at_offset(0);
    }

    #[test]
    fn end_to_end_pi_like_offset_0xe00() {
        // The Pi 4B case: low 12 bits = 0xe00.
        round_trip_at_offset(0xe00);
    }

    #[test]
    fn end_to_end_offset_one_byte() {
        round_trip_at_offset(1);
    }

    #[test]
    fn end_to_end_offset_just_below_page_size() {
        round_trip_at_offset(PAGE_SIZE as usize - 1);
    }

    #[test]
    fn end_to_end_offset_half_page() {
        round_trip_at_offset(PAGE_SIZE as usize / 2);
    }
}
