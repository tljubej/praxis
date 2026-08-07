//! `BitSet` (M8-WS5, §6.1).
//!
//! A compact set of non-negative integers, backed by a [`ReprCVec<u64>`] of
//! words. Occupancy is bit `i` of word `i / 64`. Nullary in user syntax
//! (`BitSet`, no type arg); elements are always `Int`. Iterable (yields `Int`).
//!
//! `BitSet` is its own GC payload: the words are a `Drop` vector, so the
//! descriptor's `drop_value` releases them on sweep. Equality/hash are
//! structural (two bitsets are equal iff they hold the same bits).
//!
//! The container is a [`ReprCVec`] and not a `std::Vec` because generated code
//! reads its two leading words inline for `bs.contains(x)` (ADR-118 part 2);
//! [`INLINE_BITSET_SITE`] is the one value that says where they are.

use std::fmt::Write as _;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, FormatSink, Tracer, TypeDescriptor};
use crate::repr_c_vec::ReprCVec;

/// A value a `BitSet` can actually hold: non-negative, and small enough that
/// the word vector backing it stays a real allocation.
///
/// This is the only route from a user-supplied `Int` to a bit position (RT-07).
/// `insert` used to take a bare `usize` cast from an `i64`, so `bs.insert(-1)`
/// was silently dropped and `bs.insert(10^18)` asked for a 10^16-word `Vec` —
/// an OOM abort *inside* `extern "C"`. Neither is expressible now: the range
/// check happens where the value enters, once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BitIndex(usize);

impl BitIndex {
    /// The largest member a `BitSet` accepts: 2^32 - 1, whose word vector is
    /// 512 MiB. A cap rather than "anything a `usize` holds", for the same
    /// reason [`GridExtent::MAX_CELLS`](crate::collections::GridExtent::MAX_CELLS)
    /// is one — a `Vec` request the host cannot serve is not an error the
    /// process survives.
    pub const MAX: i64 = u32::MAX as i64;

    /// The most words a `BitSet` can ever hold, which is
    /// [`MAX`](Self::MAX)`/64 + 1`.
    ///
    /// **This bound is what lets generated code omit the range test**
    /// (ADR-118 part 2). `bs.contains(x)` inline is `word = (x as u64) >> 6;
    /// if word >= words.len() { false } else { … }`, with no separate check
    /// that `x` is a member `BitIndex::new` would accept — because for every
    /// `i64` outside `0..=MAX` the *unsigned* shift already lands at or above
    /// this number, and the word count never reaches it:
    ///
    /// * `x < 0` → `x as u64 >= 2^63` → `word >= 2^57`;
    /// * `x > MAX` → `x as u64 >= 2^32` → `word >= 2^26 == MAX_WORDS`;
    /// * and [`BitSetPayload::insert`] resizes to `word + 1` for a `BitIndex`,
    ///   so `words.len() <= MAX_WORDS` always.
    ///
    /// So `word >= words.len()` subsumes the range test, exactly, for every
    /// value of the type. `the_word_probe_generated_code_emits_answers_contains`
    /// is that claim checked against `contains` rather than argued, in the
    /// module that owns the range — `small_int`'s
    /// `the_unsigned_range_test_generated_code_emits_answers_index_of` in its
    /// second place.
    pub const MAX_WORDS: usize = (Self::MAX as usize) / 64 + 1;

    /// The bit `value` names, or `None` if it is negative or above
    /// [`MAX`](Self::MAX).
    #[must_use]
    pub const fn new(value: i64) -> Option<BitIndex> {
        if value < 0 || value > Self::MAX {
            return None;
        }
        Some(BitIndex(value as usize))
    }

    /// The word holding this bit, and its position within that word.
    #[inline]
    const fn word_and_bit(self) -> (usize, usize) {
        (self.0 / 64, self.0 % 64)
    }
}

/// The `BitSet` payload: a growable vector of 64-bit words. Bit `i` is in word
/// `i / 64` at position `i % 64`. The field is `Drop`.
#[repr(C)]
pub struct BitSetPayload {
    /// The words. Trailing zero words may be present; equality/hash trim them.
    ///
    /// A [`ReprCVec`](crate::ReprCVec) rather than a `std::Vec` for ADR-118's
    /// reason, now applied to the second payload: generated code reads the
    /// words pointer and the word count inline for `bs.contains(x)`, and
    /// `std::Vec` is `#[repr(Rust)]` — its three words live inside a private
    /// `RawVec` in no guaranteed order. The container holds the same three
    /// words the `Vec` held and `Vec` still does every allocation.
    ///
    /// **W4a migrated `VecPayload` and not this one**, on the reading that
    /// `praxis_bitset_contains` was "the cleanest" of W4b's three primitives
    /// (handover 26 §4). It is the cleanest in every other respect — `Pure`, no
    /// allocation, no fault, no pacing obligation — and it was the one with no
    /// legal number to read.
    pub words: ReprCVec<u64>,
}

