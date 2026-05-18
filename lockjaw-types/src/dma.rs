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
//! # Why a separate crate (lockjaw-types) hosts this trait
//!
//! `lockjaw-mmio` depends on `lockjaw-types`. The canonical DMA
//! struct definitions (`VirtqDesc`, `VirtqUsed`, `VirtioBlkReqHeader`,
//! etc.) already live in `lockjaw-types`; the `unsafe impl DmaValue`
//! for each lives next to its definition. If `lockjaw-mmio` defined
//! the trait and `lockjaw-types` tried to impl it, that would create
//! a dependency cycle.
//!
//! Drivers in `#![forbid(unsafe_code)]` cannot `unsafe impl DmaValue`,
//! so the audited corpus of DMA-safe types stays in `lockjaw-types`.
//! New DmaValue impls go through a `lockjaw-types` review.

/// Marker for types where every bit pattern of `size_of::<Self>()`
/// bytes is a valid `Self`.
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
/// The intended uses are integer primitives and `#[repr(C)]` POD
/// structs composed of `DmaValue` fields with no padding.
pub unsafe trait DmaValue: Copy + 'static {}

// SAFETY: every bit pattern of an unsigned integer is a valid value.
unsafe impl DmaValue for u8 {}
unsafe impl DmaValue for u16 {}
unsafe impl DmaValue for u32 {}
unsafe impl DmaValue for u64 {}

// SAFETY: same for signed integers — two's complement, every bit
// pattern is a valid value.
unsafe impl DmaValue for i8 {}
unsafe impl DmaValue for i16 {}
unsafe impl DmaValue for i32 {}
unsafe impl DmaValue for i64 {}

// SAFETY: VirtqDesc is #[repr(C)] of u64 + u32 + u16 + u16 = 16 bytes
// exactly, no padding. All fields are DmaValue primitives; every bit
// pattern is valid.
unsafe impl DmaValue for crate::virtio::VirtqDesc {}

// SAFETY: VirtqAvail header is #[repr(C)] of u16 + u16 = 4 bytes, no
// padding. (The ring entries that follow it in memory are not part
// of this struct.)
unsafe impl DmaValue for crate::virtio::VirtqAvail {}

// SAFETY: VirtqUsedElem is #[repr(C)] of u32 + u32 = 8 bytes, no
// padding. Device-written; the impl is essential for DmaCell reads.
unsafe impl DmaValue for crate::virtio::VirtqUsedElem {}

// SAFETY: VirtqUsed header is #[repr(C)] of u16 + u16 = 4 bytes, no
// padding. Device-written.
unsafe impl DmaValue for crate::virtio::VirtqUsed {}

// SAFETY: VirtioBlkReqHeader is #[repr(C)] of u32 + u32 + u64 = 16
// bytes, no padding.
unsafe impl DmaValue for crate::virtio::VirtioBlkReqHeader {}
