//! Immortal singleton objects (§4.3, M3 deliverable).
//!
//! §4.3 notes that `Unit` and `Bool` "may be immortal singleton objects." M3
//! materializes this: [`Immortals`] pre-allocates `Unit`, `true`, and `false`
//! once at runtime startup. They are kept out of the sweepable `live` set, so
//! the collector never reclaims them — they live for the entire run.
//!
//! Immortals are the natural return value for runtime wrappers that must return
//! *a* `GcRef` after a fault (Appendix B: "return a valid sentinel object such
//! as `Unit`").
//!
//! **`Int` joined them.** The same §4.3 paragraph reserves the right to "intern
//! small integers", and a small `Int` satisfies exactly the two conditions the
//! `ImmortalWitness` below exists to enforce — its payload is `Copy`, and there
//! is a bounded set of them so each can be minted once at startup. The range and
//! the argument that sharing an `Int` is unobservable are [`crate::small_int`]'s;
//! this module's job is only that the table is minted here, once, like every
//! other immortal.

use crate::heap::Heap;
use crate::scalars::{BoolPayload, BOOL_PAYLOAD, INT_PAYLOAD, UNIT_PAYLOAD};
use crate::small_int::{self, SMALL_INT_COUNT, SMALL_INT_MAX, SMALL_INT_MIN};
use crate::GcRef;

/// Proof that an [`Heap::alloc_immortal`] call comes from this module.
///
/// The inner field is private to `immortal.rs`, so [`Immortals::new`] is the
/// only place a witness can be constructed and therefore the only place an
/// immortal can be minted. Restricting it here is what keeps two properties
/// true: an immortal is never swept and never dropped, so its payload must be
/// `Copy` and it must be allocated exactly once — a wrapper that minted one per
/// call consumed unregistered arena storage permanently (RT-03).
pub(crate) struct ImmortalWitness(());

/// The immortal singletons, pre-allocated at runtime start (§4.3).
#[repr(C)]
pub struct Immortals {
    unit: GcRef,
    true_: GcRef,
    false_: GcRef,
    /// One `Int` per value in [`crate::small_int`]'s range, indexed by
    /// [`small_int::index_of`].
    ///
    /// A `Box<[GcRef]>` rather than a `[GcRef; SMALL_INT_COUNT]` field because
    /// generated code reads this table through a raw pointer parked in
    /// `RuntimeContext.small_ints`, and that pointer must survive a move of the
    /// `Runtime` that owns the `Immortals`. An inline array would move with it;
    /// a boxed slice's elements do not.
    small_ints: Box<[GcRef]>,
}

impl Immortals {
    /// Allocate the immortal singletons on `heap`. They are intentionally *not*
    /// registered in the heap's live set; the collector never reclaims them.
    pub fn new(heap: &Heap) -> Self {
        // Bypass `Heap::alloc`'s live-set registration: immortals are managed
        // out-of-band. We use the same low-level layout so the descriptors and
        // accessors still work on them.
        let unit = heap.alloc_immortal(UNIT_PAYLOAD, (), ImmortalWitness(()));
        let true_ = heap.alloc_immortal(BOOL_PAYLOAD, 1_u8, ImmortalWitness(()));
        let false_ = heap.alloc_immortal(BOOL_PAYLOAD, 0_u8, ImmortalWitness(()));
        // Immortals start black so a mark phase that happens to visit them (e.g.
        // via a root that aliases them) does not transiently un-protect them.
        unit.header().set_mark_color(crate::gc::BLACK);
        true_.header().set_mark_color(crate::gc::BLACK);
        false_.header().set_mark_color(crate::gc::BLACK);
        // The interned `Int`s, in index order, so slot `i` holds
        // `SMALL_INT_MIN + i` and `small_int::index_of` is the only arithmetic
        // anyone does over this table. Minting them here — rather than lazily on
        // first use — is what keeps the `ImmortalWitness` seal meaningful: a
        // lazily-minted immortal is a wrapper that mints one per call away from
        // RT-03, and it would also make `ctx.small_ints` hold a null the backend
        // would have to test.
        let small_ints: Box<[GcRef]> = (SMALL_INT_MIN..=SMALL_INT_MAX)
            .map(|v| {
                let r = heap.alloc_immortal(INT_PAYLOAD, v, ImmortalWitness(()));
                r.header().set_mark_color(crate::gc::BLACK);
                r
            })
            .collect();
        debug_assert_eq!(small_ints.len(), SMALL_INT_COUNT);
        Immortals {
            unit,
            true_,
            false_,
            small_ints,
        }
    }

    /// The immortal `Unit` value.
    #[inline]
    pub fn unit(&self) -> GcRef {
        self.unit
    }

    /// The immortal `true` value.
    #[inline]
    pub fn true_(&self) -> GcRef {
        self.true_
    }

    /// The immortal `false` value.
    #[inline]
    pub fn false_(&self) -> GcRef {
        self.false_
    }

    /// The immortal `Bool` for a Rust `bool`.
    #[inline]
    pub fn bool_(&self, b: bool) -> GcRef {
        if b {
            self.true_
        } else {
            self.false_
        }
    }