// The offsets generated code bakes for `bs.contains(x)` (ADR-118 part 2), in
// the tree rather than in a sentence. `words` is the only field, so its
// element pointer is at payload+0 and its length at payload+8.
const _: () = assert!(std::mem::offset_of!(BitSetPayload, words) == 0);
// The payload is one container and nothing else, so the block's size class and
// the pacer's page density are exactly what they were before the migration.
const _: () = assert!(std::mem::size_of::<BitSetPayload>() == 24);
const _: () = assert!(std::mem::align_of::<BitSetPayload>() == 8);

/// The one site generated code may read a `BitSet`'s words through (ADR-118
/// part 2), minted beside the payload for [`INLINE_VEC_SITE`]'s reason.
///
/// [`INLINE_VEC_SITE`]: crate::collections::INLINE_VEC_SITE
#[cfg(not(feature = "std-vec-payload"))]
pub const INLINE_BITSET_SITE: crate::repr_c_vec::InlineSliceSite =
    crate::repr_c_vec::InlineSliceSite::new(
        BuiltinTypeId::BitSet,
        std::mem::align_of::<BitSetPayload>(),
        std::mem::offset_of!(BitSetPayload, words),
        std::mem::size_of::<u64>(),
    );

impl BitSetPayload {
    /// True iff bit `i` is set.
    pub(crate) fn contains(&self, i: BitIndex) -> bool {
        let (word, bit) = i.word_and_bit();
        match self.words.get(word) {
            Some(w) => (w >> bit) & 1 == 1,
            None => false,
        }
    }

    /// Set bit `i`, growing the words vector as needed. The growth is bounded
    /// by [`BitIndex::MAX`], which is what makes this a real allocation.
    ///
    /// **The only thing in the tree that grows `words`**, which is what makes
    /// [`BitIndex::MAX_WORDS`] a bound rather than a hope — and generated code
    /// leans on that bound to skip a range test. The assertion says so at the
    /// one site that could falsify it.
    pub(crate) fn insert(&mut self, i: BitIndex) {
        let (word, bit) = i.word_and_bit();
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
        debug_assert!(
            self.words.len() <= BitIndex::MAX_WORDS,
            "a BitSet's word count is bounded by BitIndex::MAX, and generated \
             code reads that bound as licence to skip the range test"
        );
        self.words[word] |= 1u64 << bit;
    }

    /// Clear bit `i` (no-op if not set or beyond the current words).
    pub(crate) fn remove(&mut self, i: BitIndex) {
        let (word, bit) = i.word_and_bit();
        if let Some(w) = self.words.get_mut(word) {
            *w &= !(1u64 << bit);
        }
    }

    /// The number of set bits (popcount across all words).
    pub(crate) fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Every set bit's value, **ascending**.
    ///
    /// A `BitSet` is the one keyed collection whose deterministic order needs no
    /// decision: the bits *are* the members and their word order is their
    /// numeric order. `for i in b` iterates a snapshot of this (REP-15,
    /// ADR-066), and it is the order [`bitset_format`] already prints.
    pub(crate) fn members(&self) -> impl Iterator<Item = i64> + '_ {
        self.words.iter().enumerate().flat_map(|(word_idx, &word)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1; // clear the lowest set bit
                Some((word_idx * 64 + bit) as i64)
            })
        })
    }
}

unsafe fn bitset_trace(_payload: *mut u8, _tracer: &mut dyn Tracer) {
    // BitSet holds no GcRefs (bits are not objects); nothing to trace.
}

unsafe fn bitset_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized BitSetPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut BitSetPayload) };
}

unsafe fn bitset_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an initialized BitSetPayload.
    let p = unsafe { &*(payload as *const BitSetPayload) };
    let _ = out.write_str("{");
    for (i, value) in p.members().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let _ = write!(out, "{value}");
    }
    let _ = out.write_str("}");
}

