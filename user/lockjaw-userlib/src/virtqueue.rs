//! Split virtqueue runtime for VirtIO MMIO transport.
//!
//! Operates on a contiguous physical allocation supplied via a
//! `DmaPage`. All shared-memory accesses go through `lockjaw-mmio`'s
//! `DmaCell` / `DmaSliceDyn` (wrapped in userlib's lifetime-bound
//! `CellRef` / `SliceRef`) so the virtqueue protocol logic is
//! `unsafe`-free in this crate and the driver crate above. AArch64
//! barriers come from `lockjaw_mmio::barrier::*` and order
//! descriptor writes against avail/used ring updates and MMIO notify.
//!
//! Reusable across any VirtIO device (block, net, gpu).
//!
//! Structurally, the Virtqueue stores ONLY the backing `DmaPage` +
//! layout + free-list state. Typed views are computed on each
//! access via tiny inline accessors (`desc()`, `avail_header()`,
//! etc.) so each view borrows `&self` and cannot outlive the
//! backing page. Storing owned views would require self-referential
//! types and would also let view lifetimes escape the page's.

use crate::dma::{CellRef, DmaPage, SliceRef};
use lockjaw_mmio::barrier::{dmb_ish, dmb_ishld, dmb_ishst};
use lockjaw_types::virtio::{
    VirtqAvail, VirtqDesc, VirtqUsed, VirtqUsedElem, VirtqueueLayout,
    virtqueue_layout, VIRTQ_DESC_F_NEXT,
};

/// A split virtqueue backed by a contiguous DMA region.
///
/// The caller allocates a `DmaPage` of the right size (see
/// `virtqueue_layout(queue_size).total_size`) and hands it to
/// `Virtqueue::new`. The Virtqueue takes ownership of the backing
/// region so the mapping outlives the typed views computed on each
/// access.
pub struct Virtqueue {
    /// Owns the backing DMA region; keeps the mapping alive.
    backing: DmaPage,
    /// Cached layout for the device-register PAs.
    layout: VirtqueueLayout,
    /// Head of the free descriptor chain.
    free_head: u16,
    /// Number of free descriptors remaining.
    num_free: u16,
    /// Last seen used ring index.
    last_used_idx: u16,
}

impl Virtqueue {
    /// Initialize a virtqueue inside `backing`. Panics if `backing`
    /// is too small for the layout of `queue_size`.
    pub fn new(backing: DmaPage, queue_size: u16) -> Self {
        let layout = virtqueue_layout(queue_size);
        assert!(
            layout.total_size <= backing.size_bytes(),
            "DmaPage size {} bytes too small for virtqueue_layout({}).total_size = {}",
            backing.size_bytes(), queue_size, layout.total_size
        );

        let mut vq = Virtqueue {
            backing,
            layout,
            free_head: 0,
            num_free: queue_size,
            last_used_idx: 0,
        };
        vq.init_free_chain();
        vq
    }

    // -----------------------------------------------------------------
    // Typed view accessors. Each computes a fresh lifetime-bound view
    // over the backing page; inlining means the resulting machine code
    // is the same pointer arithmetic that the original raw pointers did.
    // -----------------------------------------------------------------

