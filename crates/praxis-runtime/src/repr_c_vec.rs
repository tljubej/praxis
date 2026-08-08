//! [`ReprCVec<T>`] — a growable vector whose field layout is a promise
//! (ADR-118).
//!
//! `std::Vec<T>` is `#[repr(Rust)]`. Its length and its element pointer live
//! inside a private `RawVec`, so `offset_of!` cannot name them and the field
//! order is not stable across compiler versions. That is fine for Rust code and
//! fatal for generated code: a backend that wants to read a `Vec[T]`'s length
//! inline has nothing it is allowed to read.
//!
//! `ReprCVec` is a `#[repr(C)]` triple — pointer, length, capacity — that
//! *never holds anything a `Vec` did not hand it*. Every construction goes
//! through [`ReprCVec::from_vec`] and every mutation goes through
//! [`ReprCVec::vec_mut`], which reconstitutes a real `Vec`, lets `Vec` do the
//! growth, and takes the parts back. No allocator logic is reimplemented here;
//! `RawVec`'s amortized growth, its capacity-overflow checks and its allocation
//! failure handling are all still the ones doing the work. What is new is only
//! that the three words are somewhere the compiler is allowed to look.
//!
//! # The measurement toggle
//!
//! The `std-vec-payload` cargo feature replaces the `#[repr(C)]` triple with a
//! `#[repr(transparent)]` newtype over `std::Vec<T>`, leaving every caller in
//! the tree byte-for-byte unchanged. That is the arm A / arm B pair ADR-118 is
//! measured with: the only thing that differs between the two binaries is the
//! representation of this one struct. The change is expected to be
//! **performance neutral** — it changes a container's layout, not an algorithm
//! — so the comparison is a regression check, not a win.

use std::fmt;
use std::ops::{Deref, DerefMut};

// The three the `#[repr(C)]` arm needs and the `std-vec-payload` arm does not.
#[cfg(not(feature = "std-vec-payload"))]
use std::{marker::PhantomData, mem::ManuallyDrop, ptr::NonNull};

// ===========================================================================
// The representation, and the one place the two arms differ.
// ===========================================================================

/// A growable vector with a pinned `#[repr(C)]` layout.
///
/// **The field order is an ABI decision** (ADR-118). `ptr` is at offset 0 and
/// `len` at offset 8 because those are the two words generated code reads —
/// `praxis_vec_get` wants both, `praxis_vec_len` wants the second — and putting
/// them adjacent and first means one base register and two small displacements,
/// on the same cache line as the payload's element descriptor at offset 0 of
/// [`VecPayload`](crate::collections::VecPayload). `cap` is last because no
/// generated code has a reason to read it: capacity is the allocator's
/// business, reached only from Rust through
/// [`capacity`](ReprCVec::capacity) for GC pacing.
///
/// The order is pinned by `const _` assertions below rather than by a sentence,
/// per the house rule for `#[repr(C)]` layout claims.
#[cfg(not(feature = "std-vec-payload"))]
#[repr(C)]
pub struct ReprCVec<T> {
    /// The element buffer. Non-null even when `cap == 0`, exactly as
    /// `Vec::as_mut_ptr` is — an empty `Vec` answers a dangling aligned
    /// pointer, and this field holds whatever it answered.
    ptr: NonNull<T>,
    /// The number of initialized elements. This is the word `praxis_vec_len`
    /// exists to read.
    len: usize,
    /// The allocated capacity, in elements. This is the number
    /// `Vec::from_raw_parts` demands back — not `len` — and getting that wrong
    /// is a heap corruption rather than a wrong answer, which is why
    /// [`a_vec_survives_the_round_trip_with_its_capacity_intact`] exists.
    cap: usize,
    /// `ReprCVec<T>` owns its `T`s, so it is invariant-free but drop-relevant:
    /// this is what tells dropck and the auto traits that dropping the
    /// container drops `T`s.
    _owns: PhantomData<T>,
}