unsafe fn bitset_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized BitSetPayloads.
    let pa = unsafe { &*(a as *const BitSetPayload) };
    let pb = unsafe { &*(b as *const BitSetPayload) };
    // Compare up to the longer vector's length, treating missing words as zero.
    let len = pa.words.len().max(pb.words.len());
    for i in 0..len {
        let wa = pa.words.get(i).copied().unwrap_or(0);
        let wb = pb.words.get(i).copied().unwrap_or(0);
        if wa != wb {
            return false;
        }
    }
    true
}

unsafe fn bitset_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized BitSetPayload.
    let p = unsafe { &*(payload as *const BitSetPayload) };
    // Order-independent: hash each set bit's value and XOR. This is more
    // expensive than hashing words but is robust to trailing-zero-word
    // differences (two equal bitsets with different word-vector lengths).
    let mut acc: u64 = 0;
    for (word_idx, &word) in p.words.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            let value = (word_idx * 64 + bit) as u64;
            let mut h = crate::descriptor::StructHasher::new();
            h.write_bytes(&value.to_le_bytes());
            acc ^= h.finish();
            bits &= bits - 1;
        }
    }
    hasher.write_bytes(&acc.to_le_bytes());
}

/// Descriptor for `BitSet` (§6.1, TypeId 19). Equatable and hashable (structural
/// over the set of bits), so a BitSet can be a value in another collection.
pub static BITSET: TypeDescriptor = TypeDescriptor::builtin::<BitSetPayload>(
    BuiltinTypeId::BitSet,
    "BitSet",
    bitset_trace,
    bitset_drop,
    bitset_format,
    Some(bitset_equals),
    Some(bitset_hash),
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(bitset_owned_bytes);

/// The heap bytes `BitSet` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `BitSetPayload`.
impl BitSetPayload {
    /// The bytes this payload owns outside its GC block — the buffer, not the
    /// spine's three words.
    ///
    /// **One statement of the size, with two readers** (ADR-121). The
    /// descriptor's `owned_bytes` callback charges it once at construction;
    /// the ABI wrapper that can *grow* this collection reads it either side of
    /// the mutation and charges the delta, so the pacer sees a buffer that
    /// doubled. Writing the capacity arithmetic at the growth site instead
    /// would be a second spelling of this line, and the two would drift the
    /// first time an element type changed width.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.words.capacity() * std::mem::size_of::<u64>()
    }
}

