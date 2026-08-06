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
//! **M5 change:** `items` became growable (it was `Box<[GcRef]>` in M3) so
//! `push` mutates the vector *in place* — matching §4.2's "a `var` binding may
//! still point to a mutable object" and §11.1's `push -> Unit` (the receiver is
//! mutated, no new reference returned). The backing storage may reallocate
//! internally, but the `VecPayload` object itself stays at the same GC address
//! (non-moving collector, ADR-011), so existing `GcRef`s remain valid. Per
//! §11.5, runtime wrappers never expose an interior pointer to the vector's
//! backing buffer across a capacity-mutating op; they reload from the payload
//! each call.
//!
//! **ADR-118 change:** `VecPayload.items` is a
//! [`ReprCVec<GcRef>`](crate::repr_c_vec::ReprCVec) rather than a
//! `std::Vec<GcRef>`. Same three words, same size, same growth machinery — but
//! `#[repr(C)]`, so the length and the element pointer are at offsets a backend
//! is allowed to bake in. `DequePayload` and `GridPayload` are deliberately not
//! migrated; see the ADR.

use std::fmt;

use crate::descriptor::{BuiltinTypeId, Tracer, TypeDescriptor};
use crate::repr_c_vec::ReprCVec;
use crate::DynamicHasher;
use crate::GcRef;

/// The `Vec[T]` payload: the element descriptor plus the growable items.
///
/// `items` grows in place (§11.1) and is `Drop`, so [`VEC`]'s `drop_value`
/// releases its buffer on sweep (§12.5). The element descriptor is a `'static`
/// borrow and owns nothing.
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
    /// The elements, in order. Growable (not `Box<[T]>`) so `push` mutates in
    /// place, and a [`ReprCVec`] rather than a `std::Vec` so the length and the
    /// element pointer are at offsets generated code is allowed to know
    /// (ADR-118). `std::Vec` is `#[repr(Rust)]` and hides both inside a private
    /// `RawVec`; nothing else about the field changed, including its size.
    pub items: ReprCVec<GcRef>,
}

// The offsets W4b bakes into generated code, pinned here rather than asserted
// in prose (ADR-118). `element_descriptor` stays at 0 — it is what
// `same_element` and every `element()` call reads, and moving it would be an
// unrelated churn — so `items` starts at 8, its element pointer is at 8 and its
// length at 16.
const _: () = assert!(std::mem::offset_of!(VecPayload, element_descriptor) == 0);
const _: () = assert!(std::mem::offset_of!(VecPayload, items) == 8);
// The block size class does not move: 8 + 24 is the 32 it always was (ADR-109).
const _: () = assert!(std::mem::size_of::<VecPayload>() == 32);

/// The one site generated code may read a `Vec[T]`'s three words through
/// (ADR-118 part 2). `v.len()` reads the length; `v[i]` reads the length for the
/// bounds test and then the element.
///
/// Minted here, beside the payload whose alignment and field offset it names,
/// for `INLINE_INTERN_SITE`'s reason: [`InlineSliceSite::new`] is `pub(crate)`,
/// so the set of payloads generated code may walk is a list this crate wrote.
/// **`GridPayload` is the reason that matters** — it is also a leading word
/// followed by a growable vector, at a different offset and behind a different
/// descriptor, and a `Grid` walked as a `Vec` would read its width as an
/// element pointer.
#[cfg(not(feature = "std-vec-payload"))]
pub const INLINE_VEC_SITE: crate::repr_c_vec::InlineSliceSite =
    crate::repr_c_vec::InlineSliceSite::new(
        BuiltinTypeId::Vec,
        std::mem::align_of::<VecPayload>(),
        std::mem::offset_of!(VecPayload, items),
        std::mem::size_of::<GcRef>(),
    );

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
    // `drop_in_place` runs `ReprCVec`'s `Drop`, which hands the three words back
    // to a `Vec` and lets it free the buffer — the same single free the
    // `std::Vec` field performed before ADR-118, reached the same way. The
    // element descriptor is a static reference and is not owned.
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
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
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
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
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

/// A validated `Vec` length: a non-negative item count the runtime can actually
/// allocate.
///
/// The third of ADR-041 decision 1's validated newtypes, beside [`GridExtent`]
/// and `BitIndex`, and it exists for the same reason they do: `Vec(n, fill)`
/// (ADR-146) takes a user-supplied `Int` to a `vec![fill; n]`, so `n = -1` would
/// cast to `usize::MAX` and ask the host for 147 exabytes — an OOM abort raised
/// inside an `extern "C"` function, which the program that caused it never gets
/// to see. `praxis_vec_filled` cannot reach the allocation without one of these,
/// so the guard is not something a caller can forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VecExtent {
    len: usize,
}

