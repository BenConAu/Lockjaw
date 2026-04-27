/// Split virtqueue runtime for VirtIO MMIO transport.
///
/// Operates on a contiguous physical allocation mapped to a VA.
/// All shared-memory accesses are volatile (device reads/writes via DMA).
/// AArch64 barriers enforce ordering between descriptor writes, ring
/// updates, MMIO notify, and completion polling.
///
/// Reusable across any VirtIO device (block, net, gpu).

use core::arch::asm;
use core::ptr;
use lockjaw_types::virtio::{
    VirtqDesc, VirtqueueLayout, virtqueue_layout,
    VIRTQ_DESC_F_NEXT,
};

// ---------------------------------------------------------------------------
// AArch64 memory barriers
// ---------------------------------------------------------------------------

/// Store-store barrier: descriptor writes visible before avail ring update.
#[inline(always)]
fn dmb_ishst() {
    unsafe { asm!("dmb ishst", options(nostack, preserves_flags)); }
}

/// Full barrier: avail ring update visible before MMIO notify.
#[inline(always)]
fn dmb_ish() {
    unsafe { asm!("dmb ish", options(nostack, preserves_flags)); }
}

/// Load-load barrier: used.idx read before used.ring[] reads.
#[inline(always)]
fn dmb_ishld() {
    unsafe { asm!("dmb ishld", options(nostack, preserves_flags)); }
}

// ---------------------------------------------------------------------------
// MMIO register helpers (volatile, for transport registers)
// ---------------------------------------------------------------------------

/// Read a 32-bit VirtIO MMIO register.
#[inline(always)]
pub unsafe fn mmio_read32(base: u64, offset: u64) -> u32 {
    ptr::read_volatile((base + offset) as *const u32)
}

/// Write a 32-bit VirtIO MMIO register.
#[inline(always)]
pub unsafe fn mmio_write32(base: u64, offset: u64, val: u32) {
    ptr::write_volatile((base + offset) as *mut u32, val);
}

// ---------------------------------------------------------------------------
// Virtqueue
// ---------------------------------------------------------------------------

/// A split virtqueue backed by a contiguous physical allocation.
///
/// The caller allocates contiguous pages, maps them, and provides
/// both the VA (for CPU access) and PA (for programming device
/// registers). The Virtqueue manages descriptors, available ring,
/// and used ring within that region.
pub struct Virtqueue {
    /// Virtual address of the allocation base.
    base_va: u64,
    /// Physical address of the allocation base (for device registers).
    base_phys: u64,
    /// Layout offsets computed from queue_size.
    layout: VirtqueueLayout,
    /// Head of the free descriptor chain.
    free_head: u16,
    /// Number of free descriptors remaining.
    num_free: u16,
    /// Last seen used ring index.
    last_used_idx: u16,
}

impl Virtqueue {
    /// Initialize a virtqueue from a contiguous allocation.
    ///
    /// `va`: virtual address of the mapped region (must be zeroed).
    /// `phys`: physical address (for programming QUEUE_DESC/DRIVER/DEVICE).
    /// `queue_size`: number of descriptors (must match device's QUEUE_NUM_MAX
    ///   or be <= it).
    pub fn new(va: u64, phys: u64, queue_size: u16) -> Self {
        let layout = virtqueue_layout(queue_size);

        // Build the free descriptor chain: each descriptor's `next`
        // points to the following one, forming a linked list.
        for i in 0..queue_size {
            let desc_ptr = (va + layout.desc_offset as u64
                + i as u64 * core::mem::size_of::<VirtqDesc>() as u64)
                as *mut VirtqDesc;
            unsafe {
                ptr::write_volatile(desc_ptr, VirtqDesc {
                    addr: 0,
                    len: 0,
                    flags: if i + 1 < queue_size { VIRTQ_DESC_F_NEXT } else { 0 },
                    next: if i + 1 < queue_size { i + 1 } else { 0 },
                });
            }
        }

        Virtqueue {
            base_va: va,
            base_phys: phys,
            layout,
            free_head: 0,
            num_free: queue_size,
            last_used_idx: 0,
        }
    }

    /// Physical address of the descriptor table (for QUEUE_DESC_LOW/HIGH).
    pub fn desc_phys(&self) -> u64 {
        self.base_phys + self.layout.desc_offset as u64
    }

    /// Physical address of the available ring (for QUEUE_DRIVER_LOW/HIGH).
    pub fn avail_phys(&self) -> u64 {
        self.base_phys + self.layout.avail_offset as u64
    }

