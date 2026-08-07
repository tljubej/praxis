//! The `Tuple` value descriptor (§4.5 structural tuples, M7).
//!
//! A tuple is an anonymous, positional product: a fixed number of elements, each
//! a `GcRef`, in source order. Unlike records (§4.5) tuples carry no field names
//! — identity is the element-type sequence alone (so `(Int, Int)` and `(Int, Bool)`
//! are distinct, and `(Int, Int)` is one shape regardless of where it appears).
//!
//! Each distinct tuple *shape* (element descriptor sequence) gets a
//! [`TupleSchema`]. The schema is leaked to `&'static` (one per shape) because a
//! tuple's descriptor callbacks need a type-stable home for the element
//! descriptors; this mirrors how the codegen leaks `RecordSchema` and function
//! names.
//!
//! The descriptor dispatches element-wise through the schema (§11.4) — there are
//! no scattered type switches. A single `TUPLE`-shaped descriptor serves every
//! tuple because the per-shape knowledge lives in the schema referenced from the
//! payload. Structural equality and hashing (§5.5) recurse element-wise; a tuple
//! is equatable/hashable iff every element is. The ordering a container imposes
//! (ADR-138) recurses the same way — a `Map[(Int, Int), V]` walks its keys
//! element-wise — which is a different question from the source-level `<`, and
//! `(1, 2) < (1, 3)` is still refused at check time.

use std::fmt::Write as _;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, FormatSink, Tracer, TypeDescriptor};
use crate::GcRef;

/// The static shape of a tuple: an ordered list of element descriptors (positional,
/// no names). Leaked to `&'static` once per distinct shape by the codegen.
///
/// A slot may be **null**, meaning the compiler had no static type for that
/// element — the same honest encoding a `Vec`'s element descriptor already uses
/// (HIR-01/MONO-01: `var m = Map()` generalizes at the `var`, so a use whose
/// program never inspects the elements leaves them unresolved). The arity is
/// still exact, so nothing is lost; the *value's own* descriptor answers for a
/// null slot, and it is read from the object's header, so it is never wrong.
#[repr(C)]
pub struct TupleSchema {
    pub descriptors: &'static [*const TypeDescriptor],
}

impl TupleSchema {
    /// The number of elements in this tuple shape (its arity).
    pub fn arity(&self) -> usize {
        self.descriptors.len()
    }

    /// The descriptor to dispatch slot `i` through for `value`: the static one
    /// when the compiler had it, and the value's own otherwise.
    ///
    /// Falling back to the header is what makes a null slot safe rather than
    /// merely tolerated — the alternative that was there first, refusing to
    /// compile, rejected `var m = Map()` followed by a `for` that never looks
    /// inside the pair (REP-15).
    fn descriptor_at(&self, i: usize, value: GcRef) -> &'static TypeDescriptor {
        match self.descriptors.get(i).copied() {
            Some(d) if !d.is_null() => {
                // SAFETY: a non-null slot is a `'static` descriptor pointer.
                unsafe { &*d }
            }
            _ => value.descriptor(),
        }
    }

    /// Whether two schemas describe the *same* tuple shape: equal arity and the
    /// same element descriptor in every slot.
    ///
    /// Shape, not allocation identity (RT-11). Schemas are interned per shape
    /// *within* a producer, but there are three producers — the codegen's
    /// `tuple_schema_for` cache, the runtime's `point_schema`, and the input
    /// parser — and two of them minting an `(Int, Int)` used to yield tuples
    /// that compared unequal to each other. Descriptors are `static`, so slot
    /// comparison is pointer comparison (ADR-038).
    ///
    /// A **null** slot is unknown, not a fourth type: it agrees with whatever
    /// the other side says, and the values decide (see `tuple_equals`, which
    /// compares the two objects' own descriptors for such a slot).
    #[must_use]
    pub fn same_shape(&self, other: &TupleSchema) -> bool {
        self.descriptors.len() == other.descriptors.len()
            && self
                .descriptors
                .iter()
                .zip(other.descriptors.iter())
                .all(|(a, b)| a.is_null() || b.is_null() || std::ptr::eq(*a, *b))
    }
}

