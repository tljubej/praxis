//! Why a type could not be constructed (F5, TY-07).
//!
//! Every illegal shape the arena used to accept silently now has a name here.
//! The constructors that can produce one are [`TypeDb::collection`](crate::TypeDb::collection)
//! and the validated payloads in [`ctor`](crate::ctor); everything else is
//! either infallible by construction or takes an already-validated payload.

use std::fmt;

use crate::CollectionCtor;

/// A type constructor rejected its arguments.
///
/// These were all *representable* before F5: `db.tuple(vec![x])` interned a
/// one-element tuple that no unification could ever satisfy, `db.collection`
/// ignored [`CollectionCtor::arity`] entirely, and a record with two `x` fields
/// registered happily and then resolved `x` to whichever came first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeCtorError {
    /// A tuple needs at least two elements. A one-element parenthesized type is
    /// the inner type itself (the parser already does this); a zero-element one
    /// is [`Unit`](crate::TypeData::Unit).
    TupleArity(usize),
    /// A collection was given the wrong number of type arguments for its ctor.
    CollectionArity {
        ctor: CollectionCtor,
        got: usize,
        want: usize,
    },
    /// Two fields of one record definition share a name.
    DuplicateField(String),
    /// Two variants of one enum definition share a name.
    DuplicateVariant(String),
}

impl fmt::Display for TypeCtorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeCtorError::TupleArity(got) => {
                write!(f, "a tuple type needs at least 2 elements, got {got}")
            }
            TypeCtorError::CollectionArity { ctor, got, want } => write!(
                f,
                "`{}` takes {want} type argument(s), got {got}",
                ctor.name()
            ),
            TypeCtorError::DuplicateField(name) => {
                write!(f, "duplicate record field `{name}`")
            }
            TypeCtorError::DuplicateVariant(name) => {
                write!(f, "duplicate enum variant `{name}`")
            }
        }
    }
}

impl std::error::Error for TypeCtorError {}
