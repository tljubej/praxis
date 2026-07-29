//! The type arena ([`TypeDb`]): the single mutable store for all static types.
//!
//! Every [`Type`] and [`VarId`] is a `u32` index into one `TypeDb`. Type variables
//! live *in* the arena (as [`Slot`]s), so unification links them by mutating a
//! slot rather than threading `Rc<RefCell<…>>` through the program (ADR-007). This
//! keeps `Type` `Copy` and makes the variable lifecycle ([`VarState`]) explicit.
//!
//! The arena is **not** thread-safe; inference is single-threaded. Callers obtain
//! a `&mut TypeDb` for the duration of inference and then read it through shared
//! references for diagnostics/hover.

use praxis_stdlib::type_pattern::ScalarType;

use crate::ctor::{CollectionArgs, FieldSet, TupleElems, VariantSet};
use crate::data::{EnumDef, EnumDefId, Level, RecordDef, RecordDefId, TypeData, VarState};
use crate::error::TypeCtorError;
use crate::type_id::{Type, VarId};
use crate::CollectionCtor;

/// One arena entry. A slot is either a concrete type shape ([`TypeData`]) or — for
/// the historical reason that `Type` and `VarId` share an index space — a variable
/// in one of its lifecycle states. Because [`TypeData::Var`] already carries the
/// [`VarState`], a `Slot` is just a `TypeData`; the newtype exists so the arena is
/// self-documenting and so we can later add provenance (e.g. a span) without a
/// second parallel array.
#[derive(Clone, Debug)]
pub struct Slot {
    pub data: TypeData,
}

/// The interned type store. Create one at the start of inference and mint/follow
/// types through it.
///
/// M7 adds two side-tables for record/enum definitions (ADR-025): the heavy
/// field/variant data lives here rather than inline in [`TypeData`] (which would
/// make `Type` recursive and expensive). A [`TypeData::Record`] /
/// [`TypeData::Enum`] variant carries only a def-id index into these tables.
#[derive(Clone, Debug, Default)]
pub struct TypeDb {
    /// `pub(crate)` so the `unify`/`generalize` modules can mutate slots in place
    /// (link vars, lower levels). External callers go through the methods below.
    pub(crate) slots: Vec<Slot>,
    /// The current binding level, raised on each `let`/`fn` and lowered on exit.
    /// See ADR-008.
    level: Level,
    /// Record definitions, indexed by [`RecordDefId`] (M7, ADR-025). Each
    /// `register_record`/`anon_record` call mints a fresh def; identity for
    /// anonymous records is established through unification (not construction),
    /// mirroring how tuples/funcs work.
    pub(crate) record_defs: Vec<RecordDef>,
    /// Enum definitions, indexed by [`EnumDefId`] (M7, ADR-025).
    pub(crate) enum_defs: Vec<EnumDef>,
}

