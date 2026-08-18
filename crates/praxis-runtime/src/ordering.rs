//! The order a container walks its keys in (ADR-138).
//!
//! `Map`, `Set` and `Counter` are hash-backed, and Rust randomizes hash-table
//! iteration order *per process*. ADR-066 answers that by sorting before
//! printing and before snapshotting, so `out(s)` and `for x in s` are
//! reproducible.
//!
//! This module is that sort key: the value's own order, taken from
//! [`TypeDescriptor::compare`], so `{10, 2}` walks `2` first rather than in
//! the order the rendered forms fall. Three rules, applied in this sequence:
//!
//! 1. **Different types order by descriptor id.** By the id, never by the
//!    descriptor's *address*: addresses are assigned by the loader and differ
//!    between runs, so ordering by one would reintroduce exactly the
//!    per-process nondeterminism this exists to remove.
//! 2. **Same type with a `compare` callback orders through it.** Every type a
//!    key can be has one, so this is the rule that answers in practice, and it
//!    is the same callback `sorted()` and a heap's `Ord` already use — which is
//!    what makes `out(s)` and `out(s.sorted())` agree.
//! 3. **Same type without one falls back to the rendered forms.** A schema slot
//!    is allowed to be null (ADR-066 decision 5), so a value inside a composite
//!    key can be of a type the compiler never resolved, and a total order still
//!    has to come out. The fallback is load-bearing, not defensive: answering
//!    `Equal` here would leave ties in the sort whose resolution is the hash
//!    table's own randomized order, which is the defect ADR-066 bought off.
//!
//! Totality follows from the three rules being applied in order and each being
//! total on its own domain: rule 1 is a total order on `u32`, rule 2 is total by
//! [`CompareFn`](crate::descriptor::CompareFn)'s contract (including `Float`
//! NaN, which sorts last and equals itself — ADR-045 decision 2), and rule 3 is
//! a total order on `String`. Determinism follows from none of the three reading
//! an address.

use std::cmp::Ordering;

use crate::GcRef;
use crate::descriptor::{FormatSink, TypeDescriptor};
use crate::maps::render_into;

/// Order two values by the type each one says it is.
///
/// This is the order every hash-backed collection walks and prints its keys in.
///
/// # Safety
/// Each value's payload must match the descriptor in its own header, which is
/// the invariant `GcRef` maintains for every allocated object.
pub(crate) unsafe fn container_cmp(a: GcRef, b: GcRef) -> Ordering {
    // SAFETY: the caller guarantees each payload matches its header descriptor.
    unsafe { slot_cmp(a, b, a.descriptor(), b.descriptor()) }
}

/// Order two values dispatched through descriptors a *schema* supplied, for the
/// composite `compare` callbacks to recurse through.
///
/// Separate from [`container_cmp`] because a tuple, record or enum reads its
/// element types out of its schema — with the value's own descriptor as the
/// fallback for a null slot — exactly as `equals`, `hash` and `format` already
/// do. Both entry points apply the same three rules, so a composite orders the
/// same way whether it is a key itself or an element of one.
///
/// # Safety
/// `a`'s payload must match `da` and `b`'s must match `db`.
pub(crate) unsafe fn slot_cmp(
    a: GcRef,
    b: GcRef,
    da: &'static TypeDescriptor,
    db: &'static TypeDescriptor,
) -> Ordering {
    // Rule 1: different types are separated by id, so a heterogeneous
    // collection still has one order and it is the same order twice.
    let (ida, idb) = (da.id().to_u32(), db.id().to_u32());
    if ida != idb {
        return ida.cmp(&idb);
    }
    match da.compare {
        // Rule 2: the type's own order.
        // SAFETY: the caller guarantees both payloads match their descriptors,
        // and the ids agreeing means both match `da`.
        Some(compare) => unsafe {
            compare(
                a.payload::<u8>() as *const u8,
                b.payload::<u8>() as *const u8,
            )
        },
        // Rule 3: no order of its own, so the rendering is the tie-break.
        // SAFETY: as above.
        None => unsafe { rendered_cmp(a, da, b, db) },
    }
}