/// The `std-vec-payload` arm: the same API over an ordinary `std::Vec`.
///
/// This exists only so ADR-118's A/B has a toggle point that is exactly the
/// representation and nothing else. It is never built by `just ci`.
#[cfg(feature = "std-vec-payload")]
#[repr(transparent)]
pub struct ReprCVec<T> {
    inner: Vec<T>,
}

// The layout claims, in the tree rather than in a comment. The size equality is
// checked in both arms, and it is what keeps `VecPayload` in its block size
// class: 8 bytes of element descriptor plus 24 bytes of vector is 32.
const _: () = assert!(
    std::mem::size_of::<ReprCVec<crate::GcRef>>() == std::mem::size_of::<Vec<crate::GcRef>>()
);
const _: () = assert!(
    std::mem::align_of::<ReprCVec<crate::GcRef>>() == std::mem::align_of::<Vec<crate::GcRef>>()
);
const _: () = assert!(std::mem::size_of::<ReprCVec<crate::GcRef>>() == 24);

#[cfg(not(feature = "std-vec-payload"))]
mod layout {
    use super::ReprCVec;
    use crate::GcRef;
    use std::mem::offset_of;

    // The field order generated code bakes in. Changing any of these three
    // numbers is an ABI change even though no ABI constant names them,
    // because the moment the backend emits a load at a displacement, the
    // displacement is the contract (ABI v20, ADR-118 part 2).
    const _: () = assert!(offset_of!(ReprCVec<GcRef>, ptr) == 0);
    const _: () = assert!(offset_of!(ReprCVec<GcRef>, len) == 8);
    const _: () = assert!(offset_of!(ReprCVec<GcRef>, cap) == 16);

    // …and the same three for `u64`, which is `BitSetPayload.words`. The two
    // instantiations are asserted separately rather than argued to be equal:
    // the fields are all pointer-width whatever `T` is, so the equality is
    // obvious and therefore exactly the kind of thing that goes unchecked until
    // a `PhantomData` moves. These two are the only instantiations generated
    // code reads, and a third one wants its own pair of lines here.
    const _: () = assert!(offset_of!(ReprCVec<u64>, ptr) == 0);
    const _: () = assert!(offset_of!(ReprCVec<u64>, len) == 8);
    const _: () = assert!(offset_of!(ReprCVec<u64>, cap) == 16);
}

/// The displacement of the element pointer within a [`ReprCVec`], for the two
/// payloads generated code reads (`VecPayload.items`, `BitSetPayload.words`).
///
/// **This constant does not exist under `std-vec-payload`, and that is the
/// point.** ADR-118 part 1's measurement arm replaces the pinned triple with a
/// `std::Vec`, whose word order is precisely what nothing may assume — so a
/// backend that emitted a load at this displacement against that arm would read
/// a capacity where it wanted a length. Naming these two constants
/// unconditionally in `praxis-codegen-cranelift` makes the combination a
/// **build failure** rather than a miscompile.
#[cfg(not(feature = "std-vec-payload"))]
pub const REPR_C_VEC_ELEMENTS_OFFSET: usize = std::mem::offset_of!(ReprCVec<crate::GcRef>, ptr);

/// The displacement of the length word within a [`ReprCVec`]. See
/// [`REPR_C_VEC_ELEMENTS_OFFSET`].
#[cfg(not(feature = "std-vec-payload"))]
pub const REPR_C_VEC_LEN_OFFSET: usize = std::mem::offset_of!(ReprCVec<crate::GcRef>, len);

