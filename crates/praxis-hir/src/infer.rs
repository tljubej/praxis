//! Type inference (§5, ADR-008).
//!
//! Walks the name-resolved tree and assigns a [`Type`] to every expression and
//! a [`Scheme`] to every binding. Uses the level-based let-generalization from
//! `praxis-types` (ADR-008):
//!
//! - `let` RHS is inferred at an inner level, then **generalized** at the outer
//!   level (so `let id = fn(x){x}` becomes `forall T. (T) -> T`).
//! - `var` RHS is inferred but **never generalized** (§5.3 soundness — a `var`
//!   could be reassigned to a differently-shaped value).
//! - A `fn` is given a monomorphic placeholder var first (so the body can refer
//!   to it for recursion), its body is inferred, the placeholder is unified
//!   with the body-derived function type, and the result is generalized after.
//!
//! Name references record the inferred type of the instantiated scheme in
//! [`Inference::ref_types`], keyed by range — that is what hover displays.

use std::collections::HashMap;

use praxis_ast::{
    ArgList, AssignStmt, AstNode, BinExpr, BlockExpr, BreakExpr, CallExpr, ContinueExpr,
    ElseBranch, Expr, ExprStmt, FieldExpr, FnItem, ForExpr, IfExpr, LetStmt, Literal, LoopExpr,
    MethodCallExpr, Param, PathExpr, RecordLitExpr, ReturnExpr, SourceFile, UnaryExpr, VarStmt,
    WhileExpr,
};
use praxis_source::{BytePos, Diagnostic, FileId, FileSpan, Span};
use praxis_syntax::SyntaxKind;
use praxis_types::{
    unify::UnifyError, CapKind, Capability, CollectionArgs, CollectionCtor, Constraint, Level,
    ScalarType, Scheme, Type, TypeDb,
};
use rowan::TextRange;

use crate::diagnostics::{
    infinite_type, not_equatable, not_hashable, not_numeric, not_orderable, type_mismatch,
    type_mismatch_with_help,
};
use crate::name_table::NameTable;
use crate::resolve::{NameResolution, ResolvedRef};
use crate::scope::ScopeTree;
use crate::symbol::{SymbolId, SymbolKind};

