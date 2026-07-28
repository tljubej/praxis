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

/// The `BitSet` payload: a growable vector of 64-bit words. Bit `i` is in word
/// `i / 64` at position `i % 64`. Both fields are `Drop`.
#[repr(C)]
pub struct BitSetPayload {
    /// The words. Trailing zero words may be present; equality/hash trim them.
    pub words: Vec<u64>,
}

impl BitSetPayload {
    /// True iff bit `i` is set. `i` must be non-negative.
    pub(crate) fn contains(&self, i: usize) -> bool {
        let (word, bit) = (i / 64, i % 64);
        match self.words.get(word) {
            Some(w) => (w >> bit) & 1 == 1,
            None => false,
        }
    }

    /// Set bit `i`, growing the words vector as needed.
    pub(crate) fn insert(&mut self, i: usize) {
        let (word, bit) = (i / 64, i % 64);
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1u64 << bit;
    }

    /// Clear bit `i` (no-op if not set or out of range).
    pub(crate) fn remove(&mut self, i: usize) {
        let (word, bit) = (i / 64, i % 64);
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
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset_descriptor_reports_capabilities() {
        assert!(BITSET.is_equatable() && BITSET.is_hashable());
        assert_eq!(BITSET.name, "BitSet");
    }

    #[test]
    fn bitset_insert_contains_count() {
        let mut b = BitSetPayload { words: Vec::new() };
        b.insert(0);
        b.insert(63);
        b.insert(64);
        b.insert(1000);
        assert!(b.contains(0));
        assert!(b.contains(63));
        assert!(b.contains(64));
        assert!(b.contains(1000));
        assert!(!b.contains(1));
        assert!(!b.contains(65));
        assert_eq!(b.count(), 4);
    }

    #[test]
    fn bitset_remove_clears_bit() {
        let mut b = BitSetPayload { words: Vec::new() };
        b.insert(5);
        assert!(b.contains(5));
        b.remove(5);
        assert!(!b.contains(5));
        // Removing an unset bit is a no-op.
        b.remove(999);
    }
}
