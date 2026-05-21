//! `DmaValue` — the marker trait for types safe to read from
//! DMA-shared memory via `ptr::read_volatile`.
//!
//! # Why
//!
//! `lockjaw-mmio::dma::DmaCell<T>` lets driver code read typed
//! values back from memory the device may have written. That
//! `ptr::read_volatile::<T>(...)` is sound only if every bit pattern
//! of `size_of::<T>()` bytes is a valid `T`. Rust's stdlib does not
//! have a marker for this; we define our own.
//!
//! # Construction-safe sealing
//!
//! `DmaValue` requires the crate-private `sealed::Sealed`
//! supertrait, so it cannot be implemented outside `lockjaw-types`.
//! The canonical way to add a new `DmaValue` is via the
//! `dma_value_impl!` macro, which:
//!
//! 1. Emits a compile-time `const _: () = assert!(size_of::<T>() ==
//!    expected_size)` check — if `T` has any padding (the most
//!    common DmaValue-safety bug) the build fails at the impl site
//!    rather than at runtime through a DmaCell read/write of undef
//!    bytes.
//! 2. Emits the `impl sealed::Sealed for T` half (the macro is the
//!    only place this happens; the module is private).
//! 3. Emits the `unsafe impl DmaValue for T` half.
//!
//! Driver crates cannot construct their own `DmaValue` impls
//! because:
//! - They cannot impl `Sealed` (private module).
//! - The macro emits `impl $crate::dma::sealed::Sealed for $t`,
//!   which fails to compile from outside `lockjaw-types` because
//!   the `sealed` module is private.
//!
//! New DmaValue corpus members go in `lockjaw-types` next to the
//! struct they describe (`virtio::*`, `fwcfg::*`, etc.) and use
//! the macro. There is no escape valve from this discipline.

/// Marker for types where every bit pattern of `size_of::<Self>()`
/// bytes is a valid `Self` AND the type has no padding.
///
/// # Safety
///
/// Implementor asserts:
/// - All 2^(8*size_of::<Self>()) bit patterns represent valid `Self`
///   values.
/// - No padding bytes that could be undef.
/// - No validity invariants the compiler relies on (e.g. enums with
///   restricted discriminants, `bool`, `&T`, `NonNull<T>`, etc. do
///   NOT qualify).
///
/// Implementation discipline: use the `dma_value_impl!` macro, which
/// emits a compile-time padding check via `const_assert!` on
/// `size_of::<T>() == declared_wire_size`. Manual `unsafe impl` is
/// blocked by the `Sealed` supertrait (private to this crate).
pub unsafe trait DmaValue: Copy + 'static + sealed::Sealed {}

pub(crate) mod sealed {
    /// Crate-private sealing trait. Driver crates cannot name this
    /// trait (the module is `pub(crate)`), so they cannot impl
    /// `DmaValue` for their own types.
    pub trait Sealed {}
}

/// Implement `DmaValue` for a type with a compile-time no-padding
/// check.
///
/// Usage: `dma_value_impl!(MyStruct, size = 16);`
///
/// The `size = N` literal is the type's expected on-wire size in
/// bytes. The macro emits `const _: () = assert!(size_of::<T>() ==
/// N)` — if the type ever gains padding (e.g. someone drops a
/// `#[repr(C, packed)]` attribute or adds a misaligned field), the
/// build fails AT THE IMPL SITE with a clear error message,
/// before any driver write can leak undef bytes through a
/// DmaCell.
///
/// The macro is `#[macro_export]` for completeness, but external
/// callers cannot use it: the emitted `impl sealed::Sealed for $t`
/// fails to compile from outside `lockjaw-types` because the
/// `sealed` module is `pub(crate)`.
#[macro_export]
macro_rules! dma_value_impl {
    ($t:ty, size = $size:literal $(,)?) => {
        // SAFETY of the const_assert: `size_of::<T>()` is `const`;
        // panicking in const context fails the build. If $t has
        // padding (size > sum of field sizes), the assert fires.
        const _: () = assert!(
            core::mem::size_of::<$t>() == $size,
            concat!(
                stringify!($t),
                " has size != ", stringify!($size),
                " bytes — padding violates the DmaValue safety contract.\n",
                "Either (a) the type gained padding (check #[repr(C, packed)]),\n",
                "or (b) the `size = N` argument to dma_value_impl! is wrong.",
            ),
        );
        impl $crate::dma::sealed::Sealed for $t {}
        unsafe impl $crate::dma::DmaValue for $t {}
    };
}

// ---------------------------------------------------------------------------
// Primitive integer impls. Every bit pattern is valid; sizes are
// guaranteed by Rust's spec.
// ---------------------------------------------------------------------------

dma_value_impl!(u8,  size = 1);
dma_value_impl!(u16, size = 2);
dma_value_impl!(u32, size = 4);
dma_value_impl!(u64, size = 8);
dma_value_impl!(i8,  size = 1);
dma_value_impl!(i16, size = 2);
dma_value_impl!(i32, size = 4);
dma_value_impl!(i64, size = 8);

// ---------------------------------------------------------------------------
// Virtio DTO impls. Sizes come from `#[repr(C)]` layout of integer
// fields with natural alignment; verified at compile time by the
// macro's const_assert.
// ---------------------------------------------------------------------------

dma_value_impl!(crate::virtio::VirtqDesc,         size = 16);
dma_value_impl!(crate::virtio::VirtqAvail,        size = 4);
dma_value_impl!(crate::virtio::VirtqUsedElem,     size = 8);
dma_value_impl!(crate::virtio::VirtqUsed,         size = 4);
dma_value_impl!(crate::virtio::VirtioBlkReqHeader, size = 16);
