/// Pure PageSet table and header model for tracking allocated page sets.
///
/// This is the decision logic -- no static mut, no page allocator, no unsafe.
/// The kernel owns the mutable state and calls these functions to determine
/// what to do. Tests can create instances and verify behavior directly.

use crate::addr::PAGE_SIZE;

/// Maximum number of PageSets tracked in a table.
pub const MAX_PAGESETS: usize = 128;

/// Practical upper bound on data pages per PageSet. Not a hard ABI cap —
/// the variable-size header layout permits more, but this bounds the
/// header allocation (header_pages_for(16384) = 33 contiguous pages =
/// 132 KiB) so a runaway request can't ask for the entire physical
/// memory window of header. mmap's `MAX_MMAP_BYTES` is sized to match
/// (16384 * 4 KiB = 64 MiB max per mmap).
pub const MAX_PRACTICAL_PAGES_PER_SET: usize = 16_384;

// ---------------------------------------------------------------------------
// PageSetHeader -- lives in the first one or more allocated pages of a PageSet
// ---------------------------------------------------------------------------

/// Page-resident header for a PageSet. Stored at the start of one or
/// more physically-contiguous header pages. The metadata struct is a
/// fixed 16 bytes; the data-page address array immediately follows
/// (starting at byte offset 16) and may extend across header pages.
///
/// **Layout invariant**: when allocated by the kernel, `header_pages`
/// contiguous physical pages back this header. Reads and writes of the
/// page-addr array via [`Self::get_page`] / [`Self::set_page`] use raw
/// pointer arithmetic that may step into the second/third/etc. backing
/// page; this is sound because:
///
/// 1. The kernel allocates the backing pages contiguously
///    (`alloc_pages_contiguous`).
/// 2. The kernel addresses physical memory via a single contiguous
///    KERNEL_VA_OFFSET map, so adjacent physical pages have adjacent
///    kernel virtual addresses.
///
/// Tests construct headers via a backing `Vec<u8>` of the right size —
/// see the `TestHeader` helper in the test module.
#[repr(C)]
pub struct PageSetHeader {
    /// Number of data pages in the set (not counting header pages).
    pub count: u32,
    /// Number of contiguous header pages backing this struct (>= 1).
    /// Used by the deallocator to know how many pages to free.
    pub header_pages: u32,
    /// Handle reference count. Incremented on handle_insert, decremented
    /// on handle_remove. Initialized to 0 by page zeroing.
    pub refcount: u32,
    /// Active mapping count across all processes. Incremented by
    /// sys_map_pages, decremented by sys_unmap_pages. Pages are freed
    /// when both refcount and map_count reach zero.
    pub map_count: u32,
    // pages[]: `count` u64 values starting at byte offset 16, possibly
    // spanning into subsequent header pages. Accessed only via
    // get_page / set_page which compute the byte offset.
}

/// Number of contiguous header pages required to hold metadata + an
/// array of `count` u64 page addresses. Header metadata is 16 bytes;
/// each page address is 8 bytes; result is rounded up to whole pages.
///
/// For count ≤ 510: 1 page (the legacy single-page-header case).
/// For count = 511: 2 pages.
/// For count = 16384 (the practical cap): 33 pages.
pub const fn header_pages_for(count: usize) -> usize {
    let bytes = core::mem::size_of::<PageSetHeader>() + count * 8;
    (bytes + (PAGE_SIZE as usize) - 1) / (PAGE_SIZE as usize)
}

impl PageSetHeader {
    /// An empty header — a 16-byte stack value with zero data pages
    /// and `header_pages = 1`. Safe regardless of where it lives:
    /// `BackedHeader` / `BackedHeaderMut` are the only way to access
    /// the page-addr array, and they require an unsafe construction
    /// that would have to forge a backing-pages witness.
    pub const fn empty() -> Self {
        Self {
            count: 0,
            header_pages: 1,
            refcount: 0,
            map_count: 0,
        }
    }

    /// Number of data pages.
    pub fn data_page_count(&self) -> usize {
        self.count as usize
    }

    /// Number of contiguous header pages backing this header.
    /// Read from the inline `header_pages` field — populated by the
    /// kernel allocator on init.
    pub fn header_page_count(&self) -> usize {
        self.header_pages as usize
    }

