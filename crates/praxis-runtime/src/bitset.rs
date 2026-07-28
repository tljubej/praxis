//! `BitSet` (M8-WS5, §6.1).
//!
//! A compact set of non-negative integers, backed by a `Vec<u64>` of words.
//! Occupancy is bit `i` of word `i / 64`. Nullary in user syntax (`BitSet`, no
//! type arg); elements are always `Int`. Iterable (yields `Int`).
//!
//! `BitSet` is its own GC payload: the words are a `Drop` `Vec`, so the
//! descriptor's `drop_value` releases them on sweep. Equality/hash are
//! structural (two bitsets are equal iff they hold the same bits).

use std::fmt;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, Tracer, TypeDescriptor};

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
/// `i / 64` at position `i % 64`. Both fields are `Drop`.
#[repr(C)]
pub struct BitSetPayload {
    /// The words. Trailing zero words may be present; equality/hash trim them.
    pub words: Vec<u64>,
}

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
    pub(crate) fn insert(&mut self, i: BitIndex) {
        let (word, bit) = i.word_and_bit();
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
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
}

unsafe fn bitset_trace(_payload: *mut u8, _tracer: &mut dyn Tracer) {
    // BitSet holds no GcRefs (bits are not objects); nothing to trace.
}

unsafe fn bitset_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized BitSetPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut BitSetPayload) };
}

unsafe fn bitset_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized BitSetPayload.
    let p = unsafe { &*(payload as *const BitSetPayload) };
    let _ = out.write_str("{");
    let mut first = true;
    for (word_idx, &word) in p.words.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            let value = word_idx * 64 + bit;
            if !first {
                let _ = out.write_str(", ");
            }
            first = false;
            let _ = write!(out, "{value}");
            bits &= bits - 1; // clear the lowest set bit
        }
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
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
    None,
)
.with_owned_bytes(bitset_owned_bytes);

/// The heap bytes `BitSet` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `BitSetPayload`.
unsafe fn bitset_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized BitSetPayload.
    let p = unsafe { &*(payload as *const BitSetPayload) };
    p.words.capacity() * std::mem::size_of::<u64>()
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

    #[test]
    fn bitset_insert_contains_count() {
        let mut b = BitSetPayload { words: Vec::new() };
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
        let mut b = BitSetPayload { words: Vec::new() };
        b.insert(bit(5));
        assert!(b.contains(bit(5)));
        b.remove(bit(5));
        assert!(!b.contains(bit(5)));
        // Removing an unset bit is a no-op.
        b.remove(bit(999));
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