/// The output of inference.
pub struct Inference {
    pub db: TypeDb,
    pub names: NameTable,
    /// The resolver's scope tree, carried through untouched. Inference does not
    /// read it: every binding question it has is answered by the range-keyed
    /// `refs`/`decls`/`type_refs` maps, which is the whole of TY-13.
    pub scopes: ScopeTree,
    pub refs: HashMap<TextRange, ResolvedRef>,
    pub ref_types: HashMap<TextRange, Type>,
    /// Declaration-site ranges → SymbolId. Carried from resolution so downstream
    /// passes (M4 lowering) can map a `let`/`var`/`fn`/param name to its symbol
    /// via the declaration range (unambiguous under shadowing).
    pub decls: HashMap<TextRange, SymbolId>,
    /// Each call site, keyed by the callee name token's range → the callee
    /// symbol and concrete arg types (the monomorphization witness, WS8 §13.6).
    pub call_sites: HashMap<TextRange, crate::CallSite>,
    /// Every inferred expression's type, keyed by its node (F15).
    pub expr_types: HashMap<crate::NodeKey, Type>,
    /// Each method call, keyed by the method-name token's range (F15/HIR-02).
    pub method_refs: HashMap<TextRange, crate::MethodRef>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Run inference over a resolved program. Requires the parsed tree (the
/// resolution does not retain it).
pub(crate) fn infer_with_tree(
    file: FileId,
    resolution: NameResolution,
    root: &SourceFile,
) -> Inference {
    let NameResolution {
        names,
        scopes,
        refs,
        decls,
        type_refs,
        mut diagnostics,
    } = resolution;

    let mut inferer = Inferer {
        file,
        db: TypeDb::new(),
        names,
        refs,
        decls,
        type_refs,
        type_env: crate::decl::TypeEnv::default(),
        ref_types: HashMap::new(),
        call_sites: HashMap::new(),
        expr_types: HashMap::new(),
        method_refs: HashMap::new(),
        diagnostics: Vec::new(),
        catalog: builtin_catalog(),
        decl_site: Level::OUTERMOST,
        fn_results: Vec::new(),
        loops: Vec::new(),
    };
    inferer.seed_builtin_schemes();
    inferer.infer_declaration_group(root);
    // Merge name-resolution diagnostics with type diagnostics, sorted by span.
    diagnostics.append(&mut inferer.diagnostics);
    diagnostics.sort_by_key(|d| {
        let s = d.primary().span;
        (s.start(), s.end())
    });
    Inference {
        db: inferer.db,
        names: inferer.names,
        scopes,
        refs: inferer.refs,
        ref_types: inferer.ref_types,
        decls: inferer.decls,
        call_sites: inferer.call_sites,
        expr_types: inferer.expr_types,
        method_refs: inferer.method_refs,
        diagnostics,
    }
}

/// The inference state.
///
/// There is **no scope tree here**. Inference used to own one — the resolver's,
/// moved in — and then push empty child scopes onto it that mirrored nothing:
/// no binding was ever added, so a lookup through them walked straight out to
/// the root. `infer_assign` was the only caller, and it therefore either found
/// nothing (a local, so the assignment went unchecked) or a same-named
/// top-level binding (whose type it constrained instead). Every binding
/// question is answered by the range-keyed maps resolution produced (TY-13).
struct Inferer {
    file: FileId,
    db: TypeDb,
    names: NameTable,
    refs: HashMap<TextRange, ResolvedRef>,
    /// Declaration sites → SymbolId (resolution's map). Used to attach schemes
    /// to the exact symbol, surviving shadowing.
    decls: HashMap<TextRange, SymbolId>,
    /// Names written in type position → the type symbol they resolved to
    /// (resolution's map). An annotation is turned into a [`Type`] through this,
    /// never through a scope lookup of the inferer's own.
    type_refs: HashMap<TextRange, SymbolId>,
    /// Every type name's [`Type`] and every function's signature placeholder,
    /// sealed by the declaration pass before any expression is inferred (F19).
    type_env: crate::decl::TypeEnv,
    ref_types: HashMap<TextRange, Type>,
    /// Call sites keyed by the callee name token's range. Populated in
    /// `infer_call`; consumed by the monomorphization pass (WS8, §13.6).
    call_sites: HashMap<TextRange, crate::CallSite>,
    /// Every inferred expression's type, keyed by its node (F15). There is
    /// exactly one insertion point — [`Inferer::infer_expr`] — so a visited
    /// expression with no recorded type is unrepresentable, and lowering can
    /// *read* what inference decided rather than deriving its own answer.
    expr_types: HashMap<crate::NodeKey, Type>,
    /// Each method call, keyed by the method-name token's range (HIR-02). A
    /// method name is not a name reference, so it does not belong in
    /// `ref_types` — everything that walks references had to know to skip it,
    /// and hover, which asks `refs` first, never saw it at all.
    method_refs: HashMap<TextRange, crate::MethodRef>,
    diagnostics: Vec<Diagnostic>,
    /// The built-in method catalog (§16.2), for resolving `receiver.method()`.
    /// Immutable; shared via a process-wide `OnceLock`.
    catalog: &'static praxis_stdlib::MethodCatalog,
    /// The level the declaration group was entered from — where a function's
    /// signature generalizes (TY-01).
    decl_site: Level,
    /// The result type of each function whose body is being inferred, innermost
    /// last. A `return` is checked against the top of this stack (TY-18); a
    /// closure pushes its own, because `return` inside one leaves the closure,
    /// not the function that contains it. Empty means "not inside a function",
    /// which is what makes a top-level `return` reportable (TY-20).
    fn_results: Vec<Type>,
    /// The loops enclosing the expression being inferred, innermost last. A
    /// `break` or `continue` with none is `Y012` (TY-20); a `break` reads the
    /// top to learn which loop it leaves and what that loop has produced so far
    /// (TY-21). A closure body clears it and restores it afterwards: a closure
    /// is a function boundary, so a loop outside it is not one a `break` inside
    /// it can leave.
    loops: Vec<LoopCtx>,
}

/// One enclosing loop, while its body is being inferred (TY-21).
#[derive(Clone, Copy)]
struct LoopCtx {
    /// Which loop form this is — only a `loop` may produce a value.
    flavour: LoopFlavour,
    /// The join of every `break` value seen in this loop so far, seeded with
    /// `Never`: a loop no `break` leaves produces no value (D2). A bare `break`
    /// contributes `Unit`, so `loop { break }` is `Unit` and
    /// `loop { if c { break }\n break 1 }` is the `Y001` it should be.
    result: Type,
}

/// Which loop form a [`LoopCtx`] describes (D2, ADR-053).
///
/// `loop` is the only expression loop. A `while`/`for` also leaves by its
/// condition failing or its sequence running out, and the compiler has no value
/// to supply on that path — so a `break` there may not carry one, and the two
/// cases are distinguished here rather than by a check on the keyword text.
#[derive(Clone, Copy)]
enum LoopFlavour {
    /// `loop` — a `break` may carry a value, and the loop is that value's type.
    Expression,
    /// `while` / `for` — always `Unit`; a value `break` is `Y017`. Carries the
    /// keyword so the report can name the loop the `break` is actually in.
    Statement(&'static str),
}

/// The built-in method catalog, constructed once and cached for the process
/// lifetime (it is immutable data). Shared with the HIR lowerer.
fn builtin_catalog() -> &'static praxis_stdlib::MethodCatalog {
    static CATALOG: std::sync::OnceLock<praxis_stdlib::MethodCatalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(praxis_stdlib::builtin_catalog)
}

/// The tree range a [`FileSpan`] covers — the inverse of
/// [`Inferer::file_span`].
///
/// A [`Constraint`]'s `at` is a `FileSpan`, and the map lowering reads
/// (`method_refs`) is keyed by the method-name token's `TextRange`. A deferred
/// method resolution has to get from one to the other: the span it recorded *is*
/// that token, so this is a change of vocabulary and not a lookup.
fn range_of(at: FileSpan) -> TextRange {
    TextRange::new(at.span.start().0.into(), at.span.end().0.into())
}

impl Inferer {
    fn span(&self, range: TextRange) -> Span {
        Span::new(
            BytePos::from(u32::from(range.start())),
            BytePos::from(u32::from(range.end())),
        )
    }

    fn file_span(&self, range: TextRange) -> FileSpan {
        FileSpan::new(self.file, self.span(range))
    }

    // --- the constraint channel (F10, TY-29) --------------------------------

    /// Require `t` to have `cap`, at `at`.
    ///
    /// Two cases, and the split is the whole design. A **concrete** type is
    /// decided here and now — that is what every capability check did before,
    /// and it is right. A **variable** cannot be: it may be generalized and then
    /// instantiated at a type that does not have the capability at all, which is
    /// what TY-29 is. That one is deferred onto the channel and discharged when
    /// the variable resolves, or claimed by the scheme that quantifies it.
    fn require_cap(&mut self, t: Type, cap: Capability, at: TextRange) {
        self.require_cap_as(t, cap, at, None);
    }

    /// Require the state type of a graph-helper call to be one the walk can
    /// remember (ADR-060). A no-op for every other callee.
    ///
    /// The six §6.5 helpers keep a visited set and, for the weighted ones, a
    /// cost table keyed on the state — so a state has to be a `Set` element and
    /// a `Map` key, which is [`CapKind::HashStable`]. A state that can change
    /// after the walk has stored it cannot be found again, and the walk revisits
    /// it forever: exactly D4's rule, at a different door.
    ///
    /// The requirement is emitted **here** rather than claimed by the helper's
    /// scheme in `seed_builtin_schemes`, and the reason is the diagnostic. A
    /// constraint claimed by a prelude name's scheme has no source span to point
    /// its "this is the operation that requires it" note at; emitted at the call,
    /// it reports at the call and needs no note. It still rides the channel: a
    /// state type that is a *variable* here — `fn walk(s) { bfs(s, step) }` — is
    /// deferred, claimed by `walk`'s own scheme, and re-checked at each call to
    /// `walk`, which is the whole point of F10.
    fn require_graph_state(&mut self, callee: SymbolId, state: Option<Type>, at: TextRange) {
        let is_helper = self.names.get(callee).is_some_and(|s| {
            s.kind == SymbolKind::Builtin && praxis_stdlib::graph_helper(&s.name).is_some()
        });
        if !is_helper {
            return;
        }
        // No first argument means the call's arity is already wrong, and the
        // mismatch has been reported. There is no state type to constrain.
        let Some(state) = state else { return };
        self.require_cap(state, Capability::Kind(CapKind::HashStable), at);
    }

    /// [`require_cap`](Self::require_cap) with the caller's own wording for a
    /// failure it can decide **immediately**.
    ///
    /// One requirement, two codes, and the split is not arbitrary. A compound
    /// assignment against a `Bool` is reported *at the operation* and says so —
    /// `Y010`, "values of type `Bool` do not support this operation". The same
    /// requirement discharged *later*, because the target was a variable when the
    /// `+=` was checked and some other use pinned it afterwards, has left that
    /// operation behind: it reports the channel's own wording (`Y015`) at
    /// whatever pinned it, with the operation as the note. Both are `Numeric`;
    /// what differs is how much the report can still see.
    fn require_cap_as(
        &mut self,
        t: Type,
        cap: Capability,
        at: TextRange,
        immediate: Option<fn(FileSpan, &str) -> Diagnostic>,
    ) {
        let resolved = self.db.follow(t);
        if let Some(var) = self.db.var_id_of(resolved) {
            let span = self.file_span(at);
            self.db.require(Constraint::new(var, cap, span));
            return;
        }
        if let Err(offender) = crate::capability::check(&mut self.db, self.catalog, resolved, &cap)
        {
            let span = self.file_span(at);
            match immediate {
                Some(make) => {
                    let rendered = self.db.render(offender);
                    self.diagnostics.push(make(span, &rendered));
                }
                None => self.report_cap_failure(&cap, offender, span, None),
            }
        }
    }

    /// Require `receiver` to have a method `name` taking `params` and returning
    /// `result` — the deferred half of method resolution (TY-30).
    ///
    /// A method call whose receiver is still a variable used to constrain
    /// nothing at all: `crate::catalog::lookup` needs a catalog-representable
    /// receiver, so `fn total(values) { values.sum() }` gave up, returned a
    /// fresh variable, and lowering later reported a method it could not find on
    /// a type nobody had named. The requirement goes on the channel instead, and
    /// [`Inferer::resolve_deferred_method`] answers it when the program says what
    /// the receiver is.
    ///
    /// **The receiver is pinned to the declaration group's level**, so
    /// generalization cannot quantify it. That is not an optimization, it is the
    /// contract: there is one lowered body per source function — monomorphization
    /// clones a tree lowering has already resolved — so one method call site
    /// carries one catalog entry and one receiver type. A quantified receiver
    /// would be N receiver types at one call site and no way to lower any of
    /// them. §5.2 states the same answer from the other end: `total` is
    /// `Vec[Int] -> Int`, not a scheme.
    ///
    /// The capability's own types are pinned with it. The result variable in
    /// particular: quantifying it would let a call site instantiate a *fresh*
    /// result while discharge unified the original, and the call would come out
    /// unconstrained.
    fn require_method(
        &mut self,
        receiver: Type,
        name: String,
        params: Vec<Type>,
        result: Type,
        at: TextRange,
    ) {
        let site = self.decl_site;
        self.db.pin_to_level(receiver, site);
        for p in &params {
            self.db.pin_to_level(*p, site);
        }
        self.db.pin_to_level(result, site);
        self.require_cap(
            receiver,
            Capability::HasMethod {
                name,
                params,
                result,
            },
            at,
        );
    }

    /// Answer a `HasMethod` requirement whose receiver has since resolved: look
    /// the method up, unify the entry's signature with the types the call site
    /// holds, and record the [`crate::MethodRef`] lowering reads (TY-30).
    ///
    /// This is why `HasMethod` is a *resolution* rather than a veto. The other
    /// capabilities answer yes or no and are done; this one has to hand back the
    /// entry, because the call site it came from produced no `method_refs` entry
    /// when it was made — the receiver had no type yet — and lowering reads that
    /// map and nothing else (F15/HIR-02).
    ///
    /// A receiver that turns out **not** to have the method is left alone here:
    /// lowering owns `Y110`, it has the method-name span, and it will report the
    /// same call. Reporting here as well is the same mistake twice.
    fn resolve_deferred_method(&mut self, c: &Constraint) {
        let Capability::HasMethod {
            name,
            params,
            result,
        } = &c.cap
        else {
            return;
        };
        let (name, params, result) = (name.clone(), params.clone(), *result);
        let receiver_ty = self.db.follow(c.var_type());
        let hits = crate::catalog::lookup(&self.db, self.catalog, receiver_ty, &name, params.len());
        let Some(entry) = hits.first().copied() else {
            return;
        };
        // The entry's patterns, instantiated through one shared name map so a
        // `Var("T")` repeated across receiver, params and result is one variable
        // — the same discipline `infer_method_call` uses when it can resolve at
        // the call site.
        let mut names = HashMap::new();
        let receiver_param =
            crate::lower::pattern_to_type_named(&mut self.db, &entry.receiver, &mut names);
        let param_tys: Vec<Type> = entry
            .params
            .iter()
            .map(|p| crate::lower::pattern_to_type_named(&mut self.db, p, &mut names))
            .collect();
        let result_ty =
            crate::lower::pattern_to_type_named(&mut self.db, &entry.result, &mut names);
        let at = c.report_at();
        let _ = self.db.unify(receiver_param, receiver_ty);
        for (pattern_ty, arg_ty) in param_tys.iter().zip(params.iter()) {
            if let Err(e) = self.db.unify(*pattern_ty, *arg_ty) {
                self.diag_unify(at, e);
            }
        }
        // Unifying the result is what *pins the call*: the deferred call site
        // returned a bare variable, and this is the only thing that ever says
        // what it holds.
        if let Err(e) = self.db.unify(result_ty, result) {
            self.diag_unify(at, e);
        }
        // Now that the receiver is known, everything the entry and the receiver
        // demand applies: a deferred `m.insert(k, v)` on an unannotated parameter
        // is still an insert, and a deferred `values.sum()` still needs `Int`
        // elements (TY-31).
        let at = range_of(c.at);
        self.apply_bounds(entry, &names, at);
        self.require_collection_invariants(receiver_ty, at);
        self.method_refs.insert(
            range_of(c.at),
            crate::MethodRef {
                entry,
                receiver: receiver_ty,
                result: result_ty,
            },
        );
    }

    /// Enforce what a catalog entry declares about its own type variables
    /// (TY-31).
    ///
    /// `names` is the map [`crate::lower::pattern_to_type_named`] filled while
    /// instantiating the entry's patterns, so a bound on `"T"` is a requirement
    /// on the very type variable the receiver's element was unified with. A name
    /// the entry declares a bound for but never instantiates cannot happen —
    /// `bounds()` reads the same patterns — but a missing entry is skipped rather
    /// than asserted, because a catalog bug should not panic the compiler.
    ///
    /// A [`Bound::Is`](praxis_stdlib::Bound::Is) is enforced by **unification**,
    /// not by the constraint channel, and that is deliberate: it rejects a wrong
    /// element with `expected Int, found Bool` *and* pins an element nothing has
    /// named yet, which is what `v.map(f).sum()` needs. A capability bound could
    /// not be enforced this way — it would have to go through
    /// [`require_cap`](Self::require_cap) so an unresolved variable is deferred —
    /// and the exhaustive match below is what makes adding one a compile error
    /// rather than a silent omission.
    fn apply_bounds(
        &mut self,
        entry: &praxis_stdlib::MethodEntry,
        names: &HashMap<String, Type>,
        at: TextRange,
    ) {
        for (var, bound) in entry.bounds() {
            let Some(ty) = names.get(var).copied() else {
                continue;
            };
            match bound {
                praxis_stdlib::Bound::Is(scalar) => {
                    // `praxis_types::ScalarType` *is* the pattern language's
                    // scalar (re-exported), so there is nothing to translate.
                    let want = self.db.scalar(scalar);
                    if let Err(e) = self.db.unify(want, ty) {
                        self.diag_unify(self.file_span(at), e);
                    }
                }
            }
        }
    }

    /// Require what a collection type demands of its own arguments (TY-32,
    /// RT-08, D4).
    ///
    /// Two rules, and neither was enforced anywhere:
    ///
    /// - A `Map` key, a `Set` element and a `Counter` key must be **findable
    ///   after they are stored**. Hashing a `Vec` by its contents and then
    ///   pushing to it moves the entry's bucket without moving the entry.
    /// - A heap element must be **orderable**, because the heap orders it.
    ///   `MinHeap[fn(Int) -> Int]` has no comparison to make.
    ///
    /// A still-unresolved argument goes on the channel and is checked when the
    /// program pins it — which is the common shape, since `let m = Map()` mints
    /// two variables and the first `insert` is what says what they are.
    fn require_collection_invariants(&mut self, t: Type, at: TextRange) {
        use praxis_types::{data::TypeData, CollectionCtor};
        let resolved = self.db.follow(t);
        let Some((ctor, args)) = (match self.db.data(resolved) {
            TypeData::Collection { ctor, args } => Some((*ctor, args.to_vec())),
            _ => None,
        }) else {
            return;
        };
        match (ctor, args.first().copied()) {
            // The first argument is the key for all three.
            (CollectionCtor::Map | CollectionCtor::Set | CollectionCtor::Counter, Some(key)) => {
                self.require_cap(key, Capability::Kind(CapKind::HashStable), at);
            }
            (CollectionCtor::MinHeap | CollectionCtor::MaxHeap, Some(el)) => {
                self.require_cap(el, Capability::Kind(CapKind::Ord), at);
            }
            _ => {}
        }
    }

    /// Check every constraint whose variable has resolved since it was made,
    /// and report the ones that fail.
    ///
    /// Called where a variable's fate is settled: after a function body (before
    /// its scheme is generalized, so the requirements the scheme *owns* are
    /// still pending and get claimed rather than reported) and once more when
    /// the declaration group closes.
    fn discharge_constraints(&mut self) {
        for c in self.db.take_dischargeable() {
            // `HasMethod` is the one requirement that *produces* something when
            // it holds — the catalog entry the deferred call site could not
            // select — so it is discharged by resolving it rather than by
            // checking it. See [`Inferer::resolve_deferred_method`], including
            // why a failure is lowering's to report and not this pass's.
            if matches!(c.cap, Capability::HasMethod { .. }) {
                self.resolve_deferred_method(&c);
                continue;
            }
            let ty = c.var_type();
            if let Err(offender) = crate::capability::check(&mut self.db, self.catalog, ty, &c.cap)
            {
                self.report_cap_failure(&c.cap, offender, c.report_at(), c.origin_note());
            }
        }
    }

    /// Report a capability failure in concrete language (§5.4: never name the
    /// capability, never say "trait").
    ///
    /// `origin` is the requirement's own span when it differs from the report
    /// site — the `a == b` inside a generic function whose call is what failed.
    fn report_cap_failure(
        &mut self,
        cap: &Capability,
        offender: Type,
        at: FileSpan,
        origin: Option<FileSpan>,
    ) {
        let rendered = self.db.render(offender);
        let mut diag = match cap {
            Capability::Kind(CapKind::Eq | CapKind::Hash) => not_equatable(at, &rendered),
            Capability::Kind(CapKind::Ord) => not_orderable(at, &rendered),
            Capability::Kind(CapKind::HashStable) => not_hashable(at, &rendered),
            Capability::Kind(CapKind::Numeric) => not_numeric(at, &rendered),
            Capability::Iterable { .. } => crate::diagnostics::not_iterable(at, &rendered),
            // Reached only by a `HasMethod` required against a receiver that was
            // already concrete, which `require_method` never does — a concrete
            // receiver is resolved at the call site. The deferred ones are
            // discharged by [`Inferer::resolve_deferred_method`], which leaves a
            // failure to lowering (it owns `Y110` and has the name span). The arm
            // is the honest translation of the capability all the same, and the
            // match is what keeps it honest if a second emitter appears.
            Capability::HasMethod { name, .. } => {
                crate::diagnostics::unknown_method(at, name, &rendered)
            }
        };
        if let Some(origin) = origin {
            diag = diag.with_note(origin, "this is the operation that requires it");
        }
        self.diagnostics.push(diag);
    }

    fn diag_unify(&mut self, at: FileSpan, err: UnifyError) {
        match err {
            UnifyError::Mismatch { expected, found } => {
                let e = self.db.render(expected);
                let f = self.db.render(found);
                self.diagnostics.push(type_mismatch(at, &e, &f));
            }
            UnifyError::Occurs { .. } => {
                self.diagnostics.push(infinite_type(at));
            }
        }
    }

    /// Like [`diag_unify`](Self::diag_unify), but attaches a `help:` hint for the
    /// common, recognizable mismatch shapes (§8.2: "a concrete suggestion when
    /// available"). The hint is chosen from the expected/found type pair and is
    /// guarded so it never fires speculatively — unknown shapes fall through to
    /// the plain mismatch.
    ///
    /// `context` describes where the mismatch occurred (e.g. "a function
    /// returning `Int`") so the hint reads naturally.
    fn diag_unify_hinted(&mut self, at: FileSpan, err: UnifyError, context: &str) {
        match err {
            UnifyError::Mismatch { expected, found } => {
                let e = self.db.render(expected);
                let f = self.db.render(found);
                let label = self.hint_for(expected, found, context);
                match label {
                    Some(label) => {
                        self.diagnostics
                            .push(type_mismatch_with_help(at, &e, &f, &label));
                    }
                    None => self.diagnostics.push(type_mismatch(at, &e, &f)),
                }
            }
            UnifyError::Occurs { .. } => {
                self.diagnostics.push(infinite_type(at));
            }
        }
    }

    /// Pick a `help:` label for a type mismatch, or `None` when no concrete
    /// suggestion applies. Currently recognizes:
    /// - found `Unit` where a value was expected (forgot to return one), and
    /// - found `Text` where a numeric type was expected (suggest `.int()`).
    fn hint_for(&self, expected: Type, found: Type, context: &str) -> Option<String> {
        let found_data = self.db.data(self.db.follow(found));
        let expected_data = self.db.data(self.db.follow(expected));
        use praxis_types::TypeData;
        match (expected_data, found_data) {
            (_, TypeData::Unit) => {
                // The expected type's rendered name (e.g. "Int") for the hint.
                let exp = self.db.render(expected);
                Some(format!(
                    "this value is `Unit`; {context} expected `{exp}` — make the last expression produce a value, or change the declared type to `Unit`"
                ))
            }
            (
                TypeData::Scalar(praxis_types::ScalarType::Int),
                TypeData::Scalar(praxis_types::ScalarType::Text),
            ) => Some("this is `Text`; call `.int()` on it (or use `read lines(int)`)".into()),
            _ => None,
        }
    }

    // --- builtin schemes ---------------------------------------------------

    /// Assign each prelude builtin the scheme its contract needs. `out` is
    /// `forall T. (T) -> Unit`; `panic` is `forall T. (T) -> Never`; the numeric
    /// helpers are monomorphic on `Int` (ADR-058); every §6.1 collection ctor is
    /// `forall T. () -> Ctor[T]`.
    ///
    /// A name that is **not** seeded here gets a fresh type variable, which
    /// unifies with anything and then lowers as a call to a function nobody
    /// defined — the whole of TY-33. Every prelude name that denotes a value now
    /// has a scheme here, so there are none left in that state.
    fn seed_builtin_schemes(&mut self) {
        // Collect the ids first to avoid borrowing `self.names` while mutating it.
        let to_seed: Vec<(SymbolId, String)> = self
            .names
            .all()
            .iter()
            .filter(|s| {
                // `Some`/`None` are `SymbolKind::EnumVariant`, not `Builtin`
                // (HIR-03): the prelude declares `Option`'s two variants, and
                // they get the kind a user-declared variant gets. They still
                // need their constructor schemes seeded here.
                matches!(s.kind, SymbolKind::Builtin | SymbolKind::EnumVariant)
                    && (s.name == "out"
                        || s.name == "panic"
                        || s.name == "dbg"
                        || s.name == "assert"
                        || s.name == "Some"
                        || s.name == "None"
                        || s.name == "pi"
                        || s.name == "e"
                        || praxis_stdlib::numeric_helper(&s.name).is_some()
                        || praxis_stdlib::graph_helper(&s.name).is_some()
                        || crate::decl::collection_ctor_for(&s.name).is_some())
            })
            .map(|s| (s.id, s.name.clone()))
            .collect();
        for (id, name) in to_seed {
            // Build the monomorphic scheme at an inner level, then generalize
            // OUTSIDE the scope. Generalize quantifies vars whose level is
            // strictly greater than the current level; the fresh vars are
            // created at the inner level, so generalizing at the outer level
            // (after scoped_return restores it) is what quantifies them. (Doing
            // it inside the scope quantifies nothing — the bug that left `Vec`
            // monomorphic, so every `Vec()` shared one element type.)
            let mono = self.db.scoped_return(|db| match name.as_str() {
                "out" | "panic" => {
                    // forall T. (T) -> Unit  (out)   /   forall T. (T) -> Never  (panic)
                    let v = db.fresh_var();
                    let result = if name == "panic" {
                        db.never()
                    } else {
                        db.unit()
                    };
                    db.func(vec![v], result)
                }
                // `dbg` prints to stderr and hands the value straight back
                // (§8.1), so it is `forall T. (T) -> T` — the identity on
                // types, which is what lets it wrap any subexpression.
                "dbg" => {
                    let v = db.fresh_var();
                    db.func(vec![v], v)
                }
                // `assert` takes a condition, not a value: `(Bool) -> Unit`,
                // monomorphic. This is what makes `assert(1)` a type error
                // rather than a fresh variable that accepts anything (TY-33).
                "assert" => {
                    let bool_ty = db.bool();
                    let unit_ty = db.unit();
                    db.func(vec![bool_ty], unit_ty)
                }
                // Collection constructors (§6.1). Each yields an empty
                // collection of its ctor type; the element type is a
                // quantified variable pinned by usage (push/insert/etc.).
                "Vec" | "Deque" | "Set" | "Counter" | "MinHeap" | "MaxHeap" | "Grid" => {
                    let v = db.fresh_var();
                    let ctor = crate::decl::collection_ctor_for(&name).expect("ctor name");
                    let coll = db.unary_collection(ctor, v);
                    db.func(vec![], coll)
                }
                "Map" => {
                    // forall K V. () -> Map[K, V]
                    let k = db.fresh_var();
                    let v = db.fresh_var();
                    let coll = db.map(k, v);
                    db.func(vec![], coll)
                }
                // BitSet and Range are nullary: () -> BitSet / () -> Range.
                "BitSet" | "Range" => {
                    let ctor = crate::decl::collection_ctor_for(&name).expect("ctor name");
                    let coll = db
                        .collection(ctor, CollectionArgs::Nullary)
                        .expect("BitSet and Range are nullary");
                    db.func(vec![], coll)
                }
                // Optionality (M9). `Some : forall T. (T) -> Option[T]` and
                // `None : forall T. Option[T]`. `None` is a zero-payload variant,
                // so its scheme is the enum type directly (mirroring how
                // user-declared zero-payload variants get a monotype scheme in
                // `infer_enum`, not a `() -> ...` function). Both name the *one*
                // `Option` def (F12) and differ only in their type argument;
                // they used to register a fresh nominal def each, which is what
                // made `unify` need a same-name-and-signature arm (TY-06).
                "Some" => {
                    let t = db.fresh_var();
                    let opt = db.option_of(t);
                    db.func(vec![t], opt)
                }
                "None" => {
                    let t = db.fresh_var();
                    db.option_of(t)
                }
                // Float constants (§4.12). Both are nullary `() -> Float`,
                // monomorphic — no quantified vars. Lowered to a direct runtime
                // call (`praxis_float_pi`/`praxis_float_e`) in MIR build.
                "pi" | "e" => {
                    let float_ty = db.float();
                    db.func(vec![], float_ty)
                }
                // The §16.1 numeric helpers, all monomorphic on `Int`
                // (ADR-058). A polymorphic `min`/`max`/`abs` would have to
                // carry `Capability::Kind(CapKind::Numeric)` on its own binder
                // and then choose a lowering per instantiation; `Float` already
                // has `abs`/`sign`/`min`/`max` as *methods* (§4.12), so the
                // free functions are the `Int` ones and say so. Before this
                // every one of them got a fresh variable, which unified with
                // anything and then lowered as a call to a function nobody
                // defined (TY-33).
                name if praxis_stdlib::numeric_helper(name).is_some() => {
                    let helper = praxis_stdlib::numeric_helper(name).expect("just matched");
                    let int_ty = db.int();
                    // The arity comes from the wrapper the helper lowers to, so
                    // the scheme and the call it becomes cannot disagree.
                    let params = vec![int_ty; helper.arity()];
                    db.func(params, int_ty)
                }
                // §6.5's graph helpers (ADR-060). Each is `forall T` over one
                // state type: the first parameter is a state and every other one
                // is a function of it, which is what "closure-based algorithms
                // that do not require materializing a graph object" means. The
                // row states the *shapes*; the types are built here, because
                // `praxis-stdlib` cannot name a `Type`.
                //
                // The state type also has to be a `Set` element and a `Map` key
                // — the walks remember where they have been — and that
                // requirement is emitted at each call site rather than claimed
                // here, so a failure points at the call rather than at a prelude
                // name with no source span. See [`Inferer::require_graph_state`].
                name if praxis_stdlib::graph_helper(name).is_some() => {
                    let helper = praxis_stdlib::graph_helper(name).expect("just matched");
                    let state = db.fresh_var();
                    let params = helper
                        .params
                        .iter()
                        .map(|p| match p {
                            praxis_stdlib::GraphParam::Start => state,
                            praxis_stdlib::GraphParam::Neighbours => {
                                let states = db.vec(state);
                                db.func(vec![state], states)
                            }
                            praxis_stdlib::GraphParam::Weight => {
                                let int_ty = db.int();
                                db.func(vec![state, state], int_ty)
                            }
                            praxis_stdlib::GraphParam::Heuristic => {
                                let int_ty = db.int();
                                db.func(vec![state], int_ty)
                            }
                            praxis_stdlib::GraphParam::Goal => {
                                let bool_ty = db.bool();
                                db.func(vec![state], bool_ty)
                            }
                        })
                        .collect();
                    let result = match helper.result {
                        praxis_stdlib::GraphResult::VisitOrder => db.vec(state),
                        praxis_stdlib::GraphResult::Reached => {
                            db.unary_collection(CollectionCtor::Set, state)
                        }
                        praxis_stdlib::GraphResult::CostTable => {
                            let int_ty = db.int();
                            db.map(state, int_ty)
                        }
                        praxis_stdlib::GraphResult::Distance => {
                            let int_ty = db.int();
                            db.option_of(int_ty)
                        }
                    };
                    db.func(params, result)
                }
                other => panic!("unexpected builtin `{other}` seeded"),
            });
            // Generalize at the outer level (after scoped_return restored it) so
            // the inner-level fresh vars are quantified — yielding e.g.
            // `forall T. () -> Vec[T]`. Doing this inside the scope quantified
            // nothing (the bug that left constructors monomorphic, so every
            // `Vec()` shared one element type).
            let scheme = self.db.generalize(mono);
            if let Some(sym) = self.names.get_mut(id) {
                sym.scheme = Some(scheme);
            }
        }
    }

    // --- top-level statements ----------------------------------------------

    /// Infer one statement. `struct` and `enum` are deliberately absent: their
    /// types were registered by the declaration pass, before any expression was
    /// inferred, which is the whole of TY-10.
    fn infer_top_stmt(&mut self, node: &praxis_syntax::SyntaxNode) {
        if let Some(let_) = LetStmt::cast(node.clone()) {
            self.infer_let(&let_);
        } else if let Some(var_) = VarStmt::cast(node.clone()) {
            self.infer_var(&var_);
        } else if let Some(fn_) = FnItem::cast(node.clone()) {
            self.infer_fn(&fn_);
        } else if let Some(assign) = AssignStmt::cast(node.clone()) {
            self.infer_assign(&assign);
        } else if let Some(expr) = ExprStmt::cast(node.clone()) {
            if let Some(e) = expr.expr() {
                self.infer_expr(&e);
            }
        }
    }

    /// Look up an enum variant by constructor name, **instantiated fresh**.
    /// Returns the enum type this use denotes, the variant's index, and the
    /// variant's payload types under that instance's type arguments (empty for
    /// payload-less). Works for both payload variants (scheme is `Func -> Enum`)
    /// and zero-payload variants (scheme is the enum type directly).
    ///
    /// Instantiating is what makes `Some(x)` in a pattern bind `x` at the
    /// scrutinee's element type: `Option`'s payload is its def's parameter (F12),
    /// so the payload of a *use* is only known once the use has its own
    /// arguments and the scrutinee has unified against them.
    fn lookup_enum_variant(
        &mut self,
        symbol: SymbolId,
        name: &str,
    ) -> Option<(Type, usize, Vec<Type>)> {
        let scheme = self.names.get(symbol)?.scheme.as_ref()?.clone();
        let body = self.db.instantiate(&scheme);
        // The scheme body is either a Func returning the enum type (payload
        // variant) or the enum type itself (zero-payload variant).
        let result_ty = match self.db.data(self.db.follow(body)) {
            praxis_types::TypeData::Func { result, .. } => *result,
            praxis_types::TypeData::Enum { .. } => body,
            _ => return None,
        };
        let enum_ty = self.db.follow(result_ty);
        let (def_id, args) = match self.db.data(enum_ty) {
            praxis_types::TypeData::Enum { def, args } => (*def, args.clone()),
            _ => return None,
        };
        let idx = self.db.enum_def(def_id).variant(name)?;
        let payload = self.db.variant_payload_of(def_id, &args, idx);
        Some((enum_ty, idx, payload))
    }

    fn infer_let(&mut self, stmt: &LetStmt) {
        // Infer the RHS at an inner level so its vars can be generalized. Manage
        // the level explicitly (not via db.scoped) because the inference borrows
        // `self` mutably alongside `self.db`.
        let prev = self.db.enter_level();
        let rhs = stmt.init();
        let rhs_ty = rhs.as_ref().map(|e| self.infer_expr(e));
        let annot = stmt.ty().and_then(|t| self.resolve_type(&t));
        // Unify annotation with the inferred RHS, if both are present. Point the
        // mismatch at the RHS initializer (the value with the wrong type),
        // falling back to the whole statement.
        if let (Some(a), Some(r)) = (annot, rhs_ty) {
            let at = rhs
                .as_ref()
                .map(|e| e.syntax().text_range())
                .unwrap_or_else(|| stmt.syntax().text_range());
            if let Err(e) = self.db.unify(a, r) {
                self.diag_unify_hinted(self.file_span(at), e, "the binding's type annotation");
            }
        }
        let body_ty = annot.or(rhs_ty).unwrap_or_else(|| self.db.fresh_var());
        self.db.exit_level(prev);
        // Generalize `let` bindings (§5.3), but only when the RHS is a syntactic
        // value — the HM value restriction. An expansive RHS (a call like
        // `Vec()`, a method call, a block, `read`, …) is left monomorphic so its
        // type variables are shared across uses rather than instantiated fresh
        // per reference. Without this, `let v = Vec(); v.push(inner); v.map(...)`
        // gives `v : forall T. Vec[T]`, and the push's element-type pinning never
        // reaches the map (Gap B). An explicit type annotation overrides the
        // restriction (the user has pinned the type by writing it).
        let expansive = rhs.as_ref().is_some_and(|e| !is_syntactic_value(e));
        let scheme = if expansive && annot.is_none() {
            Scheme::monotype(body_ty)
        } else {
            self.db.generalize(body_ty)
        };
        self.attach_scheme(stmt.name(), scheme);
    }

    fn infer_var(&mut self, stmt: &VarStmt) {
        // `var` RHS is inferred but NOT generalized (§5.3).
        let prev = self.db.enter_level();
        let rhs_ty = stmt.init().map(|e| self.infer_expr(&e));
        let annot = stmt.ty().and_then(|t| self.resolve_type(&t));
        if let (Some(a), Some(r)) = (annot, rhs_ty) {
            // Point at the RHS initializer, not the whole `var` statement.
            let at = stmt
                .init()
                .map(|e| e.syntax().text_range())
                .unwrap_or_else(|| stmt.syntax().text_range());
            if let Err(e) = self.db.unify(a, r) {
                self.diag_unify_hinted(self.file_span(at), e, "the binding's type annotation");
            }
        }
        let body_ty = annot.or(rhs_ty).unwrap_or_else(|| self.db.fresh_var());
        self.db.exit_level(prev);
        // No generalization for var.
        let scheme = Scheme::monotype(body_ty);
        self.attach_scheme(stmt.name(), scheme);
    }

    /// Infer one declaration group: every top-level statement in `root`.
    ///
    /// # Two phases (F19)
    ///
    /// The [declaration pass](crate::decl) runs first and seals a
    /// [`TypeEnv`](crate::decl::TypeEnv): every `struct`/`enum` is registered
    /// in dependency order (TY-10), and every `fn` gets a monomorphic
    /// **signature placeholder** so a call to a function declared *later*
    /// unifies against the same variable that declaration will resolve, and a
    /// disagreement is a diagnostic instead of silence (TY-22). Name resolution
    /// has been two-pass since M2; inference was not, so a forward reference
    /// resolved and was then unchecked.
    ///
    /// The pass runs *inside* the group level, which is one deeper than the
    /// group's binding site (TY-01): unifying a placeholder with the derived
    /// function type lowers that type's variables to the placeholder's level,
    /// so a placeholder at the *outer* level would clamp every parameter and
    /// result out to level zero and no signature could ever generalize. That
    /// coupling is why the two are one change.
    fn infer_declaration_group(&mut self, root: &SourceFile) {
        self.decl_site = self.db.level();
        let group = self.db.enter_level();
        self.type_env = crate::decl::declare(
            self.file,
            root,
            &self.decls,
            &self.type_refs,
            &mut self.db,
            &mut self.names,
            &mut self.diagnostics,
        );
        for stmt in root.stmts() {
            self.infer_top_stmt(&stmt);
        }
        // Whatever the per-function sweeps left: a requirement made at the top
        // level, and any a later declaration resolved. What is still pending
        // after this belongs to a variable nothing pinned, which inference has
        // already reported as itself.
        self.discharge_constraints();
        self.db.exit_level(group);
    }

    fn infer_fn(&mut self, item: &FnItem) {
        // The fn name is bound to a monomorphic placeholder var, so recursive
        // *and* forward uses unify against one variable. The declaration pass
        // minted it for a top-level fn.
        //
        // Resolution declares every `fn` it accepts, so a name with no
        // declaration is one it refused: a nested function, or the second of a
        // duplicate pair. Both are already reported (N005 / N004) and neither
        // has a signature — inference used to `expect` here and panic, which
        // broke `analyze`'s contract that malformed input becomes diagnostics
        // (TY-23). The body is still inferred, so the rest of the file keeps
        // reporting.
        let fn_symbol = item
            .name()
            .and_then(|name_tok| self.decls.get(&name_tok.text_range()).copied());
        let placeholder = fn_symbol
            .and_then(|id| self.type_env.signature(id))
            .unwrap_or_else(|| self.db.fresh_var());

        // An inner level: params get fresh vars, body is inferred.
        let prev = self.db.enter_level();

        // Bind each param's symbol to its type (annotated, or a fresh var).
        let mut param_types: Vec<Type> = Vec::new();
        if let Some(pl) = item.param_list() {
            for p in pl.params() {
                let pty = self.infer_param(&p);
                param_types.push(pty);
            }
        }
        // The declared return type, if any; else a fresh var the body and any
        // `return` both constrain. The result has to exist *before* the body is
        // inferred, because a `return` inside it has to be checked against
        // something (TY-18) — that is what the context stack carries.
        let ret_annot = item.return_type().and_then(|t| self.resolve_type(&t));
        let result_ty = ret_annot.unwrap_or_else(|| self.db.fresh_var());
        // Pin the placeholder to the signature **before** the body is inferred.
        // It used to happen afterwards, so a recursive call inside the body
        // unified a bare variable with `(args) -> ?r` and the result stayed
        // unknown until the whole function was done: `fn build(n: Int) ->
        // Vec[Int] { … let v = build(n - 1); v.push(n) … }` could not resolve
        // `push`, because at that point `v` had no type but a variable. Lowering
        // hid this by re-resolving the method later, against types inference had
        // since pinned — the re-derivation F15 removes, so the ordering has to
        // be right here instead.
        let signature = self.db.func(param_types.clone(), result_ty);
        if let Err(e) = self.db.unify(placeholder, signature) {
            let at = item.syntax().text_range();
            self.diag_unify(self.file_span(at), e);
        }
        self.fn_results.push(result_ty);
        let (body_ty, tail_range) = match item.body() {
            Some(b) => {
                let (ty, range) = self.infer_block_with_tail(&b);
                (Some(ty), range)
            }
            None => (None, None),
        };
        self.fn_results.pop();
        // The body **joins** with the declared result rather than unifying with
        // it (TY-19): `fn f() -> Int { panic("x") }` has a `Never` body and is
        // fine — every path that reaches the end diverges, so there is no value
        // to disagree. Point a real mismatch at the offending tail expression
        // (e.g. the trailing `out(...)`) rather than the whole `fn`, falling
        // back to the block's range, then the whole item.
        if let Some(b) = body_ty {
            if let Err(e) = self.db.join(result_ty, b) {
                let at = tail_range.unwrap_or_else(|| item.syntax().text_range());
                self.diag_unify_hinted(self.file_span(at), e, "the function body");
            }
        }
        self.db.exit_level(prev);
        // Check the requirements this body's own uses settled, *before*
        // generalizing (F10). The ordering is the whole discipline: what is
        // still unresolved here is about a variable the scheme is about to
        // quantify, so generalization claims it and each call site checks it
        // against its own types. Draining after would report a generic body's
        // requirement against a variable nothing pinned.
        self.discharge_constraints();
        // Generalize the fn after its body is checked (§5.3), at the level the
        // declaration group was entered *from* — the group's own level is still
        // open for the signatures declared after this one, and generalizing
        // against it would quantify nothing.
        let scheme = self.db.generalize_at(placeholder, self.decl_site);
        if let Some(id) = fn_symbol {
            if let Some(sym) = self.names.get_mut(id) {
                sym.scheme = Some(scheme);
            }
        }
    }

    fn infer_param(&mut self, p: &Param) -> Type {
        let ty = p
            .ty()
            .and_then(|t| self.resolve_type(&t))
            .unwrap_or_else(|| self.db.fresh_var());
        // Attach the param type to its declared symbol (via decls, not lookup).
        if let Some(name_tok) = p.name() {
            if let Some(&id) = self.decls.get(&name_tok.text_range()) {
                if let Some(sym) = self.names.get_mut(id) {
                    sym.scheme = Some(Scheme::monotype(ty));
                }
            }
        }
        ty
    }

    /// Infer `x = e` / `x += e`.
    ///
    /// The target is the symbol **resolution** bound the name to (TY-13). This
    /// was the one place inference looked a name up itself, through a scope
    /// tree it had pushed empty children onto and never bound anything in — so
    /// the walk fell out to the root and found either nothing (a local: the
    /// assignment was unchecked) or a same-named top-level binding (whose type
    /// it then constrained instead).
    fn infer_assign(&mut self, stmt: &AssignStmt) {
        let rhs_ty = stmt.value().map(|e| self.infer_expr(&e));
        let (Some(name_tok), Some(rhs)) = (stmt.name(), rhs_ty) else {
            return;
        };
        let at = name_tok.text_range();
        let Some(target) = self.refs.get(&at).copied() else {
            return;
        };
        let Some(sym) = self.names.get(target.symbol) else {
            return;
        };
        // Only a `var` may be reassigned (§4.2). A `let`, a parameter, a `for`
        // binding and a pattern binding are all immutable, and nothing checked
        // (TY-14) — `let x = 1; x = 2` compiled and the backend wrote the slot.
        if sym.kind != SymbolKind::Var {
            let kind = describe_binding(sym.kind);
            self.diagnostics
                .push(crate::diagnostics::assign_to_immutable(
                    self.file_span(at),
                    &sym.name,
                    kind,
                ));
        }
        let Some(scheme) = sym.scheme.as_ref() else {
            return;
        };
        let existing = self.db.instantiate(scheme);
        if let Err(e) = self.db.unify(existing, rhs) {
            self.diag_unify(self.file_span(at), e);
        }
        // A compound assignment is an arithmetic operation, so its target must
        // be numeric. Matching operand types alone said nothing: `var flag =
        // true; flag += false` unified `Bool` with `Bool` and was accepted
        // (TY-15).
        //
        // It goes through the channel now (TY-31). S13 reported it only for a
        // target whose type was already *known*, deliberately: `fn f(a) { a += 1
        // }` leaves `a` a variable, and answering "not numeric" about a variable
        // is wrong while pinning it to `Int` would silently narrow every
        // unannotated numeric parameter. Deferring is the third option and the
        // right one — the requirement is recorded, generalization carries it, and
        // whatever a call site puts in `a`'s place is what has to be a number.
        let compound = stmt
            .op()
            .is_some_and(|t| !matches!(t.kind(), SyntaxKind::EQ));
        if compound {
            self.require_cap_as(
                existing,
                Capability::Kind(CapKind::Numeric),
                at,
                Some(crate::diagnostics::compound_assign_non_numeric),
            );
        }
    }

    /// Attach a scheme to the symbol declared at `name_tok`'s site. Uses the
    /// `decls` map (keyed by the declaration's range) rather than a scope lookup,
    /// so the scheme lands on the *exact* symbol even when the name is shadowed
    /// (where `scopes.lookup` would return the latest binding).
    fn attach_scheme(&mut self, name_tok: Option<praxis_syntax::SyntaxToken>, scheme: Scheme) {
        if let Some(tok) = name_tok {
            if let Some(&id) = self.decls.get(&tok.text_range()) {
                if let Some(sym) = self.names.get_mut(id) {
                    sym.scheme = Some(scheme);
                }
            }
        }
    }

    // --- expressions -------------------------------------------------------

    /// Infer `expr`'s type **and record it** (F15).
    ///
    /// This is the one insertion into [`Inferer::expr_types`]. Every path that
    /// infers an expression goes through here or through
    /// [`infer_expr_expected`](Self::infer_expr_expected), which records too —
    /// so "inference visited this node" and "there is a recorded type for this
    /// node" are the same statement, and lowering may treat a miss as an
    /// internal error rather than inventing a fresh variable.
    fn infer_expr(&mut self, expr: &Expr) -> Type {
        let ty = self.infer_expr_uncached(expr);
        self.record_expr_type(expr, ty);
        ty
    }

    /// Record `ty` as `expr`'s inferred type. Called from the two entry points
    /// above and nowhere else.
    fn record_expr_type(&mut self, expr: &Expr, ty: Type) {
        self.record_node_type(expr.syntax(), ty);
    }

    /// Record `ty` at a syntax node.
    ///
    /// Two expression nodes are never *evaluated*, so they never reach
    /// [`infer_expr`](Self::infer_expr): a call's callee name and a record
    /// literal's head. Each is a name resolved through `refs`, not a value — but
    /// each is still a `PATH_EXPR`, and a map that claims to hold every
    /// expression's type has to hold theirs.
    fn record_node_type(&mut self, node: &praxis_syntax::SyntaxNode, ty: Type) {
        self.expr_types.insert(crate::NodeKey::of(node), ty);
    }

    fn infer_expr_uncached(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Literal(l) => self.infer_literal(l),
            Expr::Path(p) => self.infer_path(p),
            Expr::Bin(b) => self.infer_bin(b),
            Expr::Range(r) => self.infer_range(r),
            Expr::Unary(u) => self.infer_unary(u),
            Expr::Paren(p) => p
                .expr()
                .map(|e| self.infer_expr(&e))
                .unwrap_or_else(|| self.db.fresh_var()),
            Expr::Tuple(t) => {
                let els: Vec<Type> = t.elements().map(|e| self.infer_expr(&e)).collect();
                crate::decl::tuple_or_degenerate(&mut self.db, els)
            }
            Expr::Block(b) => self.infer_block(b),
            Expr::If(i) => self.infer_if(i),
            Expr::While(w) => self.infer_while(w),
            Expr::For(f) => self.infer_for(f),
            Expr::Loop(l) => self.infer_loop(l),
            Expr::Break(b) => self.infer_break(b),
            Expr::Continue(c) => self.infer_continue(c),
            Expr::Return(r) => self.infer_return(r),
            Expr::Call(c) => self.infer_call(c),
            Expr::MethodCall(m) => self.infer_method_call(m),
            Expr::Read(r) => self.infer_read(r),
            Expr::Parse(p) => self.infer_parse(p),
            Expr::RecordLit(r) => self.infer_record_lit(r),
            Expr::FieldGet(f) => self.infer_field_get(f),
            Expr::Match(m) => self.infer_match(m),
            // M7-WS7: closure — type is `Func`; params bind in a child scope.
            Expr::Closure(c) => self.infer_closure(c),
            Expr::Error(_) => self.db.fresh_var(),
        }
    }

    /// Infer the type of a `|params| expr` closure (M7, §4.10). The result is a
    /// `Func` type `(P0, …) -> R` built from the param and body types. Free
    /// variables in the body resolve to outer-scope bindings (captures); the
    /// capture environment is a runtime concern, not a type-system one (§4.10).
    fn infer_closure(&mut self, c: &praxis_ast::ClosureExpr) -> Type {
        let mut param_types = Vec::new();
        for p in c.params() {
            param_types.push(self.infer_param(&p));
        }
        let result_ty = self.infer_closure_body(c);
        self.db.func(param_types, result_ty)
    }

    /// Infer a closure's body with its own result on the function-context stack,
    /// so a `return` inside it is checked against *the closure* rather than
    /// against whatever function encloses it (TY-18).
    ///
    /// The result is the join of the placeholder the `return`s pinned and the
    /// body's own type: a closure whose body diverges contributes nothing.
    fn infer_closure_body(&mut self, c: &praxis_ast::ClosureExpr) -> Type {
        let result_ty = self.db.fresh_var();
        self.fn_results.push(result_ty);
        // A closure is a function boundary, so a loop outside it is not one a
        // `break` inside it can leave.
        let enclosing_loops = std::mem::take(&mut self.loops);
        let body_ty = c.body().map_or(self.db.unit(), |b| self.infer_expr(&b));
        self.loops = enclosing_loops;
        self.fn_results.pop();
        match self.db.join(result_ty, body_ty) {
            Ok(joined) => joined,
            Err(e) => {
                let at = c
                    .body()
                    .map(|b| b.syntax().text_range())
                    .unwrap_or_else(|| c.syntax().text_range());
                self.diag_unify_hinted(self.file_span(at), e, "the closure's return type");
                result_ty
            }
        }
    }

    /// Bidirectional closure inference (M8, §3): infer a closure argument with
    /// an expected `Func` type pushed down from the combinator's signature. Each
    /// param is unified with the corresponding expected Func param BEFORE the
    /// body is inferred, so a closure param whose type is the receiver's element
    /// type (e.g. `|inner| inner.len()` over a `Vec[Vec[Int]]`) is pinned and
    /// method calls on it resolve; and a fold's accumulator param `a` is pinned
    /// to the accumulator type threaded from the init argument. The expected
    /// result type is threaded into the body. Falls back to plain
    /// [`infer_closure`](Self::infer_closure) when the expected type is not a
    /// Func (or the arity differs) — unification with a fresh var is a no-op, so
    /// this is purely additive and cannot change currently-passing inference.
    fn infer_closure_expected(&mut self, c: &praxis_ast::ClosureExpr, expected: Type) -> Type {
        // Read the expected Func's params/result (after following). If it is not
        // a Func, or the param count differs, defer to the bottom-up path.
        let (exp_params, exp_result) = match self.db.data(self.db.follow(expected)) {
            praxis_types::TypeData::Func { params, result } => (params.clone(), *result),
            _ => return self.infer_closure(c),
        };
        let closure_params: Vec<_> = c.params().collect();
        if exp_params.len() != closure_params.len() {
            return self.infer_closure(c);
        }
        let mut param_types = Vec::new();
        for (p, exp_pt) in closure_params.into_iter().zip(exp_params.iter()) {
            let pt = self.infer_param(&p);
            // Pin the param to the expected type before the body sees it. This is
            // the load-bearing step: the body's method calls on this param now
            // resolve against a concrete type.
            let _ = self.db.unify(pt, *exp_pt);
            param_types.push(pt);
        }
        // Thread the expected result type into the body. (The body is a single
        // expression; we infer it directly. A full bidirectional system would
        // push `exp_result` into block tails too, but Praxis closure bodies here
        // are single expressions or push from their own tail.)
        let _ = exp_result;
        let result_ty = self.infer_closure_body(c);
        self.db.func(param_types, result_ty)
    }

    /// Infer an expression with an expected type pushed down from context
    /// (bidirectional inference, M8 §3). Currently only closures take the hint;
    /// every other expression ignores `expected` and infers bottom-up (a fresh
    /// var unifies no-op, so this is safe).
    fn infer_expr_expected(&mut self, expr: &Expr, expected: Type) -> Type {
        match expr {
            Expr::Closure(c) => {
                let ty = self.infer_closure_expected(c, expected);
                self.record_expr_type(expr, ty);
                ty
            }
            _ => self.infer_expr(expr),
        }
    }

    /// Infer the type of a record literal `Name { field: expr, … }` (M7, §4.5).
    /// Looks up the struct type, unifies each field initializer with the declared
    /// field type, and returns the struct type.
    fn infer_record_lit(&mut self, r: &RecordLitExpr) -> Type {
        // The literal's head is an ordinary name reference, so resolution
        // already decided which symbol it names — including under shadowing,
        // where a scope lookup here would answer differently.
        let struct_ty = r
            .name()
            .and_then(|p| p.name())
            .and_then(|tok| self.refs.get(&tok.text_range()).copied())
            .and_then(|resolved| self.type_env.ty(resolved.symbol));
        // The head is a `PATH_EXPR` nothing evaluates; the type it names is its
        // type, and a head that names nothing is a fresh variable like any other
        // unresolved expression.
        if let Some(head) = r.name() {
            let head_ty = struct_ty.unwrap_or_else(|| self.db.fresh_var());
            self.record_node_type(head.syntax(), head_ty);
        }
        let Some(struct_ty) = struct_ty else {
            // Unknown struct: infer each field for diagnostics, return a fresh var.
            if let Some(fl) = r.field_list() {
                for f in fl.fields() {
                    if let Some(e) = f.expr() {
                        self.infer_expr(&e);
                    }
                }
            }
            return self.db.fresh_var();
        };
        // Get the record def to look up declared field types.
        let (def_id, def_args) = match self.db.data(self.db.follow(struct_ty)) {
            praxis_types::TypeData::Record { def, args } => (*def, args.clone()),
            _ => return struct_ty,
        };
        // A record literal is **exact**: every declared field exactly once, and
        // nothing else (HIR-04). None of that was checked. A missing field was
        // allocated as `Unit` under its declared type; a duplicate pushed a
        // second payload into an object whose schema had one slot for it; and
        // an unknown field's initializer was not lowered at all, so its side
        // effects disappeared.
        let type_name = self.db.render(self.db.follow(struct_ty));
        let declared: Vec<String> = self
            .db
            .record_def(def_id)
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();
        let mut seen: Vec<String> = Vec::new();
        if let Some(fl) = r.field_list() {
            for f in fl.fields() {
                let Some(fname_tok) = f.name() else { continue };
                let fname = fname_tok.text().to_string();
                let at = self.file_span(fname_tok.text_range());
                // The initializer is inferred whatever the name turns out to
                // be: it is an expression the program wrote, and dropping it
                // drops whatever it does.
                let init_ty = match &f.expr() {
                    Some(e) => Some(self.infer_expr(e)),
                    // Punned field `{ x }` — x must be a binding of the field's type.
                    None => {
                        // Look up the name as a path reference.
                        let range = fname_tok.text_range();
                        self.refs.get(&range).and_then(|rf| {
                            self.names
                                .get(rf.symbol)
                                .and_then(|s| s.scheme.as_ref().map(|sc| self.db.instantiate(sc)))
                        })
                    }
                };
                if seen.contains(&fname) {
                    self.diagnostics
                        .push(crate::diagnostics::duplicate_record_field(at, &fname));
                    continue;
                }
                seen.push(fname.clone());
                let Some((_, declared_ty)) = self.db.record_field_of(def_id, &def_args, &fname)
                else {
                    self.diagnostics
                        .push(crate::diagnostics::unknown_record_field(
                            at, &type_name, &fname,
                        ));
                    continue;
                };
                let init_ty = init_ty.unwrap_or_else(|| self.db.fresh_var());
                if let Err(e) = self.db.unify(declared_ty, init_ty) {
                    self.diag_unify(at, e);
                }
            }
        }
        let missing: Vec<String> = declared
            .iter()
            .filter(|d| !seen.contains(d))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let at = self.file_span(r.syntax().text_range());
            self.diagnostics
                .push(crate::diagnostics::missing_record_fields(
                    at, &type_name, &missing,
                ));
        }
        struct_ty
    }

