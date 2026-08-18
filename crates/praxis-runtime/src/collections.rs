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
//! `items` is growable, so `push` mutates the vector *in place* — matching
//! §4.2's "a `var` binding may still point to a mutable object" and §11.1's
//! `push -> Unit` (the receiver is mutated, no new reference returned). The
//! backing storage may reallocate internally, but the `VecPayload` object itself
//! stays at the same GC address (non-moving collector, ADR-011), so existing
//! `GcRef`s remain valid. Per §11.5, runtime wrappers never expose an interior
//! pointer to the vector's backing buffer across a capacity-mutating op; they
//! reload from the payload each call.
//!
//! `VecPayload.items` is a [`ReprCVec<GcRef>`](crate::repr_c_vec::ReprCVec) and
//! not a `std::Vec<GcRef>`: same three words, same size, same growth machinery,
//! but `#[repr(C)]`, so the length and the element pointer are at offsets a
//! backend is allowed to bake in (ADR-118). `DequePayload` and `GridPayload` are
//! deliberately not migrated; see the ADR.

use std::fmt::Write as _;

use crate::DynamicHasher;
use crate::GcRef;
use crate::descriptor::{BuiltinTypeId, FormatSink, Tracer, TypeDescriptor};
use crate::repr_c_vec::ReprCVec;

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
    /// A vector that has been told its element type is never retagged.
    pub element_descriptor: *const TypeDescriptor,
    /// The elements, in order. Growable (not `Box<[T]>`) so `push` mutates in
    /// place, and a [`ReprCVec`] rather than a `std::Vec` so the length and the
    /// element pointer are at offsets generated code is allowed to know
    /// (ADR-118). `std::Vec` is `#[repr(Rust)]` and hides both inside a private
    /// `RawVec`.
    pub items: ReprCVec<GcRef>,
}

// The offsets generated code bakes in, pinned here rather than asserted in prose
// (ADR-118). `element_descriptor` is at 0 — it is what `same_element` and every
// `element()` call reads — so `items` starts at 8, its element pointer is at 8
// and its length at 16.
const _: () = assert!(std::mem::offset_of!(VecPayload, element_descriptor) == 0);
const _: () = assert!(std::mem::offset_of!(VecPayload, items) == 8);
// 8 + 24 = 32, which is the block size class this payload falls in (ADR-109).
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
    ///
    /// [`ElementSeq::element`] is the body; this forwarder is the *public*
    /// spelling, and it exists because [`ElementSeq`] is `pub(crate)` while one
    /// caller is not: `praxis-codegen-cranelift`'s adversarial audit reads an
    /// empty `Vec[Float]`'s descriptor through this method, which is the check
    /// that codegen labelled the vector before any `push` could repair it.
    #[must_use]
    pub fn element(&self) -> Option<&'static TypeDescriptor> {
        ElementSeq::element(self)
    }
}

impl ElementSeq for VecPayload {
    fn element_descriptor(&self) -> *const TypeDescriptor {
        self.element_descriptor
    }

    fn items(&self) -> impl ExactSizeIterator<Item = GcRef> {
        self.items.iter().copied()
    }

    fn extra_shape(&self) -> Option<u64> {
        None
    }
}

/// Whether two collections agree on their element type.
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
/// **An unlearned label is never compared against a learned one**, which would
/// treat the label as the authority it explicitly is not: a never-inserted
/// `Map[Text, Int]`'s `values()` carries no label, and it must still equal an
/// equally-typed empty `Vec[Int]`.
///
/// Two collections that *have each* been told their element type must agree,
/// so an empty `Vec[Int]` is not an empty `Vec[Text]`.
pub(crate) fn same_element(a: *const TypeDescriptor, b: *const TypeDescriptor) -> bool {
    a.is_null() || b.is_null() || std::ptr::eq(a, b)
}

