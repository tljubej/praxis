//! The interned ASCII `Char` range (§4.3, ADR-107).
//!
//! [`crate::small_int`]'s argument, applied to the second scalar that satisfies
//! it. §4.3's uniform object model is normative "even if later optimizations
//! intern small integers, use tagged pointers, or eliminate allocations through
//! escape analysis" — provided such an optimization "preserves reference and
//! aliasing semantics". A `Char` has none to preserve, for the same five reasons
//! an `Int` has none, each re-checked against `Char` rather than inherited:
//!
//! - **There is no identity operator.** `praxis_hir`'s `BinOp` is arithmetic,
//!   the six comparisons and the two logical connectives; `UnaryOp` is
//!   `Neg`/`Not`. There is no `is`, no `===`, no `ref_eq`.
//! - **`==` on `Char` compares payloads.** `compare_kind` answers
//!   `CompareVia::Scalar(ScalarKind::Char)`, which lowers to `Inst::IntCmp` over
//!   extracted code points. The structural fallback, `praxis_struct_eq`, has no
//!   pointer fast path either — it checks descriptor identity and dispatches
//!   `char_equals`.
//! - **Keyed collections are structural.** `Map`, `Set` and `Counter` are keyed
//!   by [`DynamicKey`](crate::dynamic_key::DynamicKey), whose `eq` *does* open
//!   with a pointer comparison — but that is a fast path **for** structural
//!   equality, and `char_equals` is a reflexive `u32 ==`, so sharing an object
//!   can only make it fire more often, never change the answer. (This is the leg
//!   `Float` fails and why `Float` is not interned: `float_equals` is IEEE, so
//!   NaN ≠ NaN.)
//! - **A `Char` payload is never written after allocation.** `Inst::StoreScalar`
//!   has no builder site and the backend's arm for it is a documented no-op.
//! - **Nothing hashes an address into anything language-visible.** `impl Hash
//!   for GcRef` exists and has no users.
//!
//! **Why the ceiling is 127 and not the BMP.** A `Char` block is
//! `{16 + 4 = 20, align 8}`, which rounds to the 24-byte rung of ADR-103's
//! ladder — the same rung an `Int` takes — so the table costs
//! [`SMALL_CHAR_COUNT`] × 24 = **3 KiB** of permanently resident arena. The BMP
//! is 63,488 scalar values, or ~1.45 MiB, on a change whose whole purpose is a
//! program's memory ceiling as much as its speed (handover 21 §3.6). And 3 KiB
//! buys the whole population: Praxis reads AoC-shaped input, every byte of which
//! a UTF-8 `Text` stores in one byte is exactly a code point ≤ 127, so a grid of
//! `#`/`.`, a line of digits and every letter of an English word are all inside
//! the range. A `Char` above it — `é`, a box-drawing glyph — is what the
//! allocator is for.
//!
//! Widening past `0xD7FF` would also stop being one decision: the surrogate
//! range `0xD800..=0xDFFF` holds no scalar values, so a table over the BMP would
//! carry either a hole its index arithmetic had to know about or 2,048 immortals
//! whose payloads violate `Char`'s own invariant. `0..=127` needs no second rule
//! beside [`index_of`], and `every_interned_code_point_is_a_valid_unicode_scalar`
//! is what makes a future widening fail here rather than in the heap.
//!
//! **There is no `SMALL_CHAR_MIN`,** because 0 is the floor of the payload type
//! and a constant zero only invites a `code - MIN` that cannot be checked.
//!
//! There **is** a [`SMALL_CHAR_STRIDE`], and this paragraph used to say there
//! was not. ADR-107 Decision 2's reasoning was exact and its premise expired:
//! `crate::small_int` had a stride because the Cranelift lowering of
//! `Inst::ConstGc` indexes that table with a compile-time byte offset, and
//! nothing indexed this one from generated code — the language had no character
//! literal, so there was no `GcConst::Char`. ADR-141 is that literal, and
//! `GcConst::Char` reads this table exactly the way `GcConst::SmallInt` reads
//! the other. The stride is declared here, beside the array it measures, rather
//! than written as an `8` in the backend, for [`index_of`]'s reason: a table and
//! the arithmetic that walks it are one statement or they are two answers.
//!
//! It stays un-re-exported from the crate root, where `small_int`'s constants
//! are re-exported: the backend reaches it as
//! `praxis_runtime::small_char::SMALL_CHAR_STRIDE`, which names the table it
//! belongs to at the use site. `small_int`'s re-export predates this module and
//! is not worth reversing; a second one is not worth adding.

/// The highest code point the runtime interns: the last ASCII scalar.
///
/// See the module documentation for the range argument. The cost of raising it
/// is [`SMALL_CHAR_COUNT`] × 24 bytes of permanently resident arena (a
/// `GcHeader` is 16 bytes since ADR-109, a `CharPayload` is 4, and the 24-byte
/// rung of ADR-103's ladder is the smallest that holds them), so `0..=127` is
/// 3 KiB and the BMP would be ~1.45 MiB.
pub const SMALL_CHAR_MAX: u32 = 127;

