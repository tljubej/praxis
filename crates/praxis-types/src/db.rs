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

use std::collections::HashMap;

use praxis_stdlib::type_pattern::ScalarType;

use crate::constraint::Constraint;
use crate::ctor::{CollectionArgs, FieldSet, TupleElems, VariantSet};
use crate::data::{
    EnumDef, EnumDefId, EnumVariantDef, Level, RecordDef, RecordDefId, RecordFieldDef, TypeData,
    VarState,
};
use crate::error::TypeCtorError;
use crate::type_id::{Type, VarId};
use crate::CollectionCtor;

/// One arena entry. A slot is either a concrete type shape ([`TypeData`]) or —
/// because `Type` and `VarId` share an index space — a variable in one of its
/// lifecycle states. Because [`TypeData::Var`] already carries the
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
/// Record and enum definitions live in two side-tables (ADR-025): the heavy
/// field/variant data is there rather than inline in [`TypeData`] (which would
/// make `Type` recursive and expensive). A [`TypeData::Record`] /
/// [`TypeData::Enum`] variant carries only a def-id index into these tables.
#[derive(Clone, Debug)]
pub struct TypeDb {
    /// `pub(crate)` so the `unify`/`generalize` modules can mutate slots in place
    /// (link vars, lower levels). External callers go through the methods below.
    pub(crate) slots: Vec<Slot>,
    /// The current binding level, raised on each `var`/`fn` and lowered on exit.
    /// See ADR-008.
    level: Level,
    /// Record definitions, indexed by [`RecordDefId`] (ADR-025). Each
    /// `register_record` call mints a fresh def; identity for anonymous records
    /// is established through unification (not construction), mirroring how
    /// tuples/funcs work.
    pub(crate) record_defs: Vec<RecordDef>,
    /// The canonical field order of each **anonymous** record shape, keyed by
    /// that shape's field names sorted (ADR-152).
    ///
    /// §5.6 says field order does not decide an anonymous record's identity, so
    /// two spellings of one shape are one type — and a type has exactly one
    /// field order, because a field read compiles to a slot index. This is
    /// where that order is decided: the first spelling of a shape registers its
    /// own order, and every later one is permuted into it.
    anonymous_field_orders: HashMap<Vec<String>, Vec<String>>,
    /// Enum definitions, indexed by [`EnumDefId`] (ADR-025).
    pub(crate) enum_defs: Vec<EnumDef>,
    /// The prelude `Option`'s def, seeded at construction so there is exactly
    /// one and every `Option[T]` in the program names it.
    option_def: EnumDefId,
    /// Capability requirements that could not be decided when they were
    /// discovered, because the type they are about is still a variable.
    ///
    /// Inference pushes one per use it cannot answer, drains the *dischargeable*
    /// ones — those whose variable has since resolved — after each unification,
    /// and generalization claims whatever is left about the variables it
    /// quantifies. What survives all three belongs to a variable nothing pinned,
    /// which inference has already reported as itself.
    pending_constraints: Vec<Constraint>,
}

impl Default for TypeDb {
    fn default() -> Self {
        TypeDb::new()
    }
}

