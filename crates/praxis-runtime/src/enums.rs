//! The `Enum` value descriptor (§4.6).
//!
//! An enum value carries a discriminant (`tag`) selecting its variant, the
//! variant's payload values (one `GcRef` per payload type, in declaration
//! order), and a pointer to the [`EnumSchema`] that says **which enum type it
//! is**.
//!
//! The schema is what records and tuples already carry, for the same reason: a
//! single `ENUM` descriptor serves every enum value, so without a per-type
//! schema in the payload two unrelated enums of one shape would be one type —
//! `Colour::Red` would equal `Light::Red`, they would hash into the same
//! bucket, and a value could only render as `<variant 0: …>` because nothing
//! in the runtime would know the variant was called `Red`.
//!
//! Each distinct enum *type* gets one schema, built by whichever producer
//! allocates the value: the codegen's per-generation cache, the input parser's
//! registry, or the runtime's own [`option_schema`]. Schemas are therefore
//! compared by **type identity and shape**, never by address.

use std::fmt::Write as _;

use crate::descriptor::{BuiltinTypeId, DynamicHasher, FormatSink, Tracer, TypeDescriptor};
use crate::records::SchemaIdentity;
use crate::GcRef;

/// One variant of an enum shape: its source name plus the descriptors for its
/// payload slots, in declaration order. A payload-less variant has an empty
/// slice.
///
/// A slot may be **null**, meaning the producer had no static type for it —
/// exactly the encoding [`TupleSchema`](crate::tuples::TupleSchema) uses. The
/// runtime's own `Option` schema is the motivating case: `praxis_map_get`
/// learns `V` from the value it found, never from a static type, so `Some`'s
/// slot is unknown there and known (`Int`, `Text`, …) in the schema the codegen
/// builds for the same `Option`. The two must still be one type, and the
/// *value's* descriptor — read off its header, so never wrong — answers for an
/// unknown slot.
#[repr(C)]
pub struct EnumVariantShape {
    pub name: &'static str,
    pub payload: &'static [*const TypeDescriptor],
}

/// The static shape of an enum type: what type it is, plus its variants in
/// declaration order (the tag indexes this list).
///
/// There is no separate `name` field: [`SchemaIdentity`] already carries the
/// declared name for a nominal type and says there is none for a structural
/// one, so a second copy could only disagree with it.
#[repr(C)]
pub struct EnumSchema {
    pub identity: SchemaIdentity,
    pub variants: &'static [EnumVariantShape],
}

impl EnumSchema {
    /// The variant `tag` selects, or `None` if the tag is past the end.
    #[must_use]
    pub fn variant_at(&self, tag: usize) -> Option<&'static EnumVariantShape> {
        // `variants` is already `&'static`, so copying the slice reference out
        // of `self` before indexing keeps the element's lifetime `'static`.
        let variants: &'static [EnumVariantShape] = self.variants;
        variants.get(tag)
    }

    /// How many payload slots variant `tag` carries. Zero for an unknown tag —
    /// which `praxis_alloc_enum` refuses before it allocates.
    #[must_use]
    pub fn arity_of(&self, tag: usize) -> usize {
        self.variants.get(tag).map_or(0, |v| v.payload.len())
    }

    /// The descriptor to dispatch payload slot `i` of variant `tag` through for
    /// `value`: the static one when the producer had it, and the value's own
    /// otherwise.
    ///
    /// The fallback is what makes an unknown slot *safe* rather than merely
    /// tolerated, and it is the same rule `TupleSchema::descriptor_at` states:
    /// an object always knows what it is.
    #[must_use]
    pub fn descriptor_at(&self, tag: usize, i: usize, value: GcRef) -> &'static TypeDescriptor {
        match self
            .variants
            .get(tag)
            .and_then(|v| v.payload.get(i))
            .copied()
        {
            Some(d) if !d.is_null() => {
                // SAFETY: a non-null slot is a `'static` descriptor pointer.
                unsafe { &*d }
            }
            _ => value.descriptor(),
        }
    }

    /// Whether two schemas describe the *same enum type* — the same identity
    /// and the same variant shape.
    ///
    /// Type identity, not allocation identity, exactly as
    /// [`RecordSchema::same_type`](crate::records::RecordSchema::same_type):
    /// there are three producers of an `Option` schema (every JIT generation,
    /// the input parser's registry, and [`option_schema`]), so comparing
    /// addresses would make a `Some(1)` from the runtime unequal to a `Some(1)`
    /// the program wrote.
    ///
    /// A **null** payload slot is unknown, not a fourth type: it agrees with
    /// whatever the other side says, and the values decide (see `enum_equals`).
    /// That is what lets [`option_schema`]'s unknown `Some` slot be the same
    /// type as the codegen's `Option[Int]`.
    #[must_use]
    pub fn same_type(&self, other: &EnumSchema) -> bool {
        if self.identity != other.identity {
            return false;
        }
        self.variants.len() == other.variants.len()
            && self
                .variants
                .iter()
                .zip(other.variants.iter())
                .all(|(a, b)| {
                    a.name == b.name
                        && a.payload.len() == b.payload.len()
                        && a.payload
                            .iter()
                            .zip(b.payload.iter())
                            .all(|(x, y)| x.is_null() || y.is_null() || std::ptr::eq(*x, *y))
                })
    }
}