/// How many `Char`s the table holds — the length of
/// [`Immortals::small_chars`](crate::Immortals) and the bound every index
/// derived from [`index_of`] respects.
pub const SMALL_CHAR_COUNT: usize = SMALL_CHAR_MAX as usize + 1;

/// The size of one table element, for the backend's element-offset arithmetic.
///
/// The Cranelift `Inst::ConstGc { GcConst::Char }` lowering indexes the table
/// with a compile-time constant byte offset, so it needs the stride — the same
/// two loads a small `Int` literal costs (ADR-100, ADR-141).
pub const SMALL_CHAR_STRIDE: usize = std::mem::size_of::<crate::GcRef>();

/// `code`'s index in the interned table, or `None` if `code` is outside the
/// range and the caller must allocate.
///
/// A `const fn` and the *only* in-range test, for [`crate::small_int`]'s reason:
/// a caller that has the index has already proved the value was in range, so
/// "is it interned" and "which slot" cannot drift apart into two decisions.
///
/// The identity map is not an accident worth hiding behind arithmetic — the
/// table is built by `(0..=SMALL_CHAR_MAX)` in that order, and
/// `the_range_is_dense_and_ordered` pins the two together.
#[inline]
#[must_use]
pub const fn index_of(code: u32) -> Option<usize> {
    if code <= SMALL_CHAR_MAX {
        Some(code as usize)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_of_covers_exactly_the_declared_range() {
        // The boundary cases, which are what a table read gets wrong. There is
        // no "one below the floor": the payload is unsigned and 0 is the floor.
        assert_eq!(index_of(0), Some(0));
        assert_eq!(index_of(SMALL_CHAR_MAX), Some(SMALL_CHAR_COUNT - 1));
        assert_eq!(index_of(SMALL_CHAR_MAX + 1), None);
        // The extreme of the type, so a future range change cannot make the
        // conversion in `index_of` overflow silently.
        assert_eq!(index_of(u32::MAX), None);
        // One above the largest scalar value there is, which must be outside the
        // range for the same reason as anything else above the ceiling — the
        // range test is what refuses it, not `char::from_u32`.
        assert_eq!(index_of(0x11_0000), None);
    }

    #[test]
    fn every_index_is_within_the_table() {
        // The bound `Immortals::small_char` relies on: `index_of` never answers
        // an index the table does not have.
        for code in 0..=SMALL_CHAR_MAX {
            let i = index_of(code).expect("in range by construction");
            assert!(i < SMALL_CHAR_COUNT);
        }
    }

    #[test]
    fn the_range_is_dense_and_ordered() {
        // The table is built by iterating `0..=SMALL_CHAR_MAX` in order, so
        // consecutive code points must map to consecutive slots. `Immortals::new`
        // and every reader through `small_chars_ptr` rest on exactly this.
        let mut expected = 0;
        for code in 0..=SMALL_CHAR_MAX {
            assert_eq!(index_of(code), Some(expected));
            expected += 1;
        }
        assert_eq!(expected, SMALL_CHAR_COUNT);
    }

    /// ADR-107's cost, pinned rather than asserted in prose.
    ///
    /// "3 KiB" is a derivation — a `Char` block is `{16 + 4, align 8}`, which
    /// rounds to the 24-byte rung of ADR-103's ladder — and this codebase pins
    /// derivations. If the header moves, the ladder is re-granulated, or the
    /// range is widened, the ADR's table stops being true and this is where that
    /// is discovered. It has already earned its keep once: ADR-109 took the
    /// header from 24 bytes to 16 the same day this was written, and this test
    /// is what caught the stale figure.
    #[test]
    fn the_table_costs_three_kibibytes_of_permanently_resident_arena() {
        let (_, block) = crate::heap::BlockLayout::of(&crate::scalars::CHAR);
        let class = crate::page::SizeClass::of(block).expect("a Char is on the ladder");
        assert_eq!(
            class.block_size(),
            24,
            "ADR-103's 24-byte rung, as for `Int` — 16-byte header since ADR-109"
        );
        assert_eq!(
            SMALL_CHAR_COUNT * class.block_size(),
            3 * 1024,
            "ADR-107 Decision 1's table says 3 KiB for ASCII"
        );
    }

    /// The property [`crate::small_int`] has no analogue of, and the one that is
    /// load-bearing: every code point in the range is a valid Unicode scalar, so
    /// `Immortals::new` mints 128 objects that satisfy `Char`'s own payload
    /// invariant without checking each one.
    ///
    /// It is also the guard rail on the range. Widening `SMALL_CHAR_MAX` past
    /// `0xD7FF` walks into the surrogates, which are not scalar values; this
    /// fails then, rather than putting an invalid payload inside an immortal no
    /// collection can ever reclaim.
    #[test]
    fn every_interned_code_point_is_a_valid_unicode_scalar() {
        for code in 0..=SMALL_CHAR_MAX {
            assert!(
                crate::scalars::is_valid_char(code),
                "{code:#x} is interned but is not a Unicode scalar value"
            );
        }
    }
}
