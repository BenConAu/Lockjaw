//! Split virtqueue runtime for VirtIO MMIO transport.
//!
//! Operates on a contiguous physical allocation supplied via an
//! `OwnedDmaMapping`. All shared-memory accesses go through
//! `lockjaw-mmio`'s `DmaCell` / `DmaSliceDyn` (wrapped in userlib's
//! lifetime-bound `CellRef` / `SliceRef`) so the virtqueue protocol
//! logic is `unsafe`-free in this crate and the driver crate above.
//! AArch64 barriers come from `lockjaw_mmio::barrier::*` and order
//! descriptor writes against avail/used ring updates and MMIO notify.
//!
//! Reusable across any VirtIO device (block, net, gpu).
//!
//! Structurally, the Virtqueue stores ONLY the backing
//! `OwnedDmaMapping` + layout + free-list state. Typed views are
//! computed on each access via tiny inline accessors (`desc()`,
//! `avail_header()`, etc.) so each view borrows `&self` and cannot
//! outlive the backing mapping. Storing owned views inside the
//! Virtqueue would require self-referential types (the views borrow
//! the same struct that owns them) and would also let view lifetimes
//! escape the mapping's. Computing per-access is the same machine
//! code after inlining and avoids both problems.

use crate::dma::{CellRef, DmaMappingView, OwnedDmaMapping, SliceRef};
use crate::handle::NotificationHandle;
use crate::syscall::sys_wait_notification;
use crate::virtio::VirtioTransport;
use lockjaw_mmio::barrier::{dmb_ish, dmb_ishld, dmb_ishst};
use lockjaw_types::virtio::{
    VirtqAvail, VirtqDesc, VirtqUsed, VirtqUsedElem, VirtqueueLayout,
    virtqueue_layout, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};

/// One segment of a virtqueue chain — a single buffer the device
/// either reads from or writes to.
#[derive(Clone, Copy)]
pub struct Segment {
    /// Physical address the device should access.
    pub pa: u64,
    /// Length in bytes.
    pub len: u32,
    /// Whether the device reads (driver wrote first) or writes
    /// (device fills, driver reads).
    pub direction: Direction,
}

/// Buffer direction from the device's point of view.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Device READS from this buffer (driver pre-filled).
    DeviceReadable,
    /// Device WRITES to this buffer (driver later reads).
    DeviceWritable,
}

impl Segment {
    /// Shorthand for a device-readable segment (driver-side payload).
    pub const fn readable(pa: u64, len: u32) -> Self {
        Self { pa, len, direction: Direction::DeviceReadable }
    }
    /// Shorthand for a device-writable segment (response buffer).
    pub const fn writable(pa: u64, len: u32) -> Self {
        Self { pa, len, direction: Direction::DeviceWritable }
    }
}

/// Errors `wait_for_completion` can return.
#[derive(Debug, Clone, Copy)]
pub enum VirtqueueError {
    /// IRQ-notification wait failed at the kernel boundary.
    IrqWaitFailed,
}

/// A split virtqueue backed by a contiguous DMA region.
///
/// The caller allocates an `OwnedDmaMapping` of the right size (see
/// `virtqueue_layout(queue_size).total_size`) and hands it to
/// `Virtqueue::new`. The virtqueue takes ownership of the backing
/// region so the mapping outlives the typed views computed on each
/// access; when the virtqueue is dropped, the mapping's Drop releases
/// the underlying pageset.
pub struct Virtqueue {
    /// Owns the backing DMA region; keeps the mapping alive and
    /// closes the pageset on Drop.
    backing: OwnedDmaMapping,
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
    pub fn new(backing: OwnedDmaMapping, queue_size: u16) -> Self {
        let layout = virtqueue_layout(queue_size);
        assert!(
            layout.total_size <= backing.size_bytes(),
            "OwnedDmaMapping size {} bytes too small for virtqueue_layout({}).total_size = {}",
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

    /// Queue size negotiated at construction time.
    #[inline]
    pub fn queue_size(&self) -> u16 { self.layout.queue_size }

    // -----------------------------------------------------------------
    // Typed view accessors. Each computes a fresh lifetime-bound view
    // over the backing mapping; inlining means the resulting machine
    // code is the same pointer arithmetic that the original raw
    // pointers did. The views borrow `&self`, so they cannot outlive
    // the Virtqueue and the field updates below (`self.free_head =
    // ...`) can re-borrow `&mut self` once a view's scope ends.
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
            let flags = if i + 1 < n { VIRTQ_DESC_F_NEXT } else { 0 };
            let next = if i + 1 < n { i + 1 } else { 0 };
            desc.write(i as usize, VirtqDesc::new(0, 0, flags, next));
        }
    }

    /// Physical address of the descriptor table (for QUEUE_DESC).
    pub fn desc_phys(&self) -> u64 {
        self.backing.pa_offset(self.layout.desc_offset as u64)
    }

    /// Physical address of the available ring (for QUEUE_DRIVER).
    pub fn avail_phys(&self) -> u64 {
        self.backing.pa_offset(self.layout.avail_offset as u64)
    }

