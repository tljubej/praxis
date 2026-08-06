//! The anonymous structural `Record` descriptor (§7.8, provisional in M6).
//!
//! The input parser's named-capture templates produce anonymous records, e.g.
//! `lines(`{x:int},{y:int}`)` → `Vec[{x:Int,y:Int}]`. Nominal records and enums
//! formally land in M7; M6 ships a **provisional structural record** that holds
//! exactly what parser results need: a fixed set of named fields, each a `GcRef`.
//!
//! Each distinct record *shape* (field names + element descriptors) gets a
//! [`RecordSchema`]. The schema is leaked to `&'static` (one per parser plan)
//! because a record's descriptor callbacks need a type-stable home for the
//! field descriptors; this matches how the JIT leaks function-name strings.
//!
//! The descriptor dispatches element-wise through the schema (§11.4) — there are
//! no scattered type switches in formatting/tracing. A single `RECORD`-shaped
//! descriptor serves every record because the per-shape knowledge lives in the
//! schema referenced from the payload.

use std::fmt;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, Tracer, TypeDescriptor};
use crate::GcRef;

/// One field of a record shape: its source name plus the descriptor for the
/// values stored at that field. The descriptor pointer is `const` data shared
/// across all records of this shape.
#[repr(C)]
pub struct RecordField {
    pub name: &'static str,
    pub descriptor: *const TypeDescriptor,
}

/// Which *type* a record schema describes — the half of a record's identity
/// that its field list cannot express (RT-12).
///
/// `struct Point { x: Int, y: Int }` and `struct Vector { x: Int, y: Int }` are
/// different types with one shape, and §5.6's anonymous records are the
/// opposite case: the same shape *is* the same type, however many times it is
/// built. One enum distinguishes them.
///
/// F12 will replace the name with a `DefId + args` key once nominal identity
/// carries type arguments; until then the declared name is the identity, and it
/// is compared alongside the shape (see [`RecordSchema::same_type`]) so a
/// generic record's two instantiations do not collide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub enum SchemaIdentity {
    /// A structural record (§5.6): identity is the field shape alone. What the
    /// input parser's named-capture templates produce.
    Anonymous,
    /// A declared record type. Two schemas are the same type only if they name
    /// the same one.
    Nominal(&'static str),
}

impl SchemaIdentity {
    /// A deterministic sort key over type identity, for the container ordering
    /// (ADR-138). Anonymous shapes come first, then nominal ones by name.
    ///
    /// Derived `Ord` would do the same thing, but it would also make identity
    /// *silently* orderable everywhere and would move whenever a variant is
    /// added. This is the one place an order over it is wanted, so it is spelled
    /// once, here, and the reason travels with it. The name is the key rather
    /// than the address because a schema is interned per producer — the JIT
    /// generation, the parser registry, the runtime — and two `Point` schemas
    /// from different producers must order identically.
    pub(crate) fn order_key(self) -> (u8, &'static str) {
        match self {
            SchemaIdentity::Anonymous => (0, ""),
            SchemaIdentity::Nominal(name) => (1, name),
        }
    }
}

/// The static shape of a record: what type it is, plus an ordered list of named
/// fields, each with its value descriptor. Allocated in the JIT generation that
/// built it (or, for parser templates, in the runtime's schema registry).
#[repr(C)]
pub struct RecordSchema {
    pub identity: SchemaIdentity,
    pub fields: &'static [RecordField],
}

impl RecordSchema {
    /// The number of fields in this record shape.
    pub fn arity(&self) -> usize {
        self.fields.len()
    }