/// Everything generated code needs to walk one collection payload's pinned
/// [`ReprCVec`] inline, as **one value with private fields** (ADR-118 part 2).
///
/// This is [`InlineInternSite`](crate::InlineInternSite)'s shape and it is here
/// for the same reason. The sequence needs a descriptor to prove and three
/// displacements, and handed over as loose constants they are four independent
/// chances to pair one payload's offsets with another's descriptor — which is
/// not a hypothetical: `VecPayload` and `GridPayload` are both a `Vec<GcRef>`
/// behind a leading word, at *different* displacements, and a `Grid` proved as
/// a `Vec` would read its width as an element pointer. So they are one value,
/// its fields are private, its constructor is `pub(crate)`, and each instance is
/// minted in the module that owns the payload whose layout it describes.
///
/// The backend cannot assemble one; it can only name one this crate wrote.
///
/// **The displacements are from the object's base**, not from its payload:
/// generated code holds a `GcRef` and the header size is `GcHeader`'s business
/// (ADR-039 decision 1), so the addition happens once, here, rather than at
/// three emit sites.
#[cfg(not(feature = "std-vec-payload"))]
#[derive(Clone, Copy, Debug)]
pub struct InlineSliceSite {
    type_id: crate::descriptor::BuiltinTypeId,
    elements_offset: usize,
    len_offset: usize,
    element_shift: u8,
}

#[cfg(not(feature = "std-vec-payload"))]
impl InlineSliceSite {
    /// The site for a payload of alignment `payload_align` whose `ReprCVec`
    /// field begins at `field_offset` within it and holds elements of
    /// `element_size` bytes, in objects whose descriptor is `type_id`'s.
    ///
    /// # Panics
    /// At compile time (every call is a `const` initializer) if `element_size`
    /// is not a power of two — the emitted index arithmetic is a shift.
    pub(crate) const fn new(
        type_id: crate::descriptor::BuiltinTypeId,
        payload_align: usize,
        field_offset: usize,
        element_size: usize,
    ) -> InlineSliceSite {
        assert!(
            element_size.is_power_of_two(),
            "the element scale must be a shift"
        );
        let base = crate::GcHeader::payload_offset_for(payload_align) + field_offset;
        InlineSliceSite {
            type_id,
            elements_offset: base + REPR_C_VEC_ELEMENTS_OFFSET,
            len_offset: base + REPR_C_VEC_LEN_OFFSET,
            element_shift: element_size.trailing_zeros() as u8,
        }
    }

    /// The built-in whose descriptor an object must carry before any of the
    /// displacements below may be read. The proof is ADR-102's, and it is what
    /// makes the folded offsets the offsets the allocator actually used.
    #[must_use]
    pub const fn type_id(self) -> crate::descriptor::BuiltinTypeId {
        self.type_id
    }

    /// Object base → the element buffer pointer.
    #[must_use]
    pub const fn elements_offset(self) -> usize {
        self.elements_offset
    }

    /// Object base → the element count.
    #[must_use]
    pub const fn len_offset(self) -> usize {
        self.len_offset
    }

    /// `log2(element_size)`: the shift that turns an index into a byte offset.
    #[must_use]
    pub const fn element_shift(self) -> u8 {
        self.element_shift
    }
}

// ===========================================================================
// The primitives. Everything else in this file is written in terms of these
// four, so the two arms share all of the API surface and none of the unsafe.
// ===========================================================================

#[cfg(not(feature = "std-vec-payload"))]
impl<T> ReprCVec<T> {
    /// Decompose a `Vec` into the three words. No allocation, no copy.
    #[inline]
    #[must_use]
    pub fn from_vec(vec: Vec<T>) -> Self {
        let mut vec = ManuallyDrop::new(vec);
        let (ptr, len, cap) = (vec.as_mut_ptr(), vec.len(), vec.capacity());
        Self {
            // SAFETY: `Vec::as_mut_ptr` never returns null — for a zero-capacity
            // `Vec` it answers `NonNull::dangling()`, which is aligned and
            // non-null by construction. This is the only route into `ptr`, so
            // "the pointer came out of a live `Vec`" is an invariant of the
            // type rather than an obligation on callers.
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            len,
            cap,
            _owns: PhantomData,
        }
    }

