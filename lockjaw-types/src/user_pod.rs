/// Marker trait for types that are safe to construct from arbitrary bytes.
///
/// `Copy` alone does not guarantee this — a `bool` is `Copy` but has only
/// two valid bit patterns; constructing it from an arbitrary byte is UB.
/// `UserPod` explicitly opts-in types whose every possible bit pattern
/// produces a valid, well-defined value. This is the required bound for
/// [`copy_from_user`](crate::mm::user_access::copy_from_user) to be sound.
///
/// # Safety
///
/// The implementing type must be `repr(C)` or a primitive integer, with:
/// - No padding bytes with validity invariants.
/// - No enum discriminants that restrict the bit pattern.
/// - No references, `NonZero*`, `bool`, `char`, or similar niche types.
/// - Every possible byte sequence of `size_of::<Self>()` is a valid `Self`.
pub unsafe trait UserPod: Copy {}

// --- Primitives ---
unsafe impl UserPod for u8 {}
unsafe impl UserPod for u16 {}
unsafe impl UserPod for u32 {}
unsafe impl UserPod for u64 {}
unsafe impl UserPod for i8 {}
unsafe impl UserPod for i16 {}
unsafe impl UserPod for i32 {}
unsafe impl UserPod for i64 {}
unsafe impl UserPod for usize {}
unsafe impl UserPod for isize {}

// --- Fixed-size arrays of UserPod ---
unsafe impl<const N: usize, T: UserPod> UserPod for [T; N] {}