    /// Infer the type of a field access `receiver.field` (M7, §4.5). Returns the
    /// field's declared type.
    fn infer_field_get(&mut self, f: &FieldExpr) -> Type {
        let receiver_ty = f
            .receiver()
            .map(|r| self.infer_expr(&r))
            .unwrap_or_else(|| self.db.fresh_var());
        let Some(field_tok) = f.field_name() else {
            return receiver_ty;
        };
        let fname = field_tok.text().to_string();
        let resolved = self.db.follow(receiver_ty);
        match self.db.data(resolved) {
            praxis_types::TypeData::Record { def, args } => {
                let (def, args) = (*def, args.clone());
                self.db
                    .record_field_of(def, &args, &fname)
                    .map(|(_, t)| t)
                    .unwrap_or_else(|| self.db.fresh_var())
            }
            _ => self.db.fresh_var(),
        }
    }

    /// Infer the type of a `match scrutinee { pattern => body, … }` expression
    /// (M7, §4.6). Unifies the scrutinee with each pattern, then unifies all
    /// arm body types to determine the match's result type.
    fn infer_match(&mut self, m: &praxis_ast::MatchExpr) -> Type {
        let scrutinee_ty = m
            .scrutinee()
            .map(|s| self.infer_expr(&s))
            .unwrap_or_else(|| self.db.fresh_var());
        let arms: Vec<_> = m.arms().collect();
        // The arms **join** (TY-19). Seeded with `Never` rather than a fresh
        // variable, so an arm that diverges contributes nothing and a match
        // whose every arm diverges is itself `Never` — a fresh variable would
        // silently make it "whatever the first non-divergent use wants".
        let mut result = self.db.never();
        for arm in &arms {
            if let Some(pat) = arm.pattern() {
                self.infer_pattern(&pat, scrutinee_ty);
            }
            if let Some(body) = arm.body() {
                let body_ty = self.infer_expr(&body);
                match self.db.join(result, body_ty) {
                    Ok(joined) => result = joined,
                    Err(e) => {
                        let at = self.file_span(arm.syntax().text_range());
                        self.diag_unify(at, e);
                    }
                }
            }
        }
        result
    }