    /// Reassemble the `Vec` these three words came from.
    #[inline]
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        let me = ManuallyDrop::new(self);
        // SAFETY: the three words were produced by `from_vec` from a live `Vec`
        // (the only constructor) and have not been observed by anything else
        // since — `ptr` and `cap` are private and no method writes them except
        // through a `from_vec`. `cap` is therefore the *allocated* capacity and
        // not the length, which is `from_raw_parts`'s one easy contract to
        // break. `ManuallyDrop` is what stops the buffer being freed twice:
        // this function consumes `self`, and the returned `Vec` is now its
        // owner.
        unsafe { Vec::from_raw_parts(me.ptr.as_ptr(), me.len, me.cap) }
    }

    /// The elements, in order.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: `ptr` is the buffer of the `Vec` `from_vec` decomposed and
        // `len` is that `Vec`'s length, so `len` elements from `ptr` are
        // initialized and contiguous. The borrow of `self` is what bounds the
        // slice's lifetime to the container's.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The elements, mutably. The *length* cannot be changed through this —
    /// that is [`vec_mut`](Self::vec_mut)'s job.
    #[inline]
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, and the `&mut self` borrow is what makes the
        // aliasing exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// The allocated capacity, in elements. Read by `vec_owned_bytes` for GC
    /// pacing: the buffer's real footprint, not its occupancy.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Borrow the contents as a real `Vec` for the duration of a mutation.
    ///
    /// This is the **only** way to change the length. The returned guard hands
    /// the parts back on drop, including on unwind.
    #[inline]
    pub fn vec_mut(&mut self) -> VecMut<'_, T> {
        // Leave `self` genuinely empty rather than stale. A guard that is
        // `mem::forget`ed then leaks the buffer — which is safe — instead of
        // leaving `self` holding a pointer the `Vec` may since have
        // reallocated away from, which is a use-after-free. Three stores, on a
        // path that is about to call `RawVec::grow`.
        let taken = std::mem::take(self).into_vec();
        VecMut {
            vec: ManuallyDrop::new(taken),
            owner: self,
        }
    }
}

#[cfg(feature = "std-vec-payload")]
impl<T> ReprCVec<T> {
    /// Wrap the `Vec`. The `std-vec-payload` arm keeps it whole.
    #[inline]
    #[must_use]
    pub fn from_vec(vec: Vec<T>) -> Self {
        Self { inner: vec }
    }

    /// Unwrap the `Vec`.
    #[inline]
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.inner
    }

    /// The elements, in order.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        self.inner.as_slice()
    }

    /// The elements, mutably.
    #[inline]
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.inner.as_mut_slice()
    }

    /// The allocated capacity, in elements.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Borrow the contents as a real `Vec` for the duration of a mutation.
    #[inline]
    pub fn vec_mut(&mut self) -> VecMut<'_, T> {
        VecMut { owner: self }
    }
}

/// A `Vec` borrowed out of a [`ReprCVec`] for the duration of a mutation.
///
/// Dropping it writes the (possibly reallocated) parts back. While it is alive
/// the `ReprCVec` it came from is empty, not stale — see
/// [`ReprCVec::vec_mut`].
#[cfg(not(feature = "std-vec-payload"))]
pub struct VecMut<'a, T> {
    /// `ManuallyDrop` because [`Drop`] hands the parts back to `owner` instead
    /// of freeing them.
    vec: ManuallyDrop<Vec<T>>,
    owner: &'a mut ReprCVec<T>,
}

#[cfg(not(feature = "std-vec-payload"))]
impl<T> Drop for VecMut<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `ManuallyDrop::take` needs the value never to be used again.
        // This is `Drop::drop`, `self.vec` is private, and `self` is gone the
        // instant this returns — so there is no "again". Running on the unwind
        // path is deliberate: a wrapper whose mutation panicked still gets its
        // elements back, which is what makes the two ADR-118 arms agree even
        // under a fault.
        let vec = unsafe { ManuallyDrop::take(&mut self.vec) };
        *self.owner = ReprCVec::from_vec(vec);
    }
}

#[cfg(not(feature = "std-vec-payload"))]
impl<T> Deref for VecMut<'_, T> {
    type Target = Vec<T>;

    #[inline]
    fn deref(&self) -> &Vec<T> {
        &self.vec
    }
}

#[cfg(not(feature = "std-vec-payload"))]
impl<T> DerefMut for VecMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.vec
    }
}