impl VecExtent {
    /// The longest `Vec` the runtime will construct at a stroke: 2^28 items,
    /// which is [`GridExtent::MAX_CELLS`] and is 2 GiB of `GcRef` storage before
    /// a single element object exists.
    ///
    /// The same number as a grid's for the same reason ADR-041 decision 2 gave:
    /// a cell of one and an item of the other are the same eight bytes, and a
    /// count that merely fits in a `usize` is still an allocation no host can
    /// serve. `push` is not bounded by this and does not need to be — it grows
    /// one item at a time, so there is no single multiplication to overflow.
    pub const MAX_ITEMS: usize = GridExtent::MAX_CELLS;

    /// The extent `len` names, or `None` if it is negative or exceeds
    /// [`MAX_ITEMS`](Self::MAX_ITEMS). Zero is legal and names the empty `Vec`.
    #[must_use]
    pub const fn new(len: i64) -> Option<VecExtent> {
        if len < 0 {
            return None;
        }
        // Now non-negative, so the cast is exact on a 64-bit host.
        let len = len as usize;
        if len > Self::MAX_ITEMS {
            return None;
        }
        Some(VecExtent { len })
    }

    /// The item count, proven allocatable.
    #[inline]
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Whether the extent names the empty `Vec`. Present because clippy asks
    /// any type with a `len` for it, and it is the honest answer.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
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
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
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

    /// ADR-041 decision 1's guarantee, for the third newtype: the only route
    /// from a source `Int` to a `Vec` allocation size refuses the two inputs
    /// that used to end the process. Zero is not one of them — the empty `Vec`
    /// is a `Vec`.
    #[test]
    fn a_vec_extent_refuses_a_negative_or_absurd_length() {
        assert!(VecExtent::new(-1).is_none());
        assert!(VecExtent::new(i64::MIN).is_none());
        assert!(VecExtent::new(VecExtent::MAX_ITEMS as i64 + 1).is_none());
        assert!(VecExtent::new(i64::MAX).is_none());

        assert_eq!(VecExtent::new(0).expect("zero is a length").len(), 0);
        assert!(VecExtent::new(0).expect("zero is a length").is_empty());
        assert_eq!(VecExtent::new(7).expect("seven is a length").len(), 7);
        assert_eq!(
            VecExtent::new(VecExtent::MAX_ITEMS as i64)
                .expect("the cap itself is allowed")
                .len(),
            VecExtent::MAX_ITEMS
        );
    }

    /// The two caps are one number, stated once. ADR-041 decision 2 says a
    /// `Vec` item and a `Grid` cell are the same eight bytes, so a change to one
    /// bound that left the other behind would be a judgement made twice and
    /// agreed with once.
    #[test]
    fn a_vecs_cap_is_a_grids_cap() {
        assert_eq!(VecExtent::MAX_ITEMS, GridExtent::MAX_CELLS);
    }

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

    // ADR-118. The three properties W4b will rest on, asserted against a real
    // heap-allocated payload rather than against a standalone `ReprCVec`.

    #[test]
    fn a_vec_payload_is_thirty_two_bytes_with_the_items_at_offset_eight() {
        // Also a `const _` above; repeated as a test because the number is what
        // decides the block's size class (ADR-109) and a silent change to it
        // would move every `Vec` to a different page pool.
        assert_eq!(std::mem::size_of::<VecPayload>(), 32);
        assert_eq!(std::mem::offset_of!(VecPayload, element_descriptor), 0);
        assert_eq!(std::mem::offset_of!(VecPayload, items), 8);
    }

    // Arm-B only. Under `std-vec-payload` the payload holds a `std::Vec`, whose
    // field order is exactly the thing nothing is allowed to assume — so this
    // test failing there is the toggle working, not the toggle broken.
    #[cfg(not(feature = "std-vec-payload"))]
    #[test]
    fn a_backend_can_read_the_length_and_the_elements_out_of_a_live_payload() {
        // The rehearsal for W4b: everything below is a load at a constant
        // displacement from the payload pointer generated code already holds.
        // `praxis_vec_len` is the word at 16; `praxis_vec_get` is a bounds
        // compare against it and a load through the word at 8.
        let rt = crate::Runtime::new();
        let elements: Vec<GcRef> = (0..7_i64).map(|v| rt.alloc_int(v)).collect();
        let vec_ref = rt.alloc_vec(&crate::scalars::INT, elements);

        let base = vec_ref.payload::<u8>().cast_const();
        // SAFETY: `vec_ref` is a live `Vec`, so `base` addresses an initialized
        // `VecPayload` whose layout the `const _` assertions above pin: the
        // element pointer at 8 and the length at 16.
        let (items_ptr, len) = unsafe {
            (
                base.add(8).cast::<*const GcRef>().read(),
                base.add(16).cast::<usize>().read(),
            )
        };
        assert_eq!(len, 7);
        for i in 0..len {
            // SAFETY: `i < len`, and `items_ptr` is the live element buffer.
            let element = unsafe { *items_ptr.add(i) };
            assert!(std::ptr::eq(element.descriptor(), &crate::scalars::INT));
            // SAFETY: the descriptor check above proves the payload is an `Int`.
            assert_eq!(unsafe { *element.payload::<i64>() }, i as i64);
        }
    }

