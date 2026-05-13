/// Pure data structure backing device-manager's clock-binding table.
///
/// device-manager arbitrates non-virtualizable clock hardware on
/// behalf of drivers (see
/// `docs/book-of-lockjaw/03-non-virtualizable-hardware.md`). Each
/// `acquire` from a driver yields an opaque `handle_id` that maps to
/// `(controller_phandle, clock_id)`; subsequent `CLOCK_OP_*` calls
/// look up the binding by `(caller_token, handle_id)` and forward
/// `clock_id` to the provider.
///
/// **Invariants pinned here, exercised by tests below:**
///
/// 1. `acquire` is idempotent per `(caller_token, controller_phandle,
///    clock_id)`. Repeated calls from the same caller for the same
///    binding return the same `handle_id` rather than allocating a
///    new row. This blocks the trivial local-DoS where one client
///    exhausts the table by repeating the same request.
/// 2. Bindings are isolated per `caller_token`: `lookup` only finds
///    handles owned by the calling token. A driver cannot guess
///    another driver's `handle_id` and pivot to its clock.
/// 3. `caller_token` is `NonZeroU64`. Token 0 is the kernel's
///    "no caller" sentinel (the master / receive-only side of an
///    Endpoint, see `docs/book-of-lockjaw/02-handle-identity-tokens.md`)
///    and would alias the empty-slot sentinel inside this table —
///    the type makes that case unrepresentable instead of relying
///    on every call site to validate.
/// 4. Exhaustion of `CLOCK_HANDLE_TABLE_CAP` distinct bindings yields
///    `TableFull`. The cap is generous for the current Lockjaw shape
///    (one driver per clock, ~5 drivers); bump if a real workload
///    approaches it rather than introducing eviction policy.

use core::num::NonZeroU64;

/// Capacity of the binding table. 32 bindings is generous for the
/// current Lockjaw shape (one driver per clock, ~5 drivers expected
/// across the lifetime of a boot). Bumping this is cheap; the table
/// lives in device-manager's BSS.
pub const CLOCK_HANDLE_TABLE_CAP: usize = 32;

/// One row of the binding table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockBinding {
    /// Caller token of the driver that owns this binding. `None`
    /// (encoded as `Option::None`) marks the empty-slot sentinel.
    pub caller_token: Option<NonZeroU64>,
    /// Opaque id device-manager allocated; the driver passes this in
    /// every `CLOCK_OP_*` call.
    pub handle_id: u32,
    /// DTB phandle of the clock controller this binding speaks to.
    pub controller_phandle: u32,
    /// Per-controller clock leaf id (e.g., 51 = BCM2835_CLOCK_EMMC2).
    pub clock_id: u32,
}

impl ClockBinding {
    pub const EMPTY: Self = Self {
        caller_token: None,
        handle_id: 0,
        controller_phandle: 0,
        clock_id: 0,
    };
}

/// Outcome of a single `acquire` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireResult {
    /// The caller already had a binding for this
    /// `(controller_phandle, clock_id)`; returning the existing id.
    Existing(u32),
    /// Fresh binding allocated; `handle_id` is the new id.
    Allocated(u32),
    /// All `CLOCK_HANDLE_TABLE_CAP` slots are in use; caller should
    /// reply `CLOCK_ERR_TABLE_FULL`.
    TableFull,
}

/// device-manager's binding table.
#[derive(Clone, Copy)]
pub struct ClockHandleTable {
    bindings: [ClockBinding; CLOCK_HANDLE_TABLE_CAP],
    /// Monotonic source for `handle_id` values. Starts at 1 so the
    /// value is never zero (drivers can't usefully forge handle_id
    /// 0 anyway because the lookup is scoped by caller_token, but
    /// keeping ids nonzero matches the convention used by
    /// caller_token itself).
    next_handle_id: u32,
}

impl ClockHandleTable {
    pub const fn empty() -> Self {
        Self {
            bindings: [ClockBinding::EMPTY; CLOCK_HANDLE_TABLE_CAP],
            next_handle_id: 1,
        }
    }

    /// Acquire a clock binding. Idempotent: if the caller already
    /// holds a binding for `(controller_phandle, clock_id)`, returns
    /// `Existing(handle_id)` instead of allocating a new row.
    ///
    /// Single pass over the table tracks both the dedup match and
    /// the first free slot, so we don't iterate twice on the
    /// allocate path.
    pub fn acquire(
        &mut self,
        caller_token: NonZeroU64,
        controller_phandle: u32,
        clock_id: u32,
    ) -> AcquireResult {
        let mut existing_id: Option<u32> = None;
        let mut free_slot: Option<usize> = None;
        for (i, b) in self.bindings.iter().enumerate() {
            if existing_id.is_none()
                && b.caller_token == Some(caller_token)
                && b.controller_phandle == controller_phandle
                && b.clock_id == clock_id
            {
                existing_id = Some(b.handle_id);
            }
            if free_slot.is_none() && b.caller_token.is_none() {
                free_slot = Some(i);
            }
        }

        if let Some(id) = existing_id {
            return AcquireResult::Existing(id);
        }
        let slot = match free_slot {
            Some(i) => i,
            None => return AcquireResult::TableFull,
        };
        let id = self.next_handle_id;
        self.next_handle_id = self.next_handle_id.wrapping_add(1);
        self.bindings[slot] = ClockBinding {
            caller_token: Some(caller_token),
            handle_id: id,
            controller_phandle,
            clock_id,
        };
        AcquireResult::Allocated(id)
    }