/// A `Vec` borrowed out of a [`ReprCVec`] for the duration of a mutation.
#[cfg(feature = "std-vec-payload")]
pub struct VecMut<'a, T> {
    owner: &'a mut ReprCVec<T>,
}

#[cfg(feature = "std-vec-payload")]
impl<T> Deref for VecMut<'_, T> {
    type Target = Vec<T>;

    #[inline]
    fn deref(&self) -> &Vec<T> {
        &self.owner.inner
    }
}

#[cfg(feature = "std-vec-payload")]
impl<T> DerefMut for VecMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.owner.inner
    }
}

// ===========================================================================
// The shared API. Written entirely in terms of the five primitives above, so
// neither arm can drift from the other.
// ===========================================================================

impl<T> ReprCVec<T> {
    /// An empty vector. Allocates nothing, exactly as `Vec::new` does.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::from_vec(Vec::new())
    }

    /// An empty vector with room for `capacity` elements.
    #[inline]
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::from_vec(Vec::with_capacity(capacity))
    }

    /// The number of elements.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// Whether there are no elements.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append one element, growing if needed.
    #[inline]
    pub fn push(&mut self, value: T) {
        self.vec_mut().push(value);
    }

    /// Remove and answer the last element, or `None` when empty.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.vec_mut().pop()
    }

    /// Insert `value` at `index`, shifting everything after it right.
    #[inline]
    pub fn insert(&mut self, index: usize, value: T) {
        self.vec_mut().insert(index, value);
    }

    /// Remove and answer the element at `index`, shifting everything after it
    /// left.
    #[inline]
    pub fn remove(&mut self, index: usize) -> T {
        self.vec_mut().remove(index)
    }

    /// Remove the element at `index` by swapping the last one into its place.
    #[inline]
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.vec_mut().swap_remove(index)
    }

    /// Drop every element, keeping the capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.vec_mut().clear();
    }

    /// Shorten to `len` elements, dropping the rest. A no-op if already shorter.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.vec_mut().truncate(len);
    }

    /// Keep only the elements `f` answers `true` for, in order.
    #[inline]
    pub fn retain(&mut self, f: impl FnMut(&T) -> bool) {
        self.vec_mut().retain(f);
    }

    /// Reserve room for at least `additional` more elements.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.vec_mut().reserve(additional);
    }

    /// Move every element of `other` onto the end of this vector.
    #[inline]
    pub fn append(&mut self, other: &mut Vec<T>) {
        self.vec_mut().append(other);
    }
}

impl<T: Clone> ReprCVec<T> {
    /// Append a copy of every element of `other`.
    #[inline]
    pub fn extend_from_slice(&mut self, other: &[T]) {
        self.vec_mut().extend_from_slice(other);
    }

    /// Grow or shrink to `len` elements, filling new slots with `value`.
    #[inline]
    pub fn resize(&mut self, len: usize, value: T) {
        self.vec_mut().resize(len, value);
    }
}

impl<T> Default for ReprCVec<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for ReprCVec<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> DerefMut for ReprCVec<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

// `for x in &items` and `for x in &mut items` do not go through `Deref`:
// trait selection for `IntoIterator` does not autoderef. These two impls are
// what lets the runtime's reading sites iterate a `ReprCVec` exactly as they
// would a `Vec`.
impl<'a, T> IntoIterator for &'a ReprCVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a, T> IntoIterator for &'a mut ReprCVec<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_mut_slice().iter_mut()
    }
}

impl<T> IntoIterator for ReprCVec<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl<T> Extend<T> for ReprCVec<T> {
    #[inline]
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.vec_mut().extend(iter);
    }
}

impl<T> FromIterator<T> for ReprCVec<T> {
    #[inline]
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from_vec(iter.into_iter().collect())
    }
}

impl<T> From<Vec<T>> for ReprCVec<T> {
    #[inline]
    fn from(vec: Vec<T>) -> Self {
        Self::from_vec(vec)
    }
}