impl TypeDb {
    /// A fresh, empty arena.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current binding level.
    #[inline]
    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }

    /// The number of type slots interned. A `Type(n)` is valid iff `n` indexes a
    /// real slot (i.e. `(n as usize) < len()`); the debugger uses this to guard
    /// against the `0` "unknown" sentinel and out-of-range ids before rendering
    /// a local's type.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True iff no type slots have been interned.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Push the binding level one deeper. Returns the previous level so the caller
    /// can restore it (`db.set_level(prev)`) when the scope ends.
    #[must_use]
    pub fn enter_level(&mut self) -> Level {
        let prev = self.level;
        self.level = self.level.deeper();
        prev
    }

    /// Restore the binding level (after a scope ends).
    ///
    /// Per Pottier/Rémy, the level is just restored here; unbound vars retain the
    /// level they were created at. They are *lowered* only at the point they
    /// would otherwise be unsound — inside [`unify`](crate::TypeDb::unify), when a
    /// younger variable is linked to a type containing older variables. Lowering
    /// everything on scope exit would defeat generalization (it would pull inner
    /// vars down to the outer level, so `generalize` could never quantify them).
    pub fn exit_level(&mut self, prev: Level) {
        self.level = prev;
    }

    /// Convenience: run `f` with the level raised one step, then restore. Use this
    /// around every `let`/`fn` binding so their variables are created at the inner
    /// level (and so generalization quantifies exactly the right set).
    pub fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        let prev = self.enter_level();
        f(self);
        self.exit_level(prev);
    }

    /// Like [`scoped`](Self::scoped) but for a computation that produces a value
    /// (the common inference shape: open a scope, build a type, return it). The
    /// level is restored even though a value is returned.
    #[must_use]
    pub fn scoped_return<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.enter_level();
        let out = f(self);
        self.exit_level(prev);
        out
    }

    // --- construction -------------------------------------------------------

    /// Intern an arbitrary type shape, returning its handle.
    ///
    /// `pub(crate)` since F5: an arbitrary [`TypeData`] is exactly the shape the
    /// validated constructors exist to check, so the back door had to close with
    /// them. Reach a composite through [`tuple`](Self::tuple) /
    /// [`collection`](Self::collection) / [`func`](Self::func) /
    /// [`register_record`](Self::register_record) / [`register_enum`](Self::register_enum).
    #[must_use]
    pub(crate) fn intern(&mut self, data: TypeData) -> Type {
        let id = Type::from_raw(self.slots.len() as u32);
        self.slots.push(Slot { data });
        id
    }

    /// A tuple type from already-validated elements (F5).
    #[must_use]
    pub fn tuple(&mut self, elements: TupleElems) -> Type {
        self.intern(TypeData::Tuple(elements.into_vec()))
    }

    /// A collection type `Ctor[args]`, e.g. `Vec[elem]` (§4.4, §11.2, M5).
    ///
    /// # Errors
    /// [`TypeCtorError::CollectionArity`] if `args`'s shape is not the arity
    /// `ctor` declares — `Map[T]`, `Vec[K, V]`, `BitSet[T]`. The check existed
    /// as [`CollectionCtor::arity`] and had no caller.
    pub fn collection(
        &mut self,
        ctor: CollectionCtor,
        args: CollectionArgs,
    ) -> Result<Type, TypeCtorError> {
        if args.arity() != ctor.arity() {
            return Err(TypeCtorError::CollectionArity {
                ctor,
                got: args.arity(),
                want: ctor.arity(),
            });
        }
        Ok(self.intern(TypeData::Collection {
            ctor,
            args: args.to_vec(),
        }))
    }

    /// Recover a handle from a raw arena index, if it names a real slot.
    ///
    /// The one checked route back in for a caller that stored a
    /// [`Type::to_u32`] outside the arena — the debugger's `DebugLocalMeta`
    /// does, and used to rehydrate it as a bare `Type(id)` whether or not this
    /// `TypeDb` had ever minted that many slots.
    #[inline]
    #[must_use]
    pub fn type_from_raw(&self, raw: u32) -> Option<Type> {
        ((raw as usize) < self.slots.len()).then(|| Type::from_raw(raw))
    }

    /// Intern a scalar type — the overwhelmingly common case.
    #[must_use]
    pub fn scalar(&mut self, s: ScalarType) -> Type {
        self.intern(TypeData::Scalar(s))
    }

    /// The unit type (§4.3) — no payload.
    #[must_use]
    pub fn unit(&mut self) -> Type {
        self.intern(TypeData::Unit)
    }

    /// A fresh unbound variable at the *current* binding level.
    #[must_use]
    pub fn fresh_var(&mut self) -> Type {
        // The slot's own index *is* the var id.
        let var = VarId::from_raw(self.slots.len() as u32);
        self.slots.push(Slot {
            data: TypeData::Var(VarState::Unbound { level: self.level }),
        });
        var.as_type()
    }

    /// The variable identity of a type slot, if it is a variable.
    #[must_use]
    pub fn var_id_of(&self, t: Type) -> Option<VarId> {
        match &self.slots[t.to_u32() as usize].data {
            TypeData::Var(_) => Some(VarId::from_raw(t.to_u32())),
            _ => None,
        }
    }

    // --- resolution ---------------------------------------------------------

    /// Follow links: return the representative `Type` for `t`. If `t` is a linked
    /// var, recurse until an unlinked slot is found (with path compression — write
    /// the final representative back so subsequent lookups are O(1)).
    ///
    /// The returned type is always either a concrete shape or an unbound/generalized
    /// variable; never a `Linked` var.
    pub fn prune(&mut self, t: Type) -> Type {
        match &self.slots[t.to_u32() as usize].data {
            TypeData::Var(VarState::Linked { target }) => {
                let target = *target;
                let root = self.prune(target);
                // Path compression: point straight at the root.
                if root != target {
                    self.slots[t.to_u32() as usize].data =
                        TypeData::Var(VarState::Linked { target: root });
                }
                root
            }
            _ => t,
        }
    }

    /// Shared-reference variant of [`prune`](Self::prune) that does not compress
    /// (read-only). Returns the representative of `t`.
    #[must_use]
    pub fn follow(&self, t: Type) -> Type {
        match &self.slots[t.to_u32() as usize].data {
            TypeData::Var(VarState::Linked { target }) => self.follow(*target),
            _ => t,
        }
    }

    /// Recursively resolve `t`, following links at *every* level — including
    /// the element/param/result args of `Collection`/`Tuple`/`Func` types, which
    /// [`follow`](Self::follow) leaves untouched (it only resolves the top-level
    /// representative). Allocates new slots for any resolved composite so the
    /// returned `Type` is fully concrete to its leaves (or genuinely `Unbound`).
    ///
    /// Used by the crash debugger (M10b-WS4) to capture a local's exact static
    /// type — e.g. turning `Vec[?T]` (an element var that a later `push(11)`
    /// linked to `Int`) into `Vec[Int]` so `p EXPR` can type-check against it.
    #[must_use]
    pub fn deep_resolve(&mut self, t: Type) -> Type {
        let mut folder = DeepResolver {
            db: self,
            memo: crate::fold::FoldMemo::new(),
        };
        crate::fold::fold(&mut folder, t)
    }

    /// Borrow the data of `t`'s representative **without** pruning links. Callers
    /// that may hold the borrow across a mutation must use [`prune`](Self::prune)
    /// first and then index the returned handle.
    #[must_use]
    pub fn data(&self, t: Type) -> &TypeData {
        &self.slots[t.to_u32() as usize].data
    }

    /// Link variable `v` to `target`, recording `target`'s representative. Used by
    /// [`unify`](crate::unify); not meant for ad-hoc callers.
    pub(crate) fn link(&mut self, v: VarId, target: Type) {
        self.slots[v.to_u32() as usize].data = TypeData::Var(VarState::Linked { target });
    }

    // --- record / enum definitions (M7, ADR-025) ----------------------------

    /// Borrow a record definition by id (read-only).
    #[must_use]
    pub fn record_def(&self, def: RecordDefId) -> &RecordDef {
        &self.record_defs[def.to_u32() as usize]
    }

    /// Borrow an enum definition by id (read-only).
    #[must_use]
    pub fn enum_def(&self, def: EnumDefId) -> &EnumDef {
        &self.enum_defs[def.to_u32() as usize]
    }

    /// Register a record definition (§4.5 nominal, §5.6 anonymous structural).
    ///
    /// `name` is `Some` for a source-declared record and `None` for an anonymous
    /// structural one (a parser template, ADR-024). Each call mints a fresh def:
    /// nominal records are distinct by name even with identical fields, and
    /// anonymous ones establish identity through unification (two with the same
    /// field-name set unify and get linked), mirroring how tuples and functions
    /// work. Field display order follows the order given here.
    ///
    /// Takes a validated [`FieldSet`] — the one place a duplicate field name is
    /// rejected, rather than at whichever syntax caller happened to remember.
    pub fn register_record(&mut self, name: Option<String>, fields: FieldSet) -> RecordDefId {
        let def = RecordDef {
            name,
            fields: fields.into_vec(),
        };
        let id = RecordDefId(self.record_defs.len() as u32);
        self.record_defs.push(def);
        id
    }

    /// Register an enum definition (§4.6), nominal (`Some(name)`) or anonymous
    /// (`None`, a `choice(...)` template — §7.5).
    ///
    /// As [`register_record`](Self::register_record): a validated [`VariantSet`]
    /// is the only way in, so a duplicate variant name is rejected here.
    pub fn register_enum(&mut self, name: Option<String>, variants: VariantSet) -> EnumDefId {
        let def = EnumDef {
            name,
            variants: variants.into_vec(),
        };
        let id = EnumDefId(self.enum_defs.len() as u32);
        self.enum_defs.push(def);
        id
    }

    /// Register a record definition and intern its type in one step — what
    /// almost every caller wants.
    pub fn record(&mut self, name: Option<String>, fields: FieldSet) -> Type {
        let def = self.register_record(name, fields);
        self.record_type(def)
    }

    /// Register an enum definition and intern its type in one step.
    pub fn enum_(&mut self, name: Option<String>, variants: VariantSet) -> Type {
        let def = self.register_enum(name, variants);
        self.enum_type(def)
    }

    /// Wrap an existing [`RecordDefId`] into a `Type` (e.g. for use after
    /// fetching a shared def-id).
    #[must_use]
    pub fn record_type(&mut self, def: RecordDefId) -> Type {
        self.intern(TypeData::Record { def })
    }

    /// Wrap an existing [`EnumDefId`] into a `Type`.
    #[must_use]
    pub fn enum_type(&mut self, def: EnumDefId) -> Type {
        self.intern(TypeData::Enum { def })
    }
}

/// Deep resolution as a folder (F9): the identity fold, whose only effect is
/// that [`fold`](crate::fold::fold) prunes every type it visits, so a composite
/// whose child was linked comes back rebuilt around the child's representative.
///
/// The hand-written version ended in `_ => t`, which skipped `Record` and
/// `Enum` — the crash debugger's static-type capture (ADR-035) therefore
/// reported a record whose fields were still variables. It also had no cycle
/// guard; the fold's memo is what supplies one.
struct DeepResolver<'a> {
    db: &'a mut TypeDb,
    memo: crate::fold::FoldMemo,
}

impl crate::fold::TypeFolder for DeepResolver<'_> {
    fn db(&mut self) -> &mut TypeDb {
        self.db
    }
    fn memo(&mut self) -> &mut crate::fold::FoldMemo {
        &mut self.memo
    }
}