/// A payload's descriptor **label** slot, as an `Option`. Null means the
/// collection was never told that type — the *absence* of a label rather than
/// the label `Unknown`, which is ADR-066 decision 5 and the same null
/// [`same_element`] above treats as agreeing with anything.
///
/// Every labelled payload in the crate reads its slot through here: the three
/// element sequences via [`ElementSeq::element`], `Map`'s key and value, `Set`'s
/// and `Counter`'s and both heaps' element, and [`crate::repr::instance_repr`]
/// when it recovers a value's type arguments. The dereference is the same one
/// each time, so the reason it is sound is stated once.
#[inline]
pub(crate) fn nullable(d: *const TypeDescriptor) -> Option<&'static TypeDescriptor> {
    // SAFETY: a non-null label is always a `&'static` written by the constructor
    // that built the payload, or by the first store that taught it its type
    // (`adopt_or_reject`). Descriptors are `static`s and outlive every payload.
    (!d.is_null()).then(|| unsafe { &*d })
}

// ===========================================================================
// The element-wise descriptor callbacks, written once (§11.4).
//
// To a descriptor, `Vec`, `Deque` and `Grid` are the same collection: a
// nullable element descriptor and a sequence of `GcRef`s. So trace/format/
// equals/hash are generic over `ElementSeq` and named monomorphised
// (`seq_trace::<VecPayload>`) at each `TypeDescriptor::builtin` call — what the
// descriptor stores is still one direct, payload-specific function, and the
// element rule `same_element` above documents is written once.
//
// Comments elsewhere in the crate, and several ADRs, name these callbacks
// `vec_`/`deque_`/`grid_trace`, `_format`, `_equals` and `_hash`. Those nine
// names are the `seq_*` generics below; there is no function by any of them.
//
// Only `drop` and `owned_bytes` remain per payload, because those *are* per
// payload: three different backing stores to free.
// ===========================================================================

/// What the element-wise callbacks need from a collection payload: a nullable
/// element descriptor, the elements in order, and any further shape that is
/// part of the collection's identity.
pub(crate) trait ElementSeq {
    /// The descriptor for every element, or **null** when the collection has
    /// not been told its element type. Read it through
    /// [`element`](Self::element) rather than dereferencing it.
    fn element_descriptor(&self) -> *const TypeDescriptor;

    /// The elements, in order — row-major for a [`GridPayload`].
    fn items(&self) -> impl ExactSizeIterator<Item = GcRef>;

    /// Shape beyond the element count that is part of the collection's
    /// identity: a [`GridPayload`]'s width, and `None` for the flat sequences,
    /// which have none. A 2×3 `Grid` is not a 3×2 one however its cells fall,
    /// so `equals` compares this and `hash` folds it in.
    fn extra_shape(&self) -> Option<u64>;

    /// The element descriptor, or `None` if this collection was never told its
    /// element type. `None` implies the collection is empty.
    #[must_use]
    fn element(&self) -> Option<&'static TypeDescriptor> {
        nullable(self.element_descriptor())
    }
}

unsafe fn seq_trace<S: ElementSeq>(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized `S`.
    let p = unsafe { &*(payload as *const S) };
    for item in p.items() {
        tracer.trace(item);
    }
}

unsafe fn seq_format<S: ElementSeq>(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an initialized `S`.
    let p = unsafe { &*(payload as *const S) };
    let _ = out.write_str("[");
    // No element descriptor means no elements to format.
    let Some(elem_desc) = p.element() else {
        let _ = out.write_str("]");
        return;
    };
    for (i, item) in p.items().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        // Route element formatting through the element descriptor (§11.4).
        let elem_payload = item.payload::<u8>() as *const u8;
        // SAFETY: the descriptor came from the schema for this slot, so the slot's
        // payload is the type its `format` expects.
        unsafe { (elem_desc.format)(elem_payload, out) };
    }
    let _ = out.write_str("]");
}