    /// Infer a pattern against an expected type (M7, §4.6). Binds pattern
    /// variables and unifies variant payloads.
    #[allow(clippy::only_used_in_recursion)]
    fn infer_pattern(&mut self, pat: &praxis_ast::Pattern, expected: Type) {
        use praxis_ast::PatternKind;
        match pat.kind() {
            PatternKind::Wildcard => {
                // Matches anything; no binding, no constraint.
            }
            PatternKind::Literal => {
                // Literal patterns match scalars; infer the literal's type and
                // unify with the scrutinee. We rely on the token kind.
                if let Some(tok) = pat.name_token().or_else(|| {
                    pat.syntax().children_with_tokens().find_map(|e| match e {
                        rowan::NodeOrToken::Token(t) => Some(t),
                        _ => None,
                    })
                }) {
                    let lit_ty = match tok.kind() {
                        SyntaxKind::IntLit => self.db.int(),
                        SyntaxKind::TextLit => self.db.text(),
                        SyntaxKind::KW_TRUE | SyntaxKind::KW_FALSE => self.db.bool(),
                        _ => self.db.fresh_var(),
                    };
                    if let Err(e) = self.db.unify(expected, lit_ty) {
                        self.diag_unify(self.file_span(tok.text_range()), e);
                    }
                }
            }
            PatternKind::Name(name) => {
                // A variable bind: matches anything of the scrutinee's type.
                // Bind the variable to `expected`'s type.
                if let Some(tok) = pat.name_token() {
                    let range = tok.text_range();
                    if let Some(&symbol) = self.decls.get(&range) {
                        if let Some(sym) = self.names.get_mut(symbol) {
                            sym.scheme = Some(Scheme::monotype(expected));
                        }
                    }
                }
                let _ = name;
            }
            PatternKind::Variant(vname) => {
                // An enum variant pattern. The constructor is a name reference
                // resolution already resolved; read the variant off its symbol.
                let ctor = pat
                    .name_token()
                    .and_then(|t| self.refs.get(&t.text_range()).copied())
                    .map(|r| r.symbol);
                if let Some((enum_ty, variant_idx, payload_types)) =
                    ctor.and_then(|symbol| self.lookup_enum_variant(symbol, &vname))
                {
                    if let Err(e) = self.db.unify(expected, enum_ty) {
                        if let Some(tok) = pat.name_token() {
                            self.diag_unify(self.file_span(tok.text_range()), e);
                        }
                    }
                    // Unify sub-patterns with payload types.
                    let sub_pats: Vec<_> = pat.sub_patterns().collect();
                    for (i, sub) in sub_pats.iter().enumerate() {
                        if let Some(&payload_ty) = payload_types.get(i) {
                            self.infer_pattern(sub, payload_ty);
                        }
                    }
                    let _ = variant_idx;
                }
            }
        }
    }

