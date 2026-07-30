//! `Range` (§4.11, ADR-059).
//!
//! `a..b` is the integers from `a` up to but not including `b`; `a..=b` includes
//! `b`. Both forms build the *same* payload — an inclusiveness flag would be a
//! second way to spell one set of values, so the constructor normalizes `..=`
//! into its half-open equivalent and the payload holds only what it iterates.
//!
//! A descending range is **empty**, not a countdown. `for i in 5..0` runs zero
//! times, matching Python and Rust; a range that silently reversed direction
//! would compute a different loop than the one that was written half the time it
//! appeared. `RangeVal::new` is where that is decided, once: an `end` below
//! `start` is stored *as* `start`, so `len` is a subtraction and cannot be
//! negative.
//!
//! The payload is two plain `i64`s. Nothing is owned, nothing is traced, and the
//! bounds are immutable once built — which is what makes a `Range` hashable and
//! usable as a `Map` key (ADR-057 D4: the rule is mutability).

use std::fmt;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, Payload, Tracer, TypeDescriptor};

/// The `Range` payload: a half-open `[start, end)` interval over `Int`.
///
/// The invariant `end >= start` is established by [`RangeVal::new`] and there is
/// no other constructor and no mutator, so an "inverted" range — one whose
/// length would be negative — is unrepresentable.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RangeVal {
    start: i64,
    /// Exclusive. Always `>= start`.
    end: i64,
}

impl RangeVal {
    /// The half-open range `start..end`, normalizing a descending range to the
    /// empty range at `start`.
    #[must_use]
    pub const fn new(start: i64, end: i64) -> RangeVal {
        RangeVal {
            start,
            end: if end < start { start } else { end },
        }
    }

    /// The inclusive range `start..=end`.
    ///
    /// `..=Int::MAX` is the one input whose half-open equivalent does not exist:
    /// its exclusive end is `2^63`. It is **not** a fault — the range itself is
    /// perfectly well defined — so it saturates, which loses nothing: the
    /// element `Int::MAX` is still the last one, because there is no `Int` above
    /// it to have excluded.
    #[must_use]
    pub const fn new_inclusive(start: i64, end: i64) -> RangeVal {
        match end.checked_add(1) {
            Some(exclusive) => RangeVal::new(start, exclusive),
            None => RangeVal {
                start,
                end: i64::MAX,
            },
        }
    }

    /// The lower bound (inclusive).
    #[must_use]
    pub const fn start(&self) -> i64 {
        self.start
    }

    /// The upper bound (exclusive).
    #[must_use]
    pub const fn end(&self) -> i64 {
        self.end
    }

    /// How many integers the range contains.
    ///
    /// `end - start` in `i128`: the difference of two `i64`s does not fit an
    /// `i64` (`0..Int::MAX` is fine, but `Int::MIN..Int::MAX` is `2^64 - 1`), and
    /// a wrapping subtraction here would report a *negative* length for the
    /// widest ranges — a `for` loop that ran zero times over every integer.
    #[must_use]
    pub const fn len(&self) -> i128 {
        self.end as i128 - self.start as i128
    }

    /// Whether the range contains no integers.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end == self.start
    }

    /// The `index`-th integer, or `None` if `index` is outside the range.
    ///
    /// The addition is done in `i128` and checked on the way back, so a
    /// nonsensical index cannot wrap into a value the range does not contain.
    #[must_use]
    pub fn get(&self, index: i64) -> Option<i64> {
        if index < 0 || i128::from(index) >= self.len() {
            return None;
        }
        i64::try_from(self.start as i128 + i128::from(index)).ok()
    }
}

unsafe fn range_trace(_payload: *mut u8, _tracer: &mut dyn Tracer) {
    // A Range holds two integers and no references; nothing to trace.
}

unsafe fn range_drop(_payload: *mut u8) {
    // `RangeVal` is `Copy` and owns no heap bytes; nothing to release.
}

/// Render a range the way it was written. `..=` is *not* recovered: the payload
/// is normalized, so `1..=4` and `1..5` are the same range and print the same —
/// which is the point of normalizing.
unsafe fn range_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized RangeVal.
    let r = unsafe { &*(payload as *const RangeVal) };
    let _ = write!(out, "{}..{}", r.start, r.end);
}

unsafe fn range_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized RangeVals.
    let ra = unsafe { &*(a as *const RangeVal) };
    let rb = unsafe { &*(b as *const RangeVal) };
    ra == rb
}

unsafe fn range_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized RangeVal.
    let r = unsafe { &*(payload as *const RangeVal) };
    hasher.write_bytes(&r.start.to_le_bytes());
    hasher.write_bytes(&r.end.to_le_bytes());
}

