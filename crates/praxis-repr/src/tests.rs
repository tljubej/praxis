//! The bridge's contract: both directions are total, and they are inverses.

use super::*;
use praxis_runtime::abi::*;
use praxis_runtime::context::{Runtime, RuntimeContext};
use praxis_runtime::descriptor::BuiltinTypeId;

/// A wired context backed by a real runtime.
fn wired_ctx(rt: &mut Runtime) -> *mut RuntimeContext {
    Box::leak(Box::new(rt.context())) as *mut RuntimeContext
}

/// One live sample per built-in, so the round-trip is asserted against real
/// objects rather than against descriptors alone.
///
/// # Safety
/// `ctx` must be live and wired.
unsafe fn sample(rt: &Runtime, ctx: *mut RuntimeContext, builtin: BuiltinTypeId) -> GcRef {
    use BuiltinTypeId as B;
    let int_desc = &praxis_runtime::scalars::INT as *const _;
    let text_desc = &praxis_runtime::text::TEXT as *const _;
    unsafe {
        match builtin {
            B::Unit => praxis_alloc_unit(ctx),
            B::Bool => praxis_alloc_bool(ctx, 1),
            B::Int => praxis_alloc_int(ctx, 7),
            B::Byte => rt.alloc_byte(3),
            B::Char => praxis_alloc_char(ctx, 'x' as i64),
            B::Float => praxis_alloc_float(ctx, f64::to_bits(1.5) as i64),
            B::Text => {
                let s = "hi";
                praxis_alloc_text(ctx, s.as_ptr(), s.len())
            }
            // A *non-empty* Vec[Text]: the whole point of the inverse is that it
            // recovers `Text`, not the `Int` the old debugger guessed.
            B::Vec => {
                let v = praxis_vec_new(ctx, text_desc);
                let s = "e";
                praxis_vec_push(ctx, v, praxis_alloc_text(ctx, s.as_ptr(), s.len()));
                v
            }
            B::Deque => {
                let d = praxis_deque_new(ctx, int_desc);
                praxis_deque_push_back(ctx, d, praxis_alloc_int(ctx, 1));
                d
            }
            B::Grid => praxis_grid_new(ctx, int_desc, 0, 0),
            B::Map => {
                let m = praxis_map_new(ctx, int_desc);
                let s = "v";
                praxis_map_insert(
                    ctx,
                    m,
                    praxis_alloc_int(ctx, 1),
                    praxis_alloc_text(ctx, s.as_ptr(), s.len()),
                );
                m
            }
            B::Set => {
                let s = praxis_set_new(ctx, int_desc);
                praxis_set_insert(ctx, s, praxis_alloc_int(ctx, 1));
                s
            }
            B::Counter => {
                let c = praxis_counter_new(ctx, int_desc);
                praxis_counter_inc(ctx, c, praxis_alloc_int(ctx, 1));
                c
            }
            B::MinHeap => {
                let h = praxis_min_heap_new(ctx, int_desc);
                praxis_min_heap_push(ctx, h, praxis_alloc_int(ctx, 1));
                h
            }
            B::MaxHeap => {
                let h = praxis_max_heap_new(ctx, int_desc);
                praxis_max_heap_push(ctx, h, praxis_alloc_int(ctx, 1));
                h
            }
            B::BitSet => praxis_bitset_new(ctx),
            B::Tuple => {
                let schema = praxis_runtime::tuples::point_schema()
                    as *const praxis_runtime::tuples::TupleSchema;
                let t = praxis_alloc_tuple(ctx, schema);
                praxis_tuple_set(ctx, t, 0, praxis_alloc_int(ctx, 1));
                praxis_tuple_set(ctx, t, 1, praxis_alloc_int(ctx, 2));
                t
            }
            B::Record => {
                // Leaked, not `static`: a schema holds raw descriptor pointers
                // and is therefore not `Sync`.
                let fields: &'static [praxis_runtime::RecordField] =
                    Box::leak(Box::new([praxis_runtime::RecordField {
                        name: "x",
                        descriptor: &praxis_runtime::scalars::INT,
                    }]));
                let schema: &'static praxis_runtime::RecordSchema =
                    Box::leak(Box::new(praxis_runtime::RecordSchema {
                        identity: praxis_runtime::SchemaIdentity::Anonymous,
                        fields,
                    }));
                praxis_alloc_record(ctx, schema as *const _)
            }
            B::Enum => praxis_alloc_enum(ctx, 0, 0),
            B::Closure => praxis_alloc_closure(ctx, std::ptr::null(), 0),
            B::VarCell => praxis_alloc_var_cell(ctx, praxis_alloc_int(ctx, 1)),
        }
    }
}