    /// The interned `Int` for `value`, or `None` when `value` is outside
    /// [`crate::small_int`]'s range and the caller must allocate.
    ///
    /// `Option` rather than a fallible-looking `GcRef`: "this value is not
    /// interned" is an ordinary answer, not a failure, and every caller has a
    /// perfectly good allocating path to fall back to.
    #[inline]
    #[must_use]
    pub fn small_int(&self, value: i64) -> Option<GcRef> {
        // `index_of` proved the bound; the table was built over the same range.
        small_int::index_of(value).map(|i| self.small_ints[i])
    }

    /// The base address of the interned-`Int` table, for
    /// `RuntimeContext.small_ints`.
    ///
    /// Generated code indexes this with a byte offset it computed at compile
    /// time from [`crate::small_int::SMALL_INT_MIN`] and
    /// [`crate::small_int::SMALL_INT_STRIDE`], which is sound only because the
    /// table is dense, in index order, and never reallocated after
    /// [`Immortals::new`] — none of which is checkable from the backend, so it
    /// is stated here where the table is built.
    #[inline]
    #[must_use]
    pub fn small_ints_ptr(&self) -> *const GcRef {
        self.small_ints.as_ptr()
    }
}

/// Read a `Bool` payload, recognizing the immortal singletons.
///
/// # Safety
/// `r` must be a `GcRef` whose descriptor is `BOOL`.
pub(crate) unsafe fn read_bool(r: GcRef) -> bool {
    // SAFETY: caller guarantees `r` is a Bool.
    let v: BoolPayload = unsafe { *r.payload::<BoolPayload>() };
    v != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immortals_are_distinct_and_stable() {
        let heap = Heap::new();
        let im = Immortals::new(&heap);
        // Each accessor returns the same singleton address on every call (stability)
        // and the three singletons are mutually distinct.
        let unit = im.unit().as_ptr();
        let true_ = im.true_().as_ptr();
        let false_ = im.false_().as_ptr();
        assert_eq!(im.unit().as_ptr(), unit);
        assert_eq!(im.true_().as_ptr(), true_);
        assert_eq!(im.false_().as_ptr(), false_);
        assert_ne!(true_, false_);
        assert_ne!(unit, true_);
        assert_ne!(unit, false_);
        // `bool_` dispatches to the cached singletons rather than allocating.
        assert_eq!(im.bool_(true).as_ptr(), true_);
        assert_eq!(im.bool_(false).as_ptr(), false_);

        // Nothing is in the live set — immortals are out-of-band. The interned
        // `Int` table is a thousand more of them and must not change this: a
        // table object in the sweepable set would be finalized and its storage
        // handed back out from under every reference generated code holds.
        assert_eq!(heap.stats().live_count, 0);
    }

    #[test]
    fn the_small_int_table_is_one_object_per_value_and_stable() {
        let heap = Heap::new();
        let im = Immortals::new(&heap);

        // Every value in range answers *the same* object on every call — the
        // whole point of the table, and what makes `Inst::ConstGc`'s two loads
        // equivalent to a call to `praxis_alloc_int`.
        for v in [SMALL_INT_MIN, -1, 0, 1, 42, SMALL_INT_MAX] {
            let first = im.small_int(v).expect("in range");
            assert_eq!(im.small_int(v).unwrap().as_ptr(), first.as_ptr());
            // SAFETY: the table holds `Int`s, minted with `INT_PAYLOAD`.
            assert_eq!(
                unsafe { *first.payload::<i64>() },
                v,
                "slot holds its own value"
            );
        }

        // The four boundary cases, at the table rather than at `index_of`: one
        // below the floor and one above the ceiling are *not* interned, and the
        // exact endpoints are.
        assert!(im.small_int(SMALL_INT_MIN - 1).is_none());
        assert!(im.small_int(SMALL_INT_MAX + 1).is_none());
        assert!(im.small_int(SMALL_INT_MIN).is_some());
        assert!(im.small_int(SMALL_INT_MAX).is_some());

        // Distinct values are distinct objects. Sharing an object across two
        // *values* would make `2 == 3`, which is the one way interning could
        // change an answer.
        let a = im.small_int(7).unwrap();
        let b = im.small_int(8).unwrap();
        assert_ne!(a.as_ptr(), b.as_ptr());

        // The raw pointer the context hands generated code addresses the same
        // objects `small_int` answers, in index order. This is the invariant
        // the backend's compile-time byte offset rests on.
        let base = im.small_ints_ptr();
        for (i, v) in (SMALL_INT_MIN..=SMALL_INT_MAX).enumerate() {
            // SAFETY: `i` is below `SMALL_INT_COUNT`, the table's length.
            let through_ptr = unsafe { *base.add(i) };
            assert_eq!(through_ptr.as_ptr(), im.small_int(v).unwrap().as_ptr());
        }
    }

    #[test]
    fn minting_the_immortals_costs_the_collector_nothing() {
        // RT-04: pacing measures the pressure a program puts on the collector,
        // and an object no collection can reclaim exerts none. The table is
        // ~40 KiB against a 64 KiB initial threshold, so charging it would put
        // every program two thirds of the way to its first collection before
        // `main` ran — and would silently move that point again the next time
        // anyone tuned the interned range.
        let heap = Heap::new();
        let _im = Immortals::new(&heap);
        assert_eq!(
            heap.bytes_since_collect(),
            0,
            "minting immortals must not charge the pacing counter"
        );
    }
}