/// Order two values by their rendered forms — rule 3, kept in one place so the
/// reason it exists stays attached to it.
///
/// Two values that render identically compare `Equal`, which is honest: nothing
/// a program can observe distinguishes them, so a sort that leaves them in
/// either order still prints and iterates one sequence.
///
/// # Safety
/// As [`slot_cmp`].
unsafe fn rendered_cmp(a: GcRef, da: &TypeDescriptor, b: GcRef, db: &TypeDescriptor) -> Ordering {
    let mut left = String::new();
    let mut right = String::new();
    // **`display`, unconditionally.** This is an ordering, not a display: it
    // decides the sequence a `for` over a `Map` walks and the order a `Set`
    // prints in (ADR-138 decision 4). Reading a style off a caller would make
    // that sequence depend on who was rendering — a program iterating one order
    // and a debugger pane showing another — and the quoting is not neutral about
    // it, since `"` sorts below every printable character a value could start
    // with.
    // SAFETY: the caller guarantees each payload matches its descriptor.
    unsafe {
        render_into(&mut FormatSink::display(&mut left), da, a);
        render_into(&mut FormatSink::display(&mut right), db, b);
    }
    left.cmp(&right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Runtime;
    use crate::records::{RecordField, RecordSchema, SchemaIdentity};
    use crate::tuples::TupleSchema;

    /// One value of each type a `Map` key or `Set` member can be, plus a `Vec`
    /// — which can never be a key, and is here because it is the one descriptor
    /// in reach whose `compare` is `None`, so it exercises rule 3.
    fn one_of_every_key_type(rt: &mut Runtime) -> Vec<GcRef> {
        let tuple_schema: &'static TupleSchema = Box::leak(Box::new(TupleSchema {
            descriptors: Box::leak(
                vec![
                    &crate::scalars::INT as *const TypeDescriptor,
                    &crate::text::TEXT as *const TypeDescriptor,
                ]
                .into_boxed_slice(),
            ),
        }));
        let record_schema: &'static RecordSchema = Box::leak(Box::new(RecordSchema {
            identity: SchemaIdentity::Nominal("Point"),
            fields: Box::leak(
                vec![RecordField {
                    name: "x",
                    descriptor: &crate::scalars::INT,
                }]
                .into_boxed_slice(),
            ),
        }));

        let scalars = vec![
            rt.alloc_int(3),
            rt.alloc_int(-1),
            rt.alloc_byte(200),
            rt.alloc_char('z' as u32),
            rt.alloc_float(f64::NAN),
            rt.alloc_float(2.5),
            rt.alloc_text("abc"),
            rt.alloc_bool(true),
            rt.alloc_bool(false),
            rt.alloc_unit(),
        ];
        let (one, five, seven, seven_text) = (
            rt.alloc_int(1),
            rt.alloc_int(5),
            rt.alloc_int(7),
            rt.alloc_text("seven"),
        );

        let mut ctx = rt.context();
        let mut out = scalars;
        // SAFETY: every wrapper below is called with a live context and values
        // of the types it names.
        unsafe {
            out.push(crate::abi::praxis_range_new(&mut ctx, one, five));
            let tuple = crate::abi::praxis_alloc_tuple(&mut ctx, tuple_schema);
            crate::abi::praxis_tuple_set(&mut ctx, tuple, 0, seven);
            crate::abi::praxis_tuple_set(&mut ctx, tuple, 1, seven_text);
            out.push(tuple);
            let record = crate::abi::praxis_alloc_record(&mut ctx, record_schema);
            crate::abi::praxis_record_set_field(&mut ctx, record, 0, one);
            out.push(record);
            out.push(crate::abi::praxis_alloc_enum(
                &mut ctx,
                crate::enums::option_schema(),
                0,
            ));
            out.push(crate::abi::praxis_vec_new(&mut ctx, &crate::scalars::INT));
        }
        out
    }

    /// The property ADR-066 buys and ADR-138 has to keep: the container order is
    /// a real total order. Brute-forced over one value of every key type,
    /// because a sort whose comparator is not a total order does not merely
    /// answer oddly — `sort_by` is allowed to answer differently each time.
    #[test]
    fn container_cmp_is_a_total_order_over_every_key_type() {
        let mut rt = Runtime::new();
        let values = one_of_every_key_type(&mut rt);
        let cmp = |a: GcRef, b: GcRef| unsafe { container_cmp(a, b) };

        for &a in &values {
            assert_eq!(cmp(a, a), Ordering::Equal, "reflexive");
            for &b in &values {
                assert_eq!(
                    cmp(a, b),
                    cmp(b, a).reverse(),
                    "antisymmetric: {} vs {}",
                    a.descriptor().name,
                    b.descriptor().name
                );
                for &c in &values {
                    if cmp(a, b) != Ordering::Greater && cmp(b, c) != Ordering::Greater {
                        assert_ne!(
                            cmp(a, c),
                            Ordering::Greater,
                            "transitive through {}",
                            b.descriptor().name
                        );
                    }
                }
            }
        }
    }

    /// Rule 1 compares the descriptor *id*, not its address. A pointer order is
    /// stable within one process and arbitrary across processes, so ordering by
    /// one would put the RT-16 nondeterminism back in a place no in-process
    /// test could see it. The id is a small dense integer the runtime assigns,
    /// so it is the same number in every run.
    #[test]
    fn values_of_different_types_order_by_descriptor_id_and_not_by_address() {
        let rt = Runtime::new();
        let int = rt.alloc_int(1);
        let text = rt.alloc_text("1");
        let expected = crate::scalars::INT
            .id()
            .to_u32()
            .cmp(&crate::text::TEXT.id().to_u32());
        assert_eq!(unsafe { container_cmp(int, text) }, expected);

        // A second runtime is a second set of allocations; the answer is the
        // same because nothing in the comparison reads where anything lives.
        let rt2 = Runtime::new();
        assert_eq!(
            unsafe { container_cmp(rt2.alloc_int(1), rt2.alloc_text("1")) },
            expected
        );
    }

    /// Rule 3. A type with no `compare` still gets a deterministic answer, and
    /// it is the rendered one — the fallback a null schema slot needs
    /// (ADR-066 decision 5). Answering `Equal` here instead would leave the
    /// sort's ties to be settled by the hash table's randomized iteration
    /// order, which is the whole defect.
    #[test]
    fn a_value_whose_type_has_no_order_still_orders_deterministically() {
        let mut rt = Runtime::new();
        assert!(
            !crate::collections::VEC.is_orderable(),
            "a Vec can never be a key, so it carries no compare (ADR-057 D4)"
        );
        let (one, two) = (rt.alloc_int(1), rt.alloc_int(2));
        let mut ctx = rt.context();
        // SAFETY: a live context, and both pushes carry the `Int` the vector
        // was told it holds.
        let (ones, twos) = unsafe {
            let ones = crate::abi::praxis_vec_new(&mut ctx, &crate::scalars::INT);
            let twos = crate::abi::praxis_vec_new(&mut ctx, &crate::scalars::INT);
            crate::abi::praxis_vec_push(&mut ctx, ones, one);
            crate::abi::praxis_vec_push(&mut ctx, twos, two);
            (ones, twos)
        };

        // `"[1]"` against `"[2]"`: the rendered forms decide, and they decide
        // the same way twice.
        assert_eq!(unsafe { container_cmp(ones, twos) }, Ordering::Less);
        assert_eq!(unsafe { container_cmp(ones, twos) }, Ordering::Less);
        assert_eq!(unsafe { container_cmp(twos, ones) }, Ordering::Greater);
    }
}
