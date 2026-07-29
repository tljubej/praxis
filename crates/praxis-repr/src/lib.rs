//! The total, bidirectional bridge between a static [`Type`] and its runtime
//! [`TypeDescriptor`] (foundation F11; closes P0-11).
//!
//! # Why one module
//!
//! The two directions used to be written independently and were not inverses.
//! Codegen sent `Float`, `Unit`, `Record` and `Enum` to the `INT` descriptor
//! through a `_ => INT` fallback, so a `Float` field in a record schema
//! dispatched `Int`'s equality callback against an `f64` payload. Meanwhile the
//! debugger reconstructed every `Vec[T]` as `Vec[Int]` and every `Map` as
//! `Map[Int, Int]`. Each was locally plausible; together they made ADR-035's
//! round-trip claim false.
//!
//! Colocating them makes the round-trip a test
//! ([`tests::every_builtin_value_round_trips`]) instead of a hope, and the
//! matches on both sides are exhaustive — a new [`BuiltinTypeId`], a new
//! `ScalarType`, or a new `CollectionCtor` is a compile error here.
//!
//! # What "total" means
//!
//! [`descriptor_for_type`] returns `Result`. A type with no runtime
//! representation — `Range`, the compiler-internal `Seq`, `UInt`, `Never`, an
//! unresolved type variable — yields [`NoRuntimeRepr`] rather than a descriptor
//! that names a different type. Each of those is an upstream compiler bug at a
//! descriptor-producing site, and the JIT refusing to emit is how it becomes
//! visible instead of becoming a wrong payload read at runtime (design decision
//! D9).

use praxis_runtime::descriptor::{BuiltinTypeId, TypeDescriptor};
use praxis_runtime::repr::{instance_repr, InstanceArg, InstanceRepr};
use praxis_runtime::GcRef;
use praxis_stdlib::type_pattern::CollectionCtor;
use praxis_types::data::TypeData;
use praxis_types::{CollectionArgs, ScalarType, TupleElems, Type, TypeDb};

/// Why a type has no runtime descriptor. The two cases have different owners
/// and different correct handling, so they are not one string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoReprCause {
    /// No object of this type can exist, so no descriptor can describe one:
    /// `Never`, `UInt`, `Range`, `Seq`, and every non-built-in descriptor on the
    /// inverse side. Reaching one at a descriptor-producing site is a bug in the
    /// caller, and the JIT refuses to emit (design decision D9).
    NoSuchObject,
    /// Inference did not finish here: the type is still a variable. The MIR
    /// *should* have spelled this `MirType::Opaque` and cannot, because per-use
    /// inferred types are not threaded into HIR yet (HIR-01/MONO-01, S15). A
    /// caller for which "unknown" is representable — a collection's element
    /// descriptor — may treat this as `Opaque`; one for which it is not must
    /// still refuse. See hazard H10.
    Unresolved,
}

/// A type with no runtime representation, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoRuntimeRepr {
    /// The type that has no representation. `None` when the question started
    /// from a *value* rather than a type.
    pub ty: Option<Type>,
    /// Which of the two situations this is.
    pub cause: NoReprCause,
    /// Why, in a form fit for a compiler diagnostic.
    pub reason: &'static str,
}

impl NoRuntimeRepr {
    fn of(ty: Type, reason: &'static str) -> NoRuntimeRepr {
        NoRuntimeRepr {
            ty: Some(ty),
            cause: NoReprCause::NoSuchObject,
            reason,
        }
    }

    fn unresolved(ty: Type) -> NoRuntimeRepr {
        NoRuntimeRepr {
            ty: Some(ty),
            cause: NoReprCause::Unresolved,
            reason: "unresolved type variable: inference did not reach this site",
        }
    }

    fn value(reason: &'static str) -> NoRuntimeRepr {
        NoRuntimeRepr {
            ty: None,
            cause: NoReprCause::NoSuchObject,
            reason,
        }
    }

    /// True iff the type is merely not inferred yet, rather than unrepresentable.
    #[must_use]
    pub fn is_unresolved(&self) -> bool {
        self.cause == NoReprCause::Unresolved
    }
}

