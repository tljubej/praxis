//! A hashable structural key for a type (F12).
//!
//! A [`Type`] is an arena index, so two separately-interned `Int`s are different
//! handles for one type, and `==` on them answers "same slot", not "same type".
//! Anything that has to *group* types — a monomorphization cache, a schema
//! cache — therefore needs a canonical form.
//!
//! What stood in for one was the **pretty-printed string**: `db.render(t)` as a
//! `HashMap` key (MONO-03). Rendering is display, not identity, and it lost
//! exactly the distinction that mattered — `Option` printed as a bare name
//! whatever it held, so `id[Option[Int]]` and `id[Option[Text]]` hashed to the
//! same key and the second call reused the first's specialization.
//!
//! [`TypeKey`] is the identity: nominal types are keyed by their **def id and
//! arguments**, structural ones by their shape, and an unresolved variable by
//! its own id (two distinct variables are distinct keys; a linked one is
//! followed first).

use crate::data::{EnumDefId, RecordDefId, TypeData, VarState};
use crate::db::TypeDb;
use crate::type_id::{Type, VarId};
use crate::{CollectionCtor, ScalarType};

/// The canonical structural identity of a [`Type`]. See the module docs.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TypeKey {
    Scalar(ScalarType),
    Unit,
    Tuple(Vec<TypeKey>),
    Func {
        params: Vec<TypeKey>,
        result: Box<TypeKey>,
    },
    Collection {
        ctor: CollectionCtor,
        args: Vec<TypeKey>,
    },
    /// A record: its definition and this instance's type arguments. A *def id*
    /// rather than a name, so two same-named nominal declarations stay apart
    /// and an anonymous structural record keys by the def unification settled
    /// on.
    Record(RecordDefId, Vec<TypeKey>),
    /// An enum, keyed as [`Record`](Self::Record) is.
    Enum(EnumDefId, Vec<TypeKey>),
    /// An unresolved type variable. Distinct variables are distinct keys — an
    /// unresolved type is not equal to another unresolved type.
    Var(VarId),
}

impl TypeDb {
    /// The canonical key of `t`, following links at every level.
    ///
    /// Read-only, so it never re-interns: a key describes a type without
    /// creating one. Recursion terminates because a nominal type's key holds
    /// its *def id* rather than its fields — the side tables are not walked —
    /// and the occurs check keeps a variable out of its own binding.
    #[must_use]
    pub fn canonical_key(&self, t: Type) -> TypeKey {
        let t = self.follow(t);
        match self.data(t) {
            TypeData::Scalar(s) => TypeKey::Scalar(*s),
            TypeData::Unit => TypeKey::Unit,
            TypeData::Tuple(elements) => TypeKey::Tuple(self.keys(elements)),
            TypeData::Func { params, result } => TypeKey::Func {
                params: self.keys(params),
                result: Box::new(self.canonical_key(*result)),
            },
            TypeData::Collection { ctor, args } => TypeKey::Collection {
                ctor: *ctor,
                args: self.keys(args),
            },
            TypeData::Record { def, args } => TypeKey::Record(*def, self.keys(args)),
            TypeData::Enum { def, args } => TypeKey::Enum(*def, self.keys(args)),
            TypeData::Var(VarState::Unbound { .. }) => TypeKey::Var(VarId::from_raw(t.to_u32())),
            TypeData::Var(VarState::Linked { .. }) => unreachable!("follow resolves Linked"),
        }
    }

    fn keys(&self, types: &[Type]) -> Vec<TypeKey> {
        types.iter().map(|&t| self.canonical_key(t)).collect()
    }
}