    /// Synthesize the result type of a `read parser_expression` (§7.1, M6).
    fn infer_read(&mut self, r: &praxis_ast::ReadExpr) -> Type {
        match r.parser_expr() {
            Some(pe) => crate::parser_lower::synthesize_parser_type(
                &pe,
                self.file,
                &mut self.db,
                &mut self.diagnostics,
            )
            .unwrap_or_else(|| self.db.fresh_var()),
            None => self.db.fresh_var(),
        }
    }

    /// Synthesize the result type of a `parse(text, parser_expression)` (§7.1, M6).
    fn infer_parse(&mut self, p: &praxis_ast::ParseExpr) -> Type {
        // The text argument is an ordinary expression; resolve it — and it has
        // to be `Text`. `parse(text, parser)` runs a parser plan over a byte
        // buffer, so `parse(1, int)` reaches the runtime with an `Int` where a
        // `Text` payload is expected. Its type was inferred and then discarded
        // (TY-25).
        if let Some(text_expr) = p.text_expr() {
            let arg_ty = self.infer_expr(&text_expr);
            let text = self.db.text();
            if let Err(e) = self.db.unify(arg_ty, text) {
                let at = text_expr.syntax().text_range();
                self.diag_unify(self.file_span(at), e);
            }
        }
        match p.parser_expr() {
            Some(pe) => crate::parser_lower::synthesize_parser_type(
                &pe,
                self.file,
                &mut self.db,
                &mut self.diagnostics,
            )
            .unwrap_or_else(|| self.db.fresh_var()),
            None => self.db.fresh_var(),
        }
    }

