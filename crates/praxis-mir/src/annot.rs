//! The two slot sets a safepoint carries, and why they are two (MIR-16, F17).
//!
//! One `Vec<LocalId>` used to serve both the GC's shadow frame and the crash
//! debugger's `DebugFrame`, and one `emit_spill` wrote it into both. That is
//! why the debugger's rendered values were *accidentally* correct: liveness
//! over-approximated (its forward walk never killed a definition), so a local
//! whose last use had passed stayed in the list and stayed visible. Making the
//! GC root set exact — which is what [`RootSlots`] is for — would have silently
//! turned those rendered values into `<uninit>`.
//!
//! So the sets are split by *purpose*, not by convenience:
//!
//! - [`RootSlots`] is the GC root set: **minimal** and sound. Everything in it
//!   must be a [`LocalKind::Gc`](crate::ir::LocalKind::Gc) slot the collector
//!   may dereference, and everything live across the safepoint must be in it.
//! - [`DebugSlots`] is what the debugger must be able to render:
//!   **over-approximate** on purpose. Shrinking `RootSlots` must never shrink
//!   this.
//!
//! Neither has a public constructor that takes ids: only
//! [`crate::liveness::annotate`] may fill one, so a builder cannot hand-write a
//! root set that the pass then silently overwrites (61 such literals existed).

use crate::ir::{Inst, LocalId};

/// The two slot sets `inst` carries, by shared reference — `None` for a set this
/// kind of instruction does not have.
///
/// **One match, three readers.** The `&mut` twin of this lives in
/// [`crate::liveness`] and is what *fills* the sets; this is what reads them, and
/// before ADR-128 it was written out a second time inside
/// [`crate::verify`]'s `check_slot_sets`. The backend needs a third reader — the
/// root colouring of ADR-128 decision 2 walks every safepoint's
/// [`RootSlots::live`] before it lowers anything — and a third hand-written copy
/// of the variant list is how the debugger silently stops seeing a local. ADR-044
/// fixes the count of exhaustive matches over [`Inst`] at five; this consolidates
/// two of them into one rather than adding a sixth.
#[must_use]
pub fn slot_sets(inst: &Inst) -> (Option<&RootSlots>, Option<&DebugSlots>) {
    match inst {
        Inst::Alloc { roots, debug, .. }
        | Inst::Materialize { roots, debug, .. }
        | Inst::Call { roots, debug, .. }
        | Inst::CallIndirect { roots, debug, .. }
        | Inst::StructEq { roots, debug, .. } => (Some(roots), Some(debug)),
        Inst::CheckFault { debug, .. } => (None, Some(debug)),
        _ => (None, None),
    }
}

/// The GC root set `inst` carries, if it is a GC safepoint.
///
/// The half of [`slot_sets`] the backend's slot colouring wants, named so the
/// call site reads as the question it is asking.
#[must_use]
pub fn roots_of(inst: &Inst) -> Option<&RootSlots> {
    slot_sets(inst).0
}

/// The GC root set at one safepoint: the `Gc` locals live *across* it, plus the
/// slots whose stale values must be cleared there (MIR-01).
///
/// There is no public constructor taking ids. A builder writes
/// [`RootSlots::unannotated`]; [`crate::liveness::annotate`] is the only thing
/// that can fill one, and [`RootSlots::is_annotated`] is how the verifier tells
/// "the pass ran and found nothing" from "the pass never ran".
#[derive(Debug, Default)]
pub struct RootSlots {
    /// The `Gc` locals live across this safepoint. The backend spills exactly
    /// these into the shadow frame.
    live: Vec<LocalId>,
    /// The `Gc` locals whose shadow slot may still hold a value from an earlier
    /// safepoint but which are **dead** here (MIR-01). The backend nulls
    /// exactly these. Disjoint from `live` by construction.
    dead: Vec<LocalId>,
    /// Whether [`crate::liveness::annotate`] has run over this safepoint.
    annotated: bool,
}

impl RootSlots {
    /// What every builder site writes: "the liveness pass has not run yet".
    #[must_use]
    pub fn unannotated() -> RootSlots {
        RootSlots::default()
    }