    /// The descriptor to dispatch field `i` through for `value`: the static one
    /// when the producer had it, and the value's own otherwise.
    ///
    /// The same rule [`TupleSchema::descriptor_at`](crate::tuples::TupleSchema)
    /// states — an object always knows what it is — so a producer that had no
    /// static type for a field leaves a null rather than guessing one.
    fn descriptor_at(&self, i: usize, value: GcRef) -> &'static TypeDescriptor {
        match self.fields.get(i).map(|f| f.descriptor) {
            Some(d) if !d.is_null() => {
                // SAFETY: a non-null slot is a `'static` descriptor pointer.
                unsafe { &*d }
            }
            _ => value.descriptor(),
        }
    }

    /// Whether two schemas describe the *same record type* — the same identity
    /// and the same field shape.
    ///
    /// Type identity, not allocation identity (RT-12). Schemas are interned per
    /// def *within a generation*, and there are three producers — every JIT
    /// generation, the runtime's parser registry, and test fixtures — so
    /// `pa.schema != pb.schema` made two records of one type compare unequal
    /// as soon as they came from different compiles. The debugger hit this
    /// directly: `p` evaluates in its own module, and comparing its result to a
    /// program value was always false.
    ///
    /// The shape is compared even for a `Nominal` pair, which the name alone
    /// would settle. It costs an arity check and a slice walk, and it is what
    /// keeps two instantiations of a generic record (one name, different field
    /// descriptors) apart, and what stops a debugger session that reloaded a
    /// *changed* definition from comparing old values field-wise through new
    /// descriptors.
    #[must_use]
    pub fn same_type(&self, other: &RecordSchema) -> bool {
        if self.identity != other.identity {
            return false;
        }
        self.fields.len() == other.fields.len()
            && self
                .fields
                .iter()
                .zip(other.fields.iter())
                .all(|(a, b)| a.name == b.name && std::ptr::eq(a.descriptor, b.descriptor))
    }
}

/// The `Record` payload: a pointer to the static schema plus the field values
/// (one `GcRef` per field, in schema order).
#[repr(C)]
pub struct RecordPayload {
    /// The static field shape. `items.len()` must equal `schema.arity()`.
    pub schema: *const RecordSchema,
    /// Field values in schema order.
    pub items: Vec<GcRef>,
}

unsafe fn record_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    let p = unsafe { &*(payload as *const RecordPayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn record_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    // `drop_in_place` frees the items Vec; the schema is static and not owned.
    unsafe { std::ptr::drop_in_place(payload as *mut RecordPayload) };
}

unsafe fn record_format(payload: *const u8, out: &mut dyn fmt::Write) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    let p = unsafe { &*(payload as *const RecordPayload) };
    let schema = unsafe { &*p.schema };
    let _ = out.write_str("{ ");
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            let _ = out.write_str(", ");
        }
        let field = &schema.fields[i];
        let _ = out.write_str(field.name);
        let _ = out.write_str(": ");
        let elem_desc = unsafe { &*field.descriptor };
        (elem_desc.format)(item.payload::<u8>() as *const u8, out);
    }
    let _ = out.write_str(" }");
}