/// The runtime payload of an enum value: which enum type it is, the variant
/// discriminant, and the variant's payload values (one `GcRef` per payload
/// field, in declaration order).
///
/// `schema` is **first**, mirroring `RecordPayload` and `TuplePayload`, so the
/// tag does not sit at offset 0: the codegen reads it through `offset_of!`
/// rather than a literal.
#[repr(C)]
pub struct EnumPayload {
    /// The static enum type. `items.len()` must equal
    /// `schema.arity_of(tag)`.
    pub schema: *const EnumSchema,
    /// Which variant this value is (index into `schema.variants`).
    pub tag: u32,
    /// The variant's payload values (empty for a payload-less variant).
    pub items: Vec<GcRef>,
}

unsafe fn enum_trace(payload: *mut u8, tracer: &mut dyn Tracer) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    let p = unsafe { &*(payload as *const EnumPayload) };
    for item in p.items.iter() {
        tracer.trace(*item);
    }
}

unsafe fn enum_drop(payload: *mut u8) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    // `drop_in_place` frees the items Vec; the schema is static and not owned.
    unsafe { std::ptr::drop_in_place(payload as *mut EnumPayload) };
}

unsafe fn enum_format(payload: *const u8, out: &mut FormatSink<'_>) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    let p = unsafe { &*(payload as *const EnumPayload) };
    // The variant name comes from the schema, so `Some(3)` renders as `Some(3)`
    // rather than the `<variant 0: 3>` an enum without nominal identity could
    // only manage. A null schema is a producer bug rather than a user-reachable
    // state; render the tag rather than dereferencing null.
    if p.schema.is_null() {
        let _ = write!(out, "<variant {}>", p.tag);
        return;
    }
    // SAFETY: checked non-null above; every producer supplies a `'static` schema.
    let schema = unsafe { &*p.schema };
    let tag = p.tag as usize;
    match schema.variant_at(tag) {
        Some(variant) => {
            let _ = out.write_str(variant.name);
            if p.items.is_empty() {
                return;
            }
            let _ = out.write_str("(");
            for (i, item) in p.items.iter().enumerate() {
                if i > 0 {
                    let _ = out.write_str(", ");
                }
                let desc = schema.descriptor_at(tag, i, *item);
                (desc.format)(item.payload::<u8>() as *const u8, out);
            }
            let _ = out.write_str(")");
        }
        None => {
            let _ = write!(out, "<variant {}>", p.tag);
        }
    }
}

