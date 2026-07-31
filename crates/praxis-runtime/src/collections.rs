//! The `Vec[T]` collection descriptor (§11.2, ADR-013).
//!
//! §11.2 maps `Vec[T]` to a Rust `Vec<GcRef>`. The static element type `T` is
//! enforced by the compiler and **recorded in the collection object's payload**
//! (§11.2: "recorded in the collection object's type descriptor"). The payload
//! therefore carries an element descriptor alongside the items, so `trace`,
//! `format`, and `equals` dispatch element-wise without a type switch.
//!
//! This is the composite type that proves nested `GcRef` tracing (ADR-013):
//! `trace` forwards every element to the tracer.
//!
//! **M5 change:** `items` is now a `Vec<GcRef>` (was `Box<[GcRef]>` in M3) so
//! `push` mutates the vector *in place* — matching §4.2's "a `let` binding may
//! still point to a mutable object" and §11.1's `push -> Unit` (the receiver is
//! mutated, no new reference returned). The `Vec`'s backing storage may
//! reallocate internally, but the `VecPayload` object itself stays at the same
//! GC address (non-moving collector, ADR-011), so existing `GcRef`s remain
//! valid. Per §11.5, runtime wrappers never expose an interior pointer to the
//! `Vec`'s backing buffer across a capacity-mutating op; they reload from the
//! payload each call.

use std::fmt;

use crate::descriptor::{BuiltinTypeId, Tracer, TypeDescriptor};
use crate::DynamicHasher;
use crate::GcRef;

/// The `Vec[T]` payload: the element descriptor plus the growable items.
///
/// `items` is a `Vec<GcRef>` so `push` can grow it in place (§11.1). Both fields
/// are `Drop`, so [`VEC`]`'s `drop_value` releases them on sweep (§12.5).
#[repr(C)]
pub struct VecPayload {
    /// The descriptor for every element in `items`, or **null** when this
    /// vector has not been told its element type. Read by
    /// `trace`/`format`/`equals` to dispatch without a scattered type switch
    /// (§11.4); read it through [`VecPayload::element`], not directly.
    ///
    /// Null is the honest encoding of "unknown", and it only survives while the
    /// vector is empty: the first `push` adopts the pushed value's descriptor.
    /// It used to be spelled `INT`, which is why an empty `Vec[Float]` claimed
    /// to hold `Int`s and why `push` had licence to *retag* a vector that had
    /// been told its type (P0-11).
    pub element_descriptor: *const TypeDescriptor,
    /// The elements, in order. A `Vec` (not `Box<[T]>`) so `push` mutates in
    /// place.
    pub items: Vec<GcRef>,
}

impl VecPayload {
    /// The element descriptor, or `None` if this vector was never told its
    /// element type. `None` implies `items` is empty.
    #[must_use]
    pub fn element(&self) -> Option<&'static TypeDescriptor> {
        // SAFETY: a non-null element descriptor is always a `&'static` written
        // by the constructor or by the first `push`.
        (!self.element_descriptor.is_null()).then(|| unsafe { &*self.element_descriptor })
    }
}

/// Whether two collections agree on their element type (RT-10).
///
/// Descriptors are `static`, so pointer identity is the authoritative test
/// where both sides *have* one (ADR-038).
///
/// **A null slot agrees with anything**, which is ADR-066 decision 5's rule
/// applied here: a null slot is not the label `Unknown`, it is the *absence* of
/// a label, and what answers instead is the value's own descriptor. A
/// collection with no label has no elements — that is [`VecPayload::element`]'s
/// documented invariant, and the constructors uphold it — so there is no
/// element whose descriptor could disagree, and the length check every caller
/// performs immediately afterwards is what makes the two collections equal or
/// not. No element-wise dispatch can go wrong through a null: the side without
/// a label contributes no elements to dispatch over.
///
/// REP-42 is why this is written down. Before it, `praxis_map_new` hardcoded
/// `INT` as every `Map`'s value descriptor, so an empty `Map`'s `values()`
/// carried the label `Int` whatever the map held; after it, the label is
/// learned from the first insert and a never-inserted map's `values()` carries
/// none. Comparing by pointer identity then made an empty `Map[Text, Int]`'s
/// `values()` **unequal** to an equally-typed empty `Vec[Int]` — comparing an
/// unlearned label against a learned one, which is the label being treated as
/// the authority it is explicitly not (REP-41). The pre-REP-42 `true` was an
/// accident of the hardcoded `INT` and not a rule: the same program over a
/// `Map[Text, Text]` answered `false` both before and after.
///
/// What RT-10 asked for is untouched: two collections that have each been told
/// their element type must agree, so an empty `Vec[Int]` is still not an empty
/// `Vec[Text]`.
pub(crate) fn same_element(a: *const TypeDescriptor, b: *const TypeDescriptor) -> bool {
    a.is_null() || b.is_null() || std::ptr::eq(a, b)
}

