//! Pure typestate markers for the SDHCI per-operation lifecycle.
//!
//! `SdhciCommandInit<'a, S>` and the I/O-side state transitions live in
//! `lockjaw-userlib::sdhci`; this module carries only the pure pieces:
//!   - Four ZST op-state markers (`OpIdle`/`OpArmed`/`OpKicked`/`OpCompleted`)
//!     used as the `S` phantom of `SdhciCommandInit<'a, S>`.
//!   - A sealed `OpState` marker trait so the userlib side can write a
//!     single bound.
//!   - A value-level `OperationPhase` enum that mirrors the typestate,
//!     plus a pure `legal_next_phase` validator — used by host tests as
//!     the reference model the I/O-side typestate must agree with.
//!
//! Lifecycle semantics (one command, one envelope):
//!   - `OpIdle`     — `open()` returns the builder in this state.
//!                    `issue_no_data` / `issue_data_transfer` consume
//!                    self in this state and run the full
//!                    arm→kick→completion→idle cycle internally,
//!                    returning the builder in `OpIdle` again so the
//!                    next command can reuse it.
//!   - `OpArmed`    — register-program phase complete (BLOCK_SIZE,
//!                    BLOCK_COUNT, ARGUMENT, ADMA_ADDRESS programmed);
//!                    controller has not been kicked yet.
//!   - `OpKicked`   — combined TRANSFER_MODE+COMMAND store fired;
//!                    waiting for `CMD_COMPLETE` (and optionally
//!                    `DATA_COMPLETE`).
//!   - `OpCompleted` — completion signal observed (poll-based for
//!                    ID-phase, IRQ-based for data-phase); response
//!                    registers latched. Status W1C + transition back
//!                    to `OpIdle` happens internally inside the issue
//!                    method.
//!
//! `OpArmed`, `OpKicked`, `OpCompleted` are private substates of the
//! envelope methods; user code only ever sees `SdhciCommandInit<OpIdle>`.
//! They exist as separate states so the *implementation* of the issue
//! methods can be written as a sequence of state-consuming substeps,
//! making the program-then-kick-then-wait order checkable by the
//! compiler within the userlib.

/// Sealed marker trait for the four operation lifecycle states.
///
/// `VARIANT` ties each marker type to its value-level `OperationPhase`
/// analog at compile time. **Forward direction only:** adding a new
/// marker without naming an `OperationPhase` variant is a compile
/// error (missing trait item). The reverse direction (adding an
/// `OperationPhase` variant without a marker) is NOT compile-enforced;
/// the `each_marker_maps_to_its_own_*` runtime tests catch the
/// silent-collapse case (two markers mapping to the same variant)
/// but not "variant exists with no marker".
pub trait OpState: sealed::Sealed {
    /// The value-level `OperationPhase` analog of this marker.
    const VARIANT: OperationPhase;
}

mod sealed {
    pub trait Sealed {}
}

/// Builder is open but no register programming has happened yet.
/// `open()` returns this state; the issue methods consume it.
pub struct OpIdle;
/// Register programming complete (BLOCK_SIZE, COUNT, ARGUMENT,
/// ADMA_ADDRESS); controller not yet kicked.
pub struct OpArmed;
/// Combined TRANSFER_MODE+COMMAND store fired; controller running.
pub struct OpKicked;
/// Completion signal observed; response latched. Transient state
/// inside the issue method before returning to `OpIdle`.
pub struct OpCompleted;

impl sealed::Sealed for OpIdle {}
impl sealed::Sealed for OpArmed {}
impl sealed::Sealed for OpKicked {}
impl sealed::Sealed for OpCompleted {}

impl OpState for OpIdle      { const VARIANT: OperationPhase = OperationPhase::OpIdle; }
impl OpState for OpArmed     { const VARIANT: OperationPhase = OperationPhase::OpArmed; }
impl OpState for OpKicked    { const VARIANT: OperationPhase = OperationPhase::OpKicked; }
impl OpState for OpCompleted { const VARIANT: OperationPhase = OperationPhase::OpCompleted; }