    /// The live roots, in ascending `LocalId` order.
    #[must_use]
    pub fn live(&self) -> &[LocalId] {
        &self.live
    }

    /// The slots to null at this safepoint, in ascending `LocalId` order.
    #[must_use]
    pub fn dead(&self) -> &[LocalId] {
        &self.dead
    }

    /// Iterate the live roots.
    pub fn iter(&self) -> impl Iterator<Item = LocalId> + '_ {
        self.live.iter().copied()
    }

    /// Whether the liveness pass has filled this set. An *empty but annotated*
    /// set is a real answer ("nothing is live here"); an unannotated one is the
    /// absence of an answer, and the verifier rejects it at a safepoint.
    #[must_use]
    pub fn is_annotated(&self) -> bool {
        self.annotated
    }

    /// Fill the set. Crate-private: only [`crate::liveness::annotate`] calls it.
    pub(crate) fn set(&mut self, live: Vec<LocalId>, dead: Vec<LocalId>) {
        self.live = live;
        self.dead = dead;
        self.annotated = true;
    }
}

/// What the crash debugger must be able to render at one program point.
///
/// **Separate from [`RootSlots`] on purpose** (ADR-021, ADR-035). This set is
/// over-approximate: it holds every `Gc` local whose value has been produced on
/// the path to this point, whether or not it is still live. A user who asks
/// `locals` after `var a = 10` wants to see `a` even where the optimizer can
/// prove nothing reads it again.
///
/// It is also what [`crate::ir::Inst::CheckFault`] carries, which is not a GC
/// safepoint at all: `CheckFault` allocates nothing, so it roots nothing, but
/// it is where a fault diverts and therefore where a snapshot is taken.
///
/// **It is a contract, not a store list** (ADR-104). The backend used to emit
/// one store per member of [`DebugSlots::visible`] at every annotated point;
/// it now emits one store per `Gc` *definition*, which realizes the same
/// contents at every point a snapshot can be taken and rather fewer stores. The
/// set stays exactly as defined here — narrowing it to a per-point delta would
/// be the shrink `the_debug_set_still_shows_what_the_root_set_dropped` exists
/// to refuse.
#[derive(Debug, Default)]
pub struct DebugSlots {
    visible: Vec<LocalId>,
    annotated: bool,
}

impl DebugSlots {
    /// What every builder site writes.
    #[must_use]
    pub fn unannotated() -> DebugSlots {
        DebugSlots::default()
    }

    /// The debugger-visible locals, in ascending `LocalId` order.
    #[must_use]
    pub fn visible(&self) -> &[LocalId] {
        &self.visible
    }

    /// Iterate the debugger-visible locals.
    pub fn iter(&self) -> impl Iterator<Item = LocalId> + '_ {
        self.visible.iter().copied()
    }

    /// Whether the liveness pass has filled this set.
    #[must_use]
    pub fn is_annotated(&self) -> bool {
        self.annotated
    }

    /// Fill the set. Crate-private: only [`crate::liveness::annotate`] calls it.
    pub(crate) fn set(&mut self, visible: Vec<LocalId>) {
        self.visible = visible;
        self.annotated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unannotated_set_is_empty_and_says_so() {
        let r = RootSlots::unannotated();
        assert!(!r.is_annotated());
        assert!(r.live().is_empty());
        assert!(r.dead().is_empty());
        let d = DebugSlots::unannotated();
        assert!(!d.is_annotated());
        assert!(d.visible().is_empty());
    }

    /// The distinction the verifier depends on: "annotated, and nothing is
    /// live" must be distinguishable from "never annotated". Both are empty.
    #[test]
    fn an_annotated_empty_set_is_not_an_unannotated_one() {
        let mut r = RootSlots::unannotated();
        r.set(Vec::new(), Vec::new());
        assert!(r.is_annotated());
        assert!(r.live().is_empty());
        assert_ne!(r.is_annotated(), RootSlots::unannotated().is_annotated());
    }
}