    /// The same three words, reached the way generated code reaches them:
    /// through [`INLINE_VEC_SITE`], from the **object** base, with the header
    /// size folded in by the site rather than added at the emit site.
    ///
    /// The test above is the payload-relative rehearsal W4a owed and W4b's
    /// loads have to agree with; this is the agreement, and the reason it is a
    /// second test rather than an edit of the first is that the two pin
    /// different things — that one pins `VecPayload`'s field order, this one
    /// pins the arithmetic the site performs on top of it.
    #[cfg(not(feature = "std-vec-payload"))]
    #[test]
    fn the_inline_vec_site_addresses_a_live_vec_from_its_object_base() {
        let rt = crate::Runtime::new();
        let elements: Vec<GcRef> = (0..5_i64).map(|v| rt.alloc_int(v)).collect();
        let vec_ref = rt.alloc_vec(&crate::scalars::INT, elements);

        assert!(
            std::ptr::eq(INLINE_VEC_SITE.type_id().descriptor(), vec_ref.descriptor()),
            "the site names the descriptor the proof compares against, and it \
             must be the one a live `Vec` carries"
        );

        let base = vec_ref.as_ptr().cast::<u8>().cast_const();
        // SAFETY: `vec_ref` is a live `Vec` whose descriptor was just checked
        // against the site's, so the site's two displacements address the
        // element pointer and the length of an initialized `ReprCVec`.
        let (items, len) = unsafe {
            (
                base.add(INLINE_VEC_SITE.elements_offset())
                    .cast::<*const GcRef>()
                    .read(),
                base.add(INLINE_VEC_SITE.len_offset())
                    .cast::<usize>()
                    .read(),
            )
        };
        assert_eq!(len, 5);
        assert_eq!(INLINE_VEC_SITE.element_shift(), 3, "a GcRef is eight bytes");
        for i in 0..len {
            // SAFETY: `i < len` and `items` is the live element buffer.
            let element = unsafe { *items.add(i) };
            // SAFETY: every element was allocated as an `Int` above.
            assert_eq!(unsafe { *element.payload::<i64>() }, i as i64);
        }
    }

    #[test]
    fn a_vec_that_reallocates_across_a_collection_keeps_every_element() {
        // The failure this is aimed at: a payload left holding the address of a
        // buffer `RawVec` has already grown away from. `vec_trace` walks
        // `items` on every mark, so a stale pointer here is a wild read inside
        // the collector rather than a wrong answer.
        let rt = crate::Runtime::new();
        let mut scope = crate::roots::RootScope::new();
        let vec_ref = rt.alloc_vec(&crate::scalars::INT, Vec::new());
        scope.root(vec_ref);

        // Outside the intern range (ADR-100), so every element is a real block
        // the sweep can reclaim rather than an immortal it cannot.
        const BASE: i64 = 1_000_000;
        for i in 0..512_i64 {
            let element = rt.alloc_int(BASE + i);
            // Growing the buffer is a Rust `malloc`, not a GC allocation, so
            // nothing collects between the `alloc_int` and the `push` and the
            // element needs no root of its own.
            //
            // SAFETY: `vec_ref` is rooted in `scope` and the collector does not
            // move objects (ADR-011), so the payload address is stable and the
            // only live reference to it is this one.
            unsafe { &mut *vec_ref.payload::<VecPayload>() }
                .items
                .push(element);
        }

        // Garbage the sweep must take, so the collection is a real one.
        for i in 0..64_i64 {
            let _ = rt.alloc_int(BASE + 100_000 + i);
        }
        rt.collect_with(&scope);

        // SAFETY: `vec_ref` was rooted across the collection.
        let p = unsafe { &*vec_ref.payload::<VecPayload>() };
        assert_eq!(p.items.len(), 512);
        assert!(p.items.capacity() >= 512);
        for (i, item) in p.items.iter().enumerate() {
            // SAFETY: every element was traced through `items` and survived.
            assert_eq!(unsafe { *item.payload::<i64>() }, BASE + i as i64);
        }
    }

    #[test]
    fn the_owned_bytes_callback_charges_the_pacer_for_the_whole_buffer() {
        let rt = crate::Runtime::new();
        let elements: Vec<GcRef> = (0..10_i64).map(|v| rt.alloc_int(v)).collect();
        let vec_ref = rt.alloc_vec(&crate::scalars::INT, elements);

        // SAFETY: `vec_ref` is a live `Vec`.
        let p = unsafe { &*vec_ref.payload::<VecPayload>() };
        let expected = p.items.capacity() * std::mem::size_of::<GcRef>();
        // SAFETY: same payload, and `vec_owned_bytes` is `VEC`'s own callback.
        let reported = unsafe { vec_owned_bytes(vec_ref.payload::<u8>() as *const u8) };
        assert_eq!(reported, expected);
        assert!(reported >= 10 * std::mem::size_of::<GcRef>());
    }
}
