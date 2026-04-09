use crate::cap::object::CreateError;
use crate::mm::addr::{PhysAddr, PhysFrame};
use crate::mm::frame;

/// Maximum pages in a single PageSet.
const MAX_PAGESET_PAGES: usize = 16;

/// A PageSet represents 1..N physical pages allocated from the kernel's
/// frame bitmap. It is the unit of memory ownership in Lockjaw.
///
/// A PageSet can be:
/// - **Donated** to create a kernel object (the pages become kernel-owned)
/// - **Mapped** into a process's virtual address space as a MappedPages (Phase 6)
/// Never both — this prevents userspace from accessing kernel object internals.
#[derive(Clone, Copy)]
pub struct PageSet {
    pub count: usize,
    pub pages: [PhysAddr; MAX_PAGESET_PAGES],
}

/// Errors from page allocation.
#[derive(Clone, Copy, Debug)]
pub enum AllocError {
    OutOfMemory,
    TooManyPages,
}

/// Allocate `count` physical pages from the frame bitmap and return a PageSet.
pub fn alloc_pages(count: usize) -> Result<PageSet, AllocError> {
    if count == 0 || count > MAX_PAGESET_PAGES {
        return Err(AllocError::TooManyPages);
    }

    let mut ps = PageSet {
        count,
        pages: [PhysAddr::new(0); MAX_PAGESET_PAGES],
    };

    for i in 0..count {
        match frame::alloc_frame() {
            Some(f) => ps.pages[i] = f.start_addr(),
            None => {
                // Roll back: free any pages we already allocated
                for j in 0..i {
                    frame::dealloc_frame(PhysFrame::containing(ps.pages[j]));
                }
                return Err(AllocError::OutOfMemory);
            }
        }
    }

    Ok(ps)
}

/// Donate a PageSet for kernel object creation. Returns the base physical
/// address (first page) where the caller should initialize the object.
///
/// After this call, the pages belong to the kernel — userspace cannot
/// map or reuse them until the object is destroyed.
pub fn donate(pageset: &PageSet, required_pages: usize) -> Result<PhysAddr, CreateError> {
    if pageset.count < required_pages {
        return Err(CreateError::InvalidParameter);
    }
    Ok(pageset.pages[0])
}