    /// Wrap as a read-only backed view. The wrapper carries BOTH
    /// the trusted logical `count` and the derived backing-pages
    /// witness. All subsequent operations use these instead of the
    /// on-disk `header.count` / `header.header_pages` fields.
    ///
    /// # Safety
    /// `count` MUST come from trusted external state (the
    /// PageSetTable's registered count), NOT from `self.count`. The
    /// caller MUST guarantee that `header_pages_for(count)`
    /// PAGE_SIZE physically-contiguous pages are mapped at the
    /// address of `self`.
    pub unsafe fn backed(&self, count: usize) -> BackedHeader<'_> {
        let backing_pages = header_pages_for(count);
        BackedHeader { header: self, count, backing_pages }
    }

    /// Wrap as a writable backed view.
    ///
    /// # Safety
    /// Same contract as `backed`. `count` is the upper bound that
    /// `set_page` will accept; `init`/`set_count` can shrink it
    /// (which writes the new value to `header.count`) but cannot grow
    /// past the original backing.
    pub unsafe fn backed_mut(&mut self, count: usize) -> BackedHeaderMut<'_> {
        let backing_pages = header_pages_for(count);
        BackedHeaderMut { header: self, count, backing_pages }
    }

    /// Increment the mapping count. Called when pages are mapped into
    /// an address space via sys_map_pages.
    pub fn inc_map_count(&mut self) {
        self.map_count = self.map_count.checked_add(1)
            .unwrap_or_else(|| panic!("map_count overflow"));
    }

    /// Decrement the mapping count. Called when pages are unmapped.
    /// Returns true if both map_count and refcount are now zero
    /// (caller should free the PageSet).
    pub fn dec_map_count(&mut self) -> bool {
        assert!(self.map_count > 0, "map_count underflow");
        self.map_count -= 1;
        self.map_count == 0 && self.refcount == 0
    }

    /// Increment the handle reference count. Called when a new handle
    /// to this PageSet is inserted into a handle table.
    pub fn inc_refcount(&mut self) {
        self.refcount = self.refcount.checked_add(1)
            .unwrap_or_else(|| panic!("refcount overflow"));
    }

    /// Decrement the handle reference count. Called when a handle to
    /// this PageSet is removed (sys_close_handle).
    /// Returns true if both refcount and map_count are now zero
    /// (caller should free the PageSet).
    pub fn dec_refcount(&mut self) -> bool {
        assert!(self.refcount > 0, "refcount underflow");
        self.refcount -= 1;
        self.refcount == 0 && self.map_count == 0
    }
}

// ---------------------------------------------------------------------------
// BackedHeader / BackedHeaderMut -- safe access to the page-addr array
// ---------------------------------------------------------------------------

/// Read-only view over a `PageSetHeader` plus trusted witnesses for
/// (a) the logical data-page count and (b) the backing-pages extent.
/// Both come from external trusted state (typically the kernel's
/// `PageSetTable`); the wrapper does NOT read `header.count` or
/// `header.header_pages` for its own bounds checks. A
/// corrupted/forged on-disk header therefore cannot truncate or
/// extend the wrapper's view.
///
/// Constructed via `unsafe PageSetHeader::backed(count)`, which
/// derives `backing_pages = header_pages_for(count)`. Once
/// constructed, all page-addr accessors are safe.
///
/// Inline-field operations (refcount, map_count, the on-disk
/// header.count / header.header_pages reads) reach the underlying
/// `PageSetHeader` via `Deref`. Trusted-count operations
/// (`data_page_count`, `header_page_count`, `get_page`,
/// `pages_slice`) are inherent methods on the wrapper and take
/// precedence over the Deref-target methods of the same name.
pub struct BackedHeader<'a> {
    header: &'a PageSetHeader,
    /// Trusted data-page count from external state. Drives all
    /// logical bounds. Independent of `header.count`.
    count: usize,
    /// Derived backing-pages witness = `header_pages_for(count)`.
    /// Drives the safety bound for byte-offset checks.
    backing_pages: usize,
}

impl<'a> BackedHeader<'a> {
    /// Trusted data-page count (from the wrapper's witness, NOT from
    /// `header.count`). Inherent method; takes precedence over the
    /// Deref-target `PageSetHeader::data_page_count`.
    pub fn data_page_count(&self) -> usize {
        self.count
    }

    /// Trusted backing-pages count (= header_pages_for(count)).
    /// Inherent method; takes precedence over the Deref-target
    /// `PageSetHeader::header_page_count`.
    pub fn header_page_count(&self) -> usize {
        self.backing_pages
    }

