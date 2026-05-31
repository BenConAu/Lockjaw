//! Pure typestate markers for the MMC card lifecycle.
//!
//! `MmcCard<'a, S>` and the I/O-side state transitions live in
//! `lockjaw-userlib::sdhci`; this module carries only the pure pieces:
//!   - Six ZST state markers (`Uninit`/`Idle`/`Ready`/`Ident`/`Stby`/`Tran`)
//!     used as the `S` phantom of `MmcCard<'a, S>`.
//!   - A sealed `CardLifecycleState` marker trait the six markers impl,
//!     so the userlib side can write a single bound on `MmcCard`.
//!   - A value-level `CardState` enum that mirrors the typestate, plus a
//!     pure `legal_next_states` validator — used by host tests as the
//!     reference model the I/O-side typestate must agree with.
//!   - `BusWidth` (1-bit vs 4-bit), tracked as a runtime field on
//!     `MmcCard<Tran>` rather than as a typestate slot because the
//!     ACMD6 bus-width flip is local to `Tran`.
//!
//! The composite `CardInfo` (rca/ocr/cid/csd/bus_width) that crosses the
//! `MmcCard::<Tran>::into_parts()` boundary into `Emmc2BlockEngine::new`
//! lives in `lockjaw-userlib::sdhci`, not here, because its constructor
//! must be `pub(crate)` to lockjaw-userlib so only the typestate chain
//! can mint one — a `pub(super)` constructor in lockjaw-types couldn't
//! be reached by lockjaw-userlib at all. The proof property ("CardInfo
//! exists implies the card reached Tran") survives the move because the
//! constructor stays gated; the structural guarantee is the same.

/// Sealed marker trait for the six card lifecycle states. The userlib's
/// `MmcCard<'a, S>` bounds `S: CardLifecycleState` so only the six
/// markers below are accepted as the phantom slot.
///
/// `VARIANT` ties each marker type to its value-level `CardState`
/// analog at compile time. **Forward direction only:** adding a new
/// marker (`pub struct Programming;`) and impl'ing
/// `CardLifecycleState for Programming` without naming a `CardState`
/// variant is a compile error (missing trait item / unresolved path).
/// The reverse direction (adding a `CardState` variant + a
/// `legal_next_states` arm without a marker) is NOT compile-enforced
/// and relies on review discipline; the `each_marker_maps_to_its_own_*`
/// runtime tests catch the silent-collapse case (two markers mapping
/// to the same variant) but not "variant exists with no marker".
pub trait CardLifecycleState: sealed::Sealed {
    /// The value-level `CardState` analog of this marker.
    const VARIANT: CardState;
}

mod sealed {
    pub trait Sealed {}
}

/// Card has not been touched yet (no CMD0 issued).
pub struct Uninit;
/// After CMD0 reset; card is in Idle state, OCR not yet read.
pub struct Idle;
/// After ACMD41 loop completed (`power_up_done = true`); OCR captured.
pub struct Ready;
/// After CMD2 (ALL_SEND_CID); CID captured.
pub struct Ident;
/// After CMD3 (SEND_RELATIVE_ADDR); RCA captured. CMD9 (SEND_CSD)
/// executes in this state.
pub struct Stby;
/// After CMD7 (SELECT_CARD) + DAT_INHIBIT drain; card is the bus's
/// data target. Data transfers and ACMD6 bus-width changes execute
/// here.
pub struct Tran;

impl sealed::Sealed for Uninit {}
impl sealed::Sealed for Idle {}
impl sealed::Sealed for Ready {}
impl sealed::Sealed for Ident {}
impl sealed::Sealed for Stby {}
impl sealed::Sealed for Tran {}

impl CardLifecycleState for Uninit { const VARIANT: CardState = CardState::Uninit; }
impl CardLifecycleState for Idle   { const VARIANT: CardState = CardState::Idle; }
impl CardLifecycleState for Ready  { const VARIANT: CardState = CardState::Ready; }
impl CardLifecycleState for Ident  { const VARIANT: CardState = CardState::Ident; }
impl CardLifecycleState for Stby   { const VARIANT: CardState = CardState::Stby; }
impl CardLifecycleState for Tran   { const VARIANT: CardState = CardState::Tran; }

/// Value-level analog of the typestate markers. Used by host tests
/// and documentation; the actual type-level enforcement is the ZST
/// markers above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardState {
    Uninit,
    Idle,
    Ready,
    Ident,
    Stby,
    Tran,
}

impl CardState {
    /// Legal next states from `self` in the **Lockjaw emmc2 init+read
    /// flow** — a deliberate subset of the SD Physical Layer Spec §4.4
    /// card state diagram. The reference model encodes the transitions
    /// the typestate chain in `lockjaw-userlib::sdhci::MmcCard<S>`
    /// exercises, not every spec-legal edge.
    ///
    /// Spec edges deliberately NOT in this model:
    ///   - `Tran → Stby` via CMD7 with RCA=0 (deselect). Lockjaw never
    ///     deselects; the card stays in Tran for the program lifetime.
    ///   - `Tran → Idle` via CMD0 (reset from Tran).
    ///   - `Tran → Inactive` via CMD15.
    ///   - `Tran → Rcv` via CMD24/25 (data write). Lockjaw's emmc2 is
    ///     read-only today; CMD25 is dead code (deleted in O5).
    ///   - `Tran → Sending` via CMD17/18 with bounce-back to Tran.
    ///     Modeled as `Tran → Tran` because the data excursion ends
    ///     in the same logical state the caller cares about.
    /// If a future driver flow adds any of these edges, the model and
    /// the typestate transitions in lockjaw-userlib must both grow to
    /// match.
    pub const fn legal_next_states(self) -> &'static [CardState] {
        match self {
            // CMD0 (GO_IDLE_STATE) → Idle.
            CardState::Uninit => &[CardState::Idle],
            // ACMD41 loop → Ready (when power_up_done=1).
            CardState::Idle => &[CardState::Ready],
            // CMD2 (ALL_SEND_CID) → Ident.
            CardState::Ready => &[CardState::Ident],
            // CMD3 (SEND_RELATIVE_ADDR) → Stby.
            CardState::Ident => &[CardState::Stby],
            // CMD9 (SEND_CSD) stays in Stby; CMD7 (SELECT_CARD) → Tran.
            CardState::Stby => &[CardState::Stby, CardState::Tran],
            // ACMD6 (SET_BUS_WIDTH) stays in Tran. Data-read excursion
            // (CMD17 → Sending → Tran) is modeled as Tran→Tran because
            // the data fetch returns to Tran. See doc comment for
            // spec edges excluded from this subset.
            CardState::Tran => &[CardState::Tran],
        }
    }
}