impl std::fmt::Display for NoRuntimeRepr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no runtime representation: {}", self.reason)
    }
}

impl std::error::Error for NoRuntimeRepr {}

// ---------------------------------------------------------------------------
// Forward: Type → TypeDescriptor
// ---------------------------------------------------------------------------

/// The runtime descriptor for values of type `ty`.
///
/// Exhaustive: every arm names a descriptor or says why there is none. The
/// answer is a `&'static`, so [`std::ptr::eq`] on two results is authoritative
/// type identity (ADR-038).
///
/// One descriptor serves every instantiation of a parameterized type — `VEC` is
/// the answer for every `Vec[T]` — because the element type lives in the
/// payload (§11.2). Use [`element_descriptors_for`] to resolve what goes *into*
/// the payload at construction.
pub fn descriptor_for_type(
    db: &TypeDb,
    ty: Type,
) -> Result<&'static TypeDescriptor, NoRuntimeRepr> {
    Ok(builtin_for_type(db, ty)?.descriptor())
}

/// Which built-in a static type is represented by. The single exhaustive match;
/// [`descriptor_for_type`] is this plus the runtime's own id → descriptor table,
/// so the two cannot drift.
fn builtin_for_type(db: &TypeDb, ty: Type) -> Result<BuiltinTypeId, NoRuntimeRepr> {
    let resolved = db.follow(ty);
    match db.data(resolved) {
        TypeData::Scalar(s) => match s {
            ScalarType::Bool => Ok(BuiltinTypeId::Bool),
            ScalarType::Int => Ok(BuiltinTypeId::Int),
            ScalarType::Byte => Ok(BuiltinTypeId::Byte),
            ScalarType::Char => Ok(BuiltinTypeId::Char),
            ScalarType::Float => Ok(BuiltinTypeId::Float),
            ScalarType::Text => Ok(BuiltinTypeId::Text),
            // Reserved in the type system, with no runtime object (§4.3). A
            // value of either type cannot exist, so neither can a descriptor
            // for one.
            ScalarType::UInt => Err(NoRuntimeRepr::of(
                resolved,
                "UInt is reserved and has no runtime representation",
            )),
        },
        TypeData::Unit => Ok(BuiltinTypeId::Unit),
        TypeData::Never => Err(NoRuntimeRepr::of(
            resolved,
            "Never is the bottom type: no value of it exists",
        )),
        TypeData::Tuple(_) => Ok(BuiltinTypeId::Tuple),
        // A function value is a closure object at runtime (ADR-027), including
        // a bare top-level `fn` used as a value.
        TypeData::Func { .. } => Ok(BuiltinTypeId::Closure),
        TypeData::Record { .. } => Ok(BuiltinTypeId::Record),
        TypeData::Enum { .. } => Ok(BuiltinTypeId::Enum),
        TypeData::Collection { ctor, .. } => match ctor {
            CollectionCtor::Vec => Ok(BuiltinTypeId::Vec),
            CollectionCtor::Deque => Ok(BuiltinTypeId::Deque),
            CollectionCtor::Grid => Ok(BuiltinTypeId::Grid),
            CollectionCtor::Map => Ok(BuiltinTypeId::Map),
            CollectionCtor::Set => Ok(BuiltinTypeId::Set),
            CollectionCtor::Counter => Ok(BuiltinTypeId::Counter),
            CollectionCtor::MinHeap => Ok(BuiltinTypeId::MinHeap),
            CollectionCtor::MaxHeap => Ok(BuiltinTypeId::MaxHeap),
            CollectionCtor::BitSet => Ok(BuiltinTypeId::BitSet),
            // `Range` is unimplemented (design decision D6) and `Seq` is the
            // compiler's lazy pipeline source, which is fused away before
            // codegen and never materializes (§6.3). Reaching either here means
            // a pipeline stage escaped fusion.
            CollectionCtor::Range => Err(NoRuntimeRepr::of(
                resolved,
                "Range has no runtime object (design decision D6)",
            )),
            CollectionCtor::Seq => Err(NoRuntimeRepr::of(
                resolved,
                "Seq is a compiler-internal lazy sequence and is fused away before codegen",
            )),
        },
        // An unresolved variable at a descriptor-producing site means inference
        // did not finish. Emitting *some* descriptor here is what P0-11 was —
        // but the caller decides, because "unknown element type" is a thing a
        // collection payload can actually hold and a record schema cannot.
        TypeData::Var(_) => Err(NoRuntimeRepr::unresolved(resolved)),
    }
}