/// Value-level analog of the typestate markers, paired with a pure
/// `legal_next_phase` validator. Host-tested as the reference model
/// the I/O-side state machine must agree with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationPhase {
    OpIdle,
    OpArmed,
    OpKicked,
    OpCompleted,
}

impl OperationPhase {
    /// Legal next phase from `self`. Each command runs the full
    /// `OpIdle → OpArmed → OpKicked → OpCompleted → OpIdle` chain;
    /// the closing `OpCompleted → OpIdle` makes the envelope reusable
    /// across multiple commands.
    pub const fn legal_next_phase(self) -> OperationPhase {
        match self {
            OperationPhase::OpIdle => OperationPhase::OpArmed,
            OperationPhase::OpArmed => OperationPhase::OpKicked,
            OperationPhase::OpKicked => OperationPhase::OpCompleted,
            OperationPhase::OpCompleted => OperationPhase::OpIdle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_next_phase_cycles_through_all_four() {
        // OpIdle → OpArmed → OpKicked → OpCompleted → OpIdle.
        let mut phase = OperationPhase::OpIdle;
        phase = phase.legal_next_phase();
        assert_eq!(phase, OperationPhase::OpArmed);
        phase = phase.legal_next_phase();
        assert_eq!(phase, OperationPhase::OpKicked);
        phase = phase.legal_next_phase();
        assert_eq!(phase, OperationPhase::OpCompleted);
        phase = phase.legal_next_phase();
        assert_eq!(phase, OperationPhase::OpIdle);
    }

    #[test]
    fn legal_next_phase_is_total_function() {
        // Every variant has a defined next phase (no panics).
        let _ = OperationPhase::OpIdle.legal_next_phase();
        let _ = OperationPhase::OpArmed.legal_next_phase();
        let _ = OperationPhase::OpKicked.legal_next_phase();
        let _ = OperationPhase::OpCompleted.legal_next_phase();
    }

    #[test]
    fn legal_next_phase_never_self_loops() {
        // Each transition must change phase; otherwise the typestate
        // is purely decorative.
        for phase in [
            OperationPhase::OpIdle,
            OperationPhase::OpArmed,
            OperationPhase::OpKicked,
            OperationPhase::OpCompleted,
        ] {
            assert_ne!(
                phase,
                phase.legal_next_phase(),
                "{:?} must not loop to itself",
                phase,
            );
        }
    }

    #[test]
    fn legal_next_phase_completes_to_idle() {
        // The terminal phase loops back to OpIdle so the envelope can
        // be reused for subsequent commands without reopening.
        assert_eq!(
            OperationPhase::OpCompleted.legal_next_phase(),
            OperationPhase::OpIdle,
        );
    }

    // Compile-time check that each marker impls OpState.
    #[allow(dead_code)]
    fn _markers_impl_op_state() {
        fn assert_state<S: OpState>() {}
        assert_state::<OpIdle>();
        assert_state::<OpArmed>();
        assert_state::<OpKicked>();
        assert_state::<OpCompleted>();
    }

    #[test]
    fn each_marker_maps_to_its_own_operation_phase() {
        // Compile-time mapping (via VARIANT const) ties each op-state
        // marker to one OperationPhase variant. Catches a future
        // typestate collapse (two markers mapped to the same variant).
        let pairs = [
            (<OpIdle      as OpState>::VARIANT, "OpIdle"),
            (<OpArmed     as OpState>::VARIANT, "OpArmed"),
            (<OpKicked    as OpState>::VARIANT, "OpKicked"),
            (<OpCompleted as OpState>::VARIANT, "OpCompleted"),
        ];
        assert_eq!(pairs[0].0, OperationPhase::OpIdle);
        assert_eq!(pairs[1].0, OperationPhase::OpArmed);
        assert_eq!(pairs[2].0, OperationPhase::OpKicked);
        assert_eq!(pairs[3].0, OperationPhase::OpCompleted);
        for i in 0..pairs.len() {
            for j in (i + 1)..pairs.len() {
                assert_ne!(
                    pairs[i].0, pairs[j].0,
                    "markers {} and {} share VARIANT — typestate collapse",
                    pairs[i].1, pairs[j].1,
                );
            }
        }
    }
}
