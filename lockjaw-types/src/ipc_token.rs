/// Pure mint logic for IPC caller-identity tokens.
///
/// The kernel's `EndpointObject` carries a monotonic `next_token: u64`
/// counter; every `sys_export_handle` of an Endpoint allocates the
/// current value as the recipient's identity token and increments the
/// counter. This module owns that arithmetic so it can be host-tested
/// without dragging in kernel KVM types.
///
/// **Invariants pinned here, exercised by tests below:**
///
/// 1. The minted token is always nonzero — encoded via `NonZeroU64`
///    return type. Senders without a nonzero token cannot be
///    constructed by mint, which dovetails with the `HandleKind::
///    Endpoint { caller_token: Option<NonZeroU64> }` representation
///    where `None` means the master (receive-only) handle.
/// 2. The counter starts at 1. Initial state of a fresh
///    `EndpointObject` MUST set `next_token = 1`; otherwise the first
///    mint would attempt `NonZeroU64::new(0)` and panic.
/// 3. Counter monotonicity. Wrap-around (which would re-issue 0 next
///    mint) panics rather than silently violating invariant 1.
///
/// See `docs/book-of-lockjaw/02-handle-identity-tokens.md` for the
/// requirement → implementation mapping.

use core::num::NonZeroU64;

/// Allocate the next identity token from a monotonic counter.
///
/// Returns `(minted_token, next_counter_value)`. The caller writes the
/// returned counter value back into the endpoint state.
///
/// # Panics
///
/// - If `next_token == 0`. This means the endpoint state was never
///   initialized to 1, or the counter wrapped through 0 at u64::MAX +
///   1. Both are kernel-invariant violations.
/// - If `next_token == u64::MAX`. The next increment would wrap to 0,
///   which would mint a 0 token on the next call. We panic on the
///   *current* mint so that the wrap is observed before any 0 token
///   leaks into a handle. (2^64 mints per endpoint is unreachable
///   in practice — this is defensive.)
pub fn mint_caller_token(next_token: u64) -> (NonZeroU64, u64) {
    let token = NonZeroU64::new(next_token)
        .expect("EndpointObject.next_token must be initialized to 1; counter wrapped to 0 if you see this");
    let next = next_token
        .checked_add(1)
        .expect("EndpointObject.next_token at u64::MAX would wrap on next mint (kernel invariant)");
    (token, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_mint_from_initial_counter_returns_one() {
        // Fresh EndpointObject starts at next_token = 1. The first
        // mint produces token 1 and advances the counter to 2.
        let (token, next) = mint_caller_token(1);
        assert_eq!(token.get(), 1);
        assert_eq!(next, 2);
    }

    #[test]
    fn mint_produces_strictly_monotonic_tokens() {
        // Two consecutive mints from the same endpoint produce
        // distinct tokens. This is the property that lets the server
        // distinguish two clients (the original requirement that
        // motivates the whole token model).
        let (t1, c1) = mint_caller_token(1);
        let (t2, c2) = mint_caller_token(c1);
        assert_ne!(t1, t2);
        assert_eq!(t1.get(), 1);
        assert_eq!(t2.get(), 2);
        assert_eq!(c2, 3);
    }

    #[test]
    fn mint_runs_through_a_long_sequence_without_collision() {
        // Sweep enough mints to give confidence the counter advances
        // correctly across many calls. Each minted token must be
        // unique (== iteration index + 1) and the counter must end
        // one above the last token.
        let mut counter = 1u64;
        for expected in 1u64..1024 {
            let (token, next) = mint_caller_token(counter);
            assert_eq!(token.get(), expected);
            counter = next;
        }
        assert_eq!(counter, 1024);
    }

    #[test]
    #[should_panic(expected = "next_token must be initialized to 1")]
    fn mint_with_zero_counter_panics() {
        // Zero counter means the endpoint state was never initialized
        // (or wrapped through 0). Either is a kernel-invariant
        // violation — we want loud failure, not a 0 token slipping
        // through into a sender handle.
        let _ = mint_caller_token(0);
    }

    #[test]
    #[should_panic(expected = "would wrap on next mint")]
    fn mint_at_max_panics_to_avoid_zero_next() {
        // u64::MAX is the last value we can mint without wrapping the
        // counter. We panic *on this mint* (not the next one) so the
        // wrap is observed before a 0 token can be issued.
        let _ = mint_caller_token(u64::MAX);
    }
}
