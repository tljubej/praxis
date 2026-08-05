//! Type inference (§5, ADR-008).
//!
//! Walks the name-resolved tree and assigns a [`Type`] to every expression and
//! a [`Scheme`] to every binding. Uses the level-based let-generalization from
//! `praxis-types` (ADR-008):
//!
//! - `var` RHS is inferred at an inner level, then **generalized** at the outer
//!   level (so `var id = fn(x){x}` becomes `forall T. (T) -> T`).
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
    ElseBranch, Expr, ExprStmt, FieldExpr, FnItem, ForExpr, IfExpr, Literal, LoopExpr,
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
    arity_mismatch, infinite_type, not_equatable, not_hashable, not_numeric, not_orderable,
    type_mismatch, type_mismatch_with_help,
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
    /// passes (M4 lowering) can map a `var`/`fn`/param name to its symbol
    /// via the declaration range (unambiguous under shadowing).
    pub decls: HashMap<TextRange, SymbolId>,
    /// Each call site, keyed by the callee name token's range → the callee
    /// symbol and concrete arg types (the monomorphization witness, WS8 §13.6).
    pub call_sites: HashMap<TextRange, crate::CallSite>,
    /// Every inferred expression's type, keyed by its node (F15).
    pub expr_types: HashMap<crate::NodeKey, Type>,
    /// Each method call, keyed by the method-name token's range (F15/HIR-02).
    pub method_refs: HashMap<TextRange, crate::MethodRef>,
    /// The retained parser AST and per-node types of each `read`/`parse` body
    /// (ADR-098), in the order inference reached them.
    pub parser_exprs: Vec<crate::ParserIndex>,
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
        parser_exprs: Vec::new(),
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
        parser_exprs: inferer.parser_exprs,
        diagnostics,
    }
}

/// How a catalog dispatch reports a **concrete** receiver with no matching row
/// (REP-16).
///
/// The count is carried beside the builder because it is not the argument count:
/// a subscript store's arguments are the indices *and* the value, and it is the
/// index count the message has to name, or `v[0] = 5` reports "2 index(es)".
#[derive(Clone, Copy)]
struct UnresolvedReport {
    build: fn(FileSpan, &str, usize) -> Diagnostic,
    indices: usize,
}

