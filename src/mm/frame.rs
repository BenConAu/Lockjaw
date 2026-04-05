use crate::mm::addr::{PhysAddr, PhysFrame, RAM_START, TOTAL_FRAMES, PAGE_SIZE};

/// Bitmap size in bytes: 32768 frames / 8 bits per byte = 4096 bytes.
const BITMAP_SIZE: usize = (TOTAL_FRAMES + 7) / 8;

/// Static bitmap — lives in BSS, zeroed at boot. A set bit means allocated/reserved.
///
/// Safety: single-threaded access only. Must be wrapped in a lock once
/// interrupts or scheduling are introduced.
static mut BITMAP: [u8; BITMAP_SIZE] = [0u8; BITMAP_SIZE];

/// Hint for next-fit allocation — index of the next frame to check.
static mut NEXT_FREE_HINT: usize = 0;

/// Number of frames currently marked as reserved or allocated.
static mut ALLOCATED_COUNT: usize = 0;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize the frame allocator. Marks firmware, kernel image, and stack
/// frames as reserved. Must be called exactly once during boot.
///
/// # Safety
/// `kernel_start` and `kernel_end` must be valid physical addresses bounding
/// the kernel image (including stack). Typically derived from linker symbols.
pub unsafe fn init(kernel_start: PhysAddr, kernel_end: PhysAddr) {
    // Reserve frames below the kernel load address (firmware, DTB, etc.)
    // 0x4000_0000 to kernel_start
    let firmware_end_frame = frame_index(kernel_start);
    mark_range_reserved(0, firmware_end_frame);

    // Reserve kernel image + stack frames
    let kernel_start_frame = frame_index(kernel_start);
    let kernel_end_frame = frame_index(kernel_end);
    // Round up in case kernel_end isn't page-aligned
    let kernel_end_frame = if kernel_end.as_u64() & (PAGE_SIZE - 1) != 0 {
        kernel_end_frame + 1
    } else {
        kernel_end_frame
    };
    mark_range_reserved(kernel_start_frame, kernel_end_frame);

    // Set hint past all reserved frames
    NEXT_FREE_HINT = kernel_end_frame;

    let reserved = ALLOCATED_COUNT;
    crate::kprintln!("  Frame allocator: {} reserved, {} free",
        reserved, TOTAL_FRAMES - reserved);
}

/// Allocate a single physical frame. Returns `None` if out of memory.
pub fn alloc_frame() -> Option<PhysFrame> {
    unsafe {
        let start = NEXT_FREE_HINT;

        // Scan from hint to end
        for i in start..TOTAL_FRAMES {
            if !is_set(i) {
                set_bit(i);
                ALLOCATED_COUNT += 1;
                NEXT_FREE_HINT = i + 1;
                return Some(index_to_frame(i));
            }
        }

        // Wrap around: scan from 0 to hint
        for i in 0..start {
            if !is_set(i) {
                set_bit(i);
                ALLOCATED_COUNT += 1;
                NEXT_FREE_HINT = i + 1;
                return Some(index_to_frame(i));
            }
        }

        None
    }
}

/// Free a previously allocated frame. Panics on double-free.
pub fn dealloc_frame(frame: PhysFrame) {
    unsafe {
        let idx = frame_index(frame.start_addr());
        assert!(is_set(idx), "dealloc_frame: double free of frame {:#x}", frame.start_addr().as_u64());
        clear_bit(idx);
        ALLOCATED_COUNT -= 1;

        // Update hint if this frame is lower
        if idx < NEXT_FREE_HINT {
            NEXT_FREE_HINT = idx;
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a physical address to a frame index relative to RAM_START.
fn frame_index(addr: PhysAddr) -> usize {
    ((addr.as_u64() - RAM_START.as_u64()) / PAGE_SIZE) as usize
}

/// Convert a frame index back to a PhysFrame.
fn index_to_frame(idx: usize) -> PhysFrame {
    PhysFrame::containing(PhysAddr::new(RAM_START.as_u64() + (idx as u64) * PAGE_SIZE))
}

fn set_bit(idx: usize) {
    unsafe { BITMAP[idx / 8] |= 1 << (idx % 8); }
}

fn clear_bit(idx: usize) {
    unsafe { BITMAP[idx / 8] &= !(1 << (idx % 8)); }
}

fn is_set(idx: usize) -> bool {
    unsafe { BITMAP[idx / 8] & (1 << (idx % 8)) != 0 }
}

fn mark_range_reserved(start: usize, end_exclusive: usize) {
    unsafe {
        for i in start..end_exclusive {
            if !is_set(i) {
                set_bit(i);
                ALLOCATED_COUNT += 1;
            }
        }
    }
}