unsafe fn seq_equals<S: ElementSeq>(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized `S`s with
    // compatible element descriptors.
    let pa = unsafe { &*(a as *const S) };
    let pb = unsafe { &*(b as *const S) };
    // Runtime element type is part of collection identity — a `Grid`'s cell
    // type no less than a `Vec`'s element type. Without it an empty `Vec[Int]`
    // and an empty `Vec[Text]` compare equal (both are "zero elements") and a
    // non-empty pair dispatches the *left* element descriptor's callback against
    // the right's payloads.
    if !same_element(pa.element_descriptor(), pb.element_descriptor()) {
        return false;
    }
    if pa.extra_shape() != pb.extra_shape() {
        return false;
    }
    if pa.items().len() != pb.items().len() {
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
    for (x, y) in pa.items().zip(pb.items()) {
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        // SAFETY: both slots were just checked to carry the same descriptor, and it
        // is the one whose `equals` this is.
        if !unsafe { eq(xe, ye) } {
            return false;
        }
    }
    true
}

unsafe fn seq_hash<S: ElementSeq>(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized `S`.
    let p = unsafe { &*(payload as *const S) };
    let Some(hash_elem) = p.element().and_then(|d| d.hash) else {
        return;
    };
    // Length first to distinguish prefixes (standard sequence-hash practice),
    // then the rest of the shape, so two collections holding the same cells in
    // the same order but laid out differently do not hash alike.
    hasher.write_bytes(&(p.items().len() as u64).to_le_bytes());
    if let Some(shape) = p.extra_shape() {
        hasher.write_bytes(&shape.to_le_bytes());
    }
    for item in p.items() {
        let elem_payload = item.payload::<u8>() as *const u8;
        // SAFETY: the descriptor came from the schema for this slot, so the slot's
        // payload is the type its `hash` expects.
        unsafe { hash_elem(elem_payload, hasher) };
    }
}

unsafe fn vec_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized `VecPayload`.
    // `drop_in_place` runs `ReprCVec`'s `Drop`, which hands the three words back
    // to a `Vec` and lets it free the buffer — one single free. The element
    // descriptor is a static reference and is not owned.
    unsafe { std::ptr::drop_in_place(payload as *mut VecPayload) };
}

/// Descriptor for the `Vec[T]` collection (§11.2). The per-instance element
/// type lives in the payload, so a single descriptor serves all `Vec[T]`.
pub static VEC: TypeDescriptor = TypeDescriptor::builtin::<VecPayload>(
    BuiltinTypeId::Vec,
    "Vec",
    seq_trace::<VecPayload>,
    vec_drop,
    seq_format::<VecPayload>,
    Some(seq_equals::<VecPayload>),
    Some(seq_hash::<VecPayload>),
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(vec_owned_bytes);

impl VecPayload {
    /// The heap bytes this payload owns beyond its GC block, for GC pacing —
    /// the buffer, not the spine's three words. `capacity`, not `len`: the
    /// buffer's real footprint is what the collector is paced against.
    ///
    /// **One statement of the size, with two readers** (ADR-121). The
    /// descriptor's `owned_bytes` callback charges it once at construction;
    /// the ABI wrapper that can *grow* this collection reads it either side of
    /// the mutation and charges the delta through
    /// [`Heap::charge_owned_growth`](crate::heap::Heap::charge_owned_growth),
    /// so the pacer sees a buffer that doubled. Writing the capacity arithmetic
    /// at the growth site instead would be a second spelling of this line, and
    /// the two would drift the first time an element type changed width.
    ///
    /// **This is the statement, for every payload in the crate.** The rule is
    /// the same for a `Deque`'s ring, a `Grid`'s cells, a `Map`'s table, a
    /// heap's array and a `BitSet`'s words, so each of those `owned_bytes`
    /// methods says what it multiplies and points back here for why.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<GcRef>()
    }
}

unsafe fn vec_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized VecPayload.
    let p = unsafe { &*(payload as *const VecPayload) };
    p.owned_bytes()
}

// ===========================================================================
// Deque[T] (§6.1). A double-ended queue backed by Rust's `VecDeque`.
// Mirrors `VecPayload` exactly (element descriptor + growable items) — it is an
// `ElementSeq` like the other two, so trace/format/equals/hash are not merely
// identical but the same bodies; only the backing store and the front/back
// method surface differ.
// ===========================================================================

use std::collections::VecDeque;