unsafe fn enum_equals(a: *const u8, b: *const u8) -> bool {
    // SAFETY: caller guarantees both pointers point at initialized EnumPayloads.
    let pa = unsafe { &*(a as *const EnumPayload) };
    let pb = unsafe { &*(b as *const EnumPayload) };
    // Equality is same-type + same-variant + payload-wise equality (§5.5).
    // "Same type" is the schema's identity and variant shape, not its *address*:
    // the tag alone is not an enum value's identity, or a `Colour::Red` and a
    // `Light::Red` would be equal.
    if pa.schema.is_null() || pb.schema.is_null() {
        return false;
    }
    if !unsafe { (*pa.schema).same_type(&*pb.schema) } {
        return false;
    }
    if pa.tag != pb.tag {
        return false;
    }
    if pa.items.len() != pb.items.len() {
        return false;
    }
    let schema = unsafe { &*pa.schema };
    let tag = pa.tag as usize;
    for (i, (x, y)) in pa.items.iter().zip(pb.items.iter()).enumerate() {
        let desc = schema.descriptor_at(tag, i, *x);
        // For a slot the producer had no type for, `desc` is `x`'s own — so `y`
        // must carry the same one before its payload is read through it. Two
        // values of different types are unequal; reading one as the other is a
        // wrong-payload read, which is what `Some(1) == Some("1")` would be
        // under a single `Option` schema.
        if !std::ptr::eq(desc, schema.descriptor_at(tag, i, *y)) {
            return false;
        }
        // If a payload type is not equatable, the enum is not equatable (§5.5).
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

unsafe fn enum_hash(payload: *const u8, hasher: &mut dyn DynamicHasher) {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    let p = unsafe { &*(payload as *const EnumPayload) };
    // Everything `same_type` compares is hashed, so `Eq` and `Hash` agree:
    // equality is type identity rather than the tag alone, and two enums that
    // differ only in which type they are must be free to land in different
    // buckets.
    if !p.schema.is_null() {
        // SAFETY: checked non-null; every producer supplies a `'static` schema.
        let schema = unsafe { &*p.schema };
        match schema.identity {
            SchemaIdentity::Anonymous => hasher.write_bytes(b"anon"),
            SchemaIdentity::Nominal(name) => {
                hasher.write_bytes(b"nom");
                hasher.write_bytes(name.as_bytes());
            }
        }
        if let Some(variant) = schema.variant_at(p.tag as usize) {
            hasher.write_bytes(variant.name.as_bytes());
        }
    }
    // The tag — two values of different variants must hash distinctly even
    // before the payload is considered.
    hasher.write_bytes(&(p.tag as u64).to_le_bytes());
    // Arity, to distinguish payload prefixes.
    hasher.write_bytes(&(p.items.len() as u64).to_le_bytes());
    for (i, item) in p.items.iter().enumerate() {
        // The slot's type is part of the shape `eq` compares, so it is part of
        // the hash too. Reading it off the *value* for an unknown slot is what
        // keeps hash and eq agreeing there: both ask the object.
        let desc = if p.schema.is_null() {
            item.descriptor()
        } else {
            // SAFETY: checked non-null above.
            unsafe { &*p.schema }.descriptor_at(p.tag as usize, i, *item)
        };
        hasher.write_bytes(&desc.id().to_u32().to_le_bytes());
        // If a payload type is not hashable, the enum is not hashable (§5.5).
        let Some(hash_item) = desc.hash else {
            return;
        };
        let elem_payload = item.payload::<u8>() as *const u8;
        hash_item(elem_payload, hasher);
    }
}

unsafe fn enum_compare(a: *const u8, b: *const u8) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // SAFETY: caller guarantees both pointers point at initialized EnumPayloads.
    let pa = unsafe { &*(a as *const EnumPayload) };
    let pb = unsafe { &*(b as *const EnumPayload) };
    // A null schema is a producer bug rather than a user-reachable state, but it
    // still needs a deterministic answer rather than a hash-order one (ADR-138).
    match (pa.schema.is_null(), pb.schema.is_null()) {
        (true, true) => return pa.tag.cmp(&pb.tag),
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => {}
    }
    // SAFETY: both checked non-null above.
    let (schema_a, schema_b) = unsafe { (&*pa.schema, &*pb.schema) };
    match schema_a
        .identity
        .order_key()
        .cmp(&schema_b.identity.order_key())
    {
        Ordering::Equal => {}
        other => return other,
    }
    // The tag is the variant's **declaration** order — the order the type was
    // written in, which is the order `enum_format` names the variants in and
    // the order a `match`'s arms are read in. Sorting the variant *names*
    // instead would impose an alphabet the declaration never mentioned, so a
    // reader of the enum could not predict the order without sorting it
    // themselves.
    match pa.tag.cmp(&pb.tag) {
        Ordering::Equal => {}
        other => return other,
    }
    match pa.items.len().cmp(&pb.items.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    let tag = pa.tag as usize;
    // Payload slot-wise, through each side's own schema, falling back to the
    // value's own descriptor for an unknown slot — the null-slot rule
    // `descriptor_at` states, and the one `option_schema`'s unknown `Some` slot
    // depends on.
    for (i, (x, y)) in pa.items.iter().zip(pb.items.iter()).enumerate() {
        let dx = schema_a.descriptor_at(tag, i, *x);
        let dy = schema_b.descriptor_at(pb.tag as usize, i, *y);
        // SAFETY: each payload matches the descriptor its schema slot names, or
        // its own header's when that slot is null.
        match unsafe { crate::ordering::slot_cmp(*x, *y, dx, dy) } {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// Descriptor for the `Enum` value type (§4.6). Structural equality and
/// hashing (§5.5): two enum values are equal iff they are the same enum *type*,
/// carry the same variant tag and have equal payloads; hashing mixes the type
/// identity, the variant, then each payload. An enum is equatable/hashable iff
/// every payload type is; functions never are. This lets enums serve as
/// map/set keys — and the container ordering (ADR-138) walks the same three
/// levels, in declaration order at the tag.
pub static ENUM: TypeDescriptor = TypeDescriptor::builtin::<EnumPayload>(
    BuiltinTypeId::Enum,
    "Enum",
    enum_trace,
    enum_drop,
    enum_format,
    Some(enum_equals),
    Some(enum_hash),
    // An enum can be a key, so a container orders one (ADR-138). `a < b` on two
    // enum values is still Y006: that is `capability::supports_ord`'s question.
    Some(enum_compare),
)
.with_owned_bytes(enum_owned_bytes);

/// The heap bytes an enum value owns beyond its payload, for GC pacing.
/// `capacity`, not `len`: the buffer's real footprint is what the collector is
/// paced against.
///
/// # Safety
/// `payload` must point at an initialized `EnumPayload`.
unsafe fn enum_owned_bytes(payload: *const u8) -> usize {
    // SAFETY: caller guarantees `payload` points at an initialized EnumPayload.
    let p = unsafe { &*(payload as *const EnumPayload) };
    p.items.capacity() * std::mem::size_of::<GcRef>()
}

/// `Option`'s variant discriminants, in the order `TypeDb::new` declares them —
/// `Some` first, `None` second. The codegen uses the same order for a `Some(x)`
/// the program writes, so a runtime-built `Option` matches against the same
/// arms.
pub const OPTION_SOME_TAG: i64 = 0;
/// See [`OPTION_SOME_TAG`].
pub const OPTION_NONE_TAG: i64 = 1;

/// The runtime's own `'static` schema for the prelude `Option` (F12).
///
/// `Map.get`, `Grid.find` and the graph walks answer `Option[V]` without ever
/// learning `V` statically — they only have the value they found — so `Some`'s
/// payload slot is **unknown** here and the value's own descriptor answers for
/// it. The codegen's schema for the same `Option[Int]` names `INT` in that
/// slot; [`EnumSchema::same_type`]'s null tolerance is what makes the two one
/// type, which is what lets `match m.get(k) { Some(v) => …, None => … }` bind a
/// runtime-built value against user-written arms.
///
/// A plain `static` will not do: `*const TypeDescriptor` is neither `Send` nor
/// `Sync`. This is the `OnceLock<SyncPtr>` + `Box::leak` idiom
/// `tuples::point_schema` already uses, for the same reason.
#[must_use]
pub fn option_schema() -> &'static EnumSchema {
    use std::sync::OnceLock;
    struct SyncPtr(&'static EnumSchema);
    // SAFETY: the leaked schema and everything it points at are immutable and
    // outlive every thread (mirroring `tuples::point_schema`).
    unsafe impl Send for SyncPtr {}
    unsafe impl Sync for SyncPtr {}
    static OPTION: OnceLock<SyncPtr> = OnceLock::new();
    OPTION
        .get_or_init(|| {
            let some_payload: &'static [*const TypeDescriptor] =
                Box::leak(vec![std::ptr::null(); 1].into_boxed_slice());
            let variants: &'static [EnumVariantShape] = Box::leak(
                vec![
                    EnumVariantShape {
                        name: "Some",
                        payload: some_payload,
                    },
                    EnumVariantShape {
                        name: "None",
                        payload: &[],
                    },
                ]
                .into_boxed_slice(),
            );
            SyncPtr(Box::leak(Box::new(EnumSchema {
                identity: SchemaIdentity::Nominal("Option"),
                variants,
            })))
        })
        .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_descriptor_reports_capabilities() {
        assert!(ENUM.is_equatable());
        assert!(ENUM.is_hashable());
        assert_eq!(ENUM.name, "Enum");
        assert_eq!(ENUM.as_builtin(), Some(BuiltinTypeId::Enum));
    }
}

#[cfg(test)]
mod alloc_tests {
    use super::*;
    use crate::abi::{praxis_alloc_enum, praxis_enum_set_payload};

    /// A leaked schema of payload-less variants, standing in for one a JIT
    /// generation or the parser registry would build. Each call leaks its own,
    /// which is the point wherever two are compared: same shape, different
    /// address.
    fn leak_schema(identity: SchemaIdentity, names: &[&'static str]) -> &'static EnumSchema {
        let variants: Vec<EnumVariantShape> = names
            .iter()
            .map(|name| EnumVariantShape { name, payload: &[] })
            .collect();
        Box::leak(Box::new(EnumSchema {
            identity,
            variants: Box::leak(variants.into_boxed_slice()),
        }))
    }

    fn equal(a: GcRef, b: GcRef) -> bool {
        unsafe {
            enum_equals(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        }
    }

    fn hash_of(e: GcRef) -> u64 {
        let mut h = crate::descriptor::StructHasher::new();
        unsafe { enum_hash(e.payload::<u8>() as *const u8, &mut h) };
        h.finish()
    }

    fn rendered(e: GcRef) -> String {
        let mut s = String::new();
        unsafe {
            enum_format(
                e.payload::<u8>() as *const u8,
                &mut crate::FormatSink::display(&mut s),
            )
        };
        s
    }

    #[test]
    fn alloc_enum_round_trips_the_tag_and_the_schema() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let schema = leak_schema(SchemaIdentity::Nominal("Colour"), &["Red", "Green"]);
        for tag in 0..2_i64 {
            let eref = unsafe { praxis_alloc_enum(&mut ctx, schema, tag) };
            let payload = eref.payload::<u8>() as *const EnumPayload;
            assert_eq!(unsafe { (*payload).tag }, tag as u32);
            assert_eq!(unsafe { (*payload).schema }, schema as *const EnumSchema);
            assert_eq!(unsafe { (*payload).items.len() }, 0);
        }
    }

    /// The arity is read from the schema rather than passed alongside it, so a
    /// tag with no variant has no arity to guess at. `praxis_alloc_tuple`
    /// answers the Unit sentinel for a null schema for the same reason.
    #[test]
    fn an_out_of_range_tag_answers_the_unit_sentinel() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let schema = leak_schema(SchemaIdentity::Nominal("Colour"), &["Red", "Green"]);
        let past_the_end = unsafe { praxis_alloc_enum(&mut ctx, schema, 2) };
        assert_eq!(
            past_the_end.descriptor().id(),
            crate::scalars::UNIT.id(),
            "a tag the schema has no variant for cannot allocate an enum value"
        );
        let null = unsafe { praxis_alloc_enum(&mut ctx, std::ptr::null(), 0) };
        assert_eq!(null.descriptor().id(), crate::scalars::UNIT.id());
    }

    /// Two enum types of one shape are two types: an enum value's identity is
    /// its schema's, not its tag's, so `Colour::Red` and `Light::Red` are
    /// neither equal nor alike in hash.
    #[test]
    fn two_enum_types_of_one_shape_are_not_one_type() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let colour = leak_schema(SchemaIdentity::Nominal("Colour"), &["Red", "Green"]);
        let light = leak_schema(SchemaIdentity::Nominal("Light"), &["Red", "Green"]);
        let anon = leak_schema(SchemaIdentity::Anonymous, &["Red", "Green"]);

        let red = unsafe { praxis_alloc_enum(&mut ctx, colour, 0) };
        let stop = unsafe { praxis_alloc_enum(&mut ctx, light, 0) };
        let bare = unsafe { praxis_alloc_enum(&mut ctx, anon, 0) };
        assert!(!equal(red, stop), "two enum types are not one type");
        assert!(
            !equal(red, bare),
            "a declared type is not a structural shape"
        );

        // And a different variant of the same type is still not equal.
        let green = unsafe { praxis_alloc_enum(&mut ctx, colour, 1) };
        assert!(!equal(red, green));
    }

    /// The other half: one enum type built through two separately allocated
    /// schemas — what two JIT generations, or a generation and the parser
    /// registry, produce — is one type.
    #[test]
    fn one_enum_type_built_by_two_schema_allocations_is_one_type() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let first = leak_schema(SchemaIdentity::Nominal("Colour"), &["Red", "Green"]);
        let second = leak_schema(SchemaIdentity::Nominal("Colour"), &["Red", "Green"]);
        assert!(
            !std::ptr::eq(first, second),
            "the two schemas must really be distinct allocations"
        );

        let a = unsafe { praxis_alloc_enum(&mut ctx, first, 0) };
        let b = unsafe { praxis_alloc_enum(&mut ctx, second, 0) };
        assert!(equal(a, b));
        assert_eq!(hash_of(a), hash_of(b), "equal enums must hash equally");
    }

    /// One nominal name over two variant lists is two types — the debugger
    /// session that reloaded a *changed* definition, which must not compare a
    /// stale value's payload through the new shape.
    #[test]
    fn one_enum_name_over_two_variant_lists_is_two_types() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let two = leak_schema(SchemaIdentity::Nominal("C"), &["Red", "Green"]);
        let renamed = leak_schema(SchemaIdentity::Nominal("C"), &["Red", "Blue"]);
        // The whole variant list is the shape, so even the variant the two
        // agree on (`Red`, tag 0) does not make them one type: a value built
        // before the change must not be compared payload-wise through the
        // descriptors of a definition it never had.
        let a = unsafe { praxis_alloc_enum(&mut ctx, two, 0) };
        let b = unsafe { praxis_alloc_enum(&mut ctx, renamed, 0) };
        assert!(!equal(a, b));
    }

    /// The null-payload-slot rule, applied to enums. Under one `Option` schema
    /// whose `Some` slot is unknown, `Some(1)` and `Some("1")` are unequal
    /// rather than one being read through the other's descriptor.
    #[test]
    fn a_some_of_two_different_payload_types_is_not_equal() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let schema = option_schema();
        let build = |ctx: &mut crate::RuntimeContext, v: GcRef| {
            let e = unsafe { praxis_alloc_enum(ctx, schema, OPTION_SOME_TAG) };
            unsafe { praxis_enum_set_payload(ctx, e, 0, v) };
            e
        };
        let one = rt.alloc_int(1);
        let text = rt.alloc_text("1");
        let boxed_int = build(&mut ctx, one);
        let boxed_text = build(&mut ctx, text);
        assert!(!equal(boxed_int, boxed_text));

        // Equal payloads of one type are still equal.
        let again = build(&mut ctx, rt.alloc_int(1));
        assert!(equal(boxed_int, again));
        assert_eq!(hash_of(boxed_int), hash_of(again));
    }

    /// A known payload slot and an unknown one agree rather than contradict, so
    /// a codegen-built `Option[Int]` and a runtime-built `Some` are one type —
    /// which is what makes `match m.get(k) { Some(v) => … }` work at all.
    #[test]
    fn a_known_payload_slot_and_an_unknown_one_describe_one_option_type() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let typed: &'static EnumSchema = Box::leak(Box::new(EnumSchema {
            identity: SchemaIdentity::Nominal("Option"),
            variants: Box::leak(
                vec![
                    EnumVariantShape {
                        name: "Some",
                        payload: Box::leak(
                            vec![&crate::scalars::INT as *const TypeDescriptor].into_boxed_slice(),
                        ),
                    },
                    EnumVariantShape {
                        name: "None",
                        payload: &[],
                    },
                ]
                .into_boxed_slice(),
            ),
        }));
        assert!(typed.same_type(option_schema()));
        assert!(option_schema().same_type(typed));

        let from_codegen = unsafe { praxis_alloc_enum(&mut ctx, typed, OPTION_SOME_TAG) };
        let seven = rt.alloc_int(7);
        unsafe { praxis_enum_set_payload(&mut ctx, from_codegen, 0, seven) };
        let from_runtime = unsafe { praxis_alloc_enum(&mut ctx, option_schema(), OPTION_SOME_TAG) };
        let seven_again = rt.alloc_int(7);
        unsafe { praxis_enum_set_payload(&mut ctx, from_runtime, 0, seven_again) };
        assert!(equal(from_codegen, from_runtime));
        assert_eq!(hash_of(from_codegen), hash_of(from_runtime));
    }

    /// An enum renders its variant name, which it reads from its schema.
    #[test]
    fn an_enum_renders_its_variant_name_and_payload() {
        let mut rt = crate::Runtime::new();
        let mut ctx = rt.context();
        let some = unsafe { praxis_alloc_enum(&mut ctx, option_schema(), OPTION_SOME_TAG) };
        let three = rt.alloc_int(3);
        unsafe { praxis_enum_set_payload(&mut ctx, some, 0, three) };
        assert_eq!(rendered(some), "Some(3)");

        let none = unsafe { praxis_alloc_enum(&mut ctx, option_schema(), OPTION_NONE_TAG) };
        assert_eq!(rendered(none), "None");
    }

    /// The container order is the **variant's declaration order**, then the
    /// payload (ADR-138). `Option` declares `Some` first (see
    /// [`OPTION_SOME_TAG`]), so `Some(…)` precedes `None` — an alphabetical
    /// order over the variant names would answer the opposite, and would be an
    /// order the declaration never mentioned. Inside one variant the payload
    /// decides, through its own type's order, so `Some(2)` precedes `Some(10)`
    /// rather than trailing it as the rendered form would have said.
    #[test]
    fn enum_compare_is_declaration_order_then_payload() {
        let mut rt = crate::Runtime::new();
        let ten = rt.alloc_int(10);
        let two = rt.alloc_int(2);
        let zero = rt.alloc_int(0);
        let mut ctx = rt.context();
        let some = |ctx: &mut crate::RuntimeContext, payload| {
            // SAFETY: a live context, and `Some`'s one slot takes an `Int`.
            unsafe {
                let e = praxis_alloc_enum(ctx, option_schema(), OPTION_SOME_TAG);
                praxis_enum_set_payload(ctx, e, 0, payload);
                e
            }
        };
        let cmp = |a: GcRef, b: GcRef| unsafe {
            enum_compare(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        };
        let none = unsafe { praxis_alloc_enum(&mut ctx, option_schema(), OPTION_NONE_TAG) };
        let some_zero = some(&mut ctx, zero);
        let some_two = some(&mut ctx, two);
        let some_ten = some(&mut ctx, ten);

        assert_eq!(cmp(some_zero, none), std::cmp::Ordering::Less);
        assert_eq!(cmp(none, some_zero), std::cmp::Ordering::Greater);
        assert_eq!(cmp(some_two, some_ten), std::cmp::Ordering::Less);
        assert_eq!(cmp(some_ten, some_ten), std::cmp::Ordering::Equal);
    }
}