impl<T> From<ReprCVec<T>> for Vec<T> {
    #[inline]
    fn from(vec: ReprCVec<T>) -> Self {
        vec.into_vec()
    }
}

impl<T: Clone> Clone for ReprCVec<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self::from_vec(self.as_slice().to_vec())
    }
}

impl<T: fmt::Debug> fmt::Debug for ReprCVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl<T: PartialEq> PartialEq for ReprCVec<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for ReprCVec<T> {}

/// Freeing the buffer is `Vec`'s job; this hands it back to `Vec` to do.
///
/// Only the `#[repr(C)]` arm needs one — the `std-vec-payload` arm's field
/// drops itself.
#[cfg(not(feature = "std-vec-payload"))]
impl<T> Drop for ReprCVec<T> {
    #[inline]
    fn drop(&mut self) {
        // `into_vec` consumes a `ReprCVec`, so take one out and leave an empty
        // one behind rather than reading the fields in place: what stays behind
        // is a valid container over no allocation, so there is nothing for the
        // drop glue that runs after this body to free a second time. The empty
        // container's own `Drop::drop` is not re-entered — drop glue never
        // re-calls `Drop::drop` on the value a `Drop::drop` body wrote back —
        // and `Default::default` allocates nothing, so this is not recursive.
        drop(std::mem::take(self).into_vec());
    }
}