impl TypeDb {
    /// A fresh arena, holding only the prelude's own definitions.
    ///
    /// The single canonical `Option` def is registered here, and every
    /// `Option[T]` in the program names it. Registering one per annotation site
    /// would mint a *fresh nominal def* per use: `unify` would need a
    /// same-name-and-signature arm to put the copies back together, the
    /// monomorphizer's display-string cache key could not tell `Option[Int]`
    /// from `Option[Text]`, and a runtime enum would have no stable identity.
    #[must_use]
    pub fn new() -> Self {
        let mut db = TypeDb {
            slots: Vec::new(),
            level: Level::OUTERMOST,
            record_defs: Vec::new(),
            anonymous_field_orders: HashMap::new(),
            enum_defs: Vec::new(),
            // Overwritten below; `register_enum` needs the arena to exist.
            option_def: EnumDefId(0),
            pending_constraints: Vec::new(),
        };
        // `forall T. Option[T]`, as one def with one parameter. `T` is created
        // at the outermost level on purpose: nothing generalizes it (only
        // variables *deeper* than a binding site are quantifiable) and nothing
        // lowers it further, so the def's payload variable is inert until an
        // instance's argument substitutes for it.
        let param_ty = db.fresh_var();
        let param = VarId::from_raw(param_ty.to_u32());
        let variants = VariantSet::new(vec![
            EnumVariantDef::new("Some", vec![param_ty]),
            EnumVariantDef::bare("None"),
        ])
        .expect("Some and None are distinct");
        db.option_def = db.register_enum(Some("Option".into()), vec![param], variants);
        db
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

    /// Convenience: run `f` with the level raised one step, then restore, handing
    /// back what `f` produced (the common inference shape: open a scope, build a
    /// type, return it). The level is restored even though a value is returned.
    ///
    /// Use this around every `var`/`fn` binding so their variables are created at
    /// the inner level (and so generalization quantifies exactly the right set).
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
    /// `pub(crate)` deliberately: an arbitrary [`TypeData`] is exactly the shape
    /// the validated constructors exist to check, so the back door is closed
    /// with them. Reach a composite through [`tuple`](Self::tuple) /
    /// [`collection`](Self::collection) / [`func`](Self::func) /
    /// [`register_record`](Self::register_record) / [`register_enum`](Self::register_enum).
    #[must_use]
    pub(crate) fn intern(&mut self, data: TypeData) -> Type {
        let id = Type::from_raw(self.slots.len() as u32);
        self.slots.push(Slot { data });
        id
    }

    /// A tuple type from already-validated elements.
    #[must_use]
    pub fn tuple(&mut self, elements: TupleElems) -> Type {
        self.intern(TypeData::Tuple(elements.into_vec()))
    }

    /// A collection type `Ctor[args]`, e.g. `Vec[elem]` (§4.4, §11.2).
    ///
    /// # Errors
    /// [`TypeCtorError::CollectionArity`] if `args`'s shape is not the arity
    /// `ctor` declares — `Map[T]`, `Vec[K, V]`, `BitSet[T]`.
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
    /// does. Rehydrating it as a bare `Type(id)` would not check that this
    /// `TypeDb` ever minted that many slots.
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

    // --- the constraint channel ---------------------------------------------

    /// Record a capability requirement that cannot be decided yet.
    ///
    /// The caller has already established that `constraint.var` is unresolved —
    /// a requirement on a concrete type is answered on the spot and never
    /// becomes a pending constraint. Duplicates are dropped: the same
    /// requirement written twice at the same place is one requirement, and a
    /// loop body that emits per iteration would otherwise grow without bound.
    pub fn require(&mut self, constraint: Constraint) {
        if !self.pending_constraints.contains(&constraint) {
            self.pending_constraints.push(constraint);
        }
    }

    /// Take every pending constraint whose variable has since been **resolved**
    /// to something concrete, leaving the still-undecidable ones behind.
    ///
    /// Inference calls this after unification and checks what comes back. A
    /// constraint whose variable is still unbound stays: it is not wrong yet,
    /// and answering it now would be optimism.
    ///
    /// **One call answers one round, not the whole channel.** A capability that
    /// discharges by *producing* a type — `HasMethod`, `Iterable`, `HasField` —
    /// unifies something, and that unification can resolve the variable another
    /// still-pending constraint is waiting on. Those constraints were left
    /// behind by the call that is returning now, so the caller is required to
    /// iterate until this returns empty; `Inferer::discharge_constraints` is
    /// that loop, and ADR-137 records why the fixpoint is required. It lives
    /// there rather than here because resolution needs the catalog and the
    /// diagnostic sink, neither of which this crate has.
    #[must_use]
    pub fn take_dischargeable(&mut self) -> Vec<Constraint> {
        // No representative *is* the answer: `var_id_of` returns `None` exactly
        // when what the variable now follows to is a concrete type.
        self.drain_pending(|_, representative| representative.is_none())
    }

    /// Mark the constraints an instantiation just re-emitted as coming *via*
    /// `site`, so a failure is reported where the use is rather than inside the
    /// generic body that stated the requirement.
    ///
    /// Identifies them by the fresh variables the instantiation minted: a
    /// pending constraint about one of `mapping`'s variables and with no `via`
    /// yet is one this instantiation just pushed.
    pub(crate) fn attribute_reemitted(
        &mut self,
        scheme: &crate::Scheme,
        mapping: &[Type],
        site: praxis_source::FileSpan,
    ) {
        if scheme.constraints().is_empty() {
            return;
        }
        let fresh: Vec<VarId> = mapping.iter().filter_map(|t| self.var_id_of(*t)).collect();
        for c in &mut self.pending_constraints {
            if c.via.is_none() && fresh.contains(&c.var) {
                c.via = Some(site);
            }
        }
    }

    /// Pull every unbound variable inside `t` out to `site`, so a
    /// generalization at `site` cannot quantify it.
    ///
    /// This is Pottier's level-lowering rule, applied deliberately rather than
    /// as a consequence of a link. Unification already does it whenever a
    /// younger variable is bound to a type holding older ones; a caller reaches
    /// for it directly when it has learned something about a variable that makes
    /// quantifying it *wrong* even though no link says so.
    ///
    /// All three callers are the same fact about lowering, from three doors:
    /// **there is one lowered body per source function**, and monomorphization
    /// substitutes a clone's types from the call site's *argument types* — it
    /// does not run the constraint channel. So a variable only the channel can
    /// resolve must not be quantified, or it reaches MIR unbound.
    ///
    /// - A variable a **method** was called on. A method call lowers to exactly
    ///   one catalog entry, and pinning the receiver is what makes
    ///   `fn total(values) { values.sum() }` come out `Vec[Int] -> Int`, the
    ///   answer §5.2 states, rather than a scheme whose body no call site can
    ///   lower.
    /// - The fresh item variable a **`for`** over an *unresolved* iterator mints
    ///   (ADR-062). The deferred `Iterable { item }` is the only thing that ever
    ///   says what it holds, and MIR reads it to type the loop variable's slot.
    ///   The **iterator** is deliberately *not* pinned: MIR picks `len`/`get`
    ///   from its static ctor, so one clone per iterable kind is what makes those
    ///   symbols right.
    /// - A variable a **field** was read from, and the field's own type.
    ///   `lower_field_get` reads the receiver's record **definition** to get the
    ///   field's index, so one field-read site carries one record type — the
    ///   method case's reason at a third door.
    pub fn pin_to_level(&mut self, t: Type, site: Level) {
        self.lower_levels(t, site);
    }

    /// Every pending constraint, for a caller that has to see the ones nothing
    /// resolved (a final sweep at the end of a declaration group).
    #[inline]
    #[must_use]
    pub fn pending_constraints(&self) -> &[Constraint] {
        &self.pending_constraints
    }

    /// Take the pending constraints that are about one of `binders`, leaving
    /// the rest. Generalization's half of the channel: a scheme owns the
    /// requirements on the variables it quantifies, and only those.
    pub(crate) fn claim_constraints(&mut self, binders: &[VarId]) -> Vec<Constraint> {
        if binders.is_empty() {
            return Vec::new();
        }
        self.drain_pending(|c, representative| {
            // The representative, and it matters: the constraint was made about
            // the variable that existed *then*, and unification may since have
            // linked it to another. `var m = Map(); m.insert(k, 1)` requires the
            // map's own key variable, which `insert` then links to `k`'s — and
            // `k`'s is what generalization quantifies. Comparing the unfollowed
            // ids would leave the constraint pending forever.
            let Some(representative) = representative else {
                return false;
            };
            if !binders.contains(&representative) {
                return false;
            }
            // Re-point it at the representative so `reemit_constraints` can
            // find it among the scheme's binders.
            c.var = representative;
            true
        })
    }

    /// Take the pending constraints `take` claims — each seen with the variable
    /// it now follows to, and free to be rewritten before it is handed over —
    /// and leave the rest pending in their original order.
    ///
    /// Resolving the representative needs `&self`, so a caller cannot do it
    /// inside its own predicate: the walk holds `&mut self` to drain from. Both
    /// drains want the same resolution, so it happens here, with the pending
    /// list moved out for the duration. `None` means the variable resolved to a
    /// concrete type — exactly what [`take_dischargeable`](Self::take_dischargeable)
    /// is asking about, and what [`claim_constraints`](Self::claim_constraints)
    /// has nothing to match against a binder.
    fn drain_pending(
        &mut self,
        mut take: impl FnMut(&mut Constraint, Option<VarId>) -> bool,
    ) -> Vec<Constraint> {
        let pending = std::mem::take(&mut self.pending_constraints);
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(pending.len());
        for mut c in pending {
            let representative = self.var_id_of(self.follow(c.var_type()));
            if take(&mut c, representative) {
                taken.push(c);
            } else {
                kept.push(c);
            }
        }
        self.pending_constraints = kept;
        taken
    }

    /// Re-emit `scheme`'s constraints against the fresh variables an
    /// instantiation chose, one per binder in `mapping`.
    ///
    /// A constraint about a binder becomes a constraint about the variable that
    /// replaced it. When that variable is *already* concrete — the common case,
    /// because unifying the call's arguments happens after instantiation — the
    /// constraint is still pushed, and the next `take_dischargeable` picks it
    /// up. Deciding here would need the answer before the arguments are known.
    pub(crate) fn reemit_constraints(
        &mut self,
        scheme: &crate::Scheme,
        mapping: &[Type],
    ) -> Vec<Constraint> {
        let binders: Vec<VarId> = scheme.binders().to_vec();
        let mut emitted = Vec::new();
        for c in scheme.constraints() {
            let Some(idx) = binders.iter().position(|b| *b == c.var) else {
                // A constraint on a variable the scheme does not bind cannot be
                // re-pointed, and carrying it forward unchanged would constrain
                // the *generic* variable rather than this use's. `generalize_at`
                // only claims constraints on its own binders, so this is
                // unreachable for a scheme it built.
                continue;
            };
            // The fresh variable this use put in the binder's place. If the use
            // site has already linked it to a concrete type, the constraint
            // still names the variable — `follow` at discharge time reaches the
            // type it stands for.
            let Some(var) = self.var_id_of(mapping[idx]) else {
                continue;
            };
            let cap = self.substitute_capability(&c.cap, &binders, mapping);
            let fresh = Constraint::new(var, cap, c.at);
            self.require(fresh.clone());
            emitted.push(fresh);
        }
        emitted
    }

    /// Rewrite the types a capability carries through the same binder→fresh
    /// mapping the body went through. `Iterable { item }`'s item and
    /// `HasMethod`'s params/result are types in the generic body's terms; left
    /// alone they would constrain the scheme's own variables at every use.
    fn substitute_capability(
        &mut self,
        cap: &crate::Capability,
        binders: &[VarId],
        mapping: &[Type],
    ) -> crate::Capability {
        use crate::Capability;
        match cap {
            Capability::Kind(k) => Capability::Kind(*k),
            Capability::Iterable { item } => Capability::Iterable {
                item: crate::generalize::substitute(self, *item, binders, mapping),
            },
            Capability::HasMethod {
                name,
                params,
                result,
            } => Capability::HasMethod {
                name: name.clone(),
                params: params
                    .iter()
                    .map(|p| crate::generalize::substitute(self, *p, binders, mapping))
                    .collect(),
                result: crate::generalize::substitute(self, *result, binders, mapping),
            },
            Capability::HasField { name, ty } => Capability::HasField {
                name: name.clone(),
                ty: crate::generalize::substitute(self, *ty, binders, mapping),
            },
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
    /// Used by the crash debugger to capture a local's exact static type — e.g.
    /// turning `Vec[?T]` (an element var that a later `push(11)` linked to
    /// `Int`) into `Vec[Int]` so `p EXPR` can type-check against it.
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

    // --- record / enum definitions (ADR-025) --------------------------------

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

    /// Every registered record definition with its id, in registration order.
    ///
    /// The arena is otherwise navigated *from a type inward*, which is enough
    /// for the compiler: a def is only interesting when something has that type.
    /// The crash debugger asks the other direction — "does this program declare
    /// a `Pt`?", for an expression that names one without any local having that
    /// type — so the arena exposes the walk rather than the field.
    pub fn record_defs(&self) -> impl Iterator<Item = (RecordDefId, &RecordDef)> {
        self.record_defs
            .iter()
            .enumerate()
            .map(|(i, def)| (RecordDefId(i as u32), def))
    }

    /// Every registered enum definition with its id, in registration order.
    pub fn enum_defs(&self) -> impl Iterator<Item = (EnumDefId, &EnumDef)> {
        self.enum_defs
            .iter()
            .enumerate()
            .map(|(i, def)| (EnumDefId(i as u32), def))
    }

    /// Register a record definition (§4.5 nominal, §5.6 anonymous structural).
    ///
    /// `name` is `Some` for a source-declared record and `None` for an anonymous
    /// structural one (a parser template, ADR-024, or a `{ x: 1 }` literal,
    /// ADR-152). Each call mints a fresh def: nominal records are distinct by
    /// name even with identical fields, and anonymous ones establish identity
    /// through unification (two with the same field-name set unify and get
    /// linked), mirroring how tuples and functions work.
    ///
    /// **A nominal record's field order is the declaration's; an anonymous
    /// one's is [`canonical_field_order`](Self::canonical_field_order)'s** —
    /// the order the shape was first written in anywhere in this program. Two
    /// spellings of one anonymous shape are one type (§5.6), a type has one
    /// field order because a field read compiles to a slot index, and this is
    /// the one place that can decide it for every producer at once.
    ///
    /// Takes a validated [`FieldSet`] — the one place a duplicate field name is
    /// rejected, rather than at whichever syntax caller happened to remember.
    ///
    /// `params` are the def's own type parameters; field types written in
    /// terms of them are substituted at each instance. Every caller passes an
    /// empty list today — the language has no `struct P[T]` syntax.
    pub fn register_record(
        &mut self,
        name: Option<String>,
        params: Vec<VarId>,
        fields: FieldSet,
    ) -> RecordDefId {
        let mut fields = fields.into_vec();
        if name.is_none() {
            let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
            let order = self.canonical_field_order(&names).to_vec();
            fields.sort_by_key(|f| {
                order
                    .iter()
                    .position(|n| *n == f.name)
                    .unwrap_or(usize::MAX)
            });
        }
        let def = RecordDef {
            name,
            params,
            fields,
        };
        let id = RecordDefId(self.record_defs.len() as u32);
        self.record_defs.push(def);
        id
    }

    /// The canonical field order of the anonymous record shape whose fields are
    /// `names` — the order `names` were written in, if this shape has not been
    /// seen before, and the order the *first* spelling used if it has.
    ///
    /// Establishing it on first sight is what lets every producer of a value
    /// agree without any of them knowing about the others: a `{ h: 4, w: 3 }`
    /// literal, a ``{h:int}x{w:int}`` template and a `{w:int}x{h:int}` template
    /// are one type, and every value of it is laid out in one order — so the
    /// slot index a field read compiles to means the same thing in all three.
    ///
    /// Keyed by the **sorted** names, because that is the identity §5.6 gives
    /// the shape. The answer is in canonical order, which the caller matches its
    /// own names against; a name not in `names` cannot come back, since the key
    /// is the name set itself.
    pub fn canonical_field_order(&mut self, names: &[&str]) -> &[String] {
        let mut key: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
        key.sort();
        self.anonymous_field_orders
            .entry(key)
            .or_insert_with(|| names.iter().map(|n| (*n).to_string()).collect())
    }

    /// Register an enum definition (§4.6), nominal (`Some(name)`) or anonymous
    /// (`None`, a `choice(...)` template — §7.5).
    ///
    /// As [`register_record`](Self::register_record): a validated [`VariantSet`]
    /// is the only way in, so a duplicate variant name is rejected here, and
    /// `params` are the def's own type parameters.
    pub fn register_enum(
        &mut self,
        name: Option<String>,
        params: Vec<VarId>,
        variants: VariantSet,
    ) -> EnumDefId {
        let def = EnumDef {
            name,
            params,
            variants: variants.into_vec(),
        };
        let id = EnumDefId(self.enum_defs.len() as u32);
        self.enum_defs.push(def);
        id
    }

    /// Register a **non-generic** record definition and intern its type in one
    /// step — what almost every caller wants.
    pub fn record(&mut self, name: Option<String>, fields: FieldSet) -> Type {
        let def = self.register_record(name, Vec::new(), fields);
        self.record_type(def, Vec::new())
            .expect("a def with no params takes no args")
    }

    /// Register a **non-generic** enum definition and intern its type in one
    /// step.
    pub fn enum_(&mut self, name: Option<String>, variants: VariantSet) -> Type {
        let def = self.register_enum(name, Vec::new(), variants);
        self.enum_type(def, Vec::new())
            .expect("a def with no params takes no args")
    }

    /// Instantiate an existing [`RecordDefId`] at `args`.
    ///
    /// # Errors
    /// [`TypeCtorError::TypeArgCount`] if `args` does not match the def's
    /// parameter count.
    pub fn record_type(
        &mut self,
        def: RecordDefId,
        args: Vec<Type>,
    ) -> Result<Type, TypeCtorError> {
        let rdef = self.record_def(def);
        if args.len() != rdef.params.len() {
            return Err(TypeCtorError::TypeArgCount {
                name: rdef.name.clone().unwrap_or_else(|| "record".to_string()),
                got: args.len(),
                want: rdef.params.len(),
            });
        }
        Ok(self.intern(TypeData::Record { def, args }))
    }

    /// Instantiate an existing [`EnumDefId`] at `args`.
    ///
    /// # Errors
    /// [`TypeCtorError::TypeArgCount`] if `args` does not match the def's
    /// parameter count — `Option[Int, Text]` is the user-reachable case.
    pub fn enum_type(&mut self, def: EnumDefId, args: Vec<Type>) -> Result<Type, TypeCtorError> {
        let edef = self.enum_def(def);
        if args.len() != edef.params.len() {
            return Err(TypeCtorError::TypeArgCount {
                name: edef.name.clone().unwrap_or_else(|| "enum".to_string()),
                got: args.len(),
                want: edef.params.len(),
            });
        }
        Ok(self.intern(TypeData::Enum { def, args }))
    }

    // --- the prelude `Option` -----------------------------------------------

    /// The one `Option` def. Every `Option[T]` in a program instantiates it.
    #[inline]
    #[must_use]
    pub fn option_def(&self) -> EnumDefId {
        self.option_def
    }

    /// `Option[elem]`.
    #[must_use]
    pub fn option_of(&mut self, elem: Type) -> Type {
        let def = self.option_def;
        self.enum_type(def, vec![elem])
            .expect("Option takes one type argument")
    }

    // --- reading a def through an instance's arguments -----------------------

    /// Substitute a def's `params` by an instance's `args` in `t`.
    ///
    /// Identity — and free — when the def is not generic, which is every record
    /// and every user-declared enum today.
    pub fn substitute_params(&mut self, t: Type, params: &[VarId], args: &[Type]) -> Type {
        if params.is_empty() {
            return t;
        }
        crate::generalize::substitute(self, t, params, args)
    }

    /// A record instance's fields, with the def's parameters substituted by
    /// `args`. Field *names* never depend on the arguments, only the types do.
    #[must_use]
    pub fn record_fields_of(&mut self, def: RecordDefId, args: &[Type]) -> Vec<RecordFieldDef> {
        let rdef = self.record_def(def).clone();
        rdef.fields
            .iter()
            .map(|f| RecordFieldDef {
                name: f.name.clone(),
                ty: self.substitute_params(f.ty, &rdef.params, args),
            })
            .collect()
    }

    /// A record instance's field by name: its index in declaration order and its
    /// type under `args`.
    #[must_use]
    pub fn record_field_of(
        &mut self,
        def: RecordDefId,
        args: &[Type],
        name: &str,
    ) -> Option<(usize, Type)> {
        let rdef = self.record_def(def).clone();
        let (idx, field) = rdef
            .fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == name)?;
        Some((idx, self.substitute_params(field.ty, &rdef.params, args)))
    }

    /// An enum instance's variant payload types under `args` — for
    /// `Option[Int]`, `Some`'s payload is `[Int]`, not `[T]`.
    #[must_use]
    pub fn variant_payload_of(
        &mut self,
        def: EnumDefId,
        args: &[Type],
        variant_idx: usize,
    ) -> Vec<Type> {
        let edef = self.enum_def(def).clone();
        let Some(variant) = edef.variants.get(variant_idx) else {
            return Vec::new();
        };
        variant
            .payload
            .clone()
            .into_iter()
            .map(|t| self.substitute_params(t, &edef.params, args))
            .collect()
    }
}

/// Deep resolution as a folder: the identity fold, whose only effect is that
/// [`fold`](crate::fold::fold) prunes every type it visits, so a composite whose
/// child was linked comes back rebuilt around the child's representative.
///
/// Going through the fold is what covers `Record` and `Enum` — a hand-written
/// match that fell through on them would leave the crash debugger's static-type
/// capture (ADR-035) reporting a record whose fields are still variables — and
/// the fold's memo is the cycle guard.
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