/// The report a deferred [`Capability::HasMethod`] owes if it turns out to be a
/// subscript on a receiver that has none, or `None` for an ordinary method
/// (whose miss is `Y110`, reported by inference at both doors — ADR-093).
///
/// `args` is the requirement's argument count, which for a store is the indices
/// *and* the value.
fn subscript_unresolved_report(name: &str, args: usize) -> Option<UnresolvedReport> {
    if name == praxis_stdlib::catalog::INDEX_READ {
        return Some(UnresolvedReport {
            build: crate::diagnostics::not_indexable,
            indices: args,
        });
    }
    if name == praxis_stdlib::catalog::INDEX_STORE {
        return Some(UnresolvedReport {
            build: crate::diagnostics::not_index_assignable,
            indices: args.saturating_sub(1),
        });
    }
    // The updating stores (REP-21). Their own message, because a receiver that
    // has a plain store and no `min=` is the case worth being exact about.
    if name == praxis_stdlib::catalog::INDEX_STORE_MIN {
        return Some(UnresolvedReport {
            build: crate::diagnostics::not_index_min_updatable,
            indices: args.saturating_sub(1),
        });
    }
    if name == praxis_stdlib::catalog::INDEX_STORE_MAX {
        return Some(UnresolvedReport {
            build: crate::diagnostics::not_index_max_updatable,
            indices: args.saturating_sub(1),
        });
    }
    None
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
    /// ADR-098's spanned parser index, appended to by `synthesize_parser_type`.
    parser_exprs: Vec<crate::ParserIndex>,
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
    /// A receiver that turns out **not** to have the method is reported here
    /// (ADR-093). It used to be left to lowering, on the argument that lowering
    /// has the method-name span — but so does this pass, `c.at` *is* the name
    /// token's range, and lowering is the one place `praxis check` never runs.
    /// The consequence was `fn f(x) { x.nope() }` / `out(f(3))`: `check` exit 0
    /// and silent, `run` exit 1 with a `Y110`. Reporting here is not "the same
    /// mistake twice" because lowering no longer reports at all.
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
            // A **subscript** requirement that resolves to a receiver with no
            // such row gets the subscript wording: there is no method name to
            // report about, and `fn first(m, k) { m[k] }` applied to a `Set`
            // should say "cannot be indexed", not "no method `[]`".
            if let Some(report) = subscript_unresolved_report(&name, params.len()) {
                let rendered = self.db.render(receiver_ty);
                let at = range_of(c.at);
                self.diagnostics.push((report.build)(
                    self.file_span(at),
                    &rendered,
                    report.indices,
                ));
                return;
            }
            // An ordinary method gets `Y110`, from here, at `praxis check` time
            // (ADR-093). The receiver is concrete by construction — this is the
            // discharge path, and `take_dischargeable` only hands back a
            // constraint whose variable has resolved — so the message can name
            // it, which is what lowering's could never do.
            self.report_cap_failure(&c.cap, receiver_ty, c.report_at(), c.origin_note());
            return;
        };
        // The entry's patterns, instantiated through one shared name map so a
        // `Var("T")` repeated across receiver, params and result is one variable
        // — the same discipline `infer_method_call` uses when it can resolve at
        // the call site.
        let mut names = HashMap::new();
        let at = c.report_at();
        self.bind_receiver(entry, receiver_ty, &mut names, range_of(c.at));
        let param_tys: Vec<Type> = entry
            .params
            .iter()
            .map(|p| crate::lower::pattern_to_type_named(&mut self.db, p, &mut names))
            .collect();
        let result_ty =
            crate::lower::pattern_to_type_named(&mut self.db, &entry.result, &mut names);
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

    /// Require `receiver` to have a field `name` of type `ty` — the deferred half
    /// of a field read (REP-28).
    ///
    /// This is TY-30's shape at the third door. A field read whose receiver was
    /// still a variable constrained **nothing**: `infer_field_get` answered a fresh
    /// variable and recorded no requirement, so the parameter was generalized and
    /// the read failed later. §4.9's own example is the reproduction —
    /// `struct P { x: Int, y: Int }` / `fn dist(a) -> Int { a.x + a.y }` /
    /// `out(dist(P { x: 1, y: 2 }))` passed `praxis check` and then failed under
    /// `praxis run` with `Y112` "no field `x` on this type".
    ///
    /// Going through [`require_cap`](Self::require_cap) rather than deciding here
    /// is the discipline the progress doc names: a predicate called directly is
    /// TY-29 by another name — the answer is thrown away at generalization and
    /// never re-asked at the use site.
    ///
    /// **The receiver and the field's type are pinned to the declaration group's
    /// level**, for ADR-057 Decision 5's reason and ADR-062 Decision 2's: there is
    /// one lowered body per source function, and monomorphization substitutes from
    /// the call site's argument types without running this channel. `lower_field_get`
    /// reads the receiver's *record definition* to get the field's index, so one
    /// field-read site carries one record type. Two call sites that disagree about
    /// it are a disagreement about the function's signature, exactly as two
    /// receivers at one method call site are.
    fn require_field(&mut self, receiver: Type, name: String, ty: Type, at: TextRange) {
        let site = self.decl_site;
        self.db.pin_to_level(receiver, site);
        self.db.pin_to_level(ty, site);
        self.require_cap(receiver, Capability::HasField { name, ty }, at);
    }

    /// Answer a `HasField` requirement whose receiver has since resolved: ask that
    /// record what the field holds and **unify** it with the type the read handed
    /// back (REP-28).
    ///
    /// The third capability discharged by *producing* rather than by checking, and
    /// for the same reason as the other two: the deferred read returned a bare
    /// variable, and this is the only thing that ever says what it holds.
    ///
    /// A receiver that turns out to have **no such field** is reported here, and
    /// that is a correction rather than a design choice. Leaving it to lowering
    /// is what `HasMethod` used to do with `Y110`, and it never really worked:
    /// lowering runs at `run` and not at `check`, so `praxis check` passed and
    /// `praxis run` failed — the exact divergence REP-28 exists to close, and
    /// which ADR-093 has since closed at the method door too. Both capabilities
    /// now report from this pass, which is the one both commands run.
    ///
    /// So the failure goes through `report_cap_failure`, exactly as
    /// [`resolve_deferred_iterable`](Self::resolve_deferred_iterable)'s does, with
    /// the requirement's own span as the note.
    fn resolve_deferred_field(&mut self, c: &Constraint, name: &str, ty: Type) {
        let receiver = self.db.follow(c.var_type());
        let field = match self.db.data(receiver) {
            praxis_types::TypeData::Record { def, args } => {
                let (def, args) = (*def, args.to_vec());
                self.db.record_field_of(def, &args, name).map(|(_, t)| t)
            }
            _ => None,
        };
        let Some(field) = field else {
            self.report_cap_failure(&c.cap, receiver, c.report_at(), c.origin_note());
            return;
        };
        if let Err(e) = self.db.unify(ty, field) {
            let mut diag = self.unify_diagnostic(c.report_at(), e);
            if let Some(origin) = c.origin_note() {
                diag = diag.with_note(origin, "this is the operation that requires it");
            }
            self.diagnostics.push(diag);
        }
    }

    /// Answer an `Iterable` requirement whose receiver has since resolved: get
    /// the item that receiver actually yields, and **unify** it with the one the
    /// constraint carries (REP-04).
    ///
    /// This is the other end of REP-03. `capability::check` answers iterability
    /// as a yes/no, which is all it can do — its failure shape is "the offending
    /// type" and "iterates, but not at that element type" is a *mismatch*. So a
    /// constraint that discharged at a differently-itemed iterable was silently
    /// accepted:
    ///
    /// ```praxis
    /// fn total(r) { var t = 0
    ///   for i in r { t = t + i }        // requires Iterable { item = Int }
    ///   t }
    /// fn main() -> Int { total(names) } // names: Vec[Text] — accepted, before
    /// ```
    ///
    /// Unifying is also what makes the requirement *productive* rather than
    /// merely permissive: when the item is the fresh variable `iter_item` minted
    /// for an unresolved iterator, this is the only thing that ever says what the
    /// loop variable holds — the same role `resolve_deferred_method` plays for
    /// `HasMethod`, and the reason both are discharged by resolving rather than by
    /// checking.
    ///
    /// A receiver that is not iterable **at all** is still the channel's to
    /// report, unchanged: `Y005` at the use site with the `for` as the note.
    fn resolve_deferred_iterable(&mut self, c: &Constraint, item: Type) {
        let receiver = self.db.follow(c.var_type());
        let Some(yielded) = crate::capability::iter_item(&mut self.db, receiver) else {
            let offender = self.db.follow(receiver);
            self.report_cap_failure(&c.cap, offender, c.report_at(), c.origin_note());
            return;
        };
        // `item` is what the body requires and `yielded` is what this receiver
        // provides, so the mismatch reads "expected `Int`, found `Text`".
        if let Err(e) = self.db.unify(item, yielded) {
            let mut diag = self.unify_diagnostic(c.report_at(), e);
            if let Some(origin) = c.origin_note() {
                diag = diag.with_note(origin, "this is the operation that requires it");
            }
            self.diagnostics.push(diag);
        }
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
                // Through `require_cap`, not through `capability::check`
                // directly, and that is the whole difference between the two
                // arms. `var v = Vec()` mints `Vec[?T]` — a *concrete* receiver
                // with an *open* element — so `v.sorted()` resolves its row and
                // reaches here before any `push` has said what `?T` is.
                // `capability::check` answers **yes** to every unresolved
                // variable by design, so deciding now would accept the program
                // and nothing would ever ask again; the channel asks when the
                // `push` two lines later pins it. A scalar bound *pins*; a kind
                // bound *asks*.
                praxis_stdlib::Bound::Kind(kind) => {
                    self.require_cap(ty, Capability::Kind(kind), at);
                }
            }
        }
    }

    /// Relate a resolved catalog entry's receiver pattern to the receiver the
    /// call site actually has, sharing `names` with the row's parameters and
    /// result (ADR-127 decision 1).
    ///
    /// **One function because there are two doors.** `infer_catalog_call`
    /// resolves at the call site and `resolve_deferred_method` resolves when the
    /// channel says what the receiver is — `fn top(v) { v.map(f) }` goes through
    /// the second — and the eight lines this replaces were written twice. A
    /// second copy is a second place for the `Iterable` path to be missing.
    ///
    /// The two paths differ in what is unified:
    ///
    /// - An ordinary receiver pattern is **instantiated and unified**. That is
    ///   what pins `T` in `Vec[T].push(T)`. The result is discarded because
    ///   `lookup` already matched the shape; what can still fail is a *bound*,
    ///   and `apply_bounds` reports that.
    /// - A [`TypePattern::Iterable`] receiver is **not unified at all**: it
    ///   accepts ten different constructors, and unifying against any one of
    ///   them pins the other nine out. What is unified is the row's `item`,
    ///   against `capability::iter_item` — the `for` loop's own answer to "what
    ///   does this yield". That is what makes `Iterable { item: (K, V) }` mean
    ///   "a `Map` or a `Counter`", and it is the *only* place `[1, 2].to_map()`
    ///   can be reported: `lookup` matched the row, so the failure has to be a
    ///   type error at the method name rather than a missing method.
    fn bind_receiver(
        &mut self,
        entry: &praxis_stdlib::MethodEntry,
        receiver_ty: Type,
        names: &mut HashMap<String, Type>,
        at: TextRange,
    ) {
        let praxis_stdlib::TypePattern::Iterable { item } = &entry.receiver else {
            let receiver_param =
                crate::lower::pattern_to_type_named(&mut self.db, &entry.receiver, names);
            let _ = self.db.unify(receiver_param, receiver_ty);
            return;
        };
        // `lookup` accepted this receiver, which means it is one of the ten —
        // and every one of the ten is iterable, so `iter_item` has an answer.
        // A `None` here is the two tables disagreeing about what the pipeline
        // walks, which is a catalog bug and not a program's; leaving the item
        // unpinned instead would be a wrong inference in silence.
        let item_ty = crate::capability::iter_item(&mut self.db, receiver_ty)
            .expect("an `Iterable` row matched a receiver that `iter_item` refuses");
        let item_param = crate::lower::pattern_to_type_named(&mut self.db, item, names);
        if let Err(e) = self.db.unify(item_param, item_ty) {
            self.diag_unify(self.file_span(at), e);
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
    /// program pins it — which is the common shape, since `var m = Map()` mints
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
            // why a failure is reported there and not left to lowering.
            if matches!(c.cap, Capability::HasMethod { .. }) {
                self.resolve_deferred_method(&c);
                continue;
            }
            // `Iterable` is the second: it *produces* the item type, so it is
            // discharged by unifying that item rather than by asking whether the
            // receiver iterates at all (REP-04).
            if let Capability::Iterable { item } = c.cap {
                self.resolve_deferred_iterable(&c, item);
                continue;
            }
            // `HasField` is the third, and it produces the field's type: the
            // deferred read handed back a bare variable and nothing else ever
            // says what it holds (REP-28).
            if let Capability::HasField { ref name, ty } = c.cap {
                let name = name.clone();
                self.resolve_deferred_field(&c, &name, ty);
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
            // Live, and from the deferred door (ADR-093). It used to be dead:
            // `require_method` only ever defers a receiver that is a variable, so
            // nothing failed here immediately, and
            // [`Inferer::resolve_deferred_method`] returned silently on a miss
            // because lowering was said to own `Y110`. Lowering runs at `run` and
            // not at `check`, so that made every missing method a clean `praxis
            // check` followed by a failing `praxis run` — the divergence REP-28
            // closed at `HasField` and ADR-093 closes here.
            Capability::HasMethod { name, params, .. } => {
                crate::diagnostics::unknown_method(at, name, Some(&rendered), params.len())
            }
            // Live, and from both doors — unlike `HasMethod`'s arm above.
            // `infer_field_get` requires the field of *every* receiver it cannot
            // answer itself, so a concrete one fails in `require_cap_as` and is
            // reported at the read; a deferred one that resolves to a non-record,
            // or to a record without the field, is reported by
            // [`Inferer::resolve_deferred_field`]. Both report at `praxis check`
            // time, which is the point: lowering alone is a report `check` never
            // runs.
            Capability::HasField { name, .. } => {
                crate::diagnostics::unknown_field(at, name, &rendered)
            }
        };
        if let Some(origin) = origin {
            diag = diag.with_note(origin, "this is the operation that requires it");
        }
        self.diagnostics.push(diag);
    }

    fn diag_unify(&mut self, at: FileSpan, err: UnifyError) {
        let diag = self.unify_diagnostic(at, err);
        self.diagnostics.push(diag);
    }

    /// The diagnostic for `err`, **unpushed** — for the one caller that has a
    /// second span to attach before it goes out.
    ///
    /// A deferred `Iterable` reports at the use site and explains itself at the
    /// `for` (ADR-057 decision 2), and `Diagnostic::with_note` is what adds the
    /// second span; a helper that pushes cannot be given one.
    fn unify_diagnostic(&self, at: FileSpan, err: UnifyError) -> Diagnostic {
        match err {
            UnifyError::Mismatch { expected, found } => {
                let e = self.db.render(expected);
                let f = self.db.render(found);
                type_mismatch(at, &e, &f)
            }
            UnifyError::Arity { expected, found } => arity_mismatch(at, expected, found),
            UnifyError::Occurs { .. } => infinite_type(at),
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
            UnifyError::Arity { expected, found } => {
                // No hint: the message already names both counts, and ADR-089
                // means there is no other signature to suggest.
                self.diagnostics.push(arity_mismatch(at, expected, found));
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
        if let Some(var_) = VarStmt::cast(node.clone()) {
            self.infer_var(&var_);
        } else if let Some(fn_) = FnItem::cast(node.clone()) {
            self.infer_fn(&fn_);
        } else if let Some(assign) = AssignStmt::cast(node.clone()) {
            self.infer_assign(&assign);
        } else if let Some(assign) = praxis_ast::PlaceAssignStmt::cast(node.clone()) {
            self.infer_place_assign(&assign);
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

    fn infer_var(&mut self, stmt: &VarStmt) {
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
        // Generalize the binding (§5.3) under **two** restrictions.
        //
        // The first is the HM value restriction: only a syntactic value
        // generalizes. An expansive RHS (a call like `Vec()`, a method call, a
        // block, `read`, …) is left monomorphic so its type variables are shared
        // across uses rather than instantiated fresh per reference. Without
        // this, `var v = Vec(); v.push(inner); v.map(...)` gives
        // `v : forall T. Vec[T]`, and the push's element-type pinning never
        // reaches the map (Gap B). An explicit type annotation overrides it (the
        // user has pinned the type by writing it).
        //
        // The second is [`Symbol::reassigned`] (ADR-125), which is what the
        // `var` split used to decide. Assignment *instantiates* a scheme
        // and unifies the copy, so a generalized scheme is not constrained by
        // being written: `var f = |x| x` is a syntactic value and generalizes to
        // `forall T. T -> T`, and `f = |n| n + 1` would leave it there — so
        // `f("s")` would type-check and call the `Int` closure. A binding
        // nothing writes cannot reach that state, which is exactly the set the
        // old `var` named.
        let expansive = rhs.as_ref().is_some_and(|e| !is_syntactic_value(e));
        let reassigned = self.binding_is_reassigned(stmt.name());
        let scheme = if reassigned || (expansive && annot.is_none()) {
            Scheme::monotype(body_ty)
        } else {
            self.db.generalize(body_ty)
        };
        self.attach_scheme(stmt.name(), scheme);
    }

    /// Whether the declaration at `name_tok` is one that something reassigns.
    /// Reads the flag name resolution set (see [`crate::Symbol::reassigned`]).
    /// A malformed binding with no name token has no symbol, and nothing can
    /// write what has no name.
    fn binding_is_reassigned(&self, name_tok: Option<praxis_syntax::SyntaxToken>) -> bool {
        name_tok
            .and_then(|t| self.decls.get(&t.text_range()).copied())
            .and_then(|id| self.names.get(id))
            .is_some_and(|sym| sym.reassigned)
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
        // after this belongs to a variable nothing pinned — and that is now safe
        // to drop, which it was not before ADR-093. A `HasMethod` requirement is
        // only ever *made* when the catalog holds that name at that arity
        // (`infer_catalog_call` reports the ones it does not, on the spot), so a
        // pending one means the program never said which of those receivers it
        // meant — §5.2's uncalled `fn total(values) { values.sum() }` — and
        // monomorphization drops the uncalled polymorphic original rather than
        // lowering it. Before, the pending set silently swallowed `x.nope()` and
        // lowering met it at `run`.
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
        // Vec[Int] { … var v = build(n - 1); v.push(n) … }` could not resolve
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
            return ty;
        }
        // A **wildcard** parameter (REP-32). It names nothing, so there is nothing
        // to look up — but its slot is a real slot and needs the parameter's type,
        // or lowering reads a symbol with no scheme for a parameter that is
        // certainly there.
        if let Some(tok) = p.wildcard() {
            if let Some(&id) = self.decls.get(&tok.text_range()) {
                if let Some(sym) = self.names.get_mut(id) {
                    sym.scheme = Some(Scheme::monotype(ty));
                }
            }
            return ty;
        }
        // A **destructuring** closure parameter (REP-29). The argument's own slot
        // takes the parameter type, and the pattern is checked against it by the
        // same walk a match arm and a `for` binding go through — so each name comes
        // out at its component's type rather than at the whole argument's.
        if let Some(pat) = p.pattern() {
            let range = pat.syntax().text_range();
            if let Some(&id) = self.decls.get(&range) {
                if let Some(sym) = self.names.get_mut(id) {
                    sym.scheme = Some(Scheme::monotype(ty));
                }
            }
            self.infer_pattern(&pat, ty);
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
        // Every binding is assignable (ADR-125): a `var`, a parameter, a `for`
        // binding and a pattern binding are all just names bound to values, and
        // the only thing assignment has to respect is the binding's *type*. What
        // an assignment used to be checked for — being a `var` — was the whole
        // of `Y009`, and that code is retired.
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
        // …except `+=` on a `Text` target, which is concatenation and needs no
        // number (ADR-085). `s += "x"` is `s = s + "x"`, so what it requires is
        // what `+` requires. The other four compounds still need a numeric
        // target, and so does `+=` on anything that is not `Text` — a `Text`
        // target is *excused* here, not exempted from being checked.
        let text_concat_assign = stmt.op().is_some_and(|t| t.kind() == SyntaxKind::PLUS_EQ)
            && is_text_scalar(&self.db, existing);
        if compound && !text_concat_assign {
            self.require_cap_as(
                existing,
                Capability::Kind(CapKind::Numeric),
                at,
                Some(crate::diagnostics::compound_assign_non_numeric),
            );
        }
        // `%` is defined for integers only (§4.12), and a compound `%=` is that
        // same operation — so it is the same `Y016`. The numeric requirement
        // above does not cover it: a `Float` *is* numeric, so `f %= 2.0` passed
        // `praxis check` while `f % 2.0` was refused, and MIR then had no float
        // remainder to lower it to (REP-64).
        self.reject_float_remainder(
            stmt.op().map(|t| t.kind()) == Some(SyntaxKind::PERCENT_EQ),
            existing,
            at,
            "%=",
        );
    }

    /// Report `Y016` when a remainder operator is applied to a `Float` (§4.12).
    ///
    /// Shared by the binary `%` and both compound `%=` forms — the statement's
    /// and the subscript store's — because it is one rule and three spellings.
    /// An operand still under inference answers `false` here and is left alone:
    /// pinning it would narrow every unannotated numeric parameter, which is the
    /// reason the numeric requirement beside it goes through the deferred
    /// channel (TY-31).
    fn reject_float_remainder(
        &mut self,
        is_remainder: bool,
        operand: Type,
        at: TextRange,
        spelling: &str,
    ) {
        if is_remainder && is_float_scalar(&self.db, operand) {
            self.diagnostics
                .push(crate::diagnostics::operator_not_defined(
                    self.file_span(at),
                    spelling,
                    "Float",
                ));
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
            Expr::List(l) => self.infer_list(l),
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
            Expr::TupleIndex(t) => self.infer_tuple_index(t),
            Expr::Index(i) => self.infer_index(i),
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
    ///
    /// **The head has to be a `struct`, and nothing asked (REP-26).** A head that
    /// resolved to anything else kept that thing's type and lowered to nothing:
    /// `var x = 1` / `var p = x { a: 1 }` passed `praxis check`, printed `Unit`,
    /// and `p + 1` printed a raw pointer. That is REP-01's shape — a program the
    /// checker accepts whose value has no representation — so the report is made
    /// **here**, in inference, where `praxis check` sees it (REP-12).
    ///
    /// It is the symbol's **kind** that decides, which is REP-22's rule at another
    /// door: the head names a declaration, and `SymbolKind::Struct` is the only one
    /// a `{ … }` can build. Deciding on the head's *type* instead would let an
    /// unresolved one (a parameter, whose type is a variable) look like a failure
    /// and would say nothing useful about an `enum`, which is a perfectly good type
    /// with no fields to initialize.
    fn infer_record_lit(&mut self, r: &RecordLitExpr) -> Type {
        // The literal's head is an ordinary name reference, so resolution
        // already decided which symbol it names — including under shadowing,
        // where a scope lookup here would answer differently.
        let resolved_head = r
            .name()
            .and_then(|p| p.name())
            .and_then(|tok| self.refs.get(&tok.text_range()).copied());
        let struct_ty = resolved_head.and_then(|resolved| self.type_env.ty(resolved.symbol));
        // The head is a `PATH_EXPR` nothing evaluates; the type it names is its
        // type, and a head that names nothing is a fresh variable like any other
        // unresolved expression.
        if let Some(head) = r.name() {
            let head_ty = struct_ty.unwrap_or_else(|| self.db.fresh_var());
            self.record_node_type(head.syntax(), head_ty);
        }
        // A head that is not a `struct` (REP-26). Reported before the type is
        // consulted, so an `enum`, a `fn`, a builtin and a binding all answer the
        // same way and all of them stop here.
        if let Some(resolved) = resolved_head {
            if let Some(sym) = self.names.get(resolved.symbol) {
                if sym.kind != SymbolKind::Struct {
                    let at = r
                        .name()
                        .and_then(|p| p.name())
                        .map(|tok| tok.text_range())
                        .unwrap_or_else(|| r.syntax().text_range());
                    let kind = describe_binding(sym.kind);
                    let name = sym.name.clone();
                    self.diagnostics
                        .push(crate::diagnostics::not_a_record_literal_head(
                            self.file_span(at),
                            &name,
                            kind,
                        ));
                    return self.infer_record_lit_fields_only(r);
                }
            }
        }
        let Some(struct_ty) = struct_ty else {
            // Unknown struct: infer each field for diagnostics, return a fresh var.
            return self.infer_record_lit_fields_only(r);
        };
        // Get the record def to look up declared field types. A `struct` symbol
        // whose type is not a record is a declaration that failed to register
        // (`N006`); it has been reported and there is nothing here to check.
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

    /// Infer a record literal's field initializers for their own diagnostics, and
    /// answer a fresh variable.
    ///
    /// The two heads that cannot be checked — one that resolved to nothing
    /// (`N001`) and one that is not a `struct` (`N008`, REP-26) — both take this
    /// path: the initializers are expressions the program wrote, and dropping them
    /// drops whatever else is wrong inside them.
    fn infer_record_lit_fields_only(&mut self, r: &RecordLitExpr) -> Type {
        if let Some(fl) = r.field_list() {
            for f in fl.fields() {
                if let Some(e) = f.expr() {
                    self.infer_expr(&e);
                }
            }
        }
        self.db.fresh_var()
    }

    /// Infer the type of a field access `receiver.field` (M7, §4.5). Returns the
    /// field's declared type.
    ///
    /// Every read that cannot be answered here goes through
    /// [`require_field`](Self::require_field) — **including** one whose receiver is
    /// already concrete (REP-28, corrected). That is not symmetry for its own sake,
    /// it is what makes the requirement have teeth. A field read used to constrain
    /// nothing at all: the read answered a fresh variable and recorded no
    /// requirement, so `fn dist(a) -> Int { a.x + a.y }` / `out(dist(3))` passed
    /// `praxis check` and then failed under `praxis run` with `Y112`. That is
    /// TY-30's shape exactly, and this is TY-30's fix at the third door.
    ///
    /// The two receivers take the two arms [`require_cap_as`](Self::require_cap_as)
    /// already has. A **variable** is deferred and answered by
    /// [`resolve_deferred_field`](Self::resolve_deferred_field) when a call site
    /// says what it is. A **concrete** receiver — `Int`, or a record with no such
    /// field — is decided here and now, by `crate::capability::check`, and reported
    /// at `praxis check` time. Routing the concrete case through the same door is
    /// ADR-057's rule (a capability check goes through `require_cap`) and it is also
    /// the only thing that makes `Capability::HasField`'s rejection arm reachable:
    /// before this, that arm was dead code and `check` reported nothing at all.
    ///
    /// A receiver that is *still* a variable when lowering runs is the one case
    /// nobody can decide — no call site ever pinned it — and `lower_field_get`
    /// tolerates it for the reason `+` in an uncalled generic function is tolerated.
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
        if let praxis_types::TypeData::Record { def, args } = self.db.data(resolved) {
            let (def, args) = (*def, args.clone());
            if let Some((_, ty)) = self.db.record_field_of(def, &args, &fname) {
                return ty;
            }
        }
        let result = self.db.fresh_var();
        self.require_field(receiver_ty, fname, result, field_tok.text_range());
        result
    }

    /// `p.0` — a tuple element, selected by position (REP-08, §4.4).
    ///
    /// A `(Int, Int)` was a legal value, a legal `Map` key and a legal graph state
    /// (ADR-060) that **no function could read**: `p.0` was a `P001` at the dot.
    ///
    /// The report is emitted **here** and not at lowering, for `Y018`'s reason
    /// (ADR-061): `praxis check` does not run lowering, so a program reported only
    /// there is clean under `check` and fails under `run` — the asymmetry REP-12
    /// was about. `Y112`'s emitter, which is where a bad *field* is reported, has
    /// exactly that shape today.
    ///
    /// An unresolved receiver says nothing. That is the same optimism every
    /// capability predicate has about a variable, and it is why `fn first(p) {
    /// p.0 }` comes out with a fresh result type rather than a diagnostic; giving
    /// it a real answer needs a "has an element at position n" requirement on the
    /// constraint channel, which no finding asks for.
    fn infer_tuple_index(&mut self, t: &praxis_ast::TupleIndexExpr) -> Type {
        let receiver_ty = t
            .receiver()
            .map(|r| self.infer_expr(&r))
            .unwrap_or_else(|| self.db.fresh_var());
        let at = t.syntax().text_range();
        // A literal too large for a `usize` is out of range for every tuple, so
        // it reports like any other out-of-range index rather than specially.
        let index = t.index().unwrap_or(usize::MAX);
        let resolved = self.db.follow(receiver_ty);
        match self.db.data(resolved) {
            praxis_types::TypeData::Tuple(els) => {
                let (arity, element) = (els.len(), els.get(index).copied());
                match element {
                    Some(el) => el,
                    None => {
                        self.diagnostics
                            .push(crate::diagnostics::tuple_index_out_of_range(
                                self.file_span(at),
                                arity,
                                index,
                            ));
                        self.db.fresh_var()
                    }
                }
            }
            // Optimistic: inference has not said what this is yet.
            praxis_types::TypeData::Var(_) => self.db.fresh_var(),
            _ => {
                let rendered = self.db.render(resolved);
                self.diagnostics.push(crate::diagnostics::not_a_tuple(
                    self.file_span(at),
                    &rendered,
                    index,
                ));
                self.db.fresh_var()
            }
        }
    }

    /// `m[key]`, `grid[x, y]` — a subscript read (REP-16, §4.7/§6.2/§6.4).
    ///
    /// Dispatched through the method catalog under [`INDEX_READ`], so which
    /// collections index and at what arity is one table's answer rather than a
    /// match arm here — and so an unannotated receiver defers on the constraint
    /// channel exactly as `values.sum()` does (TY-30). `fn first(m, k) { m[k] }`
    /// therefore infers, and the requirement is answered by whatever the call site
    /// puts in `m`'s place.
    ///
    /// A concrete receiver with no row is `Y020`, reported here rather than at
    /// lowering: `praxis check` does not run lowering, so a program reported only
    /// there is clean under `check` and fails under `run` (REP-12's asymmetry).
    fn infer_index(&mut self, i: &praxis_ast::IndexExpr) -> Type {
        let receiver_ty = match i.receiver() {
            Some(r) => self.infer_expr(&r),
            None => self.db.fresh_var(),
        };
        let indices = i.indices();
        let report = UnresolvedReport {
            build: crate::diagnostics::not_indexable,
            indices: indices.len(),
        };
        self.infer_catalog_call(
            receiver_ty,
            praxis_stdlib::catalog::INDEX_READ,
            &indices,
            i.syntax().text_range(),
            Some(report),
        )
    }

    /// `m[key] = v`, `counts[key] += 1` — a subscript store (REP-16, §6.2).
    ///
    /// The store is a second catalog row ([`INDEX_STORE`]) rather than the read
    /// row written backwards, because the two surfaces are not the same set: a
    /// `Text` reads through a subscript and is immutable, so it has no element
    /// store, and a `Counter`'s read is zero-defaulting where its store is not.
    ///
    /// A compound operator needs the value the read yields *and* the value the
    /// store takes to agree, which they do by construction — both are the entry's
    /// last parameter — so what is left to check is that the type is one the
    /// arithmetic accepts. That goes through the channel for `infer_assign`'s
    /// reason (TY-31): `fn bump(m, k) { m[k] += 1 }` leaves the value a variable,
    /// and answering "not numeric" about a variable is wrong.
    fn infer_place_assign(&mut self, stmt: &praxis_ast::PlaceAssignStmt) {
        let Some(target) = stmt.target() else { return };
        let value_ty = stmt
            .value()
            .map(|e| self.infer_expr(&e))
            .unwrap_or_else(|| self.db.fresh_var());
        let idx = match &target {
            Expr::Index(idx) => idx,
            // `p.x = 5` — a store into a record field (§4.5). Its own arm rather
            // than a catalog row: a field is selected by *name against one
            // record definition*, not dispatched on a receiver shape, which is
            // the same distinction that keeps `p.x` a field read and `p.len()` a
            // method call (ADR-077).
            Expr::FieldGet(f) => {
                self.infer_field_assign(stmt, f, value_ty);
                return;
            }
            _ => {
                // `f() = 1`: a shape the grammar admits and that names no
                // storage. The target is still inferred — it is a well-formed
                // expression whose own mistakes are worth reporting.
                let at = target.syntax().text_range();
                self.infer_expr(&target);
                self.diagnostics
                    .push(crate::diagnostics::not_an_assignment_target(
                        self.file_span(at),
                    ));
                return;
            }
        };
        let receiver_ty = match idx.receiver() {
            Some(r) => self.infer_expr(&r),
            None => self.db.fresh_var(),
        };
        // The store's arguments are the indices and then the value, which is why
        // the value is inferred first: `infer_catalog_call` pushes each expected
        // param type into its argument, and the value expression is one of them.
        // Which row the store is dispatched through is the operator's answer:
        // `min=`/`max=` are rows of their own (REP-21, ADR-064), because §6.2
        // gives them a semantics no read-modify-write can express.
        let op = stmt.op();
        let row = match op {
            praxis_ast::PlaceAssignOp::Min => praxis_stdlib::catalog::INDEX_STORE_MIN,
            praxis_ast::PlaceAssignOp::Max => praxis_stdlib::catalog::INDEX_STORE_MAX,
            _ => praxis_stdlib::catalog::INDEX_STORE,
        };
        let mut args = idx.indices();
        let report = UnresolvedReport {
            // One row, one message: the same table the deferred case reads, so a
            // receiver that has no `min=` is told that and not "no store".
            build: subscript_unresolved_report(row, args.len() + 1)
                .map_or(crate::diagnostics::not_index_assignable, |r| r.build),
            // The *index* count, which is one less than the argument count: the
            // store's last argument is the value.
            indices: args.len(),
        };
        if let Some(v) = stmt.value() {
            args.push(v);
        }
        self.infer_catalog_call(
            receiver_ty,
            row,
            &args,
            idx.syntax().text_range(),
            Some(report),
        );
        // Only the arithmetic compounds need a numeric value: an updating store's
        // row already binds it to `Int`, which is what its wrapper reads.
        if op.reads_before_writing() {
            self.require_cap_as(
                value_ty,
                Capability::Kind(CapKind::Numeric),
                stmt.syntax().text_range(),
                Some(crate::diagnostics::compound_assign_non_numeric),
            );
        }
        // …and `%=` is not one of the operations a `Float` value accepts, for
        // the binary `%`'s reason (§4.12, REP-64).
        self.reject_float_remainder(
            op == praxis_ast::PlaceAssignOp::Rem,
            value_ty,
            stmt.syntax().text_range(),
            "%=",
        );
    }

    /// `p.x = 5`, `p.x += 1` — a store into a record field (§4.5).
    ///
    /// **A field is a place**, which §4.2 already implies and nothing in the
    /// language could spell: a `var` binding "may still point to a mutable
    /// object", and every record was one you could only rebuild. `p.x = 5` was
    /// `Y021` and `Point { x: 5, y: p.y }` was the whole workaround.
    ///
    /// The field's type comes from the **same** [`infer_field_get`] a read takes,
    /// which is what keeps a store from being a second answer to "what does this
    /// receiver hold": a receiver still under inference defers on the constraint
    /// channel (REP-28's `HasField`) and is answered by whatever a call site puts
    /// in its place, and a concrete receiver with no such field is reported once,
    /// there, rather than again here in different words.
    ///
    /// The numeric requirement is the target's rather than the value's, for
    /// [`infer_assign`](Self::infer_assign)'s reason and with its one exception:
    /// `+=` on a `Text` field is concatenation and needs no number (ADR-085).
    ///
    /// [`infer_field_get`]: Self::infer_field_get
    fn infer_field_assign(
        &mut self,
        stmt: &praxis_ast::PlaceAssignStmt,
        f: &FieldExpr,
        value_ty: Type,
    ) {
        let field_ty = self.infer_field_get(f);
        let at = stmt.syntax().text_range();
        let op = stmt.op();
        // `min=`/`max=` are §6.2's **map** updates, and their whole semantics is
        // about an entry that may be absent: "an absent entry accepts the first
        // value". A field is always present, so there is no operation here to
        // name — which is `Y016`'s shape (an operator a type does not have),
        // not `Y020`'s (a subscript a receiver does not have).
        let update = match op {
            praxis_ast::PlaceAssignOp::Min => Some("min="),
            praxis_ast::PlaceAssignOp::Max => Some("max="),
            _ => None,
        };
        if let Some(spelling) = update {
            let rendered = self.db.render(self.db.follow(field_ty));
            self.diagnostics
                .push(crate::diagnostics::operator_not_defined(
                    self.file_span(at),
                    spelling,
                    &rendered,
                ));
            return;
        }
        if let Err(e) = self.db.unify(field_ty, value_ty) {
            self.diag_unify(self.file_span(at), e);
        }
        let text_concat_assign =
            op == praxis_ast::PlaceAssignOp::Add && is_text_scalar(&self.db, field_ty);
        if op.reads_before_writing() && !text_concat_assign {
            self.require_cap_as(
                field_ty,
                Capability::Kind(CapKind::Numeric),
                at,
                Some(crate::diagnostics::compound_assign_non_numeric),
            );
        }
        self.reject_float_remainder(op == praxis_ast::PlaceAssignOp::Rem, field_ty, at, "%=");
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
                    self.bind_pattern_name(tok.text_range(), expected);
                }
                let _ = name;
            }
            // `(a, b)` — a tuple pattern (REP-10, §4.4). The elements are fresh
            // variables unified *through* the scrutinee, so an unannotated
            // parameter destructured in a `match` is pinned to a tuple by the
            // pattern rather than left open.
            PatternKind::Tuple => {
                let subs: Vec<_> = pat.sub_patterns().collect();
                let elems: Vec<Type> = subs.iter().map(|_| self.db.fresh_var()).collect();
                match praxis_types::TupleElems::new(elems.clone()) {
                    Ok(te) => {
                        let tuple_ty = self.db.tuple(te);
                        if let Err(e) = self.db.unify(expected, tuple_ty) {
                            self.diag_unify(self.file_span(pat.syntax().text_range()), e);
                        }
                    }
                    // A tuple type has two elements or more, so `(p)` matches
                    // nothing — the parser has no grouping form to have meant.
                    Err(_) => {
                        let at = self.file_span(pat.syntax().text_range());
                        self.diagnostics.push(crate::diagnostics::not_a_pattern(
                            at,
                            "a tuple pattern names two elements or more",
                        ));
                    }
                }
                for (sub, elem_ty) in subs.iter().zip(elems) {
                    self.infer_pattern(sub, elem_ty);
                }
            }
            // `P { x, y: p }` or a headless `{ x, y: p }` — a record pattern
            // (REP-10, §4.5; ADR-091).
            PatternKind::Record(rname) => {
                self.infer_record_pattern(pat, rname.as_deref(), expected)
            }
            PatternKind::Variant(vname) => self.infer_variant_pattern(pat, &vname, expected),
        }
    }

    /// Infer a variant pattern `V(p, …)` against `expected` (M7, §4.6; REP-56).
    ///
    /// **A variant pattern's enum is the scrutinee's.** That is the rule lowering
    /// has always had — `lower_pattern`'s own `Variant` arm reads the def off
    /// `scrutinee_ty` — and it is now the rule here, which is what makes the two
    /// halves agree instead of disagreeing about where the enum comes from.
    ///
    /// Inference used to reach the enum *only* through the constructor's resolved
    /// symbol, and its comment asserted the symbol was "resolution already
    /// resolved". That is false for every anonymous enum: a `choice(...)` type has
    /// no declaration, so `resolve_pattern_bindings` records no ref for the
    /// constructor, the symbol was `None`, and the whole arm was skipped. Three
    /// things then never happened — the scrutinee was never unified, the payload
    /// was never asked for, and the payload binding never got a type. `Mul(p) =>
    /// p.a` left `p` an unbound var, so `infer_field_get` took its REP-28
    /// tolerance arm, lowering answered `Unit` instead of emitting the field load
    /// at all, and the *next* instruction aborted the runtime reading a `Unit` as
    /// an `Int` payload. `praxis check` was clean the whole way (REP-56).
    ///
    /// The constructor symbol stays as the **fallback**, for the case that is
    /// exactly the other way round: when nothing has pinned the scrutinee, the
    /// constructor is the only thing that can pin it, which is why `fn score(m) {
    /// match m { A(n) => n, B => 0 } }` infers at all for a *nominal* enum. An
    /// anonymous enum has no constructor symbol to fall back to, and equally no
    /// name to annotate the parameter with — see ADR-091's Consequences.
    fn infer_variant_pattern(&mut self, pat: &praxis_ast::Pattern, vname: &str, expected: Type) {
        let at = pat
            .name_token()
            .map(|t| t.text_range())
            .unwrap_or_else(|| pat.syntax().text_range());
        let resolved = self.db.follow(expected);
        let scrutinee_enum = match self.db.data(resolved) {
            praxis_types::TypeData::Enum { def, args } => Some((*def, args.clone())),
            _ => None,
        };
        let found = match scrutinee_enum {
            Some((def, args)) => {
                let Some(idx) = self.db.enum_def(def).variant(vname) else {
                    // The scrutinee is a concrete enum and it has no such
                    // variant: the answer is known *here*, so say it here. This
                    // used to be lowering's alone, which made a misspelled
                    // variant REP-12's asymmetry — `praxis check` clean, `praxis
                    // run` exiting 1 on the same file — for every enum, not only
                    // the anonymous ones this row is about.
                    let rendered = self.db.render(resolved);
                    self.diagnostics
                        .push(crate::diagnostics::unknown_enum_variant(
                            self.file_span(at),
                            &rendered,
                            vname,
                        ));
                    // Do not fall back to the constructor symbol: a name that
                    // happens to be *some other* enum's variant would unify the
                    // scrutinee with that enum and bury this report under a
                    // mismatch about a type the program never mentioned.
                    self.infer_sub_patterns_freely(pat);
                    return;
                };
                Some((resolved, self.db.variant_payload_of(def, &args, idx)))
            }
            // Nothing has pinned the scrutinee (or it is not an enum at all):
            // ask the constructor, which is the only thing left that can pin it.
            None => pat
                .name_token()
                .and_then(|t| self.refs.get(&t.text_range()).copied())
                .map(|r| r.symbol)
                .and_then(|symbol| self.lookup_enum_variant(symbol, vname))
                .map(|(enum_ty, _idx, payload)| (enum_ty, payload)),
        };
        let Some((enum_ty, payload_types)) = found else {
            // Neither end knows the enum. Lowering reports what it can see
            // (`Y123` for a scrutinee that is not an enum at all); the
            // sub-patterns still get types so their own bindings work.
            self.infer_sub_patterns_freely(pat);
            return;
        };
        if let Err(e) = self.db.unify(expected, enum_ty) {
            self.diag_unify(self.file_span(at), e);
        }
        let sub_pats: Vec<_> = pat.sub_patterns().collect();
        for (i, sub) in sub_pats.iter().enumerate() {
            match payload_types.get(i) {
                Some(&payload_ty) => self.infer_pattern(sub, payload_ty),
                // More sub-patterns than payload slots is lowering's `Y124`
                // (REP-05); infer the extras anyway so what is inside them is
                // still checked.
                None => {
                    let fresh = self.db.fresh_var();
                    self.infer_pattern(sub, fresh);
                }
            }
        }
    }

    /// The sub-patterns of a variant pattern whose enum neither the scrutinee nor
    /// the constructor could name, inferred against fresh variables. Nothing is
    /// known about the payload, but a binding inside it still needs a type and a
    /// mistake inside it is still a mistake — the same tolerance
    /// [`Self::infer_record_pattern_fields_only`] has at a record's fields.
    fn infer_sub_patterns_freely(&mut self, pat: &praxis_ast::Pattern) {
        for sub in pat.sub_patterns() {
            let fresh = self.db.fresh_var();
            self.infer_pattern(&sub, fresh);
        }
    }

    /// Bind the name *declared* at `range` by a pattern to `ty`.
    ///
    /// A pattern binding is monomorphic: it names a piece of the scrutinee, and
    /// the scrutinee is one value.
    fn bind_pattern_name(&mut self, range: TextRange, ty: Type) {
        if let Some(&symbol) = self.decls.get(&range) {
            if let Some(sym) = self.names.get_mut(symbol) {
                sym.scheme = Some(Scheme::monotype(ty));
            }
        }
    }

    /// Infer a record pattern `P { x, y: p }` — or a headless `{ x, y: p }` —
    /// against `expected` (REP-10, §4.5; ADR-091).
    ///
    /// A head is a type name resolution already resolved, exactly as a record
    /// *literal*'s is; the fields are checked against that record's declared
    /// fields, and a field the record does not have is the literal's own `Y114`.
    ///
    /// With **no head** the record is the scrutinee's, which is how a tuple
    /// pattern has always worked (ADR-069 Decision 4) and the only way an
    /// anonymous record can be matched at all: a `choice(...)` payload record has
    /// no name a head could write.
    ///
    /// A headless pattern therefore needs a record it can *see*. Field names
    /// alone cannot construct a record type — the language has no row variables,
    /// so unlike a tuple pattern this one cannot pin an open scrutinee from its
    /// own shape — and it is reported when the scrutinee is still open (ADR-091
    /// Decision 2). Staying silent there, the way `infer_field_get` stays silent
    /// about an unpinned receiver (REP-28), was measured and is *not* the same
    /// trade: `var f = |{x, y}| x + y` passed `praxis check` and then aborted the
    /// runtime with "int_payload wants a `Int` payload; this value is a `Unit`",
    /// because inference had bound `x` and `y` to fresh variables while lowering
    /// — which reads the record off the scrutinee and by then knows it — stored
    /// the fields at `Int`. The binding's type and the body's disagreed, which is
    /// REP-56's own failure mode reintroduced one pattern over. A field *read*
    /// can be silent because lowering answers `Unit` too, consistently; a
    /// *binding* cannot.
    ///
    /// Unlike a literal, a pattern need not name every field: an unnamed field is
    /// a wildcard, which is HIR-06's padding rule at the second kind of composite
    /// pattern (`Some` and `Some(_)` are one test for the same reason).
    fn infer_record_pattern(
        &mut self,
        pat: &praxis_ast::Pattern,
        rname: Option<&str>,
        expected: Type,
    ) {
        let head_ty = match rname {
            Some(_) => pat
                .name_token()
                .and_then(|tok| self.refs.get(&tok.text_range()).copied())
                .and_then(|resolved| self.type_env.ty(resolved.symbol)),
            None => Some(expected),
        };
        let at = self.file_span(pat.syntax().text_range());
        // A head that names nothing has already been reported (`N001`), and a
        // head that names something which is not a record cannot match at all.
        let Some(head_ty) = head_ty else {
            self.infer_record_pattern_fields_only(pat);
            return;
        };
        let (def_id, def_args) = match self.db.data(self.db.follow(head_ty)) {
            praxis_types::TypeData::Record { def, args } => (*def, args.clone()),
            // A headless pattern whose scrutinee is still open: the fields it
            // names do not determine a record, so there is no honest type to
            // bind them at. See this function's doc comment for what silence
            // here cost. Name the record, or annotate the value.
            praxis_types::TypeData::Var(_) if rname.is_none() => {
                self.diagnostics.push(crate::diagnostics::not_a_pattern(
                    at,
                    "`{ … }` cannot tell which record it matches here; \
                     name the record (`P { … }`) or annotate the value",
                ));
                self.infer_record_pattern_fields_only(pat);
                return;
            }
            _ => {
                let rendered = self.db.render(self.db.follow(head_ty));
                let reason = match rname {
                    Some(n) => format!("`{n}` is `{rendered}`, which has no fields to match"),
                    None => format!("`{{ … }}` is not a pattern for `{rendered}`"),
                };
                self.diagnostics
                    .push(crate::diagnostics::not_a_pattern(at, &reason));
                self.infer_record_pattern_fields_only(pat);
                return;
            }
        };
        // The pattern's own type *is* the record it names; a scrutinee of some
        // other type is the ordinary mismatch, reported where it is written.
        if let Err(e) = self.db.unify(expected, head_ty) {
            self.diag_unify(at, e);
        }
        let type_name = self.db.render(self.db.follow(head_ty));
        let mut seen: Vec<String> = Vec::new();
        for field in pat.fields() {
            let Some(fname_tok) = field.name() else {
                continue;
            };
            let fname = fname_tok.text().to_string();
            let field_at = self.file_span(fname_tok.text_range());
            if seen.contains(&fname) {
                self.diagnostics
                    .push(crate::diagnostics::duplicate_pattern_field(
                        field_at, &fname,
                    ));
                continue;
            }
            seen.push(fname.clone());
            let Some((_, field_ty)) = self.db.record_field_of(def_id, &def_args, &fname) else {
                self.diagnostics
                    .push(crate::diagnostics::unknown_record_field(
                        field_at, &type_name, &fname,
                    ));
                continue;
            };
            match field.pattern() {
                // `P { y: p }` — match the sub-pattern against the field.
                Some(sub) => self.infer_pattern(&sub, field_ty),
                // `P { x }` — bind the field to its own name.
                None => self.bind_pattern_name(fname_tok.text_range(), field_ty),
            }
        }
    }

    /// The sub-patterns of a record pattern whose head named no record, inferred
    /// against fresh variables so their own bindings still get a type and their
    /// own mistakes are still reported. The head's diagnostic is the answer to
    /// what the record is; nothing further is known about the fields.
    fn infer_record_pattern_fields_only(&mut self, pat: &praxis_ast::Pattern) {
        for field in pat.fields() {
            let fresh = self.db.fresh_var();
            match field.pattern() {
                Some(sub) => self.infer_pattern(&sub, fresh),
                None => {
                    if let Some(tok) = field.name() {
                        self.bind_pattern_name(tok.text_range(), fresh);
                    }
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
                &mut self.parser_exprs,
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
            // `text` first — `parse` requires it, `arg_ty` is what was passed
            // (REP-61).
            if let Err(e) = self.db.unify(text, arg_ty) {
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
                &mut self.parser_exprs,
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
            // A backtick template reaching *here* is one written in value
            // position: the `read`/`parse` path lowers its template through
            // `parse_parser_template` and never builds a `Literal` (REP-47).
            // §7.1 enters the parser-expression sublanguage at those two words
            // and nowhere else, so this template has nothing to parse and no
            // meaning — it used to be typed `Text` and lowered as a text literal
            // containing its own braces, so `` `n = {int}` `` printed itself.
            //
            // A fresh variable, not `Text`: the report is the answer, and
            // claiming a type here would produce a second diagnostic about the
            // use of a value that does not exist.
            SyntaxKind::BacktickTemplate => {
                let at = self.file_span(tok.text_range());
                self.diagnostics
                    .push(crate::diagnostics::parser_template_outside_read(at));
                self.db.fresh_var()
            }
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
    /// The kind is what makes this answerable: a `var` bound to a closure also
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
            // `int_ty` first — the range requires it, the bound is what was
            // written (REP-61).
            if let Err(e) = self.db.unify(int_ty, bound) {
                self.diag_unify(self.file_span(at.unwrap_or(whole)), e);
            }
        }
        self.db
            .collection(CollectionCtor::Range, CollectionArgs::Nullary)
            .expect("Range is nullary")
    }

    /// `[ e1, e2, … ]` — a `Vec` literal (§6.1). Every element has the same
    /// type, and the literal's type is `Vec` of it.
    ///
    /// The element type is a **fresh variable unified with each element in turn**
    /// rather than "the first element's type, unified with the rest". The two
    /// agree on what they accept and disagree on what they report: with the first
    /// element as the expectation, `[x, 1]` for an unannotated `x` pins `x` to
    /// whatever `x` already was and blames `1`, and `[]` has no first element to
    /// take a type from at all. A fresh variable makes the empty list the same
    /// rule as the others — `[]` is `Vec[?T]`, and the use decides `?T`, exactly
    /// as `Vec()` does.
    ///
    /// The *order* of the unification is what makes the message read correctly:
    /// `unify(element, el)` puts the type established so far in `expected`, so a
    /// `[1, "a"]` says "expected `Int`, found `Text`" at the `"a"` and not the
    /// reverse (REP-61).
    fn infer_list(&mut self, l: &praxis_ast::ListExpr) -> Type {
        let element = self.db.fresh_var();
        for el in l.elements() {
            let at = el.syntax().text_range();
            let el_ty = self.infer_expr(&el);
            if let Err(e) = self.db.unify(element, el_ty) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        self.db
            .collection(CollectionCtor::Vec, CollectionArgs::Unary(element))
            .expect("Vec is unary")
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
                // A `Text` operand makes the operation `Text` (ADR-085) — the
                // same rule, with a third type in it. So `"a" + 1` is a type
                // error rather than a coercion, exactly as `1 + 2.5` is: there
                // is no implicit conversion in either direction.
                //
                // `Text` is asked *first*. A `Text` and a `Float` cannot both be
                // present without one of them being the mismatch this reports,
                // and the order decides only which of the two gets named as the
                // requirement.
                let any_text_type = [lt, rt]
                    .into_iter()
                    .flatten()
                    .any(|t| is_text_scalar(&self.db, t));
                let target = if any_text_type {
                    self.db.text()
                } else if any_float_operand || any_float_type {
                    self.db.float()
                } else {
                    self.db.int()
                };
                if let (Some(l), Some(r)) = (lt, rt) {
                    let whole = b.syntax().text_range();
                    let lhs_at = lhs_range.unwrap_or(whole);
                    let rhs_at = rhs_range.unwrap_or(whole);
                    // `target` first: it is what the operator requires, and the
                    // operand is what the program wrote. Reversed, `"a" + "b"`
                    // read `expected Text, found Int` — the operand named as the
                    // requirement (REP-61).
                    if let Err(e) = self.db.unify(target, l) {
                        self.diag_unify(self.file_span(lhs_at), e);
                    }
                    if let Err(e) = self.db.unify(target, r) {
                        self.diag_unify(self.file_span(rhs_at), e);
                    }
                }
                // `%` is defined for integers only (§4.12). MIR has no Float
                // remainder: its `lower_bin` fell through to *addition*, so
                // `5.0 % 2.0` computed `7.0` (TY-27). There is no operation to
                // lower, so there is nothing to accept.
                self.reject_float_remainder(
                    op_kind == Some(SyntaxKind::PERCENT),
                    target,
                    b.syntax().text_range(),
                    "%",
                );
                // `+` is the only operator defined for `Text` (ADR-085), and
                // the other four report `Y016` for `%`-on-`Float`'s reason:
                // both operands agree and the operation still has no meaning.
                // Without this, `"ab" * 3` would reach MIR and lower as *integer
                // multiplication of two pointers*.
                if op_kind != Some(SyntaxKind::PLUS) && is_text_scalar(&self.db, target) {
                    let at = b.syntax().text_range();
                    let spelling = match op_kind {
                        Some(SyntaxKind::MINUS) => "-",
                        Some(SyntaxKind::STAR) => "*",
                        Some(SyntaxKind::SLASH) => "/",
                        _ => "%",
                    };
                    self.diagnostics
                        .push(crate::diagnostics::operator_not_defined(
                            self.file_span(at),
                            spelling,
                            "Text",
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
            // `&&` / `||` (§4.12): both operands `Bool`, result `Bool`. There is
            // no truthiness, so this is the whole rule — the *short-circuit* is
            // MIR's, not a typing difference between the two operators.
            //
            // The operands **join** with `Bool` rather than unifying with it, so a
            // divergent one is absorbed (TY-19/ADR-053): `false && panic("x")` is
            // the exit criterion's own example, and `panic` is `Never`. Unifying
            // reported "expected Never, found Bool" — a `Y001` about the operator,
            // not about the program — which is what `||` did before `&&` existed
            // to make it visible.
            Some(SyntaxKind::PIPE2 | SyntaxKind::AMP2) => {
                let bool = self.db.bool();
                if let (Some(l), Some(r)) = (lt, rt) {
                    let whole = b.syntax().text_range();
                    let lhs_at = lhs_range.unwrap_or(whole);
                    let rhs_at = rhs_range.unwrap_or(whole);
                    if let Err(e) = self.db.join(bool, l) {
                        self.diag_unify(self.file_span(lhs_at), e);
                    }
                    if let Err(e) = self.db.join(bool, r) {
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
            // `result` first — it is the type the operator demands of its
            // operand, so `!1` reads `expected Bool, found Int` (REP-61).
            if let Err(e) = self.db.unify(result, o) {
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
        // overwrite `last`, so `{ 1; var x = 2 }` was inferred `Int` while
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
    ///
    /// **An unannotated iterated parameter is generic in the iterable and
    /// monomorphic in the item** (REP-03). `iter_item` answers an unresolved
    /// iterator with a *fresh* item variable, so the two are related by the
    /// deferred `Iterable { item }` constraint rather than by being the same
    /// variable — which is what `fn total(r) { … for i in r { t = t + i } … }`
    /// needs, because pinning the item used to pin the iterator with it.
    ///
    /// That fresh variable is then **pinned to the declaration group's level**,
    /// for ADR-057 Decision 5's reason and no other: there is one lowered body per
    /// source function, and monomorphization substitutes a clone's types from the
    /// call site's *arguments*. It does not run the constraint channel, so an
    /// item variable the channel is the only resolver of would reach MIR unbound —
    /// and MIR reads the item type to type the loop variable's slot. The
    /// **iterator** stays quantified, which is what lets one `for` body lower
    /// against `Vec`, `BitSet` and `Range`: each is its own clone, and each
    /// clone's `len`/`get` symbols are selected from a concrete ctor.
    ///
    /// So `total` generalizes to "any iterable of `Int`". Two call sites that
    /// disagree about the *element* are a disagreement about `total`'s signature,
    /// exactly as two receivers at one method call site are (ADR-057 D5).
    fn infer_for(&mut self, f: &ForExpr) -> Type {
        let iter_ty = f
            .iter()
            .map(|i| self.infer_expr(&i))
            .unwrap_or_else(|| self.db.fresh_var());
        // Whether the iterator is still a variable, asked *before* `iter_item`
        // mints anything: it decides both that the requirement will be deferred
        // and that the item is a fresh variable needing the pin above.
        let iter_deferred = self.db.var_id_of(self.db.follow(iter_ty)).is_some();
        // The iterator must be iterable; `iter_item` returns the element type
        // (None → Y005 not_iterable). Record the element type on the binding's
        // reference range so the lowerer can read it.
        let item_ty = crate::capability::iter_item(&mut self.db, iter_ty);
        // An unresolved iterator's requirement cannot be answered here: it may be
        // generalized and then instantiated at a type that is not iterable at all,
        // which is what let `fn drain(values) { for v in values { … } }` accept
        // `drain(1)` (TY-29). Defer it. A concrete iterator is decided on the
        // spot, exactly as before.
        if let Some(item) = item_ty {
            if iter_deferred {
                let site = self.decl_site;
                self.db.pin_to_level(item, site);
            }
            self.require_cap(
                iter_ty,
                Capability::Iterable { item },
                f.iter()
                    .map(|i| i.syntax().text_range())
                    .unwrap_or_else(|| f.syntax().text_range()),
            );
        }
        if let Some(pat) = f.binding() {
            if let Some(item) = item_ty {
                // The binding is a pattern (REP-25): a bare name binds the item,
                // and `(k, v)` unifies it with a pair. Either way it is the same
                // walk a match arm goes through, at the element type.
                // Keyed by the **pattern node**, which for a bare name is the
                // same range as the name token and for `(k, v)` is a range no
                // token has. Lowering reads it back for the item type, and a
                // `for` binding is not an expression, so this is `ref_types`
                // rather than the per-node expression map.
                self.ref_types.insert(pat.syntax().text_range(), item);
                self.infer_pattern(&pat, item);
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
                        // Written type arguments constrain what this call
                        // constructs (REP-09): `Counter[(Int, Int)]()` says the
                        // key type here, where inference would otherwise wait for
                        // a use to pin it. Applied to the *instantiated* callee, so
                        // the constraint lands on this call site's variables and
                        // not on the ctor's scheme.
                        self.apply_written_type_args(c, callee_ty);
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

    /// Unify a call's written type arguments with the type its callee constructs
    /// (REP-09, §3.3's `Counter[(Int, Int)]()`).
    ///
    /// `callee_ty` is the callee's type **instantiated at this call site**, so its
    /// result's arguments are this call's own variables — the ones a later use
    /// would otherwise be the only thing to pin. Unifying rather than substituting
    /// is what makes a disagreement a `Y001` at the use that disagrees:
    /// `var c = Counter[Text]()\n c.inc(1)` reports about `Int` and `Text` rather
    /// than silently preferring one.
    ///
    /// The arity check is `Y007`, the code a written `Vec[Int, Text]` annotation
    /// already gets — it is the same mistake in a second position.
    fn apply_written_type_args(&mut self, c: &CallExpr, callee_ty: Type) {
        let Some(written) = c.type_args() else { return };
        let list: Vec<praxis_ast::TypeRef> = written.args().collect();
        let at = self.file_span(written.syntax().text_range());
        // The constructed type is the callee's *result*: `Counter` is
        // `forall T. () -> Counter[T]`, and the arguments belong to the `Counter[T]`.
        let constructed = match self.db.data(self.db.follow(callee_ty)) {
            praxis_types::TypeData::Func { result, .. } => *result,
            // A callee that is not a function has already been reported (or will
            // be by the unification below); saying so again here is the cascade
            // every other report in this function avoids.
            _ => return,
        };
        let (name, params) = match self.db.data(self.db.follow(constructed)) {
            praxis_types::TypeData::Collection { ctor, args } => {
                (ctor.name().to_string(), args.clone())
            }
            praxis_types::TypeData::Enum { def, args } => {
                let name = self.db.enum_def(*def).name.clone();
                (
                    name.unwrap_or_else(|| "this type".to_string()),
                    args.clone(),
                )
            }
            _ => return,
        };
        if params.len() != list.len() {
            self.diagnostics
                .push(crate::diagnostics::wrong_type_argument_count(
                    at,
                    &name,
                    list.len(),
                    params.len(),
                ));
            return;
        }
        for (param, written_ty) in params.iter().zip(&list) {
            let Some(resolved) = self.resolve_type(written_ty) else {
                // The annotation names nothing (`N002`/`N003`, already reported by
                // resolution); inference falls back to what use says, as it does
                // for every unresolvable annotation.
                continue;
            };
            if let Err(e) = self.db.unify(*param, resolved) {
                self.diag_unify(self.file_span(written_ty.syntax().text_range()), e);
            }
        }
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
        let key = m
            .method_name()
            .map(|t| t.text_range())
            .unwrap_or_else(|| m.syntax().text_range());
        self.infer_catalog_call(receiver_ty, &name, &arg_exprs, key, None)
    }

    /// The method `name` was probably meant to be, among the rows this receiver
    /// actually has (ADR-132).
    ///
    /// The candidate set is dispatch's own — `pattern_matches` against the
    /// receiver, the same filter completion uses — so the fix offered is a call
    /// that would resolve, rather than a name that merely exists somewhere in
    /// the catalog.
    fn nearest_method(&self, receiver_ty: Type, name: &str) -> Option<&'static str> {
        let pattern = crate::catalog::type_to_pattern(&self.db, receiver_ty)?;
        let mut candidates: Vec<&'static str> = self
            .catalog
            .entries()
            .iter()
            .filter(|e| praxis_stdlib::pattern_matches(&e.receiver, &pattern))
            .map(|e| e.name)
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        praxis_source::nearest(name, candidates.iter().copied())
    }

    /// Resolve one catalog-dispatched operation — a method call, or a subscript
    /// (REP-16) — against the receiver type, the name and the arity, and return
    /// its result type.
    ///
    /// One function for both because dispatch *is* the same act: §5.7's table is
    /// keyed by receiver shape and name, and a subscript's brackets select a row
    /// of it the way a method's name does. Sharing it is not only economy — the
    /// bidirectional argument inference, the `HasMethod` deferral for a receiver
    /// that is still a variable, TY-31's bounds and TY-32's collection invariants
    /// are each load-bearing, and a second copy would be a second place for one of
    /// them to be missing.
    ///
    /// `key` is where the [`crate::MethodRef`] is recorded — lowering reads that
    /// map and nothing else (F15/HIR-02) — and where diagnostics point. A method
    /// call keys on its name token; a subscript has no name token and keys on the
    /// whole `INDEX_EXPR` node, whose range always ends in `]` and so can never
    /// collide with an identifier's.
    ///
    /// `unresolved` **overrides** the report for a concrete receiver with no
    /// matching row. A method call passes `None` and gets `Y110`, built here
    /// (ADR-093). A subscript passes its own, because there is no method name to
    /// report about — `m[k]` on a `Set` is "cannot be indexed", not "no method
    /// `[]`". Both report at `praxis check` time, which is the point: lowering
    /// is a pass `check` never runs (REP-12).
    fn infer_catalog_call(
        &mut self,
        receiver_ty: Type,
        name: &str,
        arg_exprs: &[Expr],
        key: TextRange,
        unresolved: Option<UnresolvedReport>,
    ) -> Type {
        let arity = arg_exprs.len();
        // Look up the method in the catalog via the ADR-010 bridge.
        let hits = crate::catalog::lookup(&self.db, self.catalog, receiver_ty, name, arity);
        let Some(entry) = hits.first().copied() else {
            // Infer the args anyway (for nested diagnostics), and the result is
            // a fresh var either way.
            let arg_types: Vec<Type> = arg_exprs.iter().map(|a| self.infer_expr(a)).collect();
            let result = self.db.fresh_var();
            // Three situations arrive here, and ADR-093 is the rule that sorts
            // them: **inference reports a method call that cannot resolve —
            // either because the receiver is known and has no such row, or
            // because no receiver in the catalog has that name at that arity —
            // and lowering reports nothing.**
            //
            // A receiver that is still a **variable** has usually not failed
            // anything: nothing has said what it is yet, and `catalog::lookup`
            // cannot answer about a type that does not exist. That one becomes a
            // requirement on the channel, answered when the program pins the
            // receiver — the whole of TY-30, and what makes §5.2's `fn
            // total(values) { values.sum() }` infer. But if the catalog holds the
            // name nowhere at that arity, no receiver can ever answer it, and
            // deferring is deferring forever: `take_dischargeable` only returns
            // constraints whose variable has resolved, so an unpinned one sits in
            // `pending_constraints` and is never looked at again. That is how `fn
            // f(x) { x.nope() }` used to reach lowering unreported.
            //
            // A **concrete** receiver with no matching entry has no such method,
            // full stop. `unresolved` is the subscript's own wording for that —
            // "cannot be indexed" rather than "no method `[]`"; a method call
            // passes `None` and gets `Y110` here.
            if self.db.var_id_of(self.db.follow(receiver_ty)).is_some() {
                // The name-universe half is for **method calls only**, which is
                // why it asks `unresolved.is_none()`. A subscript reaches this
                // function through the same door but has no method name to
                // report about: REP-16 gave it "values of type `X` cannot be
                // indexed with N index(es)" precisely so the user never reads
                // ``no method `[]` ``, and there is no receiver type to put in
                // that sentence here. A subscript on a receiver nothing pinned
                // therefore defers exactly as it did before.
                if unresolved.is_some() || self.catalog.has_name_at_arity(name, arity) {
                    self.require_method(receiver_ty, name.to_string(), arg_types, result, key);
                } else {
                    self.diagnostics.push(crate::diagnostics::unknown_method(
                        self.file_span(key),
                        name,
                        None,
                        arity,
                    ));
                }
            } else if let Some(report) = unresolved {
                let rendered = self.db.render(self.db.follow(receiver_ty));
                self.diagnostics.push((report.build)(
                    self.file_span(key),
                    &rendered,
                    report.indices,
                ));
            } else {
                let rendered = self.db.render(self.db.follow(receiver_ty));
                let mut diag = crate::diagnostics::unknown_method(
                    self.file_span(key),
                    name,
                    Some(&rendered),
                    arity,
                );
                // A misspelled method is the same mistake as a misspelled
                // constructor and gets the same fix (ADR-132). The candidates
                // are the rows dispatch would have searched — this receiver's,
                // not the whole catalog's — so `v.lenght()` is offered `len`
                // and never a `Map` method a `Vec` does not have.
                if let Some(near) = self.nearest_method(receiver_ty, name) {
                    diag = diag.with_suggestion(
                        self.file_span(key),
                        near,
                        format!("did you mean `{near}`?"),
                    );
                }
                self.diagnostics.push(diag);
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
        // Bind the receiver *before* the parameters are instantiated, because
        // that is what pins the element type `T` — and an argument closure whose
        // parameter is the element type (`|inner| inner.len()` over
        // `Vec[Vec[Int]]`) needs it pinned before its body is inferred.
        self.bind_receiver(entry, receiver_ty, &mut names, key);
        let param_tys: Vec<Type> = entry
            .params
            .iter()
            .map(|p| crate::lower::pattern_to_type_named(&mut self.db, p, &mut names))
            .collect();
        let result_ty =
            crate::lower::pattern_to_type_named(&mut self.db, &entry.result, &mut names);
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
        let name_range = key;
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
        // Record the resolved method at `key` (HIR-02). Lowering reads the entry
        // rather than repeating the catalog lookup against a receiver type it
        // derived itself, and hover reads the result.
        self.method_refs.insert(
            key,
            crate::MethodRef {
                entry,
                receiver: receiver_ty,
                result: result_ty,
            },
        );
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
/// (§5.3). `var x = <value>` may generalize `x`'s type; `var x = <expansive>`
/// (a call, method call, block, `read`, …) is left monomorphic so its type
/// variables are shared across uses instead of instantiated fresh per reference
/// — the standard fix for the `var r = ref []` / `var v = Vec()` generalization
/// gap. Recurses through `Paren` (a transparent wrapper) and `Tuple` of values
/// (a value iff every element is). An explicit type annotation on the `var`
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

/// Whether `t` resolves to `Text` — what makes `+` concatenation (ADR-085).
fn is_text_scalar(db: &TypeDb, t: Type) -> bool {
    matches!(
        db.data(db.follow(t)),
        praxis_types::TypeData::Scalar(ScalarType::Text)
    )
}

/// How to name a binding kind in a diagnostic, in the words the source uses.
///
/// [`SymbolKind::Var`] is spelled "a binding" rather than "a `var` binding":
/// since ADR-125 it covers a `for` variable and a pattern name as well as a
/// `var` statement, and naming the keyword would be wrong for two of the three.
fn describe_binding(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Var => "a binding",
        SymbolKind::Fn => "a function",
        SymbolKind::Param => "a parameter",
        SymbolKind::Builtin | SymbolKind::BuiltinType => "a built-in name",
        SymbolKind::Struct => "a struct type",
        SymbolKind::Enum => "an enum type",
        SymbolKind::EnumVariant => "an enum variant",
    }
}