    /// Get the physical address of data page `index`. Returns None
    /// if `index >= count`. Both bounds use trusted state.
    pub fn get_page(&self, index: usize) -> Option<u64> {
        if index >= self.count {
            return None;
        }
        let byte_offset = core::mem::size_of::<PageSetHeader>() + index * 8;
        // Defense in depth: backing_pages was derived from count via
        // header_pages_for, so this assertion always holds for
        // index < count. Crashes loudly if the invariant is broken.
        debug_assert!(
            byte_offset + 8 <= self.backing_pages * (crate::addr::PAGE_SIZE as usize),
            "BackedHeader::get_page: byte_offset {} + 8 > backing {} bytes",
            byte_offset, self.backing_pages * (crate::addr::PAGE_SIZE as usize),
        );
        // SAFETY: trusted backing_pages bounds the read; index < count
        // bounds the byte_offset within backing.
        unsafe {
            let base = self.header as *const PageSetHeader as *const u8;
            Some(core::ptr::read(base.add(byte_offset) as *const u64))
        }
    }

    /// Borrow the data-page address array as `&[u64]`. Length is the
    /// trusted count, NOT `header.count`.
    pub fn pages_slice(&self) -> &[u64] {
        // SAFETY: backing_pages = header_pages_for(count), so
        // size_of::<PageSetHeader>() + count*8 <= backing_pages *
        // PAGE_SIZE. The slice is fully within trusted backing.
        unsafe {
            let base = self.header as *const PageSetHeader as *const u8;
            let pages_ptr = base.add(core::mem::size_of::<PageSetHeader>()) as *const u64;
            core::slice::from_raw_parts(pages_ptr, self.count)
        }
    }

    /// Re-borrow the underlying raw header. Useful for inline-field
    /// reads (refcount, map_count) without going through Deref —
    /// note that `raw().count` is the on-disk value, NOT the trusted
    /// wrapper count; use `data_page_count()` for the trusted value.
    pub fn raw(&self) -> &PageSetHeader {
        self.header
    }
}

impl<'a> core::ops::Deref for BackedHeader<'a> {
    type Target = PageSetHeader;
    fn deref(&self) -> &PageSetHeader {
        self.header
    }
}

/// Writable view. Same wrapper-tracked count + backing as
/// `BackedHeader`; `set_count` and `init` update both the wrapper's
/// tracked count AND the on-disk `header.count` / `header.header_pages`
/// so kernel paths that later read the on-disk values see what the
/// allocator just wrote.
pub struct BackedHeaderMut<'a> {
    header: &'a mut PageSetHeader,
    count: usize,
    backing_pages: usize,
}

impl<'a> BackedHeaderMut<'a> {
    /// Trusted data-page count.
    pub fn data_page_count(&self) -> usize {
        self.count
    }

    /// Trusted backing-pages count.
    pub fn header_page_count(&self) -> usize {
        self.backing_pages
    }

    /// Initialize the header for `page_addrs.len()` data pages.
    /// Updates the wrapper's tracked count, writes header.count,
    /// header.header_pages, zeroes refcount/map_count, then writes
    /// the page addresses.
    ///
    /// Asserts `header_pages_for(page_addrs.len()) <= backing_pages` —
    /// calling with more pages than the wrapper was constructed for
    /// would write past the backing.
    pub fn init(&mut self, page_addrs: &[u64]) {
        self.set_count(page_addrs.len());
        self.header.refcount = 0;
        self.header.map_count = 0;
        for (i, addr) in page_addrs.iter().enumerate() {
            self.set_page(i, *addr);
        }
    }

    /// Update count: writes `header.count` and `header.header_pages`
    /// AND updates the wrapper's tracked `self.count`. Asserts the
    /// new count fits within the backing established at construction.
    pub fn set_count(&mut self, count: usize) {
        let needed = header_pages_for(count);
        assert!(
            needed <= self.backing_pages,
            "BackedHeaderMut::set_count: count {} needs {} backing pages, \
             have {}",
            count, needed, self.backing_pages,
        );
        self.header.count = count as u32;
        self.header.header_pages = needed as u32;
        self.count = count;
    }

    /// Write `addr` as the physical address of data page `index`.
    /// Bounds against the wrapper's trusted count.
    pub fn set_page(&mut self, index: usize, addr: u64) {
        assert!(
            index < self.count,
            "set_page out of range: index={}, count={}",
            index, self.count,
        );
        let byte_offset = core::mem::size_of::<PageSetHeader>() + index * 8;
        debug_assert!(
            byte_offset + 8 <= self.backing_pages * (crate::addr::PAGE_SIZE as usize),
            "set_page: byte_offset {} + 8 > backing {} bytes",
            byte_offset, self.backing_pages * (crate::addr::PAGE_SIZE as usize),
        );
        // SAFETY: index < count bounds the byte_offset; backing_pages
        // = header_pages_for(count) ensures the write lands in backing.
        unsafe {
            let base = self.header as *mut PageSetHeader as *mut u8;
            core::ptr::write(base.add(byte_offset) as *mut u64, addr);
        }
    }

