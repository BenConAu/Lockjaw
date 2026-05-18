//! Typed region wrapper for an MMIO mapping.
//!
//! `MappedRegs<T>` carries a raw pointer to a memory region the caller
//! has mapped as MMIO. It exposes safe `regs()` returning `&T` (where
//! `T` is the device's `#[repr(C)]` layout struct from `lockjaw-regs`).
//! The only `unsafe` is the constructor; drivers never call it directly
//! — the `lockjaw_userlib::devmgr::claim_typed::<T>(...)` helper is the
//! single sanctioned entry point.
//!
//! Lifetime: `regs()` returns `&T` tied to `&self`, NOT `&'static T`.
//! That keeps the type system honest about the borrow chain. (An
//! earlier draft of this substrate forged a `'static` lifetime from
//! the raw VA; Codex correctly flagged that as unsound — multiple
//! `MappedRegs<T>` for the same region would each hand out `&'static T`,
//! and there's no aliasing protection. Tying to `&self` makes the
//! borrow at least scoped to the `MappedRegs` instance, and the
//! constructor precondition disallows aliased instances for the same
//! region.)
//!
//! `MappedRegs<T>` is `!Send + !Sync` by default because it contains a
//! raw `*const T`. That is the right default: cross-thread sharing is
//! out of scope (single-threaded driver model). If a future driver
//! needs multi-threaded access, the substrate must be revisited — a
//! `Mutex<MappedRegs<T>>` doesn't work today because `Mutex<X>` only
//! becomes `Send/Sync` when `X` is `Send`, and `MappedRegs` is not.

use core::marker::PhantomData;

/// Typed region wrapper.
pub struct MappedRegs<T: 'static> {
    ptr: *const T,
    _phantom: PhantomData<T>,
}

impl<T: 'static> MappedRegs<T> {
    /// Construct from a virtual address.
    ///
    /// # Safety
    ///
    /// Caller asserts:
    /// - `va` points to a region of at least `size_of::<T>()` bytes
    ///   mapped as MMIO (Device-nGnRnE or `NormalNonCacheable` for
    ///   DMA-shared regions).
    /// - `va` is aligned to `align_of::<T>()`.
    /// - No other `MappedRegs<T>` instance aliases the same region.
    ///   The intended user is `lockjaw_userlib::devmgr::claim_typed`,
    ///   which serializes device claims through device-manager so
    ///   aliasing can't happen in practice.
    /// - The mapping outlives every `&T` derived from this instance.
    ///   In Lockjaw's process model MMIO mappings live for the entire
    ///   process lifetime, so this is satisfied by construction.
    #[inline]
    pub const unsafe fn new(va: u64) -> Self {
        Self {
            ptr: va as *const T,
            _phantom: PhantomData,
        }
    }

    /// Safe typed access to the mapped region. The returned reference
    /// is tied to `&self`, not `'static`.
    #[inline]
    pub fn regs(&self) -> &T {
        // SAFETY: `self.ptr` was constructed by `new` whose
        // preconditions assert validity for the lifetime of `self`.
        // Cells inside T expose only `&self` methods (no `&mut`),
        // so this shared reference cannot be upgraded to mutation
        // without going through the cells' UnsafeCell-based volatile
        // APIs.
        unsafe { &*self.ptr }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Rw;
    use core::cell::UnsafeCell;

    /// MappedRegs<T> must be !Send and !Sync by default (raw pointer
    /// in a field). This is the correct default; the substrate makes
    /// no cross-thread claim.
    #[test]
    fn mapped_regs_is_not_send_or_sync() {
        use static_assertions::assert_not_impl_any;
        // The wrapped T is irrelevant for the check; raw pointer
        // makes the whole struct !Send + !Sync.
        assert_not_impl_any!(MappedRegs<u32>: Send, Sync);
    }

    /// Round-trip on a backing UnsafeCell<Layout> proves the wrapper
    /// hands out a valid reference and the cell's volatile API works
    /// through it. This is the closest we can get on host without a
    /// real MMIO region.
    #[test]
    fn regs_roundtrip_on_backing_memory() {
        #[repr(C)]
        struct Layout {
            a: Rw<u32>,
            b: Rw<u32>,
        }

        let backing: UnsafeCell<Layout> = UnsafeCell::new(Layout {
            a: unsafe { core::mem::zeroed() },
            b: unsafe { core::mem::zeroed() },
        });
        // SAFETY: backing lives for the duration of this test; we
        // hold its address by raw pointer in MappedRegs and don't
        // outlive the local.
        let regs = unsafe { MappedRegs::<Layout>::new(backing.get() as u64) };
        regs.regs().a.write(0xaaaa_aaaa);
        regs.regs().b.write(0xbbbb_bbbb);
        assert_eq!(regs.regs().a.read(), 0xaaaa_aaaa);
        assert_eq!(regs.regs().b.read(), 0xbbbb_bbbb);
    }
}
