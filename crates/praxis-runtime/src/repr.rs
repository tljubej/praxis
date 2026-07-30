//! What a live value records about its own type (P0-11, foundation F11).
//!
//! A `GcRef`'s header names a [`TypeDescriptor`], and for most types that is the
//! whole story: an `Int` object is an `Int`. For the parameterized types it is
//! not — one `VEC` descriptor serves every `Vec[T]`, and the element type lives
//! in the *payload* (§11.2). Recovering a static `Type` from a value therefore
//! means reading per-instance descriptors out of payloads, which is a raw-pointer
//! operation this module performs once, safely, so the
//! [`praxis-repr`](https://docs.rs/praxis-repr) bridge never has to.
//!
//! The dispatch is a total match over [`BuiltinTypeId`]: a new built-in is a
//! compile error here, not a silent wrong answer. That is the same discipline
//! that makes the *forward* map total, and the two are inverses only because
//! both are exhaustive.

use crate::descriptor::{BuiltinTypeId, TypeDescriptor};
use crate::GcRef;

/// One per-instance type argument a value records.
///
/// `descriptor` is what the payload stores — `None` when the collection was
/// never told its element type (a null element descriptor, which every
/// `praxis_*_new` wrapper accepts as "unknown"). `sample` is a live value that
/// argument describes, when the payload holds one; recursing into it recovers
/// nested arguments a descriptor alone cannot (`Vec[Vec[Int]]`).
#[derive(Clone, Copy)]
pub struct InstanceArg {
    /// The descriptor the payload records for this argument position.
    pub descriptor: Option<&'static TypeDescriptor>,
    /// A live value of this argument's type, if the value holds one.
    pub sample: Option<GcRef>,
}

/// How much of a value's type its own payload determines.
pub enum InstanceRepr {
    /// The descriptor is the whole answer: every scalar, `Unit`, `BitSet`.
    Complete,
    /// The type takes arguments, and these are what the payload records, in the
    /// order the type spells them (`Map[K, V]` yields key then value).
    Args(Vec<InstanceArg>),
    /// The value's type is not recoverable from the value. The string says why,
    /// and is what a `NoRuntimeRepr` reports.
    Unrecorded(&'static str),
}

/// Read what `value` records about its own type.
///
/// # Safety
/// `value` must be a live `GcRef` whose payload matches its descriptor.
#[must_use]
pub unsafe fn instance_repr(value: GcRef) -> InstanceRepr {
    let Some(builtin) = value.descriptor().as_builtin() else {
        return InstanceRepr::Unrecorded("not a built-in type");
    };
    // SAFETY: forwarded from this function's contract — each arm reads the
    // payload its descriptor names.
    unsafe {
        match builtin {
            // Scalars and the two nullary collections are their own answer: a
            // `BitSet` holds `Int`s and a `Range` yields them, so neither has an
            // element descriptor to recover.
            BuiltinTypeId::Unit
            | BuiltinTypeId::Bool
            | BuiltinTypeId::Int
            | BuiltinTypeId::Byte
            | BuiltinTypeId::Char
            | BuiltinTypeId::Float
            | BuiltinTypeId::Text
            | BuiltinTypeId::BitSet
            | BuiltinTypeId::Range => InstanceRepr::Complete,

            BuiltinTypeId::Vec => {
                let p = &*value.payload::<crate::collections::VecPayload>();
                InstanceRepr::Args(vec![InstanceArg {
                    descriptor: nullable(p.element_descriptor),
                    sample: p.items.first().copied(),
                }])
            }
            BuiltinTypeId::Deque => {
                let p = &*value.payload::<crate::collections::DequePayload>();
                InstanceRepr::Args(vec![InstanceArg {
                    descriptor: nullable(p.element_descriptor),
                    sample: p.items.front().copied(),
                }])
            }
            BuiltinTypeId::Grid => {
                let p = &*value.payload::<crate::collections::GridPayload>();
                InstanceRepr::Args(vec![InstanceArg {
                    descriptor: nullable(p.element_descriptor),
                    sample: p.items.first().copied(),
                }])
            }
            BuiltinTypeId::Set => {
                let p = &*value.payload::<crate::maps::SetPayload>();
                InstanceRepr::Args(vec![InstanceArg {
                    descriptor: Some(p.element_descriptor),
                    sample: p.entries.iter().next().map(|k| k.value()),
                }])
            }
            BuiltinTypeId::MinHeap => {
                let p = &*value.payload::<crate::heaps::MinHeapPayload>();
                InstanceRepr::Args(vec![InstanceArg {
                    descriptor: Some(p.element_descriptor),
                    sample: p.items.peek().map(|e| e.0.value),
                }])
            }
            BuiltinTypeId::MaxHeap => {
                let p = &*value.payload::<crate::heaps::MaxHeapPayload>();
                InstanceRepr::Args(vec![InstanceArg {
                    descriptor: Some(p.element_descriptor),
                    sample: p.items.peek().map(|e| e.value),
                }])
            }
            BuiltinTypeId::Map => {
                let p = &*value.payload::<crate::maps::MapPayload>();
                let entry = p.entries.iter().next();
                InstanceRepr::Args(vec![
                    InstanceArg {
                        descriptor: Some(p.key_descriptor),
                        sample: entry.map(|(k, _)| k.value()),
                    },
                    InstanceArg {
                        descriptor: Some(p.value_descriptor),
                        sample: entry.map(|(_, v)| *v),
                    },
                ])
            }
            // `Counter[T]` is unary: its values are always `Int` (§6.2).
            BuiltinTypeId::Counter => {
                let p = &*value.payload::<crate::maps::CounterPayload>();
                InstanceRepr::Args(vec![InstanceArg {
                    descriptor: Some(p.key_descriptor),
                    sample: p.entries.keys().next().map(|k| k.value()),
                }])
            }
            BuiltinTypeId::Tuple => {
                let p = &*value.payload::<crate::tuples::TuplePayload>();
                if p.schema.is_null() {
                    return InstanceRepr::Unrecorded("tuple has no schema");
                }
                let schema = &*p.schema;
                InstanceRepr::Args(
                    schema
                        .descriptors
                        .iter()
                        .enumerate()
                        .map(|(i, d)| InstanceArg {
                            descriptor: nullable(*d),
                            sample: p.items.get(i).copied(),
                        })
                        .collect(),
                )
            }
            // A record/enum object carries its *field* schema, not which named
            // type it is: two records with the same field descriptors are
            // indistinguishable here. Nominal identity is F12 (S10).
            BuiltinTypeId::Record => {
                InstanceRepr::Unrecorded("a record value does not record its nominal identity")
            }
            BuiltinTypeId::Enum => {
                InstanceRepr::Unrecorded("an enum value does not record its nominal identity")
            }
            BuiltinTypeId::Closure => {
                InstanceRepr::Unrecorded("a closure records no parameter or result types")
            }
            // A `VarCell` is the compiler's mutable slot, not a source type.
            BuiltinTypeId::VarCell => {
                InstanceRepr::Unrecorded("a VarCell is a compiler-internal slot, not a source type")
            }
        }
    }
}

/// A payload's element-descriptor slot, as an `Option`. Null means "this
/// collection was never told its element type".
#[inline]
fn nullable(d: *const TypeDescriptor) -> Option<&'static TypeDescriptor> {
    // SAFETY: a non-null element descriptor is always a `&'static` written by
    // the constructor that built the payload.
    (!d.is_null()).then(|| unsafe { &*d })
}