    /// Read a page address. Bounds against the wrapper's trusted count.
    pub fn get_page(&self, index: usize) -> Option<u64> {
        BackedHeader {
            header: self.header,
            count: self.count,
            backing_pages: self.backing_pages,
        }
        .get_page(index)
    }
}

impl<'a> core::ops::Deref for BackedHeaderMut<'a> {
    type Target = PageSetHeader;
    fn deref(&self) -> &PageSetHeader {
        self.header
    }
}

impl<'a> core::ops::DerefMut for BackedHeaderMut<'a> {
    fn deref_mut(&mut self) -> &mut PageSetHeader {
        self.header
    }
}

// ---------------------------------------------------------------------------
// PageSetEntry -- thin entry stored in the tracking table
// ---------------------------------------------------------------------------

/// A tracked PageSet: just the count and the header's kernel virtual
/// address in the KVM pool. The actual data page addresses live in
/// the header itself, not in the table.
///
/// `header_kva` is a `KernelVa`, not a `PhysAddr`: PageSet headers
/// live in the KVM pool (virtually contiguous, physically scattered
/// frames stitched into the kernel TTBR1 tree). Code that reads a
/// PageSet header through this field uses `KernelMut/Ref::from_kva`,
/// not `from_paddr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSetEntry {
    pub count: usize,
    pub header_kva: crate::addr::KernelVa,
}

/// Errors from PageSet table operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSetError {
    TableFull,
    InvalidCount,
    InvalidId,
    SlotEmpty,
}

/// A table that tracks allocated PageSets by slot index.
/// No global state -- the caller owns the instance.
pub struct PageSetTable {
    slots: [Option<PageSetEntry>; MAX_PAGESETS],
}

