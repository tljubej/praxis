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
//! **Why not `Float` or `Text`.** `Float` fails the reflexivity argument that
//! carries `DynamicKey`'s fast path — `float_equals` is IEEE, so NaN ≠ NaN — and
//! interning it would make two separately-written NaN literals compare equal as
//! map keys. `Text` fails a different test: `TextPayload::Owned(OwnedText)` is
//! not `Copy`, and [`Heap::alloc_immortal`](crate::Heap) requires `Copy`
//! *because* an immortal is invisible to `Heap`'s `Drop` — an immortal `Text`
//! would leak its `Box<str>` at teardown (RT-02).
//!
//! `Char` passes every leg of the argument above, and [`crate::small_char`] is
//! the second interned scalar range (ADR-107): `char_equals` is a reflexive
//! `u32 ==`, a `CharPayload` is `Copy`, and ASCII is a bounded set. It is a
//! separate module rather than a second constant here because the two ranges
//! have different consumers — this one is read by three crates and by generated
//! code, and that one only by the runtime.
//!
//! **This module is the one statement of the range.** `praxis-mir` asks
//! [`index_of`] whether a literal is in range at compile time and `praxis-runtime`
//! asks it again at run time; the Cranelift backend derives the element offset
//! from the same [`SMALL_INT_MIN`]. A second spelling of the bounds anywhere
//! would let the compiler emit a table read for a value the table does not hold.

use crate::heap::InlineInternSite;
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
/// The cost of raising it is [`SMALL_INT_COUNT`] × 24 bytes of permanently
/// resident pages (a `GcHeader` is 16 bytes since ADR-109 and an `IntPayload`
/// is 8, and 24 is a rung of the size-class ladder exactly), so `-256..=1024`
/// is ~30 KiB. Anyone tuning this should re-run the suite rather than reason
/// about it: the table is free only while it stays in cache — and that 24 is
/// why ADR-109 paid here as well as on the allocation path, since the table is
/// resident for the whole process.
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

/// Everything the Cranelift backend bakes in to answer an in-range `Int`
/// inline, as one value (ADR-113).
///
/// **This is the only [`InlineInternSite`] in the workspace, and that is the
/// mechanism.** `InlineInternSite::new` is `pub(crate)`, so the backend cannot
/// assemble a site of its own; it can only name one this crate minted, and there
/// is one, here, beside the bounds it describes. A future inline `Char` probe
/// (handover 23's P-4a) mints its own next to [`crate::small_char`]'s range —
/// which is what stops it from being written as a copy of the `Int` arm reading
/// `small_chars` with `SMALL_INT_MIN`/`SMALL_INT_MAX`, a probe past the end of a
/// table whose only bound is its length.
///
/// The site also carries the pacing predicate's two offsets, which `new` fills
/// from `Heap` rather than taking as arguments: permission to read this table
/// inline and the obligation to test [`Heap::collection_is_due`] first are one
/// value, because they are one decision (ADR-113 decision 1).
///
/// This is the module's fourth reader and the third to take the bounds from
/// here rather than restate them — `praxis-mir` at compile time, the runtime's
/// `int_ref` at run time, and now generated code between the two.
pub const INLINE_INTERN_SITE: InlineInternSite = InlineInternSite::new(
    core::mem::offset_of!(crate::RuntimeContext, small_ints),
    SMALL_INT_MIN,
    SMALL_INT_MAX,
    SMALL_INT_STRIDE,
);

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

    /// The single unsigned compare generated code emits answers [`index_of`],
    /// for every `i64` that matters — and hands back the same index.
    ///
    /// Generated code cannot afford `index_of`'s two signed compares and two
    /// branches on the hot path, so it emits the two's-complement identity
    /// instead: `(v - MIN) as u64 <= (MAX - MIN) as u64` iff `MIN <= v <= MAX`,
    /// with the subtraction wrapping. The identity is exact and standard, and it
    /// is also the kind of thing that is *believed* rather than checked until a
    /// range change makes it false — so it is checked here, in the module that
    /// owns the range, against the function that is the range's one statement.
    ///
    /// The extremes of the type are the cases that fail if the wrap is not
    /// deliberate: `i64::MAX - (-256)` overflows a signed subtract, and the
    /// unsigned result must land *above* the span rather than wrapping back into
    /// it.
    #[test]
    fn the_unsigned_range_test_generated_code_emits_answers_index_of() {
        let span = INLINE_INTERN_SITE.span();
        assert_eq!(
            span,
            (SMALL_INT_MAX - SMALL_INT_MIN) as u64,
            "the site's span is the range's width"
        );

        let inline = |v: i64| {
            let biased = v.wrapping_sub(INLINE_INTERN_SITE.min()) as u64;
            (biased <= span).then_some(biased as usize)
        };

        for v in [
            i64::MIN,
            i64::MIN + 1,
            SMALL_INT_MIN - 1,
            SMALL_INT_MIN,
            SMALL_INT_MIN + 1,
            -1,
            0,
            1,
            SMALL_INT_MAX - 1,
            SMALL_INT_MAX,
            SMALL_INT_MAX + 1,
            i64::MAX - 1,
            i64::MAX,
        ] {
            assert_eq!(
                inline(v),
                index_of(v),
                "the inline range test and `index_of` disagree about {v}"
            );
        }
        // And densely across the range and a margin either side of it, because
        // the boundary list above cannot see an off-by-one in the middle.
        for v in (SMALL_INT_MIN - 64)..=(SMALL_INT_MAX + 64) {
            assert_eq!(inline(v), index_of(v), "at {v}");
        }
    }

    /// The site's immediates are the table's own, not a second spelling of them.
    ///
    /// `the_inline_check_proves_exactly_what_the_wrapper_would` for the
    /// allocation path: the fast path and `int_ref` must hold one notion of
    /// which slot holds which value, or a program reads the wrong `Int` — a
    /// wrong *answer*, silently, which is the only place in ADR-113 that failure
    /// mode appears.
    #[test]
    fn the_inline_sites_immediates_are_the_tables_own() {
        assert_eq!(INLINE_INTERN_SITE.min(), SMALL_INT_MIN);
        assert_eq!(
            INLINE_INTERN_SITE.span() as usize + 1,
            SMALL_INT_COUNT,
            "a span is one less than a count, and the table is dense"
        );
        assert_eq!(
            1usize << INLINE_INTERN_SITE.stride_shift(),
            SMALL_INT_STRIDE,
            "the shift generated code scales an index by is the table's stride"
        );
        assert_eq!(
            INLINE_INTERN_SITE.table_offset(),
            core::mem::offset_of!(crate::RuntimeContext, small_ints),
            "the site names the context field `Immortals::small_ints` is parked in"
        );
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