    #[inline(always)]
    fn desc(&self) -> SliceRef<'_, VirtqDesc> {
        self.backing.slice(
            self.layout.desc_offset as u64,
            self.layout.queue_size as usize,
        )
    }

    #[inline(always)]
    fn avail_header(&self) -> CellRef<'_, VirtqAvail> {
        self.backing.cell(self.layout.avail_offset as u64)
    }

    #[inline(always)]
    fn avail_ring(&self) -> SliceRef<'_, u16> {
        self.backing.slice(
            self.layout.avail_offset as u64 + core::mem::size_of::<VirtqAvail>() as u64,
            self.layout.queue_size as usize,
        )
    }

    #[inline(always)]
    fn used_header(&self) -> CellRef<'_, VirtqUsed> {
        self.backing.cell(self.layout.used_offset as u64)
    }

    #[inline(always)]
    fn used_ring(&self) -> SliceRef<'_, VirtqUsedElem> {
        self.backing.slice(
            self.layout.used_offset as u64 + core::mem::size_of::<VirtqUsed>() as u64,
            self.layout.queue_size as usize,
        )
    }

    fn init_free_chain(&mut self) {
        let n = self.layout.queue_size;
        let desc = self.desc();
        for i in 0..n {
            desc.write(i as usize, VirtqDesc {
                addr: 0,
                len: 0,
                flags: if i + 1 < n { VIRTQ_DESC_F_NEXT } else { 0 },
                next: if i + 1 < n { i + 1 } else { 0 },
            });
        }
    }

    /// Physical address of the descriptor table (for QUEUE_DESC_LOW/HIGH).
    pub fn desc_phys(&self) -> u64 {
        self.backing.pa_offset(self.layout.desc_offset as u64)
    }

    /// Physical address of the available ring (for QUEUE_DRIVER_LOW/HIGH).
    pub fn avail_phys(&self) -> u64 {
        self.backing.pa_offset(self.layout.avail_offset as u64)
    }

    /// Physical address of the used ring (for QUEUE_DEVICE_LOW/HIGH).
    pub fn used_phys(&self) -> u64 {
        self.backing.pa_offset(self.layout.used_offset as u64)
    }

    /// Allocate a 3-descriptor chain for a block I/O request.
    ///
    /// Returns the head descriptor index, or None if not enough free
    /// descriptors.
    ///
    /// Each (pa, len, flags) tuple describes one buffer. Flags should
    /// include VIRTQ_DESC_F_WRITE for device-writable buffers.
    /// The chain is linked via VIRTQ_DESC_F_NEXT automatically.
    pub fn alloc_chain3(
        &mut self,
        buf0_pa: u64, len0: u32, flags0: u16,
        buf1_pa: u64, len1: u32, flags1: u16,
        buf2_pa: u64, len2: u32, flags2: u16,
    ) -> Option<u16> {
        if self.num_free < 3 {
            return None;
        }

        let head = self.free_head;
        let descs = [
            (buf0_pa, len0, flags0),
            (buf1_pa, len1, flags1),
            (buf2_pa, len2, flags2),
        ];
        // Scope the desc borrow so we can write back free_head after.
        let new_free_head = {
            let desc = self.desc();
            let mut idx = head;
            let mut last_next = 0u16;
            for (i, &(pa, len, user_flags)) in descs.iter().enumerate() {
                let next = desc.read(idx as usize).next;

                let is_last = i == 2;
                let flags = user_flags | if is_last { 0 } else { VIRTQ_DESC_F_NEXT };

                desc.write(idx as usize, VirtqDesc {
                    addr: pa,
                    len,
                    flags,
                    next: if is_last { 0 } else { next },
                });

                if is_last {
                    last_next = next;
                } else {
                    idx = next;
                }
            }
            last_next
        };

        self.free_head = new_free_head;
        self.num_free -= 3;
        Some(head)
    }

    /// Push a descriptor chain head into the available ring.
    ///
    /// Inserts a store-store barrier before updating the avail index
    /// so the device sees completed descriptor writes before the
    /// index bump.
    pub fn submit(&mut self, head: u16) {
        let header = self.avail_header();
        let avail = header.read();
        let ring_idx = (avail.idx % self.layout.queue_size) as usize;
        self.avail_ring().write(ring_idx, head);

        // Barrier: descriptor + ring entry writes visible before idx bump.
        dmb_ishst();

        header.write(VirtqAvail {
            flags: avail.flags,
            idx: avail.idx.wrapping_add(1),
        });

        // Barrier: avail.idx visible before MMIO notify.
        dmb_ish();
    }

    /// Poll the used ring for completed requests.
    ///
    /// Returns `Some((head_idx, bytes_written))` if a new completion
    /// is available, `None` otherwise.
    pub fn poll_used(&mut self) -> Option<(u16, u32)> {
        let used_idx = self.used_header().read().idx;
        if used_idx == self.last_used_idx {
            return None;
        }

        // Barrier: used.idx read before ring element reads.
        dmb_ishld();

        let ring_idx = (self.last_used_idx % self.layout.queue_size) as usize;
        let elem = self.used_ring().read(ring_idx);

        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((elem.id as u16, elem.len))
    }

    /// Free a 3-descriptor chain back to the free list.
    ///
    /// Walks the chain via `next` pointers and returns all descriptors
    /// to the head of the free list.
    pub fn free_chain(&mut self, head: u16) {
        // Snapshot pre-loop self state because the desc borrow below
        // pins `&self` for its scope. We update self.free_head /
        // self.num_free once the desc borrow drops.
        let prev_free_head = self.free_head;
        let prev_num_free = self.num_free;
        let count = {
            let desc = self.desc();
            let mut idx = head;
            let mut count = 0u16;
            loop {
                let entry = desc.read(idx as usize);
                count += 1;

                if entry.flags & VIRTQ_DESC_F_NEXT == 0 {
                    // Last descriptor — point its next to prev free_head.
                    desc.write(idx as usize, VirtqDesc {
                        addr: 0,
                        len: 0,
                        flags: if prev_num_free > 0 { VIRTQ_DESC_F_NEXT } else { 0 },
                        next: prev_free_head,
                    });
                    break;
                }

                idx = entry.next;
            }
            count
        };

        self.free_head = head;
        self.num_free += count;
    }
}