    /// Look up the binding for an op call. Returns `None` if the
    /// `handle_id` is not owned by `caller_token` or was never
    /// allocated. The scoping is the "drivers can't guess each
    /// other's handle_ids" property.
    pub fn lookup(
        &self,
        caller_token: NonZeroU64,
        handle_id: u32,
    ) -> Option<&ClockBinding> {
        self.bindings.iter().find(|b| {
            b.caller_token == Some(caller_token) && b.handle_id == handle_id
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: u64) -> NonZeroU64 {
        NonZeroU64::new(n).unwrap()
    }

    #[test]
    fn first_acquire_returns_allocated_with_id_one() {
        let mut t = ClockHandleTable::empty();
        match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Allocated(id) => assert_eq!(id, 1),
            other => panic!("expected Allocated(1), got {:?}", other),
        }
    }

    #[test]
    fn repeat_acquire_same_triple_returns_existing_same_id() {
        // The dedup property: the same caller asking for the same
        // (controller, clock_id) must not eat a fresh row.
        let mut t = ClockHandleTable::empty();
        let first = match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("expected Allocated, got {:?}", other),
        };
        match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Existing(id) => assert_eq!(id, first),
            other => panic!("expected Existing({}), got {:?}", first, other),
        }
        match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Existing(id) => assert_eq!(id, first),
            other => panic!("expected Existing({}), got {:?}", first, other),
        }
    }

    #[test]
    fn distinct_callers_get_distinct_handles_for_same_clock() {
        // Two drivers each ask for the same (controller, clock_id).
        // They get distinct handles — caller-token isolation.
        let mut t = ClockHandleTable::empty();
        let a = match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        let b = match t.acquire(token(8), 0xAA, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        assert_ne!(a, b);
    }

    #[test]
    fn same_caller_distinct_clocks_get_distinct_handles() {
        let mut t = ClockHandleTable::empty();
        let a = match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        let b = match t.acquire(token(7), 0xAA, 52) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        assert_ne!(a, b);
    }

    #[test]
    fn same_caller_distinct_controllers_get_distinct_handles() {
        let mut t = ClockHandleTable::empty();
        let a = match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        let b = match t.acquire(token(7), 0xBB, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        assert_ne!(a, b);
    }

    #[test]
    fn one_caller_repeating_same_request_cannot_exhaust_table() {
        // The local-DoS regression test Codex flagged: repeating the
        // same acquire many times must not consume rows.
        let mut t = ClockHandleTable::empty();
        for _ in 0..(CLOCK_HANDLE_TABLE_CAP * 4) {
            match t.acquire(token(7), 0xAA, 51) {
                AcquireResult::Allocated(_) | AcquireResult::Existing(_) => {}
                AcquireResult::TableFull => panic!("dedup failed: same request consumed rows"),
            }
        }
        // Other (controller, clock_id) bindings must still be available
        // for the same caller — table is essentially empty bar the one row.
        for i in 0..(CLOCK_HANDLE_TABLE_CAP - 1) {
            match t.acquire(token(7), 0xAA, 100 + i as u32) {
                AcquireResult::Allocated(_) => {}
                other => panic!("slot {} unexpected: {:?}", i, other),
            }
        }
    }

    #[test]
    fn exhausting_distinct_bindings_returns_table_full() {
        let mut t = ClockHandleTable::empty();
        for i in 0..CLOCK_HANDLE_TABLE_CAP {
            match t.acquire(token(7), 0xAA, i as u32) {
                AcquireResult::Allocated(_) => {}
                other => panic!("slot {} unexpected: {:?}", i, other),
            }
        }
        // One more distinct binding must fail.
        assert_eq!(
            t.acquire(token(7), 0xAA, 9999),
            AcquireResult::TableFull,
        );
        // But repeating an existing binding still dedups (idempotent
        // lookup doesn't depend on free slots).
        match t.acquire(token(7), 0xAA, 0) {
            AcquireResult::Existing(_) => {}
            other => panic!("expected Existing, got {:?}", other),
        }
    }

    #[test]
    fn lookup_finds_own_binding_by_handle_id() {
        let mut t = ClockHandleTable::empty();
        let id = match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        let found = t.lookup(token(7), id).expect("own binding visible");
        assert_eq!(found.controller_phandle, 0xAA);
        assert_eq!(found.clock_id, 51);
    }

    #[test]
    fn lookup_does_not_cross_caller_token_boundaries() {
        // Driver B cannot use Driver A's handle_id.
        let mut t = ClockHandleTable::empty();
        let a_id = match t.acquire(token(7), 0xAA, 51) {
            AcquireResult::Allocated(id) => id,
            other => panic!("got {:?}", other),
        };
        // Other token tries A's handle_id.
        assert!(t.lookup(token(8), a_id).is_none());
        // And A still sees its own.
        assert!(t.lookup(token(7), a_id).is_some());
    }

    #[test]
    fn lookup_returns_none_for_unknown_handle_id() {
        let mut t = ClockHandleTable::empty();
        let _ = t.acquire(token(7), 0xAA, 51);
        assert!(t.lookup(token(7), 999).is_none());
    }
}