unsafe fn vec_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    let p = unsafe { &*(payload as *const VecPayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn vec_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    // `drop_in_place` frees the boxed slice; the element descriptor is a static
    // reference and is not owned.
    unsafe { std::ptr::drop_in_place(payload as *mut VecPayload) };
}

unsafe fn vec_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    let p = unsafe { &*(payload as *const VecPayload) };
    let _ = out.write_str("[");
    // No element descriptor means no elements to format.
    let Some(elem_desc) = p.element() else {
        let _ = out.write_str("]");
        return;
    };
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        // Route element formatting through the element descriptor (§11.4).
        let elem_payload = item.payload::<u8>() as *const u8;
        (elem_desc.format)(elem_payload, out);
    }
    let _ = out.write_str("]");
}

unsafe fn vec_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized VecPayload`s
    // with compatible element descriptors.
    let pa = unsafe { &*(a as *const VecPayload) };
    let pb = unsafe { &*(b as *const VecPayload) };
    // Runtime element type is part of collection identity (RT-10). Without it
    // an empty `Vec[Int]` and an empty `Vec[Text]` compared equal — both were
    // "zero elements" — and a non-empty pair dispatched the *left* element
    // descriptor's callback against the right's payloads.
    if !same_element(pa.element_descriptor, pb.element_descriptor) {
        return false;
    }
    if pa.items.len() != pb.items.len() {
        return false;
    }
    // Element-wise equality through the element descriptor (§11.4). If the
    // element type is not equatable, the collection is not equatable (§5.5).
    let Some(elem) = pa.element() else {
        // Both are element-typeless, hence both empty, hence equal.
        return true;
    };
    let Some(eq) = elem.equals else {
        return false;
    };
    for (x, y) in pa.items.iter().zip(pb.items.iter()) {
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        if !eq(xe, ye) {
            return false;
        }
    }
    true
}

unsafe fn vec_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    let p = unsafe { &*(payload as *const VecPayload) };
    let Some(hash_elem) = p.element().and_then(|d| d.hash) else {
        return;
    };
    // Length first to distinguish prefixes (standard sequence-hash practice).
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for item in p.items.iter() {
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_elem(elem_payload, hasher);
    }
}

/// Descriptor for the `Vec[T]` collection (§11.2). The per-instance element
/// type lives in the payload, so a single descriptor serves all `Vec[T]`.
pub static VEC: TypeDescriptor = TypeDescriptor::builtin::<VecPayload>(
    BuiltinTypeId::Vec,
    "Vec",
    vec_trace,
    vec_drop,
    vec_format,
    Some(vec_equals),
    Some(vec_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
)
.with_owned_bytes(vec_owned_bytes);

/// The heap bytes `Vec[T]` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `VecPayload`.
unsafe fn vec_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized VecPayload.
    let p = unsafe { &*(payload as *const VecPayload) };
    p.items.capacity() * std::mem::size_of::<GcRef>()
}

// ===========================================================================
// Deque[T] (M8-WS2, §6.1). A double-ended queue backed by Rust's `VecDeque`.
// Mirrors `VecPayload` exactly (element descriptor + growable items) so trace/
// format/equals/hash dispatch identically; only the backing store and the
// front/back method surface differ.
// ===========================================================================

use std::collections::VecDeque;

/// The `Deque[T]` payload: the element descriptor plus a growable `VecDeque`.
/// `VecDeque` (not `Vec`) so `push_front`/`pop_front` are O(1) amortized.
/// Both fields are `Drop`, so [`DEQUE`]'s `drop_value` releases them on sweep.
#[repr(C)]
pub struct DequePayload {
    /// The descriptor for every element, or null for "not told yet" —
    /// [`VecPayload::element_descriptor`]'s contract exactly. Read it through
    /// [`DequePayload::element`].
    pub element_descriptor: *const TypeDescriptor,
    /// The elements. A `VecDeque` so both ends are cheap to mutate.
    pub items: VecDeque<GcRef>,
}

impl DequePayload {
    /// The element descriptor, or `None` if this deque was never told its
    /// element type. `None` implies `items` is empty.
    #[must_use]
    pub fn element(&self) -> Option<&'static TypeDescriptor> {
        // SAFETY: a non-null element descriptor is always a `&'static`.
        (!self.element_descriptor.is_null()).then(|| unsafe { &*self.element_descriptor })
    }
}