/// SD card bus width on the controller side. `Bit1` after card reset;
/// `Bit4` after a successful ACMD6 SET_BUS_WIDTH on the card AND a
/// HOST_CONTROL_1.DAT_4BIT flip on the controller. Tracked as a runtime
/// field on `MmcCard<Tran>` rather than a typestate slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusWidth {
    /// 1-bit DAT lane only (post-reset default).
    Bit1,
    /// 4 DAT lanes (post-ACMD6 + post-host-control flip).
    Bit4,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_next_uninit_is_idle_only() {
        assert_eq!(CardState::Uninit.legal_next_states(), &[CardState::Idle]);
    }

    #[test]
    fn legal_next_idle_is_ready() {
        assert_eq!(CardState::Idle.legal_next_states(), &[CardState::Ready]);
    }

    #[test]
    fn legal_next_ready_is_ident() {
        assert_eq!(CardState::Ready.legal_next_states(), &[CardState::Ident]);
    }

    #[test]
    fn legal_next_ident_is_stby() {
        assert_eq!(CardState::Ident.legal_next_states(), &[CardState::Stby]);
    }

    #[test]
    fn legal_next_stby_allows_self_and_tran() {
        // CMD9 (SEND_CSD) stays in Stby; CMD7 → Tran. Both legal.
        let nexts = CardState::Stby.legal_next_states();
        assert!(nexts.contains(&CardState::Stby));
        assert!(nexts.contains(&CardState::Tran));
    }

    #[test]
    fn legal_next_tran_is_self_only() {
        // ACMD6 + data transfers stay in Tran. No deselect path today.
        assert_eq!(CardState::Tran.legal_next_states(), &[CardState::Tran]);
    }

    #[test]
    fn no_state_transitions_backwards_in_id_phase() {
        // None of Uninit→...→Stby can return to an earlier state.
        for from in [
            CardState::Uninit,
            CardState::Idle,
            CardState::Ready,
            CardState::Ident,
        ] {
            for &to in from.legal_next_states() {
                // Stby allows self-loop; that's the only exception.
                assert!(
                    to != from,
                    "{:?} should not loop to itself in ID-phase",
                    from,
                );
            }
        }
    }

    #[test]
    fn no_state_skips_intermediate_phase() {
        // The chain is strictly Uninit→Idle→Ready→Ident→Stby→Tran;
        // a transition from earlier than Stby cannot skip to Tran.
        for from in [
            CardState::Uninit,
            CardState::Idle,
            CardState::Ready,
            CardState::Ident,
        ] {
            for &to in from.legal_next_states() {
                assert!(
                    to != CardState::Tran,
                    "{:?} must not jump straight to Tran",
                    from,
                );
            }
        }
    }

    #[test]
    fn bus_width_variants_distinct() {
        assert_ne!(BusWidth::Bit1, BusWidth::Bit4);
    }

    // ZST markers compile-time check: this fn is never called but
    // forces the compiler to confirm each marker impls CardLifecycleState
    // and is a distinct type. If a marker is missing from the impl list
    // above this won't compile.
    #[allow(dead_code)]
    fn _markers_impl_card_lifecycle_state() {
        fn assert_state<S: CardLifecycleState>() {}
        assert_state::<Uninit>();
        assert_state::<Idle>();
        assert_state::<Ready>();
        assert_state::<Ident>();
        assert_state::<Stby>();
        assert_state::<Tran>();
    }

    #[test]
    fn each_marker_maps_to_its_own_card_state_variant() {
        // Compile-time mapping (via VARIANT const) ties each marker
        // type to one CardState variant. A future contributor mapping
        // two markers to the same variant — a silent typestate
        // collapse — is caught here at runtime; the more important
        // protection is the impl line itself, which can't be written
        // without naming a CardState variant.
        let pairs = [
            (<Uninit as CardLifecycleState>::VARIANT, "Uninit"),
            (<Idle   as CardLifecycleState>::VARIANT, "Idle"),
            (<Ready  as CardLifecycleState>::VARIANT, "Ready"),
            (<Ident  as CardLifecycleState>::VARIANT, "Ident"),
            (<Stby   as CardLifecycleState>::VARIANT, "Stby"),
            (<Tran   as CardLifecycleState>::VARIANT, "Tran"),
        ];
        assert_eq!(pairs[0].0, CardState::Uninit);
        assert_eq!(pairs[1].0, CardState::Idle);
        assert_eq!(pairs[2].0, CardState::Ready);
        assert_eq!(pairs[3].0, CardState::Ident);
        assert_eq!(pairs[4].0, CardState::Stby);
        assert_eq!(pairs[5].0, CardState::Tran);
        // Uniqueness check: pairwise distinct variants.
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