/// The `Deque[T]` payload: the element descriptor plus a growable `VecDeque`.
/// `VecDeque` (not `Vec`) so `push_front`/`pop_front` are O(1) amortized.
/// Both fields are `Drop`, so [`DEQUE`]'s `drop_value` releases them on sweep.
#[repr(C)]
pub struct DequePayload {
    /// The descriptor for every element, or null for "not told yet" —
    /// [`VecPayload::element_descriptor`]'s contract exactly. Read it through
    /// [`ElementSeq::element`].
    pub element_descriptor: *const TypeDescriptor,
    /// The elements. A `VecDeque` so both ends are cheap to mutate.
    pub items: VecDeque<GcRef>,
}

impl ElementSeq for DequePayload {
    fn element_descriptor(&self) -> *const TypeDescriptor {
        self.element_descriptor
    }

    fn items(&self) -> impl ExactSizeIterator<Item = GcRef> {
        self.items.iter().copied()
    }

    fn extra_shape(&self) -> Option<u64> {
        None
    }
}

unsafe fn deque_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    unsafe { std::ptr::drop_in_place(payload as *mut DequePayload) };
}

/// Descriptor for the `Deque[T]` collection (§6.1). The per-instance element
/// type lives in the payload, so a single descriptor serves all `Deque[T]`.
pub static DEQUE: TypeDescriptor = TypeDescriptor::builtin::<DequePayload>(
    BuiltinTypeId::Deque,
    "Deque",
    seq_trace::<DequePayload>,
    deque_drop,
    seq_format::<DequePayload>,
    Some(seq_equals::<DequePayload>),
    Some(seq_hash::<DequePayload>),
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(deque_owned_bytes);

impl DequePayload {
    /// The ring buffer this payload owns beyond its GC block, for GC pacing —
    /// `capacity`, not `len`.
    ///
    /// One statement of the size, with two readers (ADR-121):
    /// [`VecPayload::owned_bytes`] is that statement.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<GcRef>()
    }
}

unsafe fn deque_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized DequePayload.
    let p = unsafe { &*(payload as *const DequePayload) };
    p.owned_bytes()
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
// Grid[T] (§7.5 `grid`, §7.8 type derivation). Row-major storage with a known
// width, so the synthesized type is the spec-faithful `Grid[T]`.
// ===========================================================================

/// A validated grid shape: a non-negative width and height whose product is a
/// cell count the runtime can actually allocate.
///
/// This is the *only* route from a user-supplied `Int` pair to a cell count.
/// Reaching `vec![unit; (w as usize) * (h as usize)]` directly would let
/// `w = -1` become `usize::MAX` and the product overflow — either an allocation
/// the host cannot serve (an OOM abort) or a capacity-overflow panic, both
/// crossing `extern "C"`. Neither is expressible here: `GridExtent` holds
/// `usize`s, and the multiplication it proves is the one `cells()` returns.
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
/// but carries rectangular shape so indexing and neighbourhood walks are cheap.
#[repr(C)]
pub struct GridPayload {
    /// The descriptor for every cell in `items`, or null for "not told yet" —
    /// [`VecPayload::element_descriptor`]'s contract exactly. Read it through
    /// [`ElementSeq::element`].
    pub element_descriptor: *const TypeDescriptor,
    /// Row-major cells: `items[row * width + col]`.
    pub items: Vec<GcRef>,
    /// The number of columns (all rows share this width).
    pub width: usize,
}

impl ElementSeq for GridPayload {
    fn element_descriptor(&self) -> *const TypeDescriptor {
        self.element_descriptor
    }

    fn items(&self) -> impl ExactSizeIterator<Item = GcRef> {
        self.items.iter().copied()
    }

    /// The width, which is the shape a flat sequence does not have: the same
    /// cells in the same order are a different `Grid` at a different width, so
    /// `equals` and `hash` must see it.
    fn extra_shape(&self) -> Option<u64> {
        Some(self.width as u64)
    }
}

unsafe fn grid_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    unsafe { std::ptr::drop_in_place(payload as *mut GridPayload) };
}