/// Every built-in, in declaration order. Exhaustiveness is enforced by
/// `BuiltinTypeId::COUNT`, so a new built-in fails this list rather than
/// silently skipping the round-trip.
const ALL: [BuiltinTypeId; BuiltinTypeId::COUNT] = {
    use BuiltinTypeId as B;
    [
        B::Unit,
        B::Bool,
        B::Int,
        B::Byte,
        B::Char,
        B::Float,
        B::Text,
        B::Vec,
        B::Deque,
        B::Grid,
        B::Map,
        B::Set,
        B::Counter,
        B::MinHeap,
        B::MaxHeap,
        B::BitSet,
        B::Tuple,
        B::Record,
        B::Enum,
        B::Closure,
        B::VarCell,
    ]
};

/// The round trip F11 exists for: for every built-in whose values *do* record
/// their type, `descriptor_for_type(type_for_value(v)) == v.descriptor()` by
/// **pointer**. The built-ins that do not are named, so "we forgot one" and "the
/// runtime genuinely does not record it" cannot be confused.
#[test]
fn every_builtin_value_round_trips() {
    use BuiltinTypeId as B;
    let mut rt = Runtime::new();
    let ctx = wired_ctx(&mut rt);
    let mut db = TypeDb::new();

    for builtin in ALL {
        let value = unsafe { sample(&rt, ctx, builtin) };
        assert!(
            std::ptr::eq(value.descriptor(), builtin.descriptor()),
            "{builtin:?}'s sample is not of that type"
        );
        let recovered = unsafe { type_for_value(value, &mut db) };
        match builtin {
            // The four the runtime genuinely does not record. Each is a stated
            // limitation with an owner: nominal identity is F12 (S10); a
            // closure has no runtime signature; a VarCell is not a source type.
            B::Record | B::Enum | B::Closure | B::VarCell => {
                assert!(
                    recovered.is_err(),
                    "{builtin:?} must report why it cannot be recovered, not guess"
                );
                continue;
            }
            _ => {}
        }
        let ty = recovered.unwrap_or_else(|e| panic!("{builtin:?} did not recover: {e}"));
        let back = descriptor_for_type(&db, ty)
            .unwrap_or_else(|e| panic!("{builtin:?} recovered a type with no descriptor: {e}"));
        assert!(
            std::ptr::eq(back, builtin.descriptor()),
            "{builtin:?} round-tripped to {}",
            back.name
        );
    }
}

/// DBG-02's half of P0-11: the inverse used to answer `Vec[Int]` for every
/// vector and `Map[Int, Int]` for every map. It must read the payload.
#[test]
fn the_inverse_recovers_real_element_types_not_int() {
    let mut rt = Runtime::new();
    let ctx = wired_ctx(&mut rt);
    let mut db = TypeDb::new();

    let vec_of_text = unsafe { sample(&rt, ctx, BuiltinTypeId::Vec) };
    let ty = unsafe { type_for_value(vec_of_text, &mut db) }.expect("Vec[Text] recovers");
    assert_eq!(
        db.render(ty),
        "Vec[Text]",
        "a Vec of Text must not recover as Vec[Int]"
    );

    let map = unsafe { sample(&rt, ctx, BuiltinTypeId::Map) };
    let ty = unsafe { type_for_value(map, &mut db) }.expect("Map[Int, Text] recovers");
    assert_eq!(
        db.render(ty),
        "Map[Int, Text]",
        "a Map[Int, Text] must not recover as Map[Int, Int]"
    );
}

/// Nesting is why the inverse walks live elements rather than the element
/// descriptor: every nested vector's descriptor is `VEC`, so a descriptor-only
/// walk cannot tell `Vec[Vec[Int]]` from `Vec[Vec[Text]]`.
#[test]
fn a_nested_collection_recovers_through_its_elements() {
    let mut rt = Runtime::new();
    let ctx = wired_ctx(&mut rt);
    let mut db = TypeDb::new();

    let ty = unsafe {
        let inner = praxis_vec_new(ctx, &praxis_runtime::text::TEXT as *const _);
        let s = "a";
        praxis_vec_push(ctx, inner, praxis_alloc_text(ctx, s.as_ptr(), s.len()));
        let outer = praxis_vec_new(ctx, &praxis_runtime::collections::VEC as *const _);
        praxis_vec_push(ctx, outer, inner);
        type_for_value(outer, &mut db)
    }
    .expect("Vec[Vec[Text]] recovers");

    assert_eq!(db.render(ty), "Vec[Vec[Text]]");
}