unsafe fn deque_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn deque_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    unsafe { std::ptr::drop_in_place(payload as *mut DequePayload) };
}

unsafe fn deque_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    let _ = out.write_str("[");
    let Some(elem_desc) = p.element() else {
        let _ = out.write_str("]");
        return;
    };
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let elem_payload = item.payload::<u8>() as *const u8;
        (elem_desc.format)(elem_payload, out);
    }
    let _ = out.write_str("]");
}

unsafe fn deque_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized DequePayloads.
    let pa = unsafe { &*(a as *const DequePayload) };
    let pb = unsafe { &*(b as *const DequePayload) };
    // Element type is part of identity (RT-10).
    if !same_element(pa.element_descriptor, pb.element_descriptor) {
        return false;
    }
    if pa.items.len() != pb.items.len() {
        return false;
    }
    let Some(elem) = pa.element() else {
        return true;
    };
    let Some(eq) = elem.equals else {
        return false;
    };
    for (x, y) in pa.items.iter().zip(pb.items.iter()) {
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        if !eq(xe, ye) {
            return false;
        }
    }
    true
}

unsafe fn deque_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    let Some(hash_elem) = p.element().and_then(|d| d.hash) else {
        return;
    };
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for item in p.items.iter() {
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_elem(elem_payload, hasher);
    }
}

/// Descriptor for the `Deque[T]` collection (§6.1). The per-instance element
/// type lives in the payload, so a single descriptor serves all `Deque[T]`.
pub static DEQUE: TypeDescriptor = TypeDescriptor::builtin::<DequePayload>(
    BuiltinTypeId::Deque,
    "Deque",
    deque_trace,
    deque_drop,
    deque_format,
    Some(deque_equals),
    Some(deque_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
)
.with_owned_bytes(deque_owned_bytes);

/// The heap bytes `Deque[T]` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `DequePayload`.
unsafe fn deque_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    p.items.capacity() * std::mem::size_of::<GcRef>()
}

// ===========================================================================
// Grid[T] (M6, §7.5 `grid`, §7.8 type derivation). M6 ships a minimal runtime
// type — row-major storage with a known width — so the synthesized type is the
// spec-faithful `Grid[T]`. Grid methods (neighbors, indexing, etc.) are M8.
// ===========================================================================

/// A validated grid shape: a non-negative width and height whose product is a
/// cell count the runtime can actually allocate.
///
/// This is the *only* route from a user-supplied `Int` pair to a cell count
/// (RT-07). `Grid[T](w, h)` used to reach `vec![unit; (w as usize) * (h as
/// usize)]` directly, where `w = -1` became `usize::MAX` and the product
/// overflowed — either an allocation the host could not serve (an OOM abort) or
/// a capacity-overflow panic, both crossing `extern "C"`. Neither is expressible
/// now: `GridExtent` holds `usize`s, and the multiplication it proves is the one
/// `cells()` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridExtent {
    width: usize,
    height: usize,
    cells: usize,
}

impl GridExtent {
    /// The largest grid the runtime will construct: 2^28 cells, which is 2 GiB
    /// of `GcRef` storage before a single cell object exists.
    ///
    /// A cap, not a `checked_mul`, because a product that merely *fits* in a
    /// `usize` is still an allocation no host can serve — `Grid[Int](2^40, 2)`
    /// multiplies cleanly and then aborts the process. The number is a judgement
    /// about what a Praxis program plausibly asks for; a program that wants more
    /// gets a fault it can see rather than a SIGKILL it cannot.
    pub const MAX_CELLS: usize = 1 << 28;

    /// The extent `width × height` names, or `None` if either side is negative
    /// or the grid would exceed [`MAX_CELLS`](Self::MAX_CELLS).
    #[must_use]
    pub const fn new(width: i64, height: i64) -> Option<GridExtent> {
        if width < 0 || height < 0 {
            return None;
        }
        // Both are now non-negative, so the casts are exact on a 64-bit host.
        let (width, height) = (width as usize, height as usize);
        let Some(cells) = width.checked_mul(height) else {
            return None;
        };
        if cells > Self::MAX_CELLS {
            return None;
        }
        Some(GridExtent {
            width,
            height,
            cells,
        })
    }

    /// The column count.
    #[inline]
    #[must_use]
    pub const fn width(self) -> usize {
        self.width
    }

    /// The row count.
    #[inline]
    #[must_use]
    pub const fn height(self) -> usize {
        self.height
    }

    /// The total cell count — `width * height`, proven not to overflow.
    #[inline]
    #[must_use]
    pub const fn cells(self) -> usize {
        self.cells
    }
}