/// Descriptor for the `Grid[T]` collection (§7.8). Element-wise, like Vec —
/// literally so: the callbacks are the shared `ElementSeq` ones, and the width
/// reaches them as `extra_shape`. It is equatable and hashable, but not a
/// `Map` key: that requires immutability too (ADR-057 D4).
pub static GRID: TypeDescriptor = TypeDescriptor::builtin::<GridPayload>(
    BuiltinTypeId::Grid,
    "Grid",
    seq_trace::<GridPayload>,
    grid_drop,
    seq_format::<GridPayload>,
    Some(seq_equals::<GridPayload>),
    Some(seq_hash::<GridPayload>),
    // No container order: a mutable collection can never be a `Map` key or a
    // `Set` member (ADR-057 D4), so nothing ever has to put one in a
    // deterministic sequence (ADR-138).
    None,
)
.with_owned_bytes(grid_owned_bytes);

impl GridPayload {
    /// The row-major cell buffer this payload owns beyond its GC block, for GC
    /// pacing — `capacity`, not `cells()`. A grid is built at its full extent,
    /// but what the allocator was asked for is still the vector's.
    ///
    /// One statement of the size, with two readers (ADR-121):
    /// [`VecPayload::owned_bytes`] is that statement.
    #[must_use]
    pub(crate) fn owned_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<GcRef>()
    }
}

unsafe fn grid_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized GridPayload.
    let p = unsafe { &*(payload as *const GridPayload) };
    p.owned_bytes()
}

#[cfg(test)]
mod tests {
    // The Vec descriptor is exercised end-to-end through the Heap in heap.rs
    // (allocation, tracing, collection of nested references). Here we only
    // sanity-check the descriptor is well-formed.
    use super::*;

    /// ADR-041 decision 1's guarantee, for the third newtype: the only route
    /// from a source `Int` to a `Vec` allocation size refuses a negative length
    /// and one past the cap — the two that would otherwise end the process.
    /// Zero is not one of them: the empty `Vec` is a `Vec`.
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

    /// The one thing a `Grid` adds to the shared element-wise callbacks: its
    /// width is part of its identity, and it reaches `seq_equals` as
    /// [`ElementSeq::extra_shape`]. Same cells, same order, different rectangle.
    #[test]
    fn grids_that_differ_only_in_width_are_not_equal() {
        let rt = crate::Runtime::new();
        let cells: Vec<GcRef> = (0..6_i64).map(|v| rt.alloc_int(v)).collect();
        let two_wide = rt.alloc_grid(&crate::scalars::INT, cells.clone(), 2);
        let three_wide = rt.alloc_grid(&crate::scalars::INT, cells.clone(), 3);
        let also_two_wide = rt.alloc_grid(&crate::scalars::INT, cells, 2);

        assert!(
            !two_wide.equals(&three_wide),
            "a 3×2 grid is not a 2×3 one however its cells fall"
        );
        assert!(
            two_wide.equals(&also_two_wide),
            "the shape check must not reject two grids that agree on it"
        );
    }

    // ADR-118. The three layout properties generated code rests on, asserted
    // against a real heap-allocated payload rather than a standalone
    // `ReprCVec`.

    #[test]
    fn a_vec_payload_is_thirty_two_bytes_with_the_items_at_offset_eight() {
        // Also a `const _` above; repeated as a test because the number is what
        // decides the block's size class (ADR-109) and a silent change to it
        // would move every `Vec` to a different page pool.
        assert_eq!(std::mem::size_of::<VecPayload>(), 32);
        assert_eq!(std::mem::offset_of!(VecPayload, element_descriptor), 0);
        assert_eq!(std::mem::offset_of!(VecPayload, items), 8);
    }

    // Only without `std-vec-payload`: under that feature the payload holds a
    // `std::Vec`, whose field order is exactly the thing nothing is allowed to
    // assume — so this test failing there is the toggle working, not broken.
    #[cfg(not(feature = "std-vec-payload"))]
    #[test]
    fn a_backend_can_read_the_length_and_the_elements_out_of_a_live_payload() {
        // Everything below is a load at a constant displacement from the payload
        // pointer generated code already holds. `praxis_vec_len` is the word at
        // 16; `praxis_vec_get` is a bounds compare against it and a load through
        // the word at 8.
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
    /// A second test rather than an edit of the one above because the two pin
    /// different things: that one pins `VecPayload`'s field order, this one pins
    /// the arithmetic the site performs on top of it.
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
        // buffer `RawVec` has already grown away from. `seq_trace` walks
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