    fn infer_literal(&mut self, lit: &Literal) -> Type {
        let tok = match lit.token() {
            Some(t) => t,
            None => return self.db.fresh_var(),
        };
        match tok.kind() {
            SyntaxKind::IntLit => self.db.int(),
            SyntaxKind::FloatLit => self.db.float(),
            SyntaxKind::TextLit => self.db.text(),
            SyntaxKind::KW_TRUE | SyntaxKind::KW_FALSE => self.db.bool(),
            // Backtick templates are M6; treat as Text for now (a fresh var would
            // be sounder but Text matches the eventual type).
            SyntaxKind::BacktickTemplate => self.db.text(),
            _ => self.db.fresh_var(),
        }
    }

    fn infer_path(&mut self, p: &PathExpr) -> Type {
        let tok = match p.name() {
            Some(t) => t,
            None => return self.db.fresh_var(),
        };
        let range = tok.text_range();
        // Find the resolved symbol; instantiate its scheme and record the type.
        if let Some(resolved) = self.refs.get(&range).copied() {
            let scheme = self
                .names
                .get(resolved.symbol)
                .and_then(|s| s.scheme.clone());
            if let Some(scheme) = scheme {
                let site = self.file_span(range);
                let ty = self.db.instantiate_at(&scheme, site);
                self.ref_types.insert(range, ty);
                // A `fn` name reaches here only in **value** position: a call's
                // callee is instantiated by `infer_call`, which records its own
                // node type. A monomorphic one becomes a closure over its adapter
                // (REP-01, ADR-061); a *generic* one has nothing to adapt, because
                // monomorphization is driven by call sites and a value has none —
                // so the adapter would call a clone-source the mono pass drops,
                // and the JIT would fail with "unresolved user function". That is
                // reported here instead, where the name is written, and where
                // `praxis check` can see it.
                if self.is_generic_fn(resolved.symbol, &scheme) {
                    let name = self
                        .names
                        .get(resolved.symbol)
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.diagnostics
                        .push(crate::diagnostics::generic_function_as_value(site, &name));
                }
                return ty;
            }
        }
        // Unresolved names were already reported by resolution; return a fresh var
        // so downstream inference does not cascade spurious errors.
        self.db.fresh_var()
    }

    /// Whether `symbol` is a `fn` whose scheme quantifies anything — the case a
    /// function *value* cannot represent (REP-01).
    ///
    /// The kind is what makes this answerable: a `let` bound to a closure also
    /// has a `Func` scheme, and a generalized one at that, so the scheme alone
    /// cannot tell a declaration from a binding that holds a value (the same
    /// reason `SymbolKind::EnumVariant` exists — HIR-03).
    fn is_generic_fn(&self, symbol: SymbolId, scheme: &praxis_types::Scheme) -> bool {
        self.names.get(symbol).map(|s| s.kind) == Some(SymbolKind::Fn)
            && !scheme.binders().is_empty()
    }

    /// `a..b` / `a..=b` (§4.11, ADR-059). Both bounds must be `Int`; the range
    /// itself is the nullary `Range` collection, whose element type
    /// `capability::iter_item` already answers as `Int`.
    ///
    /// The bounds are `Int` **only**. A `Float` range would need a step, and
    /// `0.0..1.0` has no elements to iterate — `iter_item` says a range yields
    /// `Int`, and admitting float bounds would make that a lie (D6).
    fn infer_range(&mut self, r: &praxis_ast::RangeExpr) -> Type {
        let (start, end) = r.bounds();
        let start_range = start.as_ref().map(|e| e.syntax().text_range());
        let end_range = end.as_ref().map(|e| e.syntax().text_range());
        let st = start.map(|e| self.infer_expr(&e));
        let et = end.map(|e| self.infer_expr(&e));
        let whole = r.syntax().text_range();
        let int_ty = self.db.int();
        for (bound, at) in [(st, start_range), (et, end_range)] {
            let Some(bound) = bound else { continue };
            if let Err(e) = self.db.unify(bound, int_ty) {
                self.diag_unify(self.file_span(at.unwrap_or(whole)), e);
            }
        }
        self.db
            .collection(CollectionCtor::Range, CollectionArgs::Nullary)
            .expect("Range is nullary")
    }

    fn infer_bin(&mut self, b: &BinExpr) -> Type {
        let (lhs, rhs) = b.operands();
        // Keep each operand's node so a type mismatch can point at the specific
        // bad operand rather than the whole binary expression (the earlier
        // behavior underlined `a + b` even when only `a` was at fault).
        let lhs_range = lhs.as_ref().map(|e| e.syntax().text_range());
        let rhs_range = rhs.as_ref().map(|e| e.syntax().text_range());
        // Detect a float-literal operand before the operands are moved below;
        // arithmetic inference uses this to pick Float vs Int under the strict
        // per-literal model (§4.12).
        let any_float_operand = operand_is_float_literal(&lhs) || operand_is_float_literal(&rhs);
        let lt = lhs.map(|e| self.infer_expr(&e));
        let rt = rhs.map(|e| self.infer_expr(&e));
        let op_kind = b.op().map(|t| t.kind());
        match op_kind {
            // Arithmetic (§4.12). The result type follows the operand literal
            // kind under the strict per-literal model: if either operand is a
            // float literal, both operands and the result are Float; otherwise
            // Int. Mixing a float literal with an Int-typed expression (e.g.
            // `1 + 2.5` where `1` is an int literal and `2.5` is float) fails
            // unification → a clean type error; there is no implicit widening.
            Some(
                SyntaxKind::PLUS
                | SyntaxKind::MINUS
                | SyntaxKind::STAR
                | SyntaxKind::SLASH
                | SyntaxKind::PERCENT,
            ) => {
                // The result type follows the operands under the strict
                // per-literal model (§4.12): Float if any operand is a float
                // literal OR has an already-inferred Float type (e.g. a method
                // call like `16.0.sqrt()`); otherwise Int. Mixing a Float
                // operand with an Int-typed expression fails unification → a
                // clean type error; there is no implicit widening.
                let any_float_type = [lt, rt]
                    .into_iter()
                    .flatten()
                    .any(|t| is_float_scalar(&self.db, t));
                let target = if any_float_operand || any_float_type {
                    self.db.float()
                } else {
                    self.db.int()
                };
                if let (Some(l), Some(r)) = (lt, rt) {
                    let whole = b.syntax().text_range();
                    let lhs_at = lhs_range.unwrap_or(whole);
                    let rhs_at = rhs_range.unwrap_or(whole);
                    if let Err(e) = self.db.unify(l, target) {
                        self.diag_unify(self.file_span(lhs_at), e);
                    }
                    if let Err(e) = self.db.unify(r, target) {
                        self.diag_unify(self.file_span(rhs_at), e);
                    }
                }
                // `%` is defined for integers only (§4.12). MIR has no Float
                // remainder: its `lower_bin` fell through to *addition*, so
                // `5.0 % 2.0` computed `7.0` (TY-27). There is no operation to
                // lower, so there is nothing to accept.
                if op_kind == Some(SyntaxKind::PERCENT) && is_float_scalar(&self.db, target) {
                    let at = b.syntax().text_range();
                    self.diagnostics
                        .push(crate::diagnostics::operator_not_defined(
                            self.file_span(at),
                            "%",
                            "Float",
                        ));
                }
                target
            }
            // Comparisons: operands must match; result is Bool.
            Some(
                SyntaxKind::EQ2
                | SyntaxKind::NEQ
                | SyntaxKind::LT
                | SyntaxKind::GT
                | SyntaxKind::LTEQ
                | SyntaxKind::GTEQ,
            ) => {
                if let (Some(l), Some(r)) = (lt, rt) {
                    // Point at the RHS operand for a comparison mismatch: the LHS
                    // establishes the expected type, the RHS is what failed to
                    // match it. Falls back to the whole expression.
                    let at = rhs_range.unwrap_or_else(|| b.syntax().text_range());
                    if let Err(e) = self.db.unify(l, r) {
                        self.diag_unify(self.file_span(at), e);
                    }
                    // Equality (`==`/`!=`) on a composite type (record/tuple/
                    // enum/collection) is structural (§5.5) and requires every
                    // contained type to be equatable; functions are never
                    // equatable. Emit Y004 for a type that cannot be compared
                    // with `==`.
                    //
                    // Ordering (`<`, `>`, `<=`, `>=`) requires `supports_ord`,
                    // which is the scalars with a `compare` callback and
                    // nothing else (ADR-045). This check is new: the capability
                    // existed since M5 and was **never called**, so `true <
                    // false` and `(1, 2) < (1, 3)` compiled and compared two
                    // reinterpreted payload words (P0-12). Emit Y006.
                    //
                    // Both go through the constraint channel (F10, TY-29). A
                    // concrete operand is decided on the spot, exactly as
                    // before; an operand that is still a variable is *deferred*,
                    // because it may be generalized and then instantiated at a
                    // type with no such operation — which is what let
                    // `fn equal(a, b) { a == b }` accept `equal(f, g)`.
                    let operand_ty = self.db.follow(l);
                    let kind = if matches!(op_kind, Some(SyntaxKind::EQ2 | SyntaxKind::NEQ)) {
                        CapKind::Eq
                    } else {
                        CapKind::Ord
                    };
                    self.require_cap(operand_ty, Capability::Kind(kind), at);
                }
                self.db.bool()
            }
            // `||` (logical or) — lexed; result Bool, operands Bool.
            Some(SyntaxKind::PIPE2) => {
                let bool = self.db.bool();
                if let (Some(l), Some(r)) = (lt, rt) {
                    let whole = b.syntax().text_range();
                    let lhs_at = lhs_range.unwrap_or(whole);
                    let rhs_at = rhs_range.unwrap_or(whole);
                    if let Err(e) = self.db.unify(l, bool) {
                        self.diag_unify(self.file_span(lhs_at), e);
                    }
                    if let Err(e) = self.db.unify(r, bool) {
                        self.diag_unify(self.file_span(rhs_at), e);
                    }
                }
                bool
            }
            _ => self.db.fresh_var(),
        }
    }