unsafe fn record_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized RecordPayloads
    // with compatible schemas.
    let pa = unsafe { &*(a as *const RecordPayload) };
    let pb = unsafe { &*(b as *const RecordPayload) };
    // Equality is same-type + field-wise equality (§5.5). "Same type" is the
    // schema's identity and shape, not its *address* (RT-12): each JIT
    // generation interns its own schemas, so comparing pointers made two
    // `Point { x: 1, y: 2 }`s from different compiles unequal.
    if pa.schema.is_null() || pb.schema.is_null() {
        return false;
    }
    if !unsafe { (*pa.schema).same_type(&*pb.schema) } {
        return false;
    }
    if pa.items.len() != pb.items.len() {
        return false;
    }
    let schema = unsafe { &*pa.schema };
    // Field-wise equality through each field's descriptor (§11.4), short-circuiting
    // on the first non-equal field. If a field type is not equatable, the record is
    // not equatable (§5.5).
    for (i, (x, y)) in pa.items.iter().zip(pb.items.iter()).enumerate() {
        let Some(eq) = unsafe { &*schema.fields[i].descriptor }.equals else {
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

unsafe fn record_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    let p = unsafe { &*(payload as *const RecordPayload) };
    let schema = unsafe { &*p.schema };
    // Everything `same_type` compares is hashed, so `Eq` and `Hash` still agree
    // now that equality is type identity rather than a schema address: two
    // records that differ only in which type they are must be free to land in
    // different buckets.
    match schema.identity {
        SchemaIdentity::Anonymous => hasher.write_bytes(b"anon"),
        SchemaIdentity::Nominal(name) => {
            hasher.write_bytes(b"nom");
            hasher.write_bytes(name.as_bytes());
        }
    }
    // Arity first to distinguish records of different field counts.
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for (i, item) in p.items.iter().enumerate() {
        hasher.write_bytes(schema.fields[i].name.as_bytes());
        let field_desc = unsafe { &*schema.fields[i].descriptor };
        hasher.write_bytes(&field_desc.id().to_u32().to_le_bytes());
        // If the field type is not hashable, the record is not hashable (§5.5).
        let Some(hash_field) = field_desc.hash else {
            return;
        };
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_field(elem_payload, hasher);
    }
}

unsafe fn record_compare(a: *const u8, b: *const u8) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // SAFETY: caller guarantees both pointers point at initialized RecordPayloads.
    let pa = unsafe { &*(a as *const RecordPayload) };
    let pb = unsafe { &*(b as *const RecordPayload) };
    // A null schema is a producer bug and not a user-reachable state, but it
    // still needs a deterministic answer rather than a hash-order one (ADR-138).
    match (pa.schema.is_null(), pb.schema.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => {}
    }
    // SAFETY: both checked non-null above.
    let (schema_a, schema_b) = unsafe { (&*pa.schema, &*pb.schema) };
    // Type identity first, so two record *types* in one collection never
    // interleave — and by name, never by schema address, for the reason
    // `same_type` gives: there are three producers of a schema and their
    // addresses differ.
    match schema_a
        .identity
        .order_key()
        .cmp(&schema_b.identity.order_key())
    {
        Ordering::Equal => {}
        other => return other,
    }
    match pa.items.len().cmp(&pb.items.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    // Field-wise in schema order, short-circuiting at the first difference. The
    // field *name* participates because two anonymous shapes with one arity are
    // different types, and `record_hash` already mixes the names for the same
    // reason.
    for (i, (x, y)) in pa.items.iter().zip(pb.items.iter()).enumerate() {
        let (na, nb) = (schema_a.fields[i].name, schema_b.fields[i].name);
        match na.cmp(nb) {
            Ordering::Equal => {}
            other => return other,
        }
        let dx = schema_a.descriptor_at(i, *x);
        let dy = schema_b.descriptor_at(i, *y);
        // SAFETY: each field's payload matches the descriptor its schema slot
        // names, or its own header's when that slot is null.
        match unsafe { crate::ordering::slot_cmp(*x, *y, dx, dy) } {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// Descriptor for the structural `Record` type (§4.5/§7.8). Structural equality,
/// hashing (§5.5) and the container ordering (ADR-138) recurse field-wise
/// through the per-shape schema's field descriptors. A record is
/// equatable/hashable iff every field is; functions never are, so a record
/// containing a function field is neither. This lets records serve as map/set
/// keys (M8 containers) — which is why a container has to be able to order one.
pub static RECORD: TypeDescriptor = TypeDescriptor::builtin::<RecordPayload>(
    BuiltinTypeId::Record,
    "Record",
    record_trace,
    record_drop,
    record_format,
    Some(record_equals),
    Some(record_hash),
    // A record can be a key, so a container orders one (ADR-138). `p < q` on
    // two records is still Y006: that is `capability::supports_ord`'s question.
    Some(record_compare),
)
.with_owned_bytes(record_owned_bytes);

/// The heap bytes a record owns beyond its payload, for GC pacing (RT-04).
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `RecordPayload`.
unsafe fn record_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized RecordPayload.
    let p = unsafe { &*(payload as *const RecordPayload) };
    p.items.capacity() * std::mem::size_of::<GcRef>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_descriptor_reports_capabilities() {
        assert!(RECORD.is_equatable());
        assert!(RECORD.is_hashable());
        assert_eq!(RECORD.name, "Record");
        assert_eq!(RECORD.as_builtin(), Some(BuiltinTypeId::Record));
    }

    #[test]
    fn grid_descriptor_reports_capabilities() {
        // M8-WS5: Grid is now equatable and hashable (grid-as-map-key enabled),
        // closing the M6 "grid-as-key deferred" note.
        assert!(crate::collections::GRID.is_equatable());
        assert!(crate::collections::GRID.is_hashable());
        assert_eq!(crate::collections::GRID.name, "Grid");
        assert_eq!(
            crate::collections::GRID.as_builtin(),
            Some(BuiltinTypeId::Grid)
        );
    }

    /// A leaked schema of `(name, Int)` fields, standing in for one a JIT
    /// generation or the parser registry would build. Each call leaks its own,
    /// which is the point wherever two are compared: same shape, different
    /// address.
    fn leak_schema(identity: SchemaIdentity, names: &[&'static str]) -> &'static RecordSchema {
        let fields: Vec<RecordField> = names
            .iter()
            .map(|name| RecordField {
                name,
                descriptor: &crate::scalars::INT,
            })
            .collect();
        Box::leak(Box::new(RecordSchema {
            identity,
            fields: Box::leak(fields.into_boxed_slice()),
        }))
    }

    /// Allocate a record of `schema` and fill it with `values` as `Int`s.
    fn record_of(
        ctx: &mut crate::RuntimeContext,
        schema: &'static RecordSchema,
        values: &[i64],
    ) -> GcRef {
        let r = unsafe { crate::abi::praxis_alloc_record(ctx, schema) };
        for (i, v) in values.iter().enumerate() {
            let boxed = unsafe { crate::abi::praxis_alloc_int(ctx, *v) };
            unsafe { crate::abi::praxis_record_set_field(ctx, r, i as u32, boxed) };
        }
        r
    }

    fn equal(a: GcRef, b: GcRef) -> bool {
        unsafe {
            record_equals(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        }
    }

    fn hash_of(r: GcRef) -> u64 {
        let mut h = crate::descriptor::StructHasher::new();
        unsafe { record_hash(r.payload::<u8>() as *const u8, &mut h) };
        h.finish()
    }

    /// RT-12. Two schemas of one anonymous shape, separately allocated — what
    /// two JIT generations, or a generation and the parser registry, produce
    /// for the same `{x: Int, y: Int}`. Records built through them are the same
    /// value, and `record_equals` compared schema *addresses*, so they were not.
    #[test]
    fn anonymous_records_of_one_shape_are_equal_across_schema_allocations() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let first = leak_schema(SchemaIdentity::Anonymous, &["x", "y"]);
        let second = leak_schema(SchemaIdentity::Anonymous, &["x", "y"]);
        assert!(
            !std::ptr::eq(first, second),
            "the two schemas must really be distinct allocations"
        );

        let a = record_of(&mut ctx, first, &[1, 2]);
        let b = record_of(&mut ctx, second, &[1, 2]);
        assert!(equal(a, b));
        assert_eq!(hash_of(a), hash_of(b), "equal records must hash equally");

        // Same shape, different values: still not equal.
        let c = record_of(&mut ctx, second, &[1, 3]);
        assert!(!equal(a, c));
    }

    /// The ordering analogue of the test above (ADR-138). A record's container
    /// order is its type identity, then its fields — and identity is compared
    /// by *name*, never by schema address, for the same reason equality is:
    /// there are three producers of a schema and their allocations differ, so
    /// an address order would have two `Point`s from two generations sort into
    /// an order that changes between runs.
    #[test]
    fn record_compare_is_identity_then_fields() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let cmp = |a: GcRef, b: GcRef| unsafe {
            record_compare(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        };

        let first = leak_schema(SchemaIdentity::Anonymous, &["x", "y"]);
        let second = leak_schema(SchemaIdentity::Anonymous, &["x", "y"]);
        assert!(!std::ptr::eq(first, second));
        let a = record_of(&mut ctx, first, &[1, 2]);
        let same = record_of(&mut ctx, second, &[1, 2]);
        assert_eq!(
            cmp(a, same),
            std::cmp::Ordering::Equal,
            "one shape, one value"
        );

        // Fields decide, left to right, through each field's own order — so
        // `2` precedes `10` rather than trailing it as `"10"` would.
        let bigger = record_of(&mut ctx, second, &[1, 10]);
        let smaller = record_of(&mut ctx, second, &[1, 2]);
        assert_eq!(cmp(smaller, bigger), std::cmp::Ordering::Less);

        // Identity comes first: an anonymous shape sorts before a nominal one,
        // whatever its fields say.
        let point = leak_schema(SchemaIdentity::Nominal("Point"), &["x", "y"]);
        let p = record_of(&mut ctx, point, &[0, 0]);
        assert_eq!(cmp(a, p), std::cmp::Ordering::Less);
        assert_eq!(cmp(p, a), std::cmp::Ordering::Greater);
    }

    /// RT-12's other half: a *nominal* record is its declared type, so two
    /// records with identical fields and different type names are not equal —
    /// and a nominal record is never equal to a structural one of the same
    /// shape (§5.6).
    #[test]
    fn nominal_records_of_different_types_are_never_equal() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let point = leak_schema(SchemaIdentity::Nominal("Point"), &["x", "y"]);
        let vector = leak_schema(SchemaIdentity::Nominal("Vector"), &["x", "y"]);
        let anon = leak_schema(SchemaIdentity::Anonymous, &["x", "y"]);

        let p = record_of(&mut ctx, point, &[1, 2]);
        let v = record_of(&mut ctx, vector, &[1, 2]);
        let a = record_of(&mut ctx, anon, &[1, 2]);
        assert!(!equal(p, v), "two record types are not one type");
        assert!(!equal(p, a), "a declared type is not a structural shape");

        // And the same nominal type from two generations *is* one type.
        let point_again = leak_schema(SchemaIdentity::Nominal("Point"), &["x", "y"]);
        let p2 = record_of(&mut ctx, point_again, &[1, 2]);
        assert!(equal(p, p2));
        assert_eq!(hash_of(p), hash_of(p2));
    }

    /// A shape check rides along with the name, so one nominal name over two
    /// different field shapes — a generic record's instantiations, or a
    /// debugger session that reloaded a changed definition — does not compare
    /// field-wise through the wrong descriptors.
    #[test]
    fn one_nominal_name_over_two_shapes_is_two_types() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let two_fields = leak_schema(SchemaIdentity::Nominal("P"), &["x", "y"]);
        let renamed = leak_schema(SchemaIdentity::Nominal("P"), &["x", "z"]);

        let a = record_of(&mut ctx, two_fields, &[1, 2]);
        let b = record_of(&mut ctx, renamed, &[1, 2]);
        assert!(!equal(a, b));
    }

    #[test]
    fn record_equals_identical_int_fields() {
        // Build two records with the same schema and equal Int fields; their
        // structural equals must be true, and unequal fields must be false.
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let descriptors: &'static [*const TypeDescriptor] =
            Box::leak(vec![&crate::scalars::INT as *const TypeDescriptor; 2].into_boxed_slice());
        let schema = Box::leak(Box::new(RecordSchema {
            identity: SchemaIdentity::Anonymous,
            fields: Box::leak(
                vec![
                    RecordField {
                        name: "x",
                        descriptor: descriptors[0],
                    },
                    RecordField {
                        name: "y",
                        descriptor: descriptors[1],
                    },
                ]
                .into_boxed_slice(),
            ),
        }));
        // Allocate two records and fill with Int 1, 2.
        let a = unsafe { crate::abi::praxis_alloc_record(&mut ctx, schema) };
        let b = unsafe { crate::abi::praxis_alloc_record(&mut ctx, schema) };
        let one = unsafe { crate::abi::praxis_alloc_int(&mut ctx, 1) };
        let two = unsafe { crate::abi::praxis_alloc_int(&mut ctx, 2) };
        unsafe {
            crate::abi::praxis_record_set_field(&mut ctx, a, 0, one);
            crate::abi::praxis_record_set_field(&mut ctx, a, 1, two);
            crate::abi::praxis_record_set_field(&mut ctx, b, 0, one);
            crate::abi::praxis_record_set_field(&mut ctx, b, 1, two);
        }
        assert!(unsafe {
            record_equals(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        });

        // Now make b's second field differ (3) → not equal.
        let three = unsafe { crate::abi::praxis_alloc_int(&mut ctx, 3) };
        unsafe { crate::abi::praxis_record_set_field(&mut ctx, b, 1, three) };
        assert!(!unsafe {
            record_equals(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        });
    }
}
