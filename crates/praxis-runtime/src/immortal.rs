//! Immortal singleton objects (§4.3).
//!
//! §4.3 notes that `Unit` and `Bool` "may be immortal singleton objects."
//! [`Immortals`] pre-allocates `Unit`, `true`, and `false` once at runtime
//! startup. They are allocated on pages the sweep does not walk (ADR-103), so
//! the collector never reclaims them — they live for the entire run.
//!
//! Immortals are the natural return value for runtime wrappers that must return
//! *a* `GcRef` after a fault (Appendix B: "return a valid sentinel object such
//! as `Unit`").
//!
//! **Small `Int`s and ASCII `Char`s are immortal too.** The same §4.3 paragraph
//! reserves the right to "intern small integers", and a small `Int` satisfies
//! exactly the two conditions the `ImmortalWitness` below exists to enforce —
//! its payload is `Copy`, and there is a bounded set of them so each can be
//! minted once at startup. An ASCII `Char` satisfies both identically
//! (ADR-107). The ranges and the argument that sharing such an object is
//! unobservable are [`crate::small_int`]'s and [`crate::small_char`]'s; this
//! module's job is only that the tables are minted here, once, like every other
//! immortal.

use crate::heap::Heap;
use crate::scalars::{BoolPayload, BOOL_PAYLOAD, CHAR_PAYLOAD, INT_PAYLOAD, UNIT_PAYLOAD};
use crate::small_char::{self, SMALL_CHAR_COUNT, SMALL_CHAR_MAX};
use crate::small_int::{self, SMALL_INT_COUNT, SMALL_INT_MAX, SMALL_INT_MIN};
use crate::GcRef;

/// Proof that an [`Heap::alloc_immortal`] call comes from this module.
///
/// The inner field is private to `immortal.rs`, so [`Immortals::new`] is the
/// only place a witness can be constructed and therefore the only place an
/// immortal can be minted. Restricting it here is what keeps two properties
/// true: an immortal is never swept and never dropped, so its payload must be
/// `Copy` and it must be allocated exactly once — a wrapper that minted one per
/// call would consume storage no collection could ever reclaim.
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
    /// One `Char` per code point in [`crate::small_char`]'s range, indexed by
    /// [`small_char::index_of`].
    ///
    /// A `Box<[GcRef]>` for `small_ints`' reason: this table too is read through
    /// a raw pointer parked in `RuntimeContext.small_chars` — by generated code
    /// for a character literal and by the parser interpreter (`parser.rs`'s `Rt`
    /// holds nothing but a `*mut RuntimeContext`) — and that pointer must survive
    /// a move of the `Runtime` that owns the `Immortals`. An inline array would
    /// move with it; a boxed slice's elements do not.
    small_chars: Box<[GcRef]>,
}