/// Descriptor for `Range` (§4.11). Equatable and hashable over its two bounds;
/// not orderable, because only the five scalars with a `compare` callback are
/// (ADR-045).
pub static RANGE: TypeDescriptor = TypeDescriptor::builtin::<RangeVal>(
    BuiltinTypeId::Range,
    "Range",
    range_trace,
    range_drop,
    range_format,
    Some(range_equals),
    Some(range_hash),
    None,
);

/// `Range`'s payload handle (REP-02): the two-`i64` value, not a scalar.
pub static RANGE_PAYLOAD: Payload<RangeVal> = Payload::new(&RANGE);

#[cfg(test)]
mod tests {
    use super::*;

    /// A descending range is empty, not a countdown (ADR-059 D3). The decision
    /// lives in the constructor, so there is no path that produces a range whose
    /// length is negative — which is what a `for` loop reads.
    #[test]
    fn a_descending_range_is_empty_and_cannot_be_built_inverted() {
        let down = RangeVal::new(5, 0);
        assert!(down.is_empty());
        assert_eq!(down.len(), 0);
        assert_eq!(down.get(0), None);
        // The normalization is to `start`, so the range is empty *at* 5 rather
        // than being silently widened.
        assert_eq!(down.start(), 5);
        assert_eq!(down.end(), 5);

        // …and every range's length is non-negative, including the widest one
        // that exists, whose `i64` subtraction would wrap.
        assert_eq!(RangeVal::new(i64::MIN, i64::MAX).len(), u64::MAX as i128);
        assert!(RangeVal::new(i64::MAX, i64::MIN).is_empty());
    }

    /// `..` excludes its end and `..=` includes it — the only difference between
    /// the two forms, and it is resolved at construction so nothing downstream
    /// has to remember which was written.
    #[test]
    fn inclusive_and_half_open_differ_by_exactly_one_element() {
        let half = RangeVal::new(1, 5);
        let incl = RangeVal::new_inclusive(1, 5);
        assert_eq!(half.len(), 4);
        assert_eq!(incl.len(), 5);
        assert_eq!(half.get(3), Some(4));
        assert_eq!(half.get(4), None);
        assert_eq!(incl.get(4), Some(5));
        assert_eq!(incl.get(5), None);
        // `1..=4` and `1..5` are the same range, which is what normalizing buys.
        assert_eq!(RangeVal::new_inclusive(1, 4), half);
        // An empty inclusive range is one whose end is below its start.
        assert!(RangeVal::new_inclusive(5, 4).is_empty());
        assert_eq!(RangeVal::new_inclusive(5, 5).len(), 1);
    }

    /// `..=Int::MAX` has no exclusive end inside `Int`. It saturates rather than
    /// faulting, and the saturation loses no element: there is no `Int` above
    /// `Int::MAX` that the range would otherwise have excluded.
    #[test]
    fn an_inclusive_range_to_the_last_int_keeps_that_int() {
        let r = RangeVal::new_inclusive(i64::MAX - 2, i64::MAX);
        assert_eq!(r.end(), i64::MAX);
        assert_eq!(r.len(), 2);
        assert_eq!(r.get(0), Some(i64::MAX - 2));
        assert_eq!(r.get(1), Some(i64::MAX - 1));
        // The last element is unreachable by index — the one value the
        // saturation costs, and it costs it at the very top of the range rather
        // than reporting a fault for a range the program legitimately wrote.
        assert_eq!(r.get(2), None);
    }

    /// An index outside the range has no element, and the arithmetic that
    /// answers so cannot wrap.
    #[test]
    fn an_out_of_range_index_has_no_element() {
        let r = RangeVal::new(-3, 3);
        assert_eq!(r.len(), 6);
        assert_eq!(r.get(0), Some(-3));
        assert_eq!(r.get(5), Some(2));
        assert_eq!(r.get(6), None);
        assert_eq!(r.get(-1), None);
        assert_eq!(r.get(i64::MAX), None);
    }

    /// A `Range` is equatable and hashable over its bounds, which is what makes
    /// it a legal `Map` key: the bounds cannot change after it is stored
    /// (ADR-057 D4 — the rule is mutability, not container-ness).
    #[test]
    fn range_descriptor_reports_its_capabilities() {
        assert!(RANGE.is_equatable() && RANGE.is_hashable());
        assert_eq!(RANGE.name, "Range");
        // Not orderable: only the five scalars with a `compare` are (ADR-045).
        assert!(!RANGE.is_orderable());
    }
}