/// The per-instance argument descriptors a collection of type `ty` must be
/// constructed with — one for `Vec[T]`/`Set[T]`/…, two for `Map[K, V]`.
///
/// `Counter[T]` reports only its key: its values are always `Int` (§6.2).
/// Nullary collections (`BitSet`, `Range`) report none.
///
/// Returns `Err` if `ty` is not a collection, or if any argument has no runtime
/// representation — a `Vec[Range]` cannot be constructed, and saying so at
/// compile time is the point.
pub fn element_descriptors_for(
    db: &TypeDb,
    ty: Type,
) -> Result<Vec<&'static TypeDescriptor>, NoRuntimeRepr> {
    let resolved = db.follow(ty);
    let TypeData::Collection { ctor, args } = db.data(resolved) else {
        return Err(NoRuntimeRepr::of(resolved, "not a collection type"));
    };
    let wanted = ctor.arity();
    args.iter()
        .take(wanted)
        .map(|a| descriptor_for_type(db, *a))
        .collect()
}

// ---------------------------------------------------------------------------
// Inverse: value → Type
// ---------------------------------------------------------------------------

/// The static type of a live value, read from what the value itself records.
///
/// Faithful where the value is: a `Vec[Text]` recovers as `Vec[Text]`, and a
/// non-empty `Vec[Vec[Int]]` recovers exactly, because the recursion walks a
/// live element rather than the element *descriptor* (which is `VEC` for every
/// nested vector). An empty collection recovers only as far as its element
/// descriptor reaches.
///
/// `Err` where the value genuinely does not record its type: a record or enum
/// object carries a field schema, not which named type it is (nominal identity
/// is F12), and a closure records nothing about its signature.
///
/// # Safety
/// `value` must be a live `GcRef` whose payload matches its descriptor.
pub unsafe fn type_for_value(value: GcRef, db: &mut TypeDb) -> Result<Type, NoRuntimeRepr> {
    let descriptor = value.descriptor();
    // SAFETY: forwarded from this function's contract.
    let repr = unsafe { instance_repr(value) };
    match repr {
        InstanceRepr::Complete => type_for_descriptor(descriptor, db),
        InstanceRepr::Unrecorded(reason) => Err(NoRuntimeRepr::value(reason)),
        InstanceRepr::Args(args) => {
            let mut arg_types = Vec::with_capacity(args.len());
            for arg in args {
                arg_types.push(unsafe { type_for_arg(arg, db) }?);
            }
            compose(descriptor, arg_types, db)
        }
    }
}

/// The type of one per-instance argument: from a live sample if there is one
/// (so nesting recovers exactly), otherwise from the recorded descriptor alone.
///
/// # Safety
/// `arg.sample`, if present, must be a live `GcRef`.
unsafe fn type_for_arg(arg: InstanceArg, db: &mut TypeDb) -> Result<Type, NoRuntimeRepr> {
    if let Some(sample) = arg.sample {
        // SAFETY: forwarded from this function's contract.
        return unsafe { type_for_value(sample, db) };
    }
    match arg.descriptor {
        Some(d) => type_for_descriptor(d, db),
        None => Err(NoRuntimeRepr::value(
            "collection was never told its element type",
        )),
    }
}