    /// Physical address of the used ring (for QUEUE_DEVICE).
    pub fn used_phys(&self) -> u64 {
        self.backing.pa_offset(self.layout.used_offset as u64)
    }

    /// Submit a chain of segments. Returns the head descriptor index
    /// on success, or `None` if not enough free descriptors.
    ///
    /// Descriptor flag computation (VIRTQ_DESC_F_NEXT,
    /// VIRTQ_DESC_F_WRITE) is internal — drivers express I/O as
    /// `Segment::readable(...)` / `Segment::writable(...)` and never
    /// see the flag constants. Scales to any chain length (block
    /// drivers use 3, future virtio drivers may differ).
    ///
    /// Inserts a store-store barrier before bumping `avail.idx` so
    /// the device observes descriptor + ring writes in the right
    /// order; final store-any barrier ensures `avail.idx` is visible
    /// before the driver's MMIO notify.
    pub fn submit_chain(&mut self, segments: &[Segment]) -> Option<u16> {
        let n = segments.len();
        if n == 0 || (n as u16) > self.num_free {
            return None;
        }

        let head = self.free_head;
        // Scope the desc borrow so we can write back free_head /
        // num_free after — the desc view holds &self for its scope,
        // and the field assignments below need &mut self.
        let new_free_head = {
            let desc = self.desc();
            let mut idx = head;
            let mut last_next = 0u16;
            for (i, seg) in segments.iter().enumerate() {
                let next = desc.read(idx as usize).next();
                let is_last = i + 1 == n;
                let mut flags = 0u16;
                if seg.direction == Direction::DeviceWritable {
                    flags |= VIRTQ_DESC_F_WRITE;
                }
                if !is_last {
                    flags |= VIRTQ_DESC_F_NEXT;
                }
                desc.write(
                    idx as usize,
                    VirtqDesc::new(seg.pa, seg.len, flags, if is_last { 0 } else { next }),
                );
                if is_last {
                    last_next = next;
                } else {
                    idx = next;
                }
            }
            last_next
        };

        self.free_head = new_free_head;
        self.num_free -= n as u16;

        // Push head into the available ring and advance avail.idx.
        let header = self.avail_header();
        let avail = header.read();
        let ring_idx = (avail.idx() % self.layout.queue_size) as usize;
        self.avail_ring().write(ring_idx, head);

        // Barrier: descriptor + ring entry writes visible before idx bump.
        dmb_ishst();

        header.write(VirtqAvail::new(avail.flags(), avail.idx().wrapping_add(1)));

        // Barrier: avail.idx visible before MMIO notify the caller issues.
        dmb_ish();

        Some(head)
    }

    /// Poll the used ring for completed requests.
    ///
    /// Returns `Some((head_idx, bytes_written))` if a new completion
    /// is available, `None` otherwise. Most callers should prefer
    /// `wait_for_completion`, which bakes the IRQ + ack + poll loop.
    pub fn poll_used(&mut self) -> Option<(u16, u32)> {
        let used_idx = self.used_header().read().idx();
        if used_idx == self.last_used_idx {
            return None;
        }
        dmb_ishld();
        let ring_idx = (self.last_used_idx % self.layout.queue_size) as usize;
        let elem = self.used_ring().read(ring_idx);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((elem.id() as u16, elem.len()))
    }

    /// Wait for the next completion, handling the IRQ + ack + poll
    /// loop internally.
    ///
    /// Order matters: poll the used ring FIRST in case completion
    /// already arrived (e.g. a previous IRQ delivered multiple
    /// completions), then wait for the IRQ notification, ACK the
    /// transport interrupt (W1C semantics), then re-poll. Every
    /// virtio driver needs this loop; baking it into the substrate
    /// stops drivers from accidentally reordering the ack and the
    /// poll (a known virtio race shape).
    ///
    /// `irq_threshold` is the caller's monotonic counter, incremented
    /// once per successful IRQ wait. Caller passes a `&mut u64` so
    /// the threshold survives across calls.
    pub fn wait_for_completion(
        &mut self,
        irq_notif: NotificationHandle,
        irq_threshold: &mut u64,
        transport: &VirtioTransport,
    ) -> Result<(u16, u32), VirtqueueError> {
        loop {
            if let Some(c) = self.poll_used() {
                return Ok(c);
            }
            sys_wait_notification(irq_notif, *irq_threshold)
                .map_err(|_| VirtqueueError::IrqWaitFailed)?;
            *irq_threshold += 1;
            let pending = transport.read_interrupt_status();
            transport.clear_interrupt_ack(pending);
        }
    }

    /// Free a descriptor chain back to the free list.
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
                if entry.flags() & VIRTQ_DESC_F_NEXT == 0 {
                    let flags = if prev_num_free > 0 { VIRTQ_DESC_F_NEXT } else { 0 };
                    desc.write(idx as usize, VirtqDesc::new(0, 0, flags, prev_free_head));
                    break;
                }
                idx = entry.next();
            }
            count
        };
        self.free_head = head;
        self.num_free += count;
    }
}