impl Immortals {
    /// Allocate the immortal singletons on `heap`. They land on pages flagged
    /// immortal, which the sweep does not walk; the collector never reclaims
    /// them.
    ///
    /// They carry no pre-set mark colour: the page flag protects them
    /// permanently where a mark bit would protect them only until the next
    /// sweep. Sweep never clears an immortal page's `allocated` bit, whatever
    /// its mark bit says — which is what makes a mark phase that reaches an
    /// immortal through an aliasing root harmless.
    pub fn new(heap: &Heap) -> Self {
        // `alloc_immortal` uses the same low-level layout as every other
        // allocation, so the descriptors and accessors work on immortals
        // unchanged; only the page they come from differs.
        let unit = heap.alloc_immortal(UNIT_PAYLOAD, (), ImmortalWitness(()));
        let true_ = heap.alloc_immortal(BOOL_PAYLOAD, 1_u8, ImmortalWitness(()));
        let false_ = heap.alloc_immortal(BOOL_PAYLOAD, 0_u8, ImmortalWitness(()));
        // The interned `Int`s, in index order, so slot `i` holds
        // `SMALL_INT_MIN + i` and `small_int::index_of` is the only arithmetic
        // anyone does over this table. Minting them here — rather than lazily on
        // first use — is what keeps the `ImmortalWitness` seal meaningful: a
        // lazily-minted immortal is one step from a wrapper that mints one per
        // call and leaks, and it would also make `ctx.small_ints` hold a null the
        // backend would have to test.
        let small_ints: Box<[GcRef]> = (SMALL_INT_MIN..=SMALL_INT_MAX)
            .map(|v| heap.alloc_immortal(INT_PAYLOAD, v, ImmortalWitness(())))
            .collect();
        debug_assert_eq!(small_ints.len(), SMALL_INT_COUNT);
        // The interned ASCII `Char`s, on the same terms and appended after the
        // `Int`s rather than interleaved with them: a `Char` block rounds to the
        // same 24-byte rung, so these 128 land on the class-1 immortal pages the
        // `Int` table already opened, and `small_ints_ptr`'s density argument is
        // untouched by construction. Slot `i` holds code point `i` — the map is
        // the identity, which `small_char::index_of` is the single statement of.
        let small_chars: Box<[GcRef]> = (0..=SMALL_CHAR_MAX)
            .map(|code| heap.alloc_immortal(CHAR_PAYLOAD, code, ImmortalWitness(())))
            .collect();
        debug_assert_eq!(small_chars.len(), SMALL_CHAR_COUNT);
        Immortals {
            unit,
            true_,
            false_,
            small_ints,
            small_chars,
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

    /// The interned `Char` for `code`, or `None` when `code` is outside
    /// [`crate::small_char`]'s range and the caller must allocate.
    ///
    /// [`Immortals::small_int`]'s shape and its `Option` for the same reason:
    /// "this code point is not interned" is an ordinary answer, not a failure,
    /// and every caller has a perfectly good allocating path to fall back to.
    #[inline]
    #[must_use]
    pub fn small_char(&self, code: u32) -> Option<GcRef> {
        // `index_of` proved the bound; the table was built over the same range.
        small_char::index_of(code).map(|i| self.small_chars[i])
    }

    /// The base address of the interned-`Char` table, for
    /// `RuntimeContext.small_chars`.
    ///
    /// Two readers: `parser.rs`'s `Rt::alloc_char`, which reaches the runtime
    /// only through a `*mut RuntimeContext`, and the Cranelift lowering of
    /// `Inst::ConstGc { GcConst::Char }` for a character literal (ADR-141),
    /// which indexes from this base with a compile-time byte offset. Both are
    /// sound only because the table is dense, in index order, and never
    /// reallocated after [`Immortals::new`] — not checkable from either reader,
    /// so stated here where the table is built.
    #[inline]
    #[must_use]
    pub fn small_chars_ptr(&self) -> *const GcRef {
        self.small_chars.as_ptr()
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

        // Nothing is counted — immortals live on pages the sweep does not walk.
        // The interned `Int` table is a thousand more of them, and the `Char`
        // table a hundred and twenty-eight more again; neither may change this.
        // A table object on a sweepable page would be finalized and its storage
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
    fn the_small_char_table_is_one_object_per_code_point_and_stable() {
        let heap = Heap::new();
        let im = Immortals::new(&heap);

        // Every code point in range answers *the same* object on every call —
        // the whole point of the table, and what makes `t[i]` in a loop cost no
        // allocation at all.
        for code in [0_u32, 'a' as u32, '#' as u32, '9' as u32, SMALL_CHAR_MAX] {
            let first = im.small_char(code).expect("in range");
            assert_eq!(im.small_char(code).unwrap().as_ptr(), first.as_ptr());
            // SAFETY: the table holds `Char`s, minted with `CHAR_PAYLOAD`.
            assert_eq!(
                unsafe { *first.payload::<u32>() },
                code,
                "slot holds its own code point"
            );
        }

        // The boundary cases at the table rather than at `index_of`: the exact
        // ceiling is interned and one above it is not. There is no floor case —
        // the payload is unsigned and 0 is interned.
        assert!(im.small_char(SMALL_CHAR_MAX).is_some());
        assert!(im.small_char(SMALL_CHAR_MAX + 1).is_none());
        // `é`, the code point every `Char` test in this crate uses for "outside
        // the range", and the surrogate floor, which no widening may ever reach.
        assert!(im.small_char('é' as u32).is_none());
        assert!(im.small_char(0xD800).is_none());

        // Distinct code points are distinct objects. Sharing an object across
        // two *values* would make `'a' == 'b'`, which is the one way interning
        // could change an answer.
        let a = im.small_char('a' as u32).unwrap();
        let b = im.small_char('b' as u32).unwrap();
        assert_ne!(a.as_ptr(), b.as_ptr());

        // Every slot is its own object: 128 distinct addresses, not 128 handles
        // onto one. A `map` that reused a buffer would pass every check above.
        let mut seen: Vec<*const u8> = (0..=SMALL_CHAR_MAX)
            .map(|c| im.small_char(c).unwrap().as_ptr() as *const u8)
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), SMALL_CHAR_COUNT);

        // The raw pointer the context hands its readers addresses the same
        // objects `small_char` answers, in index order — the invariant both
        // `Rt::alloc_char`'s `add(i)` and the backend's compile-time element
        // offset rest on.
        let base = im.small_chars_ptr();
        for code in 0..=SMALL_CHAR_MAX {
            // SAFETY: `code` is below `SMALL_CHAR_COUNT`, the table's length.
            let through_ptr = unsafe { *base.add(code as usize) };
            assert_eq!(
                through_ptr.as_ptr(),
                im.small_char(code).unwrap().as_ptr(),
                "slot {code} through the raw pointer must be slot {code}"
            );
        }

        // And the two tables are disjoint: an interned `Char` is never an
        // interned `Int` with the same numeric value. They share a size class,
        // so a table built over the wrong descriptor would be the same shape and
        // the same width — and `praxis_char_load` would then read an `Int`.
        for code in 0..=SMALL_CHAR_MAX {
            let ch = im.small_char(code).unwrap();
            assert!(
                std::ptr::eq(ch.descriptor(), &crate::scalars::CHAR),
                "slot {code} must be a Char"
            );
            let int = im.small_int(i64::from(code)).unwrap();
            assert_ne!(ch.as_ptr(), int.as_ptr());
        }
    }

    #[test]
    fn minting_the_immortals_costs_the_collector_nothing() {
        // Pacing measures the pressure a program puts on the collector, and an
        // object no collection can reclaim exerts none. The `Int` table is
        // ~30 KiB against a 64 KiB initial threshold, so charging it would put
        // every program halfway to its first collection before `main` ran — and
        // would silently move that point again the next time anyone tuned an
        // interned range. The `Char` table is 3 KiB more of exactly the same
        // argument (ADR-107), and it must not charge either.
        let heap = Heap::new();
        let _im = Immortals::new(&heap);
        assert_eq!(
            heap.bytes_since_collect(),
            0,
            "minting immortals must not charge the pacing counter"
        );
    }
}