// `Vec<T>` is `Send`/`Sync` when `T` is, on the grounds that the container
// uniquely owns its elements. The `#[repr(C)]` arm holds a `NonNull<T>`, which
// is unconditionally neither, so without these two impls the two ADR-118 arms
// would differ in something other than layout. The reasoning is `Vec`'s,
// unchanged: `ReprCVec` is the sole owner of its buffer and hands out
// references only through `&self`/`&mut self`.
//
// SAFETY: sole ownership of the `T`s, so moving the container between threads
// moves the `T`s and nothing else observes them.
#[cfg(not(feature = "std-vec-payload"))]
unsafe impl<T: Send> Send for ReprCVec<T> {}
// SAFETY: `&ReprCVec<T>` hands out only `&T`, so sharing the container across
// threads shares the `T`s and nothing more.
#[cfg(not(feature = "std-vec-payload"))]
unsafe impl<T: Sync> Sync for ReprCVec<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_repr_c_vec_is_the_same_three_words_a_std_vec_is() {
        assert_eq!(
            std::mem::size_of::<ReprCVec<u64>>(),
            std::mem::size_of::<Vec<u64>>()
        );
        assert_eq!(
            std::mem::align_of::<ReprCVec<u64>>(),
            std::mem::align_of::<Vec<u64>>()
        );
    }

    #[cfg(not(feature = "std-vec-payload"))]
    #[test]
    fn the_pointer_is_first_the_length_is_second_and_the_capacity_is_last() {
        use std::mem::offset_of;
        assert_eq!(offset_of!(ReprCVec<u64>, ptr), 0);
        assert_eq!(offset_of!(ReprCVec<u64>, len), 8);
        assert_eq!(offset_of!(ReprCVec<u64>, cap), 16);
    }

    #[cfg(not(feature = "std-vec-payload"))]
    #[test]
    fn the_length_word_is_readable_at_its_declared_offset() {
        // The whole point of the type: a reader that knows only the offsets can
        // find the length. This is the Rust-side rehearsal of the `load`
        // generated code emits at displacement 8.
        let v = ReprCVec::from_vec(vec![1_u64, 2, 3, 4, 5]);
        let base = std::ptr::addr_of!(v).cast::<u8>();
        // SAFETY: `base` points at a live `ReprCVec<u64>`, whose layout the
        // `const _` assertions above pin: a pointer at 0 and a `usize` at 8.
        let len = unsafe { base.add(8).cast::<usize>().read() };
        assert_eq!(len, 5);
        // SAFETY: as above, the element pointer is the word at offset 0.
        let ptr = unsafe { base.cast::<*const u64>().read() };
        // SAFETY: `ptr` is the live buffer and index 3 is in bounds.
        assert_eq!(unsafe { *ptr.add(3) }, 4);
    }

    #[test]
    fn a_vec_survives_the_round_trip_with_its_capacity_intact() {
        // `Vec::from_raw_parts` wants the *allocated* capacity, not the length.
        // Handing it the length frees the wrong number of bytes, which is a
        // heap corruption rather than a wrong answer — so the capacity is
        // asserted, not just the contents.
        let mut original: Vec<u64> = Vec::with_capacity(17);
        original.extend([10, 20, 30]);
        let capacity = original.capacity();
        assert!(
            capacity >= 17,
            "with_capacity should over-reserve, not exact"
        );

        let wrapped = ReprCVec::from_vec(original);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(wrapped.capacity(), capacity);
        assert_eq!(wrapped.as_slice(), &[10, 20, 30]);

        let back = wrapped.into_vec();
        assert_eq!(back, vec![10, 20, 30]);
        assert_eq!(
            back.capacity(),
            capacity,
            "the capacity is the allocation's, and it must survive the trip"
        );
    }

    #[test]
    fn an_empty_vec_round_trips_without_touching_the_allocator() {
        let wrapped = ReprCVec::from_vec(Vec::<u64>::new());
        assert_eq!(wrapped.capacity(), 0);
        assert!(wrapped.is_empty());
        let back = wrapped.into_vec();
        assert_eq!(back.capacity(), 0);
        assert!(back.is_empty());
    }

    #[test]
    fn a_push_that_reallocates_leaves_the_container_pointing_at_the_new_buffer() {
        let mut v = ReprCVec::<u64>::new();
        for i in 0..1000_u64 {
            v.push(i);
        }
        assert_eq!(v.len(), 1000);
        assert_eq!(v[0], 0);
        assert_eq!(v[999], 999);
        assert!(v.capacity() >= 1000);
        // Read every element: a stale pointer left behind by a realloc shows up
        // here and nowhere else.
        assert_eq!(v.iter().sum::<u64>(), (0..1000).sum::<u64>());
    }

    /// Counts its own drops, so a leak and a double free are both visible.
    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_the_container_drops_every_element_exactly_once() {
        let count = Arc::new(AtomicUsize::new(0));
        {
            let mut v = ReprCVec::new();
            for _ in 0..64 {
                v.push(DropProbe(Arc::clone(&count)));
            }
            assert_eq!(count.load(Ordering::SeqCst), 0, "no drops while it lives");
        }
        assert_eq!(count.load(Ordering::SeqCst), 64);
    }

    #[test]
    fn a_round_trip_through_a_vec_does_not_drop_anything() {
        let count = Arc::new(AtomicUsize::new(0));
        let v = ReprCVec::from_vec(vec![
            DropProbe(Arc::clone(&count)),
            DropProbe(Arc::clone(&count)),
        ]);
        let back = v.into_vec();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(back);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_removed_element_is_dropped_by_its_new_owner_and_not_by_the_container() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut v = ReprCVec::new();
        v.push(DropProbe(Arc::clone(&count)));
        v.push(DropProbe(Arc::clone(&count)));
        let taken = v.pop().expect("two were pushed");
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(taken);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(v);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn clear_and_truncate_drop_what_they_remove() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut v = ReprCVec::new();
        for _ in 0..10 {
            v.push(DropProbe(Arc::clone(&count)));
        }
        v.truncate(4);
        assert_eq!(count.load(Ordering::SeqCst), 6);
        assert_eq!(v.len(), 4);
        v.clear();
        assert_eq!(count.load(Ordering::SeqCst), 10);
        assert!(v.is_empty());
        // Capacity survives a clear, which is what `vec_owned_bytes` reports.
        assert!(v.capacity() >= 10);
    }

    // Arm-B only, and not because the `std-vec-payload` arm is weaker: that arm
    // has no raw pointer that could go stale, so its guard is a plain borrow and
    // forgetting it changes nothing. The property being asserted here only
    // exists where the hazard does.
    #[cfg(not(feature = "std-vec-payload"))]
    #[test]
    fn a_forgotten_mutation_guard_leaves_the_container_empty_rather_than_stale() {
        // `mem::forget` on the guard is the one way the write-back can be
        // skipped. It must leak the buffer, not leave a dangling pointer in a
        // container the collector will read: `vec_trace` walks `items` on every
        // mark.
        let count = Arc::new(AtomicUsize::new(0));
        let mut v = ReprCVec::new();
        v.push(DropProbe(Arc::clone(&count)));

        let mut guard = v.vec_mut();
        guard.push(DropProbe(Arc::clone(&count)));
        std::mem::forget(guard);

        assert_eq!(v.len(), 0, "a forgotten guard leaves an empty container");
        assert_eq!(v.capacity(), 0);
        assert!(v.iter().next().is_none());
        drop(v);
        // Both probes are leaked with the buffer. A leak is safe; a stale
        // pointer read by `vec_trace` would not be.
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_mutation_that_panics_still_hands_the_elements_back() {
        let mut v = ReprCVec::from_vec(vec![1_u64, 2, 3]);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = v.vec_mut();
            guard.push(4);
            panic!("the wrapper faulted mid-mutation");
        }));
        assert!(result.is_err());
        assert_eq!(
            v.as_slice(),
            &[1, 2, 3, 4],
            "the guard's Drop runs on the unwind path too"
        );
    }

    #[test]
    fn the_reading_api_is_the_slice_api() {
        let v = ReprCVec::from_vec(vec![3_u64, 1, 2]);
        assert_eq!(v.len(), 3);
        assert_eq!(v[1], 1);
        assert_eq!(v.first().copied(), Some(3));
        assert_eq!(v.last().copied(), Some(2));
        assert!(v.contains(&2));
        assert_eq!(v.iter().copied().max(), Some(3));
        let doubled: Vec<u64> = (&v).into_iter().map(|x| x * 2).collect();
        assert_eq!(doubled, vec![6, 2, 4]);
    }

    #[test]
    fn the_mutable_slice_api_reaches_through_deref_mut() {
        let mut v = ReprCVec::from_vec(vec![3_u64, 1, 2]);
        v.sort_unstable();
        assert_eq!(v.as_slice(), &[1, 2, 3]);
        v.swap(0, 2);
        assert_eq!(v.as_slice(), &[3, 2, 1]);
        for x in &mut v {
            *x += 1;
        }
        assert_eq!(v.as_slice(), &[4, 3, 2]);
    }

    #[test]
    fn extend_insert_remove_and_retain_agree_with_a_std_vec() {
        let mut ours = ReprCVec::<u64>::new();
        let mut theirs = Vec::<u64>::new();

        ours.extend(0..20);
        theirs.extend(0..20);

        ours.insert(5, 99);
        theirs.insert(5, 99);

        assert_eq!(ours.remove(0), theirs.remove(0));
        assert_eq!(ours.swap_remove(3), theirs.swap_remove(3));

        ours.retain(|x| x % 2 == 0);
        theirs.retain(|x| x % 2 == 0);

        ours.extend_from_slice(&[7, 7, 7]);
        theirs.extend_from_slice(&[7, 7, 7]);

        assert_eq!(ours.as_slice(), theirs.as_slice());
        assert_eq!(ours.into_vec(), theirs);
    }

    #[test]
    fn collect_clone_and_equality_behave() {
        let v: ReprCVec<u64> = (0..5).collect();
        let w = v.clone();
        assert_eq!(v, w);
        assert_eq!(format!("{v:?}"), "[0, 1, 2, 3, 4]");
        let owned: Vec<u64> = v.into_iter().collect();
        assert_eq!(owned, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_zero_sized_element_type_round_trips() {
        // `Vec<()>` has a dangling pointer and `usize::MAX` capacity; the
        // decomposition must not care.
        let v = ReprCVec::from_vec(vec![(), (), ()]);
        assert_eq!(v.len(), 3);
        let back = v.into_vec();
        assert_eq!(back.len(), 3);
    }
}