/// The `Tuple` payload: a pointer to the static schema plus the element values
/// (one `GcRef` per element, in schema order).
#[repr(C)]
pub struct TuplePayload {
    /// The static element shape. `items.len()` must equal `schema.arity()`.
    pub schema: *const TupleSchema,
    /// Element values in schema (positional) order.
    pub items: Vec<GcRef>,
}

unsafe fn tuple_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized TuplePayload.
    let p = unsafe { &*(payload as *const TuplePayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn tuple_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized TuplePayload.
    // `drop_in_place` frees the items Vec; the schema is static and not owned.
    unsafe { std::ptr::drop_in_place(payload as *mut TuplePayload) };
}

unsafe fn tuple_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an initialized TuplePayload.
    let p = unsafe { &*(payload as *const TuplePayload) };
    let schema = unsafe { &*p.schema };
    let _ = out.write_str("(");
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let elem_desc = schema.descriptor_at(i, *item);
        (elem_desc.format)(item.payload::<u8>() as *const u8, out);
    }
    let _ = out.write_str(")");
}

unsafe fn tuple_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized TuplePayloads
    // with compatible element descriptors.
    let pa = unsafe { &*(a as *const TuplePayload) };
    let pb = unsafe { &*(b as *const TuplePayload) };
    // Structural equality is shape + element-wise equality (§5.5). Shape is
    // compared slot by slot, not by schema *address*: three independent
    // producers intern schemas, so two `(Int, Int)` tuples could hold different
    // pointers to the same shape and compare unequal (RT-11).
    if pa.schema.is_null() || pb.schema.is_null() {
        return false;
    }
    if !unsafe { (*pa.schema).same_shape(&*pb.schema) } {
        return false;
    }
    if pa.items.len() != pb.items.len() {
        return false;
    }
    let schema = unsafe { &*pa.schema };
    // Element-wise equality through the element descriptor (§11.4). If the
    // element type is not equatable, the tuple is not equatable (§5.5).
    for (i, (x, y)) in pa.items.iter().zip(pb.items.iter()).enumerate() {
        let desc = schema.descriptor_at(i, *x);
        // For a slot the compiler had no type for, `desc` is `x`'s own — so `y`
        // must carry the same one before its payload is read through it. Two
        // values of different types are unequal; reading one as the other is the
        // wrong-payload read P0-11 is about.
        if !std::ptr::eq(desc, schema.descriptor_at(i, *y)) {
            return false;
        }
        let Some(eq) = desc.equals else {
            return false;
        };
        let xe = x.payload::<u8>() as *const u8;
        let ye = y.payload::<u8>() as *const u8;
        if !eq(xe, ye) {
            return false;
        }
    }
    true
}

unsafe fn tuple_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized TuplePayload.
    let p = unsafe { &*(payload as *const TuplePayload) };
    let schema = unsafe { &*p.schema };
    // Length first to distinguish prefixes (standard sequence-hash practice).
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for (i, item) in p.items.iter().enumerate() {
        // The slot's type is part of the shape `eq` now compares, so it is part
        // of the hash too — two tuples that differ only in shape must be free to
        // land in different buckets. Reading it off the *value* for an unknown
        // slot is what keeps hash and eq agreeing there: both ask the object.
        let elem_desc = schema.descriptor_at(i, *item);
        hasher.write_bytes(&elem_desc.id().to_u32().to_le_bytes());
        // If the element type is not hashable, the tuple is not hashable (§5.5).
        let Some(hash_elem) = elem_desc.hash else {
            return;
        };
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_elem(elem_payload, hasher);
    }
}