unsafe fn bitset_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized BitSetPayload.
    let p = unsafe { &*(payload as *const BitSetPayload) };
    p.owned_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset_descriptor_reports_capabilities() {
        assert!(BITSET.is_equatable() && BITSET.is_hashable());
        assert_eq!(BITSET.name, "BitSet");
    }

    /// Shorthand: a bit that is in range by construction.
    fn bit(i: i64) -> BitIndex {
        BitIndex::new(i).expect("in-range test bit")
    }

    /// **REP-15.** A `BitSet`'s members come out ascending, across word
    /// boundaries — the order `for i in b` walks and the order it prints.
    #[test]
    fn a_bitsets_members_come_out_ascending() {
        let mut b = BitSetPayload {
            words: ReprCVec::new(),
        };
        // Deliberately inserted out of order and spanning three words, so a
        // per-word or per-insertion order is a different sequence.
        for i in [130, 5, 64, 0, 63] {
            b.insert(bit(i));
        }
        assert_eq!(b.members().collect::<Vec<_>>(), vec![0, 5, 63, 64, 130]);
        // The same rule the formatter uses, so `out(b)` and a `for` agree.
        let mut rendered = String::new();
        // SAFETY: `b` is an initialized BitSetPayload.
        unsafe {
            bitset_format(
                (&b as *const BitSetPayload).cast::<u8>(),
                &mut crate::FormatSink::display(&mut rendered),
            )
        };
        assert_eq!(rendered, "{0, 5, 63, 64, 130}");
        // An empty one yields nothing rather than one member or forever.
        let empty = BitSetPayload {
            words: ReprCVec::new(),
        };
        assert_eq!(empty.members().count(), 0);
        // …and so does one whose words are present but all zero, which is the
        // shape `remove` leaves behind.
        let cleared = BitSetPayload {
            words: ReprCVec::from_vec(vec![0, 0]),
        };
        assert_eq!(cleared.members().count(), 0);
    }

    #[test]
    fn bitset_insert_contains_count() {
        let mut b = BitSetPayload {
            words: ReprCVec::new(),
        };
        b.insert(bit(0));
        b.insert(bit(63));
        b.insert(bit(64));
        b.insert(bit(1000));
        assert!(b.contains(bit(0)));
        assert!(b.contains(bit(63)));
        assert!(b.contains(bit(64)));
        assert!(b.contains(bit(1000)));
        assert!(!b.contains(bit(1)));
        assert!(!b.contains(bit(65)));
        assert_eq!(b.count(), 4);
    }

    #[test]
    fn bitset_remove_clears_bit() {
        let mut b = BitSetPayload {
            words: ReprCVec::new(),
        };
        b.insert(bit(5));
        assert!(b.contains(bit(5)));
        b.remove(bit(5));
        assert!(!b.contains(bit(5)));
        // Removing an unset bit is a no-op.
        b.remove(bit(999));
    }

    /// **ADR-118 part 2's load-bearing arithmetic**, checked against
    /// [`BitSetPayload::contains`] rather than argued.
    ///
    /// This is the sequence the backend emits, transcribed: an unsigned shift,
    /// a compare against the word count, a shift and a mask — and **no range
    /// test**, because [`BitIndex::MAX_WORDS`] makes the compare subsume it.
    /// The claim is exact for every `i64`, which is precisely why it is the
    /// kind of thing that gets believed rather than checked: the interesting
    /// values are the ones no program produces on purpose.
    ///
    /// `small_int`'s `the_unsigned_range_test_generated_code_emits_answers_index_of`
    /// is the same test for ADR-113's range identity, and this is written to
    /// its shape: both extremes of the type, every boundary, and a dense sweep.
    #[test]
    fn the_word_probe_generated_code_emits_answers_contains() {
        /// Exactly what the emitted fast path computes, in the same order.
        fn probe(words: &[u64], member: i64) -> bool {
            let word = (member as u64) >> 6;
            if word >= words.len() as u64 {
                return false;
            }
            let w = words[word as usize];
            (w >> ((member as u64) & 63)) & 1 == 1
        }

        let mut b = BitSetPayload {
            words: ReprCVec::new(),
        };
        for i in [0, 1, 63, 64, 65, 127, 128, 1000, 4095, 4096] {
            b.insert(bit(i));
        }

        // The extremes and the boundaries, where the two forms could disagree
        // and where no program would look.
        let corners = [
            i64::MIN,
            i64::MIN + 1,
            -4096,
            -65,
            -64,
            -1,
            0,
            1,
            63,
            64,
            BitIndex::MAX - 1,
            BitIndex::MAX,
            BitIndex::MAX + 1,
            i64::MAX - 1,
            i64::MAX,
        ];
        for member in corners {
            let expected = BitIndex::new(member).is_some_and(|i| b.contains(i));
            assert_eq!(
                probe(&b.words, member),
                expected,
                "the word probe and `contains` disagree at {member}"
            );
        }
        // …and densely across the populated range plus a margin on both sides.
        for member in -64_i64..4200 {
            let expected = BitIndex::new(member).is_some_and(|i| b.contains(i));
            assert_eq!(probe(&b.words, member), expected, "at {member}");
        }

        // The bound the omitted range test rests on, stated as a number: a
        // word count can never reach the index a non-member shifts down to.
        assert_eq!(BitIndex::MAX_WORDS, 1 << 26);
        assert!((BitIndex::MAX as u64) >> 6 < BitIndex::MAX_WORDS as u64);
        assert!(((BitIndex::MAX + 1) as u64) >> 6 >= BitIndex::MAX_WORDS as u64);
        assert!((-1_i64 as u64) >> 6 >= BitIndex::MAX_WORDS as u64);
        assert!((i64::MIN as u64) >> 6 >= BitIndex::MAX_WORDS as u64);
    }

    /// RT-07. A member the set cannot hold has no `BitIndex`, so there is no
    /// value to hand `insert` — the resize toward a huge word count is
    /// unwritable rather than merely unreached.
    #[test]
    fn a_bit_outside_the_representable_range_has_no_index() {
        assert!(BitIndex::new(-1).is_none(), "negative");
        assert!(BitIndex::new(i64::MIN).is_none(), "most negative");
        assert!(
            BitIndex::new(BitIndex::MAX + 1).is_none(),
            "one past the cap"
        );
        assert!(
            BitIndex::new(i64::MAX).is_none(),
            "the value that asked for a 10^16-word Vec"
        );
        assert!(BitIndex::new(0).is_some(), "zero is a member");
        assert!(
            BitIndex::new(BitIndex::MAX).is_some(),
            "the cap is a member"
        );
    }
}