/// The type a descriptor names on its own, with no value to consult.
///
/// Exact for scalars and `Unit`; `Err` for every parameterized descriptor,
/// because one `VEC` describes every `Vec[T]` and guessing `Int` is exactly
/// DBG-02.
pub fn type_for_descriptor(
    descriptor: &'static TypeDescriptor,
    db: &mut TypeDb,
) -> Result<Type, NoRuntimeRepr> {
    let Some(builtin) = descriptor.as_builtin() else {
        return Err(NoRuntimeRepr::value("not a built-in descriptor"));
    };
    match builtin {
        BuiltinTypeId::Unit => Ok(db.unit()),
        BuiltinTypeId::Bool => Ok(db.bool()),
        BuiltinTypeId::Int => Ok(db.int()),
        BuiltinTypeId::Byte => Ok(db.scalar(ScalarType::Byte)),
        BuiltinTypeId::Char => Ok(db.char()),
        BuiltinTypeId::Float => Ok(db.float()),
        BuiltinTypeId::Text => Ok(db.text()),
        // Nullary: the descriptor is the whole type.
        BuiltinTypeId::BitSet => db
            .collection(CollectionCtor::BitSet, CollectionArgs::Nullary)
            .map_err(|_| NoRuntimeRepr::value("BitSet takes no type arguments")),
        BuiltinTypeId::Vec
        | BuiltinTypeId::Deque
        | BuiltinTypeId::Grid
        | BuiltinTypeId::Map
        | BuiltinTypeId::Set
        | BuiltinTypeId::Counter
        | BuiltinTypeId::MinHeap
        | BuiltinTypeId::MaxHeap
        | BuiltinTypeId::Tuple => Err(NoRuntimeRepr::value(
            "a parameterized descriptor names no single type; its arguments live in the payload",
        )),
        BuiltinTypeId::Record | BuiltinTypeId::Enum => Err(NoRuntimeRepr::value(
            "a record/enum descriptor does not name which nominal type it describes",
        )),
        BuiltinTypeId::Closure => Err(NoRuntimeRepr::value(
            "a closure descriptor records no parameter or result types",
        )),
        BuiltinTypeId::VarCell => Err(NoRuntimeRepr::value(
            "a VarCell is a compiler-internal slot, not a source type",
        )),
    }
}

/// Rebuild the composite type `descriptor` describes from its recovered
/// arguments. The inverse of [`builtin_for_type`]'s composite arms.
fn compose(
    descriptor: &'static TypeDescriptor,
    args: Vec<Type>,
    db: &mut TypeDb,
) -> Result<Type, NoRuntimeRepr> {
    let Some(builtin) = descriptor.as_builtin() else {
        return Err(NoRuntimeRepr::value("not a built-in descriptor"));
    };
    let ctor = match builtin {
        BuiltinTypeId::Vec => CollectionCtor::Vec,
        BuiltinTypeId::Deque => CollectionCtor::Deque,
        BuiltinTypeId::Grid => CollectionCtor::Grid,
        BuiltinTypeId::Map => CollectionCtor::Map,
        BuiltinTypeId::Set => CollectionCtor::Set,
        BuiltinTypeId::Counter => CollectionCtor::Counter,
        BuiltinTypeId::MinHeap => CollectionCtor::MinHeap,
        BuiltinTypeId::MaxHeap => CollectionCtor::MaxHeap,
        // A tuple's arguments are its elements, not a collection's.
        // A tuple's arguments are its elements, not a collection's. Fewer than
        // two is the backend's degenerate empty-schema tuple (MIR-05), which is
        // not a tuple type — say so rather than interning one (F5).
        BuiltinTypeId::Tuple => {
            let elems = TupleElems::new(args).map_err(|_| {
                NoRuntimeRepr::value("a tuple payload with fewer than two elements names no type")
            })?;
            return Ok(db.tuple(elems));
        }
        // Every other built-in reported `Complete` or `Unrecorded`, so it never
        // reaches composition.
        _ => {
            return Err(NoRuntimeRepr::value(
                "descriptor takes no per-instance arguments",
            ))
        }
    };
    let args = CollectionArgs::new(ctor, args).map_err(|_| {
        NoRuntimeRepr::value(
            "the payload recovered a different number of arguments than the ctor takes",
        )
    })?;
    db.collection(ctor, args).map_err(|_| {
        NoRuntimeRepr::value(
            "the payload recovered a different number of arguments than the ctor takes",
        )
    })
}

#[cfg(test)]
mod tests;
