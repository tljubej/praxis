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
//! is equatable/hashable iff every element is.

use std::fmt;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, Tracer, TypeDescriptor};
use crate::GcRef;

/// The static shape of a tuple: an ordered list of element descriptors (positional,
/// no names). Leaked to `&'static` once per distinct shape by the codegen.
#[repr(C)]
pub struct TupleSchema {
    pub descriptors: &'static [*const TypeDescriptor],
}

impl TupleSchema {
    /// The number of elements in this tuple shape (its arity).
    pub fn arity(&self) -> usize {
        self.descriptors.len()
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
    #[must_use]
    pub fn same_shape(&self, other: &TupleSchema) -> bool {
        self.descriptors.len() == other.descriptors.len()
            && self
                .descriptors
                .iter()
                .zip(other.descriptors.iter())
                .all(|(a, b)| std::ptr::eq(*a, *b))
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

unsafe fn tuple_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized TuplePayload.
    let p = unsafe { &*(payload as *const TuplePayload) };
    let schema = unsafe { &*p.schema };
    let _ = out.write_str("(");
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let elem_desc = unsafe { &*schema.descriptors[i] };
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
        let Some(eq) = unsafe { &*schema.descriptors[i] }.equals else {
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
        // land in different buckets.
        let elem_desc = unsafe { &*schema.descriptors[i] };
        hasher.write_bytes(&elem_desc.id().to_u32().to_le_bytes());
        // If the element type is not hashable, the tuple is not hashable (§5.5).
        let Some(hash_elem) = elem_desc.hash else {
            return;
        };
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_elem(elem_payload, hasher);
    }
}

/// Descriptor for the `Tuple` value type (M7, §4.5). Structural equality and
/// hashing (§5.5) recurse element-wise through the per-shape schema's element
/// descriptors. A tuple is equatable/hashable iff every element is; functions
/// never are, so a tuple containing a function is neither.
pub static TUPLE: TypeDescriptor = TypeDescriptor::builtin::<TuplePayload>(
    BuiltinTypeId::Tuple,
    "Tuple",
    tuple_trace,
    tuple_drop,
    tuple_format,
    Some(tuple_equals),
    Some(tuple_hash),
    // Ordering: see the ordering ADR; no built-in declares `compare` yet.
    None,
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
}