unsafe fn tuple_compare(a: *const u8, b: *const u8) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // SAFETY: caller guarantees both pointers point at initialized TuplePayloads.
    let pa = unsafe { &*(a as *const TuplePayload) };
    let pb = unsafe { &*(b as *const TuplePayload) };
    // A null schema is a producer bug rather than a user-reachable state, but it
    // still has to get an answer, and the answer has to be the same one twice —
    // so it sorts first, by a rule and not by whatever the hash table happened
    // to yield (ADR-138).
    match (pa.schema.is_null(), pb.schema.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => {}
    }
    // Arity first, so a prefix orders before its extension — the same reason
    // `tuple_hash` writes the length first, and what keeps `(1,)` and `(1, 0)`
    // from colliding on their shared first element.
    match pa.items.len().cmp(&pb.items.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    // SAFETY: both checked non-null above.
    let (schema_a, schema_b) = unsafe { (&*pa.schema, &*pb.schema) };
    // Element-wise, short-circuiting at the first difference. Each side is
    // dispatched through *its own* schema slot, falling back to the value's own
    // descriptor for a null one, exactly as `tuple_equals` and `tuple_format`
    // do — and `slot_cmp` separates two slots of different types by descriptor
    // id before it reads either payload, so a mismatched pair is ordered rather
    // than misread.
    for (i, (x, y)) in pa.items.iter().zip(pb.items.iter()).enumerate() {
        let dx = schema_a.descriptor_at(i, *x);
        let dy = schema_b.descriptor_at(i, *y);
        // SAFETY: each element's payload matches the descriptor its schema slot
        // names, or its own header's when the slot is null.
        match unsafe { crate::ordering::slot_cmp(*x, *y, dx, dy) } {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// Descriptor for the `Tuple` value type (M7, §4.5). Structural equality,
/// hashing (§5.5) and the container ordering (ADR-138) all recurse element-wise
/// through the per-shape schema's element descriptors. A tuple is
/// equatable/hashable iff every element is; functions never are, so a tuple
/// containing a function is neither.
pub static TUPLE: TypeDescriptor = TypeDescriptor::builtin::<TuplePayload>(
    BuiltinTypeId::Tuple,
    "Tuple",
    tuple_trace,
    tuple_drop,
    tuple_format,
    Some(tuple_equals),
    Some(tuple_hash),
    // A tuple is the workhorse composite key — `Map[(Int, Int), V]` is how a
    // grid memo is spelled — so a container has to order one (ADR-138).
    // `(1, 2) < (1, 3)` in source is still Y006: see `capability::supports_ord`.
    Some(tuple_compare),
)
.with_owned_bytes(tuple_owned_bytes);

/// The heap bytes a tuple owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `TuplePayload`.
unsafe fn tuple_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized TuplePayload.
    let p = unsafe { &*(payload as *const TuplePayload) };
    p.items.capacity() * std::mem::size_of::<GcRef>()
}

/// The cached `'static` schema for a `(Int, Int)` point tuple. Used by Grid
/// methods that return `(x, y)` points (§6.4). Built once and leaked; the two
/// element descriptors are both `INT`. This avoids the codegen round-trip for
/// tuple schemas when the runtime allocates points directly.
pub fn point_schema() -> &'static TupleSchema {
    use std::sync::OnceLock;
    // `*const TypeDescriptor` is not `Sync`, so wrap the leaked slice pointer in
    // a `Send + Sync` newtype (the underlying static descriptors outlive all
    // threads — mirroring the `SendPtr` idiom used by the codegen's tuple cache).
    struct SyncPtr(&'static TupleSchema);
    unsafe impl Send for SyncPtr {}
    unsafe impl Sync for SyncPtr {}
    static POINT: OnceLock<SyncPtr> = OnceLock::new();
    POINT
        .get_or_init(|| {
            let descriptors: &'static [*const TypeDescriptor] = Box::leak(
                vec![
                    &crate::scalars::INT as *const _,
                    &crate::scalars::INT as *const _,
                ]
                .into_boxed_slice(),
            );
            SyncPtr(Box::leak(Box::new(TupleSchema { descriptors })))
        })
        .0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{praxis_alloc_tuple, praxis_tuple_set};

    #[test]
    fn tuple_descriptor_reports_capabilities() {
        assert!(TUPLE.is_equatable());
        assert!(TUPLE.is_hashable());
        assert_eq!(TUPLE.name, "Tuple");
        assert_eq!(TUPLE.as_builtin(), Some(BuiltinTypeId::Tuple));
    }

    #[test]
    fn alloc_tuple_round_trips_arity() {
        // Allocate a 2-tuple via the ABI wrapper and verify both element slots
        // round-trip through praxis_tuple_get.
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        // Build a 2-element schema of INT descriptors.
        let descriptors: &'static [*const TypeDescriptor] =
            Box::leak(vec![&crate::scalars::INT as *const TypeDescriptor; 2].into_boxed_slice());
        let schema = Box::leak(Box::new(TupleSchema { descriptors }));
        let tref = unsafe { praxis_alloc_tuple(&mut ctx, schema) };
        // The schema pointer must be embedded in the payload.
        let payload = tref.payload::<u8>() as *const TuplePayload;
        let embedded = unsafe { (*payload).schema };
        assert_eq!(embedded, schema as *const TupleSchema);
        assert_eq!(unsafe { (*payload).items.len() }, 2);
    }

    /// **REP-15's collateral.** A schema slot may be **null** — the compiler had
    /// no static type for that element — and the value's own descriptor answers
    /// for it.
    ///
    /// `var m = Map()` followed by a `for kv in m` whose body never opens the
    /// pair is the program that produces one: nothing ever resolves K or V, and
    /// refusing to compile it (which is what happened first) rejects a working
    /// program. It is the same answer `collection_arg_descriptor` already gives
    /// an unresolved element type, and the header is why it is safe rather than
    /// merely permissive — an object always knows what it is.
    #[test]
    fn an_unknown_schema_slot_reads_the_values_own_descriptor() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let unknown: &'static TupleSchema = Box::leak(Box::new(TupleSchema {
            descriptors: Box::leak(vec![std::ptr::null(); 2].into_boxed_slice()),
        }));
        let build = |ctx: &mut crate::RuntimeContext, a: GcRef, b: GcRef| {
            let t = unsafe { praxis_alloc_tuple(ctx, unknown) };
            unsafe {
                praxis_tuple_set(ctx, t, 0, a);
                praxis_tuple_set(ctx, t, 1, b);
            }
            t
        };

        // Arity is still exact, so both elements are stored rather than dropped.
        let (one, txt) = (rt.alloc_int(1), rt.alloc_text("hi"));
        let mixed = build(&mut ctx, one, txt);
        assert_eq!(
            unsafe { (*(mixed.payload::<TuplePayload>())).items.len() },
            2
        );
        // Formatting reads each element through its own descriptor, so the Text
        // renders as a Text and not as an `i64` read of its buffer pointer.
        let mut rendered = String::new();
        unsafe {
            tuple_format(
                mixed.payload::<u8>() as *const u8,
                &mut crate::FormatSink::display(&mut rendered),
            );
        }
        assert_eq!(rendered, "(1, hi)");

        // Equality still works, and still distinguishes.
        let same = build(&mut ctx, rt.alloc_int(1), rt.alloc_text("hi"));
        let other = build(&mut ctx, rt.alloc_int(1), rt.alloc_text("no"));
        assert!(mixed.equals(&same));
        assert!(!mixed.equals(&other));

        // Two values of *different* types in one slot are unequal rather than
        // one being read as the other (P0-11).
        let swapped = build(&mut ctx, rt.alloc_text("hi"), rt.alloc_int(1));
        assert!(!mixed.equals(&swapped));

        // An unknown slot agrees with a known one rather than contradicting it,
        // so a `(?, ?)` and an `(Int, Int)` holding the same values are equal.
        let known = unsafe { praxis_alloc_tuple(&mut ctx, point_schema()) };
        for (index, value) in [3_i64, 4].into_iter().enumerate() {
            let v = rt.alloc_int(value);
            unsafe { praxis_tuple_set(&mut ctx, known, index as i64, v) };
        }
        let unknown_pair = build(&mut ctx, rt.alloc_int(3), rt.alloc_int(4));
        assert!(known.equals(&unknown_pair));
        assert!(unknown_pair.equals(&known));
    }

    #[test]
    fn tuple_equality_uses_shape_not_schema_allocation_identity() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let runtime_schema = point_schema();
        let independently_interned_schema = Box::leak(Box::new(TupleSchema {
            descriptors: Box::leak(
                vec![
                    &crate::scalars::INT as *const TypeDescriptor,
                    &crate::scalars::INT as *const TypeDescriptor,
                ]
                .into_boxed_slice(),
            ),
        }));
        let left = unsafe { praxis_alloc_tuple(&mut ctx, runtime_schema) };
        let right = unsafe { praxis_alloc_tuple(&mut ctx, independently_interned_schema) };
        for (index, value) in [3_i64, 4].into_iter().enumerate() {
            let left_value = rt.alloc_int(value);
            let right_value = rt.alloc_int(value);
            unsafe {
                praxis_tuple_set(&mut ctx, left, index as i64, left_value);
                praxis_tuple_set(&mut ctx, right, index as i64, right_value);
            }
        }

        assert!(
            left.equals(&right),
            "equivalent (Int, Int) schemas from runtime and codegen must describe the same tuple type"
        );
    }

    /// A tuple's container order is arity first, then element-wise left to
    /// right (ADR-138). Arity first is what keeps `(1,)` ahead of `(1, 0)` — a
    /// prefix before its extension, the same reason `tuple_hash` writes the
    /// length first — and element-wise is what makes a `Map[(Int, Int), V]`
    /// come out in reading order instead of by the printed pair.
    #[test]
    fn tuple_compare_is_arity_first_then_element_wise() {
        let mut rt = crate::Runtime::new();
        let ints = |n: usize| -> &'static TupleSchema {
            Box::leak(Box::new(TupleSchema {
                descriptors: Box::leak(
                    vec![&crate::scalars::INT as *const TypeDescriptor; n].into_boxed_slice(),
                ),
            }))
        };
        let one_slot = ints(1);
        let two_slots = ints(2);
        let unknown: &'static TupleSchema = Box::leak(Box::new(TupleSchema {
            descriptors: Box::leak(vec![std::ptr::null(); 1].into_boxed_slice()),
        }));

        let values: Vec<GcRef> = [1_i64, 0, 1, 2, 10]
            .iter()
            .map(|&n| rt.alloc_int(n))
            .collect();
        let text = rt.alloc_text("hi");
        let mut ctx = rt.context();
        let build =
            |ctx: &mut crate::RuntimeContext, schema: &'static TupleSchema, items: &[GcRef]| {
                // SAFETY: a live context, and each value matches the slot the
                // schema names (or the slot is null and the value answers).
                unsafe {
                    let t = praxis_alloc_tuple(ctx, schema);
                    for (i, v) in items.iter().enumerate() {
                        praxis_tuple_set(ctx, t, i as i64, *v);
                    }
                    t
                }
            };
        let cmp = |a: GcRef, b: GcRef| unsafe {
            tuple_compare(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        };

        // Arity first: a one-element tuple precedes any two-element one.
        let single = build(&mut ctx, one_slot, &[values[0]]);
        let pair = build(&mut ctx, two_slots, &[values[0], values[1]]);
        assert_eq!(cmp(single, pair), std::cmp::Ordering::Less);
        assert_eq!(cmp(pair, single), std::cmp::Ordering::Greater);

        // Then element-wise, through each element's own order: `(1, 2)` before
        // `(1, 10)`, which the rendered pair would reverse.
        let low = build(&mut ctx, two_slots, &[values[2], values[3]]);
        let high = build(&mut ctx, two_slots, &[values[2], values[4]]);
        assert_eq!(cmp(low, high), std::cmp::Ordering::Less);
        assert_eq!(cmp(low, low), std::cmp::Ordering::Equal);

        // A null slot whose two values are of different types is separated by
        // descriptor id rather than read as one another's layout — the same
        // rule `tuple_equals` applies, arriving at an order instead of `false`.
        let an_int = build(&mut ctx, unknown, &[values[0]]);
        let a_text = build(&mut ctx, unknown, &[text]);
        assert_eq!(cmp(an_int, a_text), cmp(an_int, a_text));
        assert_eq!(cmp(an_int, a_text), cmp(a_text, an_int).reverse());
        assert_ne!(cmp(an_int, a_text), std::cmp::Ordering::Equal);
    }
}