impl PageSetTable {
    /// Create an empty table.
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_PAGESETS],
        }
    }

    /// Reserve a slot and store a PageSet entry. Returns the slot index (ID).
    pub fn insert(&mut self, entry: PageSetEntry) -> Result<usize, PageSetError> {
        if entry.count == 0 || entry.count > MAX_PRACTICAL_PAGES_PER_SET {
            return Err(PageSetError::InvalidCount);
        }

        let slot = self.slots.iter()
            .position(|s| s.is_none())
            .ok_or(PageSetError::TableFull)?;

        self.slots[slot] = Some(entry);
        Ok(slot)
    }

    /// Look up a PageSet by ID.
    pub fn get(&self, id: usize) -> Result<&PageSetEntry, PageSetError> {
        if id >= MAX_PAGESETS {
            return Err(PageSetError::InvalidId);
        }
        self.slots[id].as_ref().ok_or(PageSetError::SlotEmpty)
    }

    /// Remove a PageSet by ID, returning it. The caller is responsible for
    /// freeing the physical pages.
    pub fn remove(&mut self, id: usize) -> Result<PageSetEntry, PageSetError> {
        if id >= MAX_PAGESETS {
            return Err(PageSetError::InvalidId);
        }
        self.slots[id].take().ok_or(PageSetError::SlotEmpty)
    }

    /// Number of occupied slots.
    pub fn count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Find a PageSet by its header kernel virtual address. Returns
    /// the slot index, or None if no entry has that header_kva.
    /// Used for reverse lookup when consuming via a handle.
    pub fn find_by_header_kva(&self, header_kva: crate::addr::KernelVa) -> Option<usize> {
        for i in 0..MAX_PAGESETS {
            if let Some(entry) = &self.slots[i] {
                if entry.header_kva == header_kva {
                    return Some(i);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(count: usize) -> PageSetEntry {
        PageSetEntry {
            count,
            header_kva: crate::addr::KernelVa::new(0x1000 * (count as u64 + 1)),
        }
    }

    // --- PageSetTable tests ---

    #[test]
    fn insert_and_get() {
        let mut table = PageSetTable::new();
        let entry = make_entry(2);
        let id = table.insert(entry).unwrap();
        assert_eq!(id, 0);

        let got = table.get(id).unwrap();
        assert_eq!(got.count, 2);
    }

    #[test]
    fn insert_fills_slots_sequentially() {
        let mut table = PageSetTable::new();
        for i in 0..5 {
            let id = table.insert(make_entry(1)).unwrap();
            assert_eq!(id, i);
        }
        assert_eq!(table.count(), 5);
    }

    #[test]
    fn insert_reuses_removed_slot() {
        let mut table = PageSetTable::new();
        let id0 = table.insert(make_entry(1)).unwrap();
        let id1 = table.insert(make_entry(1)).unwrap();
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);

        table.remove(id0).unwrap();
        assert_eq!(table.count(), 1);

        let id2 = table.insert(make_entry(1)).unwrap();
        assert_eq!(id2, 0);
    }

    #[test]
    fn table_full() {
        let mut table = PageSetTable::new();
        for _ in 0..MAX_PAGESETS {
            table.insert(make_entry(1)).unwrap();
        }
        assert_eq!(table.insert(make_entry(1)), Err(PageSetError::TableFull));
    }

    #[test]
    fn invalid_count_zero() {
        let mut table = PageSetTable::new();
        assert_eq!(
            table.insert(PageSetEntry { count: 0, header_kva: crate::addr::KernelVa::new(0x1000) }),
            Err(PageSetError::InvalidCount)
        );
    }

    #[test]
    fn invalid_count_too_large() {
        let mut table = PageSetTable::new();
        assert_eq!(
            table.insert(PageSetEntry { count: MAX_PRACTICAL_PAGES_PER_SET + 1, header_kva: crate::addr::KernelVa::new(0x1000) }),
            Err(PageSetError::InvalidCount)
        );
    }

    #[test]
    fn get_invalid_id() {
        let table = PageSetTable::new();
        assert_eq!(table.get(MAX_PAGESETS), Err(PageSetError::InvalidId));
        assert_eq!(table.get(999), Err(PageSetError::InvalidId));
    }

    #[test]
    fn get_empty_slot() {
        let table = PageSetTable::new();
        assert_eq!(table.get(0), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn consume_after_get_prevents_reuse() {
        let mut table = PageSetTable::new();
        let id = table.insert(make_entry(1)).unwrap();

        let entry = table.get(id).unwrap();
        assert_eq!(entry.count, 1);

        table.remove(id).unwrap();

        assert_eq!(table.get(id), Err(PageSetError::SlotEmpty));
        assert_eq!(table.remove(id), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn consume_does_not_affect_other_entries() {
        let mut table = PageSetTable::new();
        let id0 = table.insert(make_entry(1)).unwrap();
        let id1 = table.insert(make_entry(2)).unwrap();

        table.remove(id0).unwrap();

        let entry = table.get(id1).unwrap();
        assert_eq!(entry.count, 2);
    }

    #[test]
    fn remove_returns_entry() {
        let mut table = PageSetTable::new();
        let id = table.insert(make_entry(3)).unwrap();

        let removed = table.remove(id).unwrap();
        assert_eq!(removed.count, 3);

        assert_eq!(table.get(id), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn remove_empty_slot_fails() {
        let mut table = PageSetTable::new();
        assert_eq!(table.remove(0), Err(PageSetError::SlotEmpty));
    }

    #[test]
    fn remove_invalid_id_fails() {
        let mut table = PageSetTable::new();
        assert_eq!(table.remove(MAX_PAGESETS), Err(PageSetError::InvalidId));
    }

    // --- PageSetHeader tests ---

    extern crate std;
    use std::alloc::{alloc_zeroed, dealloc, Layout};

    /// Test backing for a variable-size PageSetHeader. Allocates a
    /// `header_pages_for(count) * PAGE_SIZE` byte block, aligned to
    /// PAGE_SIZE so the layout matches the kernel's
    /// `alloc_pages_contiguous(header_pages)` invariant. PAGE_SIZE
    /// alignment also satisfies the smaller alignof(PageSetHeader) = 4
    /// and alignof(u64) = 8 needed by typed accesses. The block stays
    /// alive for the test's lifetime via Drop.
    ///
    /// Tests get a `BackedHeader` / `BackedHeaderMut` wrapper from the
    /// helpers below — there is exactly one `unsafe { backed_mut(...) }`
    /// per test, encapsulated in `TestHeader::backed_mut`.
    struct TestHeader {
        ptr: *mut u8,
        layout: Layout,
        /// The trusted count this TestHeader was sized for. Passed to
        /// `backed`/`backed_mut` as the wrapper's logical-count
        /// witness. Mirrors the kernel's PageSetTable count.
        count: usize,
    }
    impl TestHeader {
        fn new(count: usize) -> Self {
            let pages = header_pages_for(count);
            let size = pages * (PAGE_SIZE as usize);
            let layout = Layout::from_size_align(size, PAGE_SIZE as usize).unwrap();
            // SAFETY: layout has nonzero size; alloc_zeroed returns
            // PAGE_SIZE-aligned zeroed memory or null on failure.
            let ptr = unsafe { alloc_zeroed(layout) };
            assert!(!ptr.is_null(), "TestHeader alloc failed");
            Self { ptr, layout, count }
        }

        /// Read-only backed wrapper. The single point of unsafety:
        /// the `backed` constructor needs the witness that
        /// `header_pages_for(count) * PAGE_SIZE` bytes are mapped
        /// contiguously, which alloc_zeroed established above.
        fn backed(&self) -> BackedHeader<'_> {
            // SAFETY: alloc_zeroed gave us header_pages_for(count) *
            // PAGE_SIZE bytes, PAGE_SIZE-aligned, owned exclusively
            // for the lifetime of self. The count is what was passed
            // at construction; the wrapper derives backing_pages
            // internally as header_pages_for(count).
            unsafe {
                let h = &*(self.ptr as *const PageSetHeader);
                h.backed(self.count)
            }
        }

        /// Writable backed wrapper. Same single unsafe point.
        fn backed_mut(&mut self) -> BackedHeaderMut<'_> {
            // SAFETY: same as backed(), plus we hold &mut self so no
            // aliasing into the storage exists.
            unsafe {
                let h = &mut *(self.ptr as *mut PageSetHeader);
                h.backed_mut(self.count)
            }
        }
    }
    impl Drop for TestHeader {
        fn drop(&mut self) {
            // SAFETY: ptr/layout are exactly what alloc_zeroed returned.
            unsafe { dealloc(self.ptr, self.layout); }
        }
    }

    #[test]
    fn header_empty() {
        // PageSetHeader::empty() is a stack value with count=0. Inline
        // accessors (data_page_count, header_page_count) are safe to
        // call directly; page-addr accessors are not reachable without
        // an unsafe `backed` construction (which a sound caller cannot
        // perform on a 16-byte stack value).
        let h = PageSetHeader::empty();
        assert_eq!(h.data_page_count(), 0);
        assert_eq!(h.header_page_count(), 1);
    }

    #[test]
    fn header_init_and_get() {
        let mut t = TestHeader::new(3);
        t.backed_mut().init(&[0x1000, 0x2000, 0x3000]);
        let b = t.backed();
        assert_eq!(b.data_page_count(), 3);
        assert_eq!(b.get_page(0), Some(0x1000));
        assert_eq!(b.get_page(1), Some(0x2000));
        assert_eq!(b.get_page(2), Some(0x3000));
        assert_eq!(b.get_page(3), None);
    }

    #[test]
    fn header_single_page() {
        let mut t = TestHeader::new(1);
        t.backed_mut().init(&[0xABCD_0000]);
        let b = t.backed();
        assert_eq!(b.data_page_count(), 1);
        assert_eq!(b.get_page(0), Some(0xABCD_0000));
    }

    #[test]
    fn header_many_pages() {
        // 75 pages (320x240x4 framebuffer-ish). Single-page header,
        // same as before — still well within the 510-entry first page.
        let mut t = TestHeader::new(75);
        let mut addrs = [0u64; 75];
        for i in 0..75 {
            addrs[i] = (i as u64 + 1) * 0x1000;
        }
        t.backed_mut().init(&addrs);
        let b = t.backed();
        assert_eq!(b.data_page_count(), 75);
        assert_eq!(b.get_page(0), Some(0x1000));
        assert_eq!(b.get_page(74), Some(75 * 0x1000));
        assert_eq!(b.get_page(75), None);
        assert_eq!(b.header_page_count(), 1);
    }

    #[test]
    fn header_metadata_is_16_bytes() {
        // Pinned ABI: the metadata struct is exactly 16 bytes so the
        // page-addr array starts at byte offset 16. Variable-size
        // header layout depends on this.
        assert_eq!(core::mem::size_of::<PageSetHeader>(), 16);
    }

    // --- header_pages_for arithmetic ---

    #[test]
    fn header_pages_for_small_counts() {
        assert_eq!(header_pages_for(0), 1);   // metadata only fits in 1 page
        assert_eq!(header_pages_for(1), 1);
        assert_eq!(header_pages_for(510), 1); // (16 + 510*8) / 4096 = 1
    }

    #[test]
    fn header_pages_for_boundary_at_510() {
        // 510 entries: 16 + 510*8 = 4096 bytes exactly → 1 page.
        assert_eq!(header_pages_for(510), 1);
        // 511 entries: 16 + 511*8 = 4104 bytes → 2 pages.
        assert_eq!(header_pages_for(511), 2);
    }

    #[test]
    fn header_pages_for_boundary_at_1022() {
        // 1022 entries: 16 + 1022*8 = 8192 bytes exactly → 2 pages.
        assert_eq!(header_pages_for(1022), 2);
        // 1023: needs 3 pages.
        assert_eq!(header_pages_for(1023), 3);
    }

    #[test]
    fn header_pages_for_max_practical() {
        // 16384 entries: 16 + 16384*8 = 131088 bytes → 33 pages.
        assert_eq!(header_pages_for(MAX_PRACTICAL_PAGES_PER_SET), 33);
    }

    // --- Cross-page get/set round-trips ---

    #[test]
    fn header_get_set_at_page_boundary() {
        // Index 510 is the first entry on header page 1. Index 511 is
        // immediately after. Round-trip through both to verify the
        // raw-pointer arithmetic crosses page boundaries correctly.
        let mut t = TestHeader::new(513);
        let mut bm = t.backed_mut();
        bm.set_count(513);
        bm.set_page(509, 0xAAAA_0000);
        bm.set_page(510, 0xBBBB_0000); // first entry on page 2
        bm.set_page(511, 0xCCCC_0000);
        bm.set_page(512, 0xDDDD_0000);
        drop(bm);
        let b = t.backed();
        assert_eq!(b.get_page(509), Some(0xAAAA_0000));
        assert_eq!(b.get_page(510), Some(0xBBBB_0000));
        assert_eq!(b.get_page(511), Some(0xCCCC_0000));
        assert_eq!(b.get_page(512), Some(0xDDDD_0000));
        assert_eq!(b.header_page_count(), 2);
    }

    #[test]
    fn header_init_writes_across_pages() {
        // Initialize a 2-page-header PageSet and verify every entry.
        let mut t = TestHeader::new(800);
        let mut addrs = std::vec::Vec::with_capacity(800);
        for i in 0..800 {
            addrs.push((i as u64 + 1) * 0x1000);
        }
        t.backed_mut().init(&addrs);
        let b = t.backed();
        assert_eq!(b.data_page_count(), 800);
        assert_eq!(b.header_page_count(), 2);
        for i in 0..800 {
            assert_eq!(b.get_page(i), Some((i as u64 + 1) * 0x1000));
        }
        assert_eq!(b.get_page(800), None);
    }

    #[test]
    fn header_init_at_max_practical() {
        // Sanity: a maximum-size PageSet round-trips correctly.
        // 16384 entries, 33 header pages.
        let mut t = TestHeader::new(MAX_PRACTICAL_PAGES_PER_SET);
        let mut addrs = std::vec::Vec::with_capacity(MAX_PRACTICAL_PAGES_PER_SET);
        for i in 0..MAX_PRACTICAL_PAGES_PER_SET {
            addrs.push((i as u64 + 1) * 0x1000);
        }
        t.backed_mut().init(&addrs);
        let b = t.backed();
        assert_eq!(b.data_page_count(), MAX_PRACTICAL_PAGES_PER_SET);
        assert_eq!(b.header_page_count(), 33);
        // Spot-check first, last, and a couple of cross-page indices.
        assert_eq!(b.get_page(0), Some(0x1000));
        assert_eq!(b.get_page(510), Some(511 * 0x1000));
        assert_eq!(b.get_page(1022), Some(1023 * 0x1000));
        assert_eq!(b.get_page(MAX_PRACTICAL_PAGES_PER_SET - 1),
                   Some(MAX_PRACTICAL_PAGES_PER_SET as u64 * 0x1000));
    }

    #[test]
    #[should_panic(expected = "needs 2 backing pages")]
    fn header_init_rejects_too_large_for_backing() {
        // TestHeader allocates header_pages_for(3) = 1 page. Calling
        // init with 511 page addrs requires 2 header pages — the
        // wrapper must panic instead of writing past backing.
        let mut t = TestHeader::new(3);
        let addrs = std::vec![0u64; 511];
        t.backed_mut().init(&addrs);
    }

    #[test]
    #[should_panic(expected = "set_count")]
    fn header_set_count_rejects_too_large_for_backing() {
        let mut t = TestHeader::new(3);
        // Try to expand to 511 entries (would need 2 header pages)
        // when only 1 is backed.
        t.backed_mut().set_count(511);
    }

    // --- find_by_header_kva ---

    #[test]
    fn find_by_header_kva_found() {
        let mut table = PageSetTable::new();
        let e0 = PageSetEntry { count: 1, header_kva: crate::addr::KernelVa::new(0xA000) };
        let e1 = PageSetEntry { count: 2, header_kva: crate::addr::KernelVa::new(0xB000) };
        let id0 = table.insert(e0).unwrap();
        let _id1 = table.insert(e1).unwrap();

        assert_eq!(table.find_by_header_kva(crate::addr::KernelVa::new(0xA000)), Some(id0));
        assert_eq!(table.find_by_header_kva(crate::addr::KernelVa::new(0xB000)), Some(1));
    }

    #[test]
    fn find_by_header_kva_not_found() {
        let mut table = PageSetTable::new();
        table.insert(PageSetEntry { count: 1, header_kva: crate::addr::KernelVa::new(0xA000) }).unwrap();
        assert_eq!(table.find_by_header_kva(crate::addr::KernelVa::new(0xC000)), None);
    }

    #[test]
    fn find_by_header_kva_after_remove() {
        let mut table = PageSetTable::new();
        let id = table.insert(PageSetEntry { count: 1, header_kva: crate::addr::KernelVa::new(0xA000) }).unwrap();
        table.remove(id).unwrap();
        assert_eq!(table.find_by_header_kva(crate::addr::KernelVa::new(0xA000)), None);
    }

    // --- map_count lifecycle ---

    #[test]
    fn map_count_inc_dec() {
        let mut h = PageSetHeader::empty();
        h.refcount = 1;
        h.inc_map_count();
        assert_eq!(h.map_count, 1);
        h.inc_map_count();
        assert_eq!(h.map_count, 2);
        assert!(!h.dec_map_count()); // map_count=1, refcount=1 → not free
        assert!(!h.dec_map_count()); // map_count=0, refcount=1 → not free
    }

    #[test]
    fn map_count_zero_with_zero_refcount_signals_free() {
        let mut h = PageSetHeader::empty();
        h.refcount = 0;
        h.inc_map_count();
        assert!(h.dec_map_count()); // map_count=0, refcount=0 → free
    }

    #[test]
    #[should_panic(expected = "map_count underflow")]
    fn map_count_underflow_panics() {
        let mut h = PageSetHeader::empty();
        h.dec_map_count();
    }

    // --- zeroed header makes stale reads inert ---

    #[test]
    fn zeroed_header_has_no_pages() {
        let h = PageSetHeader::empty();
        // A zeroed header (as would result from page zeroing after
        // consumption) must report 0 pages. Page-addr access requires
        // a BackedHeader, which a sound caller can't construct from
        // an empty stack header — that's the type-system enforced
        // safety property.
        assert_eq!(h.data_page_count(), 0);
    }

    // --- refcount ---

    #[test]
    fn inc_refcount() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        assert_eq!(h.refcount, 1);
        h.inc_refcount();
        assert_eq!(h.refcount, 2);
    }

    #[test]
    fn dec_refcount_not_zero() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        h.inc_refcount();
        assert!(!h.dec_refcount()); // refcount 1, not free-on-zero
    }

    #[test]
    fn dec_refcount_free_on_zero() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        assert!(h.dec_refcount()); // refcount 0, map_count 0 → free
    }

    #[test]
    fn dec_refcount_not_free_if_mapped() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        h.inc_map_count();
        assert!(!h.dec_refcount()); // refcount 0, map_count 1 → NOT free
    }

    #[test]
    fn dec_map_count_free_when_refcount_zero() {
        let mut h = PageSetHeader::empty();
        h.inc_map_count();
        // refcount is 0, map_count goes to 0 → free
        assert!(h.dec_map_count());
    }

    #[test]
    fn dec_map_count_not_free_when_refcount_nonzero() {
        let mut h = PageSetHeader::empty();
        h.inc_refcount();
        h.inc_map_count();
        assert!(!h.dec_map_count()); // refcount 1 → NOT free
    }

    #[test]
    #[should_panic(expected = "refcount underflow")]
    fn dec_refcount_underflow_panics() {
        let mut h = PageSetHeader::empty();
        h.dec_refcount();
    }
}