/// The `Grid[T]` payload: a row-major sequence of `GcRef`s plus the fixed
/// column count (width). `items.len() == width * height`. Mirrors `VecPayload`
/// but carries rectangular shape so M8 methods and indexing are cheap.
#[repr(C)]
pub struct GridPayload {
    /// The descriptor for every cell in `items`, or null for "not told yet" —
    /// [`VecPayload::element_descriptor`]'s contract exactly. Read it through
    /// [`GridPayload::element`].
    pub element_descriptor: *const TypeDescriptor,
    /// Row-major cells: `items[row * width + col]`.
    pub items: Vec<GcRef>,
    /// The number of columns (all rows share this width).
    pub width: usize,
}

impl GridPayload {
    /// The cell descriptor, or `None` if this grid was never told its cell
    /// type. `None` implies `items` is empty.
    #[must_use]
    pub fn element(&self) -> Option<&'static TypeDescriptor> {
        // SAFETY: a non-null element descriptor is always a `&'static`.
        (!self.element_descriptor.is_null()).then(|| unsafe { &*self.element_descriptor })
    }
}

unsafe fn grid_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    let p = unsafe { &*(payload as *const GridPayload) };
    for cell in p.items.iter() {
        tracer.trace(*cell);
    }
}

unsafe fn grid_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut GridPayload) };
}

unsafe fn grid_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    let p = unsafe { &*(payload as *const GridPayload) };
    let _ = out.write_str("[");
    let Some(elem_desc) = p.element() else {
        let _ = out.write_str("]");
        return;
    };
    for (i, cell) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let elem_payload = cell.payload::<u8>() as *const u8;
        (elem_desc.format)(elem_payload, out);
    }
    let _ = out.write_str("]");
}

unsafe fn grid_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized GridPayloads.
    let pa = unsafe { &*(a as *const GridPayload) };
    let pb = unsafe { &*(b as *const GridPayload) };
    // Cell type is part of identity (RT-10).
    if !same_element(pa.element_descriptor, pb.element_descriptor) {
        return false;
    }
    if pa.width != pb.width || pa.items.len() != pb.items.len() {
        return false;
    }
    let Some(elem) = pa.element() else {
        return true;
    };
    let Some(eq) = elem.equals else {
        return false;
    };
    for (x, y) in pa.items.iter().zip(pb.items.iter()) {
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        if !eq(xe, ye) {
            return false;
        }
    }
    true
}

unsafe fn grid_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    let p = unsafe { &*(payload as *const GridPayload) };
    let Some(hash_elem) = p.element().and_then(|d| d.hash) else {
        return;
    };
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    hasher.write_bytes(&(p.width as u64).to_le_bytes());
    for cell in p.items.iter() {
        let elem_payload = cell.payload::<u8>() as *const u8;
        hash_elem(elem_payload, hasher);
    }
}

/// Descriptor for the `Grid[T]` collection (M6, §7.8; M8-WS5 enables equality
/// and hashing so a Grid can be used as a map key). Element-wise, like Vec.
pub static GRID: TypeDescriptor = TypeDescriptor::builtin::<GridPayload>(
    BuiltinTypeId::Grid,
    "Grid",
    grid_trace,
    grid_drop,
    grid_format,
    Some(grid_equals),
    Some(grid_hash),
    // Not orderable: only Int/Byte/Char/Float/Text are (ADR-045).
    None,
)
.with_owned_bytes(grid_owned_bytes);

/// The heap bytes `Grid[T]` owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `GridPayload`.
unsafe fn grid_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    let p = unsafe { &*(payload as *const GridPayload) };
    p.items.capacity() * std::mem::size_of::<GcRef>()
}

#[cfg(test)]
mod tests {
    // The Vec descriptor is exercised end-to-end through the Heap in heap.rs
    // (allocation, tracing, collection of nested references). Here we only
    // sanity-check the descriptor is well-formed.
    use super::*;

    #[test]
    fn vec_descriptor_reports_capabilities() {
        assert!(VEC.is_equatable());
        assert!(VEC.is_hashable());
        assert_eq!(VEC.name, "Vec");
    }

    #[test]
    fn empty_vectors_with_different_element_types_are_not_equal() {
        let rt = crate::Runtime::new();
        let ints = rt.alloc_vec(&crate::scalars::INT, Vec::new());
        let floats = rt.alloc_vec(&crate::scalars::FLOAT, Vec::new());

        assert!(
            !ints.equals(&floats),
            "a collection's element descriptor is part of its runtime type identity"
        );
    }
}
