//! The interned small-`Int` range (§4.3).
//!
//! §4.3's uniform object model is normative "even if later optimizations intern
//! small integers, use tagged pointers, or eliminate allocations through escape
//! analysis" — provided such an optimization "preserves reference and aliasing
//! semantics". For `Int` there are none to preserve, and the language already
//! ships the existence proof: `Unit` and `Bool` have been interned singletons
//! since ABI v10, so every `true` in every program is one object and always has
//! been ([`crate::immortal::Immortals`]).
//!
//! **Why sharing an `Int` is unobservable.** There is no identity operator in
//! the language — `praxis_hir`'s `BinOp` is arithmetic, comparison and the two
//! logical connectives, and nothing else. `==` on `Int` lowers to `Inst::IntCmp`
//! over extracted payloads; the structural fallback `praxis_struct_eq` has no
//! pointer fast path either. `Map`/`Set`/`Counter` keys go through
//! [`DynamicKey`](crate::dynamic_key::DynamicKey), whose `eq` *does* open with a
//! pointer comparison — but that is a fast path **for** structural equality, and
//! `int_equals` is reflexive, so sharing an object can only make it fire more
//! often, never change the answer. And an `Int` payload is never written after
//! its allocation: `Inst::StoreScalar` has no builder site and the backend's arm
//! for it is a documented no-op.
//!
//! **Why only `Int`.** `Float` fails the reflexivity argument that carries
//! `DynamicKey`'s fast path — `float_equals` is IEEE, so NaN ≠ NaN — and
//! interning it would make two separately-written NaN literals compare equal as
//! map keys. `Text` fails a different test: `TextPayload::Owned(Box<str>)` is
//! not `Copy`, and [`Heap::alloc_immortal`](crate::Heap) requires `Copy`
//! *because* an immortal is invisible to `Heap`'s `Drop` — an immortal `Text`
//! would leak its `Box<str>` at teardown (RT-02).
//!
//! **This module is the one statement of the range.** `praxis-mir` asks
//! [`index_of`] whether a literal is in range at compile time and `praxis-runtime`
//! asks it again at run time; the Cranelift backend derives the element offset
//! from the same [`SMALL_INT_MIN`]. A second spelling of the bounds anywhere
//! would let the compiler emit a table read for a value the table does not hold.

use crate::GcRef;

/// The lowest `Int` the runtime interns.
///
/// Negative values are worth a bucket because a Praxis program's negative
/// integers are overwhelmingly small: `-1` as a "not found" sentinel, the four
/// neighbour offsets a grid walk steps by, an accumulator's initial `-1`.
pub const SMALL_INT_MIN: i64 = -256;

/// The highest `Int` the runtime interns.
///
/// Chosen against the benchmark suite (`benchmarks/praxis/`): it covers every
/// literal the suite contains, the digits and small constants AoC-shaped input
/// parsing produces, and the loop counters and collection lengths of a program
/// whose working set is a few hundred elements. It is deliberately *not* sized
/// to cover a program's whole data — a `Counter` over a million-line input will
/// leave the range immediately, and that is the case the allocator is for.
///
/// The cost of raising it is [`SMALL_INT_COUNT`] × 32 bytes of permanently
/// resident arena (a `GcHeader` is 24 bytes and an `IntPayload` is 8), so
/// `-256..=1024` is ~40 KiB. Anyone tuning this should re-run the suite rather
/// than reason about it: the table is free only while it stays in cache.
pub const SMALL_INT_MAX: i64 = 1024;

/// How many `Int`s the table holds — the length of
/// [`Immortals::small_ints`](crate::Immortals) and the bound every index derived
/// from [`index_of`] respects.
pub const SMALL_INT_COUNT: usize = (SMALL_INT_MAX - SMALL_INT_MIN + 1) as usize;

/// The size of one table element, for the backend's element-offset arithmetic.
///
/// The Cranelift `Inst::ConstGc` lowering indexes the table with a compile-time
/// constant byte offset, so it needs the stride. Reading it from here rather
/// than writing `8` there means the stride and the array it indexes are one
/// statement — the same reason [`index_of`] is the only in-range test.
pub const SMALL_INT_STRIDE: usize = std::mem::size_of::<GcRef>();

/// `v`'s index in the interned table, or `None` if `v` is outside the range.
///
/// A `const fn` so `praxis-mir` can ask it while lowering a literal and the
/// runtime can ask it on the allocation path, and both get the same answer by
/// construction. Returning an `Option<usize>` rather than a bool-plus-arithmetic
/// pair is what keeps "in range" and "which slot" from being two decisions: a
/// caller that has the index has already proved the value was in range.
#[inline]
#[must_use]
pub const fn index_of(v: i64) -> Option<usize> {
    if v >= SMALL_INT_MIN && v <= SMALL_INT_MAX {
        // Cannot overflow or go negative: the branch above bounds `v` on both
        // sides, and the difference is at most `SMALL_INT_COUNT - 1`.
        Some((v - SMALL_INT_MIN) as usize)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_of_covers_exactly_the_declared_range() {
        // The four boundary cases, which are what a table read gets wrong: one
        // below the floor, the floor, the ceiling, and one above it.
        assert_eq!(index_of(SMALL_INT_MIN - 1), None);
        assert_eq!(index_of(SMALL_INT_MIN), Some(0));
        assert_eq!(index_of(SMALL_INT_MAX), Some(SMALL_INT_COUNT - 1));
        assert_eq!(index_of(SMALL_INT_MAX + 1), None);
        assert_eq!(index_of(0), Some((-SMALL_INT_MIN) as usize));
        // The extremes of the type, so a future range change cannot make the
        // subtraction in `index_of` overflow silently.
        assert_eq!(index_of(i64::MIN), None);
        assert_eq!(index_of(i64::MAX), None);
    }

    #[test]
    fn every_index_is_within_the_table() {
        // The bound `Immortals::small_int` relies on: `index_of` never answers
        // an index the table does not have.
        for v in SMALL_INT_MIN..=SMALL_INT_MAX {
            let i = index_of(v).expect("in range by construction");
            assert!(i < SMALL_INT_COUNT);
        }
    }

    #[test]
    fn the_range_is_dense_and_ordered() {
        // The table is indexed by `v - SMALL_INT_MIN`, so consecutive values
        // must map to consecutive slots — the property the backend's
        // compile-time byte offset depends on.
        let mut expected = 0;
        for v in SMALL_INT_MIN..=SMALL_INT_MAX {
            assert_eq!(index_of(v), Some(expected));
            expected += 1;
        }
        assert_eq!(expected, SMALL_INT_COUNT);
    }
}