    fn infer_unary(&mut self, u: &UnaryExpr) -> Type {
        let operand_node = u.operand();
        let operand = operand_node.as_ref().map(|e| self.infer_expr(e));
        let result = match u.op().map(|t| t.kind()) {
            // Negation follows the operand's literal kind under the strict
            // per-literal model (§4.12): `-3.5` is Float, `-3` is Int.
            Some(SyntaxKind::MINUS) => {
                // Follow the operand's **type**, not only its literal syntax.
                // `-x` where `x: Float` used to come out `Int` and then fail to
                // unify with its own operand, so `fn negate(x: Float) -> Float
                // { -x }` was rejected — the one shape a per-literal rule cannot
                // see (TY-26). Binary arithmetic already asked both questions;
                // negation asked only the first.
                let is_float = operand_is_float_literal(&operand_node)
                    || operand.is_some_and(|t| is_float_scalar(&self.db, t));
                if is_float {
                    self.db.float()
                } else {
                    self.db.int()
                }
            }
            Some(SyntaxKind::BANG) => self.db.bool(),
            _ => self.db.fresh_var(),
        };
        if let Some(o) = operand {
            // Point at the operand, not the whole unary expression.
            let at = operand_node
                .as_ref()
                .map(|e| e.syntax().text_range())
                .unwrap_or_else(|| u.syntax().text_range());
            if let Err(e) = self.db.unify(o, result) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        result
    }

    fn infer_block(&mut self, block: &BlockExpr) -> Type {
        self.infer_block_inner(block).0
    }

    /// Like [`infer_block`](Self::infer_block), but also returns the source
    /// range of the expression that produced the block's value (its trailing
    /// expression), or the block's own range when there is no trailing
    /// expression (the block's value is then `Unit`). Used by `infer_fn` so a
    /// return-type mismatch can point at the offending tail expression rather
    /// than the whole `fn`.
    fn infer_block_with_tail(&mut self, block: &BlockExpr) -> (Type, Option<TextRange>) {
        self.infer_block_inner(block)
    }

    /// Shared body: infer every statement, returning the block's value type and
    /// the range that produced it (the trailing expression, or the block itself
    /// when the value is the implicit trailing `Unit`).
    fn infer_block_inner(&mut self, block: &BlockExpr) -> (Type, Option<TextRange>) {
        // Only the **last** statement can be the block's value, and only if it
        // is an expression statement. Every expression statement used to
        // overwrite `last`, so `{ 1; let x = 2 }` was inferred `Int` while
        // lowering demoted the `1` to an effect and gave the block a `Unit`
        // tail — inference and execution disagreed about the result type
        // (TY-16). `lower_block` is the shape this mirrors: a *pending* tail
        // that any following statement, of any kind, demotes.
        let unit = self.db.unit();
        let mut pending: Option<(Type, TextRange)> = None;
        for child in block.stmts() {
            if let Some(expr_stmt) = ExprStmt::cast(child.clone()) {
                if let Some(e) = expr_stmt.expr() {
                    let ty = self.infer_expr(&e);
                    pending = Some((ty, e.syntax().text_range()));
                    continue;
                }
            }
            self.infer_top_stmt(&child);
            pending = None;
        }
        let (last, tail_range) = match pending {
            Some((ty, range)) => (ty, Some(range)),
            None => (unit, None),
        };
        // A block is the one expression inference can reach *without* going
        // through `infer_expr`: a branch body, a loop body and a function body
        // are all `infer_block` calls. Recording here as well is what makes
        // "every expression node has a type" true of the whole tree rather than
        // of the subset that happened to sit in an expression position (F15).
        self.expr_types
            .insert(crate::NodeKey::of(block.syntax()), last);
        // No trailing expression: the value is Unit; point at the whole block so
        // the reader still sees where the implicit Unit comes from.
        let tail_range = tail_range.or_else(|| Some(block.syntax().text_range()));
        (last, tail_range)
    }

    fn infer_if(&mut self, i: &IfExpr) -> Type {
        if let Some(cond) = i.cond() {
            let ct = self.infer_expr(&cond);
            let bool = self.db.bool();
            // Condition must be Bool: point at the condition, not the whole `if`.
            let at = cond.syntax().text_range();
            if let Err(e) = self.db.unify(bool, ct) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        let then_ty = i.then_branch().map(|b| self.infer_block(&b));
        let else_ty = i.else_branch().and_then(|e| self.infer_else(&e));
        match (then_ty, else_ty) {
            // The two branches **join**, they do not unify (TY-19). A branch
            // that diverges — `if flag { panic("stop") } else { 1 }` — has type
            // `Never` and produces no value, so asking the two to be equal
            // rejects a program that is fine.
            (Some(t), Some(e)) => match self.db.join(t, e) {
                Ok(joined) => joined,
                Err(err) => {
                    // Branches disagree: point at the `else` branch (the one that
                    // diverges from the `then` branch's established type).
                    let at = i
                        .else_branch()
                        .and_then(|eb| eb.body())
                        .map(|body| body.syntax().text_range())
                        .unwrap_or_else(|| i.syntax().text_range());
                    self.diag_unify(self.file_span(at), err);
                    t
                }
            },
            // No `else`: MIR materializes `Unit` on the false path, so the
            // expression's value is the join of the then branch and `Unit`
            // (TY-17). `if c { 1 }` is therefore a mismatch — the value the
            // then branch produces has nowhere to come from when the condition
            // is false — while `if c { return 1 }` and `if c { panic("x") }`
            // stay legal, because a divergent branch is absorbed.
            (Some(t), None) => {
                let unit = self.db.unit();
                match self.db.join(t, unit) {
                    Ok(joined) => joined,
                    Err(err) => {
                        let at = i
                            .then_branch()
                            .map(|b| b.syntax().text_range())
                            .unwrap_or_else(|| i.syntax().text_range());
                        self.diag_unify_hinted(self.file_span(at), err, "an `if` with no `else`");
                        unit
                    }
                }
            }
            (None, Some(e)) => e,
            (None, None) => self.db.fresh_var(),
        }
    }

    fn infer_else(&mut self, e: &ElseBranch) -> Option<Type> {
        e.body().map(|body| match body {
            Expr::Block(b) => self.infer_block(&b),
            other => self.infer_expr(&other),
        })
    }

    fn infer_while(&mut self, w: &WhileExpr) -> Type {
        if let Some(cond) = w.cond() {
            let ct = self.infer_expr(&cond);
            let bool = self.db.bool();
            // Condition must be Bool: point at the condition, not the whole `while`.
            let at = cond.syntax().text_range();
            if let Err(e) = self.db.unify(bool, ct) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        if let Some(body) = w.body() {
            self.in_loop(LoopFlavour::Statement("while"), |me| {
                me.infer_block(&body);
            });
        }
        // `while` yields Unit.
        self.db.unit()
    }

    /// `for binding in iter { body }` (M8, §4.11). The iterator must be iterable
    /// (§5.4); the binding gets the element type. `for` yields Unit.
    fn infer_for(&mut self, f: &ForExpr) -> Type {
        let iter_ty = f
            .iter()
            .map(|i| self.infer_expr(&i))
            .unwrap_or_else(|| self.db.fresh_var());
        // The iterator must be iterable; `iter_item` returns the element type
        // (None → Y005 not_iterable). Record the element type on the binding's
        // reference range so the lowerer can read it.
        let item_ty = crate::capability::iter_item(&mut self.db, iter_ty);
        // An unresolved iterator yields *itself* from `iter_item`, which is the
        // optimism that let `fn drain(values) { for v in values { … } }` accept
        // `drain(1)`: the requirement was answered against a variable and then
        // discarded at generalization. Defer it instead (TY-29). A concrete
        // iterator is decided here, exactly as before.
        if let Some(item) = item_ty {
            self.require_cap(
                iter_ty,
                Capability::Iterable { item },
                f.iter()
                    .map(|i| i.syntax().text_range())
                    .unwrap_or_else(|| f.syntax().text_range()),
            );
        }
        if let Some(name_tok) = f.binding() {
            if let Some(item) = item_ty {
                self.ref_types.insert(name_tok.text_range(), item);
                // Attach the element type as the binding's scheme (monomorphic —
                // a loop variable is never generalized).
                self.attach_scheme(Some(name_tok.clone()), Scheme::monotype(item));
            } else {
                // Not iterable: emit Y005. Render the iterator type for the message.
                let ty_str = self.db.render(iter_ty);
                self.diagnostics.push(crate::diagnostics::not_iterable(
                    self.file_span(f.syntax().text_range()),
                    &ty_str,
                ));
            }
        }
        if let Some(body) = f.body() {
            self.in_loop(LoopFlavour::Statement("for"), |me| {
                me.infer_block(&body);
            });
        }
        self.db.unit()
    }

    /// `loop { body }` (M8, §4.11). The **only** expression loop: its type is
    /// the join of every `break` value (TY-21, D2).
    ///
    /// A `loop` no `break` leaves — `loop { }`, or one exited only by `return` —
    /// produces no value, so it is `Never` and not `Unit`: that is what makes
    /// `if c { 1 } else { loop { } }` an `Int`. The body's own type is
    /// discarded; a loop repeats it rather than producing it.
    fn infer_loop(&mut self, l: &LoopExpr) -> Type {
        let Some(body) = l.body() else {
            return self.db.never();
        };
        self.in_loop(LoopFlavour::Expression, |me| {
            me.infer_block(&body);
        })
    }

    /// Infer `body` with one more enclosing loop, and answer with what that
    /// loop's `break`s produced (seeded with `Never`, so an unbroken loop is
    /// `Never` and a `while`/`for` frame is simply discarded by its caller).
    fn in_loop(&mut self, flavour: LoopFlavour, body: impl FnOnce(&mut Self)) -> Type {
        let never = self.db.never();
        self.loops.push(LoopCtx {
            flavour,
            result: never,
        });
        body(self);
        self.loops
            .pop()
            .expect("the loop frame pushed above is the one popped here")
            .result
    }

    /// `break [expr]` (M8, §4.11). Diverges; type `Never`.
    ///
    /// The *value* is what the enclosing loop produces, so it is joined into
    /// that loop's running result (TY-21). A bare `break` contributes `Unit` —
    /// it leaves the loop with nothing — which is why `loop { break }` is `Unit`
    /// and why mixing the two spellings is a mismatch rather than a coincidence.
    fn infer_break(&mut self, b: &BreakExpr) -> Type {
        let value = b.value();
        let (value_ty, at) = match &value {
            Some(v) => (self.infer_expr(v), v.syntax().text_range()),
            None => (self.db.unit(), b.syntax().text_range()),
        };
        if self.check_in_loop(b.syntax().text_range(), "break") {
            self.record_break_value(value.is_some(), value_ty, at);
        }
        self.db.never()
    }

    /// Join a `break`'s value into the loop it leaves, or report why it cannot
    /// (TY-21). `carries_value` distinguishes a written `break e` from the bare
    /// `break` whose contribution is `Unit`: only the first is `Y017` in a
    /// `while`/`for`.
    fn record_break_value(&mut self, carries_value: bool, value_ty: Type, at: TextRange) {
        let Some(&LoopCtx {
            flavour,
            result: so_far,
        }) = self.loops.last()
        else {
            return;
        };
        if let LoopFlavour::Statement(keyword) = flavour {
            if carries_value {
                self.diagnostics
                    .push(crate::diagnostics::value_break_outside_loop_expression(
                        self.file_span(at),
                        keyword,
                    ));
            }
            return;
        }
        match self.db.join(so_far, value_ty) {
            Ok(joined) => {
                if let Some(ctx) = self.loops.last_mut() {
                    ctx.result = joined;
                }
            }
            Err(e) => self.diag_unify_hinted(self.file_span(at), e, "this `loop`"),
        }
    }

    /// `continue` (M8, §4.11). Diverges; type `Never`.
    fn infer_continue(&mut self, c: &ContinueExpr) -> Type {
        self.check_in_loop(c.syntax().text_range(), "continue");
        self.db.never()
    }

    /// Whether there is an enclosing loop to leave; `Y012` when there is not
    /// (TY-20). MIR's builder used to tolerate the absent loop context with an
    /// `if let`; the check belongs here, where the source position is.
    fn check_in_loop(&mut self, at: TextRange, keyword: &str) -> bool {
        if self.loops.is_empty() {
            self.diagnostics.push(crate::diagnostics::outside_loop(
                self.file_span(at),
                keyword,
            ));
            return false;
        }
        true
    }

    /// `return [expr]` (M8, §4.11). Diverges; type `Never`.
    /// `return e` — the value must be what the enclosing function produces
    /// (TY-18). It used to be inferred and thrown away, so
    /// `fn bad() -> Int { return "wrong"; 1 }` type-checked: the tail had the
    /// declared type and nothing else was asked.
    ///
    /// The expression itself is `Never`: it diverges, so it contributes nothing
    /// to whatever branch it sits in.
    fn infer_return(&mut self, r: &ReturnExpr) -> Type {
        let value = r.value();
        let (value_ty, at) = match &value {
            Some(v) => (self.infer_expr(v), v.syntax().text_range()),
            // A bare `return` produces `Unit`, which is what the function must
            // then be declared to return.
            None => (self.db.unit(), r.syntax().text_range()),
        };
        match self.fn_results.last() {
            Some(&result) => {
                if let Err(e) = self.db.unify(result, value_ty) {
                    self.diag_unify_hinted(self.file_span(at), e, "the function's return type");
                }
            }
            // Nothing to return *from* (TY-20).
            None => self
                .diagnostics
                .push(crate::diagnostics::return_outside_function(
                    self.file_span(r.syntax().text_range()),
                )),
        }
        self.db.never()
    }

    fn infer_call(&mut self, c: &CallExpr) -> Type {
        // Collect argument types.
        let arg_types: Vec<Type> = c
            .arg_list()
            .map(|a| self.collect_args(&a))
            .unwrap_or_default();
        // Postfix call on an arbitrary expression (`expr(args)`, M8 §4.10):
        // when there is no named callee, the callee is an expression (e.g.
        // `fs.get(0)`). Infer its type, unify it against `(arg_types) -> result`
        // (which pins a fresh closure param to a Func and checks the arity), and
        // return `result`. The lowered callee_expr carries this Func type, so the
        // HIR lowerer reads its result type for the call's type.
        if c.callee().is_none() {
            if let Some(callee_expr) = c.callee_expr() {
                let callee_ty = self.infer_expr(&callee_expr);
                let result = self.db.fresh_var();
                let expected = self.db.func(arg_types, result);
                if let Err(e) = self.db.unify(callee_ty, expected) {
                    let at = c.syntax().text_range();
                    self.diag_unify(self.file_span(at), e);
                }
                return result;
            }
        }
        // Resolve the callee to a function scheme and instantiate it.
        if let Some(callee) = c.callee() {
            if let Some(name_tok) = callee.name() {
                let range = name_tok.text_range();
                if let Some(resolved) = self.refs.get(&range).copied() {
                    let scheme = self
                        .names
                        .get(resolved.symbol)
                        .and_then(|s| s.scheme.clone());
                    if let Some(scheme) = scheme {
                        // Instantiating **at the call site** (F10): a
                        // requirement the callee's scheme carries is re-emitted
                        // against this call's types, and a failure is reported
                        // here rather than inside the generic body — where the
                        // very same expression is correct for every other
                        // instantiation.
                        let site = self.file_span(c.syntax().text_range());
                        let callee_ty = self.db.instantiate_at(&scheme, site);
                        // The callee name is a `PATH_EXPR` that nothing
                        // evaluates, and this instantiation is its type. It is
                        // deliberately *not* also written to `ref_types`: hover
                        // over a callee shows the binding's generalized scheme,
                        // which is what `hover_over_out_shows_polymorphic_scheme`
                        // pins, and a per-use entry would displace it.
                        self.record_node_type(callee.syntax(), callee_ty);
                        let result = self.db.fresh_var();
                        // Snapshot the concrete arg types before they are moved
                        // into the expected Func type — this is the call site's
                        // monomorphization witness (WS8, §13.6).
                        let arg_types_snapshot = arg_types.clone();
                        // A graph helper's first argument is the state the walk
                        // starts from, and the requirement below is about its
                        // type. Read before the witness is moved.
                        let state_arg = arg_types_snapshot.first().copied();
                        let expected = self.db.func(arg_types, result);
                        if let Err(e) = self.db.unify(callee_ty, expected) {
                            let at = c.syntax().text_range();
                            self.diag_unify(self.file_span(at), e);
                        }
                        // Record the witness: the callee symbol + the concrete
                        // arg types. After unification these pin the callee's
                        // quantified vars, so the mono pass can instantiate the
                        // scheme with them. (Captured even for monomorphic
                        // callees — cheap, and keeps the mono pass uniform.)
                        self.call_sites.insert(
                            range,
                            crate::CallSite {
                                callee: resolved.symbol,
                                arg_types: arg_types_snapshot,
                                // The result is a witness too: a callee whose
                                // quantified variable appears only in its result
                                // (`fn empty() { Vec() }`) has nothing to
                                // specialize from otherwise (MONO-02).
                                result,
                            },
                        );
                        // A graph helper's state type has to be a `Set` element
                        // and a `Map` key, because the walk remembers where it
                        // has been (ADR-060).
                        self.require_graph_state(
                            resolved.symbol,
                            state_arg,
                            c.syntax().text_range(),
                        );
                        // For the common builtin `out(x)`, the scheme is
                        // forall T. (T) -> Unit, so the result unifies to Unit.
                        if self.is_builtin(resolved.symbol, "out") {
                            return self.db.unit();
                        }
                        if self.is_builtin(resolved.symbol, "panic") {
                            return self.db.never();
                        }
                        return result;
                    }
                    // Builtin with no scheme yet (shouldn't happen for out/panic,
                    // but be defensive): fall through to a fresh var.
                }
            }
        }
        self.db.fresh_var()
    }

    fn collect_args(&mut self, args: &ArgList) -> Vec<Type> {
        args.args().map(|a| self.infer_expr(&a)).collect()
    }

    /// Infer `receiver.method(args)` (M5, §16.2). Resolves the method against
    /// the built-in catalog by receiver type + name + arity, unifies the
    /// element-type variable with the receiver's element type, checks arg types,
    /// and returns the result type. Records the method-name range in
    /// `ref_types` for hover.
    fn infer_method_call(&mut self, m: &MethodCallExpr) -> Type {
        // Infer the receiver's type.
        let receiver_ty = match m.receiver() {
            Some(r) => self.infer_expr(&r),
            None => self.db.fresh_var(),
        };
        let name = m
            .method_name()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        // Collect the argument expressions (do NOT infer them yet — bidirectional
        // inference below pushes each arg's expected param type into it before
        // inferring, so a closure arg whose param type is the element type gets
        // pinned). Arity is the arg count.
        let arg_exprs: Vec<Expr> = m.arg_list().map(|a| a.args().collect()).unwrap_or_default();
        let arity = arg_exprs.len();

        // Look up the method in the catalog via the ADR-010 bridge.
        let hits = crate::catalog::lookup(&self.db, self.catalog, receiver_ty, &name, arity);
        let Some(entry) = hits.first().copied() else {
            // Infer the args anyway (for nested diagnostics), and the result is
            // a fresh var either way.
            let arg_types: Vec<Type> = arg_exprs.iter().map(|a| self.infer_expr(a)).collect();
            let result = self.db.fresh_var();
            // Two different situations arrive here and only one of them is a
            // mistake. A **concrete** receiver with no matching entry has no such
            // method, and the HIR lowerer reports it (`Y110`; it has the
            // method-name span). A receiver that is still a **variable** has not
            // failed anything: nothing has said what it is yet, and
            // `catalog::lookup` cannot answer about a type that does not exist.
            // That one becomes a requirement on the channel, answered when the
            // program pins the receiver — which is the whole of TY-30, and what
            // makes §5.2's `fn total(values) { values.sum() }` infer.
            if self.db.var_id_of(self.db.follow(receiver_ty)).is_some() {
                let at = m
                    .method_name()
                    .map(|t| t.text_range())
                    .unwrap_or_else(|| m.syntax().text_range());
                self.require_method(receiver_ty, name, arg_types, result, at);
            }
            return result;
        };

        // Bidirectional inference (M8, §3): instantiate the method's full
        // signature (params + result) with ONE shared name map, so repeated
        // `Var(name)` occurrences in the catalog entry are the same type
        // variable (fold's `Acc` appears in the init param, both closure params,
        // and the result — they must be one type). The receiver's element type is
        // also a shared `Var("T")` in most combinators; unify the receiver
        // against the entry's receiver pattern (instantiated from the same map)
        // to pin `T` BEFORE inferring the arguments — so an argument closure
        // whose param is the element type (e.g. `|inner| inner.len()` over
        // `Vec[Vec[Int]]`) gets pinned and its body's method calls resolve.
        // Catalog `params` are exactly the user arguments (the receiver is
        // separate, in `entry.receiver`).
        let mut names = HashMap::new();
        let receiver_param =
            crate::lower::pattern_to_type_named(&mut self.db, &entry.receiver, &mut names);
        let param_tys: Vec<Type> = entry
            .params
            .iter()
            .map(|p| crate::lower::pattern_to_type_named(&mut self.db, p, &mut names))
            .collect();
        let result_ty =
            crate::lower::pattern_to_type_named(&mut self.db, &entry.result, &mut names);
        // Unify the receiver against its pattern. This pins the element type `T`.
        let _ = self.db.unify(receiver_param, receiver_ty);
        // Infer each argument with its expected param type pushed down, unifying
        // it against its param IMMEDIATELY so a shared variable (e.g. fold's
        // `Acc`, which appears in the init arg and the closure's signature)
        // propagates to subsequent args before they are inferred. For a closure
        // argument whose expected type is a Func, the closure's params are pinned
        // to the Func's param types before its body is inferred (closing the §3
        // gaps).
        let mut arg_types: Vec<Type> = Vec::with_capacity(arg_exprs.len());
        for (i, arg) in arg_exprs.iter().enumerate() {
            let expected = param_tys.get(i).copied();
            let at = match expected {
                Some(et) => self.infer_expr_expected(arg, et),
                None => self.infer_expr(arg),
            };
            // Unify now (not deferred) so shared vars pin before the next arg.
            if let Some(pt) = param_tys.get(i) {
                if let Err(e) = self.db.unify(*pt, at) {
                    let at_range = arg.syntax().text_range();
                    self.diag_unify(self.file_span(at_range), e);
                }
            }
            arg_types.push(at);
        }
        let name_range = m
            .method_name()
            .map(|t| t.text_range())
            .unwrap_or_else(|| m.syntax().text_range());
        // What the entry declares about its own type variables (TY-31). Applied
        // after the receiver and the arguments have unified, so a bound on `T`
        // is asked about the type the call site actually chose.
        self.apply_bounds(entry, &names, name_range);
        // A `Map`/`Set`/`Counter` key must be findable after it is stored, and a
        // heap element must be orderable (TY-32, D4). Required at the method
        // call because that is where a program actually puts a value into one —
        // and required *after* the arguments have unified, so `m.insert(key, 1)`
        // has pinned `K` to the key's type by now.
        self.require_collection_invariants(receiver_ty, name_range);
        // Record the resolved method at its name token (HIR-02). Lowering reads
        // the entry rather than repeating the catalog lookup against a receiver
        // type it derived itself, and hover reads the result.
        if let Some(tok) = m.method_name() {
            self.method_refs.insert(
                tok.text_range(),
                crate::MethodRef {
                    entry,
                    receiver: receiver_ty,
                    result: result_ty,
                },
            );
        }
        result_ty
    }

    fn is_builtin(&self, id: SymbolId, name: &str) -> bool {
        self.names
            .get(id)
            .is_some_and(|s| s.kind == SymbolKind::Builtin && s.name == name)
    }

    // --- type resolution ---------------------------------------------------

    /// Resolve a written type annotation to a [`Type`]. Returns `None` if the
    /// annotation names something with no type (already reported by resolution
    /// as `N002`/`N003`); the caller then falls back to inference.
    ///
    /// The work lives in [`crate::decl::Annotations`], which the declaration
    /// pass uses too — one answer to "what type is this written down as", for
    /// the pass that builds the environment and the pass that reads it.
    fn resolve_type(&mut self, ty: &praxis_ast::TypeRef) -> Option<Type> {
        crate::decl::Annotations::new(
            self.file,
            &mut self.db,
            &self.type_env,
            &self.type_refs,
            &mut self.diagnostics,
        )
        .resolve(ty)
    }
}

/// Whether an expression is a *syntactic value* for the HM value restriction
/// (§5.3). `let x = <value>` may generalize `x`'s type; `let x = <expansive>`
/// (a call, method call, block, `read`, …) is left monomorphic so its type
/// variables are shared across uses instead of instantiated fresh per reference
/// — the standard fix for the `let r = ref []` / `let v = Vec()` generalization
/// gap. Recurses through `Paren` (a transparent wrapper) and `Tuple` of values
/// (a value iff every element is). An explicit type annotation on the `let`
/// overrides the restriction, handled by the caller.
fn is_syntactic_value(e: &Expr) -> bool {
    match e {
        // Pure values.
        Expr::Literal(_) | Expr::Path(_) | Expr::Closure(_) => true,
        // A paren is transparent: `(v)` is a value iff `v` is.
        Expr::Paren(p) => p
            .expr()
            .map(|inner| is_syntactic_value(&inner))
            .unwrap_or(false),
        // A tuple is a value iff every element is (classic ML).
        Expr::Tuple(t) => t.elements().all(|el| is_syntactic_value(&el)),
        // Everything else is expansive: calls (`Vec()`), method calls, blocks,
        // control flow, `read`/`parse`, record literals, field access, matches,
        // binary/unary ops, and parse errors. Conservative but sound.
        _ => false,
    }
}

/// True iff `operand` is a float literal (`3.14`), possibly wrapped in
/// parentheses. Used by arithmetic inference under the strict per-literal model
/// to decide whether a binary op resolves to Float or Int (§4.12). Only a
/// syntactic float literal triggers the Float path; a float-typed *variable*
/// or expression unifies against whichever target the literal side picks, so
/// `1.5 + x` infers `x : Float` (and `1 + x` keeps `x : Int`).
fn operand_is_float_literal(operand: &Option<Expr>) -> bool {
    let e = match operand {
        Some(e) => e,
        None => return false,
    };
    match e {
        Expr::Literal(lit) => lit
            .token()
            .map(|t| t.kind() == SyntaxKind::FloatLit)
            .unwrap_or(false),
        Expr::Paren(p) => p
            .expr()
            .map(|inner| operand_is_float_literal(&Some(inner)))
            .unwrap_or(false),
        _ => false,
    }
}

/// True iff `t` follows to a concrete `ScalarType::Float`. Used by arithmetic
/// inference to pick the Float path when an operand is a float-typed
/// *expression* (e.g. a method call like `16.0.sqrt()`), not just a literal.
fn is_float_scalar(db: &TypeDb, t: Type) -> bool {
    matches!(
        db.data(db.follow(t)),
        praxis_types::TypeData::Scalar(ScalarType::Float)
    )
}

/// How to name a binding kind in a diagnostic, in the words the source uses.
fn describe_binding(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Let => "a `let` binding",
        SymbolKind::Var => "a `var` binding",
        SymbolKind::Fn => "a function",
        SymbolKind::Param => "a parameter",
        SymbolKind::Builtin | SymbolKind::BuiltinType => "a built-in name",
        SymbolKind::Struct => "a struct type",
        SymbolKind::Enum => "an enum type",
        SymbolKind::EnumVariant => "an enum variant",
    }
}