    /// Physical address of the used ring (for QUEUE_DEVICE_LOW/HIGH).
    pub fn used_phys(&self) -> u64 {
        self.base_phys + self.layout.used_offset as u64
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

        // Pop 3 descriptors from the free list.
        let head = self.free_head;
        let mut idx = head;
        let descs = [
            (buf0_pa, len0, flags0),
            (buf1_pa, len1, flags1),
            (buf2_pa, len2, flags2),
        ];

        for (i, &(pa, len, user_flags)) in descs.iter().enumerate() {
            let desc_ptr = self.desc_ptr(idx);
            let next = unsafe { ptr::read_volatile(desc_ptr) }.next;

            let is_last = i == 2;
            let flags = user_flags | if is_last { 0 } else { VIRTQ_DESC_F_NEXT };

            unsafe {
                ptr::write_volatile(desc_ptr, VirtqDesc {
                    addr: pa,
                    len,
                    flags,
                    next: if is_last { 0 } else { next },
                });
            }

            if !is_last {
                idx = next;
            } else {
                // Advance free_head past the chain.
                self.free_head = next;
            }
        }

        self.num_free -= 3;
        Some(head)
    }

    /// Push a descriptor chain head into the available ring.
    ///
    /// Inserts a store-store barrier before updating the avail index
    /// so the device sees completed descriptor writes before the
    /// index bump.
    pub fn submit(&mut self, head: u16) {
        let avail_va = self.base_va + self.layout.avail_offset as u64;

        // Read current avail.idx.
        let avail_idx = unsafe {
            ptr::read_volatile((avail_va + 2) as *const u16) // offset 2 = idx
        };

        // Write the head index into avail.ring[avail_idx % queue_size].
        let ring_offset = 4 + (avail_idx % self.layout.queue_size) as u64 * 2;
        unsafe {
            ptr::write_volatile((avail_va + ring_offset) as *mut u16, head);
        }

        // Barrier: descriptor + ring entry writes visible before idx bump.
        dmb_ishst();

        // Bump avail.idx.
        unsafe {
            ptr::write_volatile((avail_va + 2) as *mut u16, avail_idx.wrapping_add(1));
        }

        // Barrier: avail.idx visible before MMIO notify.
        dmb_ish();
    }

    /// Poll the used ring for completed requests.
    ///
    /// Returns `Some((head_idx, bytes_written))` if a new completion
    /// is available, `None` otherwise.
    pub fn poll_used(&mut self) -> Option<(u16, u32)> {
        let used_va = self.base_va + self.layout.used_offset as u64;

        // Read used.idx.
        let used_idx = unsafe {
            ptr::read_volatile((used_va + 2) as *const u16) // offset 2 = idx
        };

        if used_idx == self.last_used_idx {
            return None;
        }

        // Barrier: used.idx read before ring element reads.
        dmb_ishld();

        // Read used.ring[last_used_idx % queue_size].
        // Each element is 8 bytes: id(u32) + len(u32).
        let elem_offset = 4 + (self.last_used_idx % self.layout.queue_size) as u64 * 8;
        let id = unsafe {
            ptr::read_volatile((used_va + elem_offset) as *const u32)
        };
        let len = unsafe {
            ptr::read_volatile((used_va + elem_offset + 4) as *const u32)
        };

        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((id as u16, len))
    }

    /// Free a 3-descriptor chain back to the free list.
    ///
    /// Walks the chain via `next` pointers and returns all descriptors
    /// to the head of the free list.
    pub fn free_chain(&mut self, head: u16) {
        let mut idx = head;
        let mut count = 0u16;

        loop {
            let desc_ptr = self.desc_ptr(idx);
            let desc = unsafe { ptr::read_volatile(desc_ptr) };
            count += 1;

            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                // Last descriptor — point its next to current free_head.
                unsafe {
                    ptr::write_volatile(desc_ptr, VirtqDesc {
                        addr: 0,
                        len: 0,
                        flags: if self.num_free > 0 { VIRTQ_DESC_F_NEXT } else { 0 },
                        next: self.free_head,
                    });
                }
                break;
            }

            idx = desc.next;
        }

        self.free_head = head;
        self.num_free += count;
    }

    // --- private helpers ---

    fn desc_ptr(&self, idx: u16) -> *mut VirtqDesc {
        (self.base_va + self.layout.desc_offset as u64
            + idx as u64 * core::mem::size_of::<VirtqDesc>() as u64)
            as *mut VirtqDesc
    }
}