/// P0-11 itself: the four types that used to arrive at the `INT` descriptor
/// through a `_ =>` arm now arrive at their own.
#[test]
fn the_types_that_used_to_fall_back_to_int_resolve_to_themselves() {
    let mut db = TypeDb::new();
    let float = db.float();
    let unit = db.unit();
    let int = db.int();
    let tuple = db.pair(int, int);

    let cases: [(Type, &'static TypeDescriptor); 3] = [
        (float, &praxis_runtime::scalars::FLOAT),
        (unit, &praxis_runtime::scalars::UNIT),
        (tuple, &praxis_runtime::tuples::TUPLE),
    ];
    for (ty, expected) in cases {
        let got = descriptor_for_type(&db, ty).expect("has a descriptor");
        assert!(
            std::ptr::eq(got, expected),
            "expected {}, got {}",
            expected.name,
            got.name
        );
    }
    assert!(
        !std::ptr::eq(
            descriptor_for_type(&db, float).unwrap(),
            &praxis_runtime::scalars::INT
        ),
        "Float must not resolve to the Int descriptor"
    );
}

/// A function value is a closure object, not an `Int` (P0-11's Func arm).
#[test]
fn a_function_type_resolves_to_the_closure_descriptor() {
    let mut db = TypeDb::new();
    let int = db.int();
    let f = db.func(vec![int], int);
    assert!(std::ptr::eq(
        descriptor_for_type(&db, f).expect("closures have a descriptor"),
        &praxis_runtime::closures::CLOSURE
    ));
}

/// The types with no runtime object must say so rather than name one. Each of
/// these used to reach `INT`, which is what made a wrong-layout payload read
/// reachable from a compiler bug.
#[test]
fn a_type_with_no_runtime_object_has_no_descriptor() {
    let mut db = TypeDb::new();
    let never = db.scalar(ScalarType::Never);
    let uint = db.scalar(ScalarType::UInt);
    let int = db.int();
    // `Range` is nullary (F5 is what makes the old `args: vec![int]` here
    // unrepresentable); `Seq` is the compiler-internal unary sequence.
    let range = db
        .collection(CollectionCtor::Range, praxis_types::CollectionArgs::Nullary)
        .expect("Range is nullary");
    let seq = db.unary_collection(CollectionCtor::Seq, int);
    let var = db.fresh_var();

    for (ty, what) in [
        (never, "Never"),
        (uint, "UInt"),
        (range, "Range"),
        (seq, "Seq"),
        (var, "an unresolved variable"),
    ] {
        let err = descriptor_for_type(&db, ty)
            .err()
            .unwrap_or_else(|| panic!("{what} must have no descriptor"));
        assert!(
            !err.reason.is_empty(),
            "{what}'s refusal must explain itself"
        );
    }
}

/// Construction descriptors come from the type's arguments, and a collection of
/// an unrepresentable type is refused rather than silently built.
#[test]
fn element_descriptors_follow_the_collection_arity() {
    let mut db = TypeDb::new();
    let int = db.int();
    let text = db.text();

    let vec_text = db.vec(text);
    let got = element_descriptors_for(&db, vec_text).expect("Vec[Text]");
    assert_eq!(got.len(), 1);
    assert!(std::ptr::eq(got[0], &praxis_runtime::text::TEXT));

    let map = db.map(text, int);
    let got = element_descriptors_for(&db, map).expect("Map[Text, Int]");
    assert_eq!(got.len(), 2, "Map reports key and value");
    assert!(std::ptr::eq(got[0], &praxis_runtime::text::TEXT));
    assert!(std::ptr::eq(got[1], &praxis_runtime::scalars::INT));

    // `Counter[T]` is unary: its values are always Int and are not an argument.
    let counter = db.unary_collection(CollectionCtor::Counter, text);
    assert_eq!(element_descriptors_for(&db, counter).unwrap().len(), 1);

    // BitSet is nullary.
    let bitset = db
        .collection(
            CollectionCtor::BitSet,
            praxis_types::CollectionArgs::Nullary,
        )
        .expect("BitSet is nullary");
    assert!(element_descriptors_for(&db, bitset).unwrap().is_empty());

    // A collection of an unrepresentable element is not constructible.
    let never = db.scalar(ScalarType::Never);
    let vec_never = db.vec(never);
    assert!(element_descriptors_for(&db, vec_never).is_err());
}
