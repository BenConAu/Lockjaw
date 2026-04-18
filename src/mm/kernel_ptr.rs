//! Typed pointers to kernel objects living in donated physical pages.
//!
//! The kernel's dominant operation on a paddr is:
//!
//! ```text
//! paddr -> paddr + KERNEL_VA_OFFSET -> *mut SomeType -> (*ptr).field
//! ```
//!
//! Before this module, that cast-and-deref was written out longhand at
//! every access — two `unsafe` blocks per site, dozens of sites, each
//! with its own `// SAFETY:` comment. [`KernelRef`] and [`KernelMut`]
//! own that cast exactly once (at [`from_paddr`]), after which the rest
//! of the kernel can call [`get`](KernelRef::get) / [`get_mut`](KernelMut::get_mut)
//! with no `unsafe` at the callsite.
//!
//! ### Aliasing and lifetimes
//!
//! The `'a` lifetime parameter is advisory. Rust's borrow checker does
//! not extend its aliasing rules across raw pointers, so it is possible
//! (and the caller's responsibility to avoid) constructing two
//! `KernelMut<T>` to the same physical page at the same time. The real
//! guarantee that keeps us sound today is single-core execution with
//! IRQs masked during kernel entry — that invariant already prevents
//! concurrent access to any kernel object. The lifetime still catches
//! the common mistake of keeping a [`KernelRef`] alive past the block
//! that invalidates the underlying paddr.
//!
//! ### Zero-cost
//!
//! `KernelRef<T>` is one pointer-sized word plus PhantomData; it
//! compiles to the same code as writing `&*(paddr_plus_offset as *mut
//! T)` by hand.

#![allow(dead_code)]

use crate::mm::addr::{KERNEL_VA_OFFSET, PhysAddr};
use core::marker::PhantomData;

/// Shared reference to a `T` that lives in a donated physical page,
/// accessed through the kernel's higher-half mapping. Analogous to
/// `&'a T` but constructed from a [`PhysAddr`].
#[derive(Clone, Copy)]
pub struct KernelRef<'a, T> {
    ptr: *const T,
    _marker: PhantomData<&'a T>,
}

/// Exclusive reference to a `T` that lives in a donated physical page.
/// Analogous to `&'a mut T`.
pub struct KernelMut<'a, T> {
    ptr: *mut T,
    _marker: PhantomData<&'a mut T>,
}

impl<'a, T> KernelRef<'a, T> {
    /// Construct a `KernelRef<T>` from a physical address.
    ///
    /// # Safety
    /// - `paddr` must point to a live, properly-initialized `T` in a
    ///   donated kernel-owned page that outlives `'a`.
    /// - While this `KernelRef` exists, no [`KernelMut<T>`] to the same
    ///   page may be constructed (same aliasing rule as Rust references,
    ///   not checked at compile time for raw-pointer wrappers).
    #[inline]
    pub unsafe fn from_paddr(paddr: PhysAddr) -> Self {
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let ptr = (paddr.as_u64() + KERNEL_VA_OFFSET) as *const T;
        Self { ptr, _marker: PhantomData }
    }

    /// Safe accessor: returns a `&T` bound to the wrapper's lifetime.
    #[inline]
    pub fn get(&self) -> &T {
        // SAFETY: constructor established validity of ptr and kept the
        // lifetime; no mutable aliases may exist per the from_paddr contract.
        unsafe { &*self.ptr }
    }

    /// Raw pointer, for the few sites that still need one (e.g. FFI,
    /// `ptr::read_volatile` on stack canaries).
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        self.ptr
    }
}

impl<'a, T> KernelMut<'a, T> {
    /// Construct a `KernelMut<T>` from a physical address.
    ///
    /// # Safety
    /// - `paddr` must point to a live, properly-initialized `T` (or, for
    ///   the initializing factory path, at least page-aligned storage of
    ///   `size_of::<T>()` bytes that is about to be written).
    /// - While this `KernelMut` exists, no other `KernelMut<T>` or
    ///   `KernelRef<T>` to the same page may coexist.
    #[inline]
    pub unsafe fn from_paddr(paddr: PhysAddr) -> Self {
        // SAFETY: kernel VA (via KERNEL_VA_OFFSET)
        let ptr = (paddr.as_u64() + KERNEL_VA_OFFSET) as *mut T;
        Self { ptr, _marker: PhantomData }
    }

    /// Safe shared accessor.
    #[inline]
    pub fn get(&self) -> &T {
        // SAFETY: constructor contract.
        unsafe { &*self.ptr }
    }

    /// Safe exclusive accessor.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: constructor contract — no other references exist.
        unsafe { &mut *self.ptr }
    }

    /// Reborrow this `KernelMut` as a shorter-lived `KernelRef`.
    /// Mirrors `&*foo` on a `&mut T`.
    #[inline]
    pub fn as_ref(&self) -> KernelRef<'_, T> {
        // SAFETY: mut-to-const downgrade of the wrapper's own pointer
        KernelRef { ptr: self.ptr as *const T, _marker: PhantomData }
    }

    /// Raw `*mut T` without creating `&mut T`.
    ///
    /// Use this for blocking IPC paths where a `&mut T` reference must
    /// not survive across `block_current()`. Raw pointers do not create
    /// Stacked Borrows tags, so they are the correct representation for
    /// kernel objects that may be accessed by other threads while this
    /// thread is descheduled. Callers should derive short-lived scoped
    /// `&mut T` references from this pointer only when needed, and drop
    /// them before any blocking operation.
    #[inline]
    pub fn raw_ptr(&self) -> *mut T {
        self.ptr
    }

    /// Raw `*const` for sites that need a pointer without `&T` semantics
    /// (e.g. volatile reads in crash diagnostics, FFI to context_switch).
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        // SAFETY: mut-to-const downgrade of the wrapper's own pointer
        self.ptr as *const T
    }

    /// Raw `*mut` for the page-initializing factories (`ptr::write` into
    /// fresh storage) and other low-level sites. Prefer `get_mut()` when
    /// the `T` is already live.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr
    }
}

// `KernelRef<T>` is one pointer word plus zero-sized PhantomData —
// confirm zero-cost with a compile-time assertion.
const _: () = {
    assert!(core::mem::size_of::<KernelRef<'static, u8>>() == core::mem::size_of::<*const u8>());
    assert!(core::mem::size_of::<KernelMut<'static, u8>>() == core::mem::size_of::<*mut u8>());
};
