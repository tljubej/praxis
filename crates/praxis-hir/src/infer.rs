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
use praxis_types::{unify::UnifyError, CollectionArgs, Level, ScalarType, Scheme, Type, TypeDb};
use rowan::TextRange;

use crate::diagnostics::{
    infinite_type, not_equatable, not_orderable, type_mismatch, type_mismatch_with_help,
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
        diagnostics: Vec::new(),
        catalog: builtin_catalog(),
        decl_site: Level::OUTERMOST,
        fn_results: Vec::new(),
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
    /// not the function that contains it.
    fn_results: Vec<Type>,
}

/// The built-in method catalog, constructed once and cached for the process
/// lifetime (it is immutable data). Shared with the HIR lowerer.
fn builtin_catalog() -> &'static praxis_stdlib::MethodCatalog {
    static CATALOG: std::sync::OnceLock<praxis_stdlib::MethodCatalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(praxis_stdlib::builtin_catalog)
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

    /// Assign polymorphic schemes to the prelude builtins that need them. `out`
    /// is `forall T. (T) -> Unit`; `panic` is `forall T. (T) -> Never`. The rest
    /// of the prelude (numeric helpers, collections) gets no scheme in M2 and
    /// references to them infer a fresh var (their dispatch lands in M5).
    fn seed_builtin_schemes(&mut self) {
        // Collect the ids first to avoid borrowing `self.names` while mutating it.
        // Only builtins whose call form needs a polymorphic scheme are seeded:
        // `out`/`panic` (forall T. (T) -> ...) and every §6.1 collection
        // constructor name (forall T. () -> Ctor[T], nullary for BitSet/Range).
        // Other prelude builtins (dbg/assert/abs/min/...) are handled as free
        // functions at lower time and are intentionally left scheme-less here.
        let to_seed: Vec<(SymbolId, String)> = self
            .names
            .all()
            .iter()
            .filter(|s| {
                s.kind == SymbolKind::Builtin
                    && (s.name == "out"
                        || s.name == "panic"
                        || s.name == "Some"
                        || s.name == "None"
                        || s.name == "pi"
                        || s.name == "e"
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
        let fn_ty = self.db.func(param_types, result_ty);
        // Unify the placeholder with the derived function type.
        if let Err(e) = self.db.unify(placeholder, fn_ty) {
            let at = item.syntax().text_range();
            self.diag_unify(self.file_span(at), e);
        }
        self.db.exit_level(prev);
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
        // (TY-15). Reported only for a target whose type is *known* — an
        // unconstrained one is a variable that a later use may still pin.
        let compound = stmt
            .op()
            .is_some_and(|t| !matches!(t.kind(), SyntaxKind::EQ));
        if compound {
            let resolved = self.db.follow(existing);
            if !is_numeric(&self.db, resolved) && !is_unconstrained(&self.db, resolved) {
                let rendered = self.db.render(resolved);
                self.diagnostics
                    .push(crate::diagnostics::compound_assign_non_numeric(
                        self.file_span(at),
                        &rendered,
                    ));
            }
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

    fn infer_expr(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Literal(l) => self.infer_literal(l),
            Expr::Path(p) => self.infer_path(p),
            Expr::Bin(b) => self.infer_bin(b),
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
        let body_ty = c.body().map_or(self.db.unit(), |b| self.infer_expr(&b));
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
            Expr::Closure(c) => self.infer_closure_expected(c, expected),
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
        if let Some(fl) = r.field_list() {
            for f in fl.fields() {
                let Some(fname_tok) = f.name() else { continue };
                let fname = fname_tok.text().to_string();
                if let Some((_, declared_ty)) = self.db.record_field_of(def_id, &def_args, &fname) {
                    let init_ty = match &f.expr() {
                        Some(e) => self.infer_expr(e),
                        // Punned field `{ x }` — x must be a binding of the field's type.
                        None => {
                            // Look up the name as a path reference.
                            let range = fname_tok.text_range();
                            self.refs
                                .get(&range)
                                .and_then(|rf| {
                                    self.names.get(rf.symbol).and_then(|s| {
                                        s.scheme.as_ref().map(|sc| self.db.instantiate(sc))
                                    })
                                })
                                .unwrap_or_else(|| self.db.fresh_var())
                        }
                    };
                    if let Err(e) = self.db.unify(declared_ty, init_ty) {
                        self.diag_unify(self.file_span(fname_tok.text_range()), e);
                    }
                }
            }
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
        // The text argument is an ordinary expression; resolve it.
        if let Some(text_expr) = p.text_expr() {
            self.infer_expr(&text_expr);
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
                let ty = self.db.instantiate(&scheme);
                self.ref_types.insert(range, ty);
                return ty;
            }
        }
        // Unresolved names were already reported by resolution; return a fresh var
        // so downstream inference does not cascade spurious errors.
        self.db.fresh_var()
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
                    let operand_ty = self.db.follow(l);
                    if matches!(op_kind, Some(SyntaxKind::EQ2 | SyntaxKind::NEQ)) {
                        if !crate::capability::supports_eq(&self.db, operand_ty) {
                            let rendered = self.db.render(operand_ty);
                            self.diagnostics
                                .push(not_equatable(self.file_span(at), &rendered));
                        }
                    } else if !crate::capability::supports_ord(&self.db, operand_ty) {
                        let rendered = self.db.render(operand_ty);
                        self.diagnostics
                            .push(not_orderable(self.file_span(at), &rendered));
                    }
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
                if operand_is_float_literal(&operand_node) {
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
            self.infer_block(&body);
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
            self.infer_block(&body);
        }
        self.db.unit()
    }

    /// `loop { body }` (M8, §4.11). Yields Unit (a value-producing loop via
    /// `break expr` refines this; the HIR conservatively reports Unit).
    fn infer_loop(&mut self, l: &LoopExpr) -> Type {
        if let Some(body) = l.body() {
            self.infer_block(&body);
        }
        self.db.unit()
    }

    /// `break [expr]` (M8, §4.11). Diverges; type `Never`.
    fn infer_break(&mut self, b: &BreakExpr) -> Type {
        if let Some(v) = b.value() {
            self.infer_expr(&v);
        }
        self.db.never()
    }

    /// `continue` (M8, §4.11). Diverges; type `Never`.
    fn infer_continue(&mut self, _c: &ContinueExpr) -> Type {
        self.db.never()
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
        if let Some(&result) = self.fn_results.last() {
            if let Err(e) = self.db.unify(result, value_ty) {
                self.diag_unify_hinted(self.file_span(at), e, "the function's return type");
            }
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
                        let callee_ty = self.db.instantiate(&scheme);
                        let result = self.db.fresh_var();
                        // Snapshot the concrete arg types before they are moved
                        // into the expected Func type — this is the call site's
                        // monomorphization witness (WS8, §13.6).
                        let arg_types_snapshot = arg_types.clone();
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
                            },
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

        // Record the method-name range for hover (the result type).
        if let Some(tok) = m.method_name() {
            self.ref_types.insert(tok.text_range(), receiver_ty);
        }

        // Look up the method in the catalog via the ADR-010 bridge.
        let hits = crate::catalog::lookup(&self.db, self.catalog, receiver_ty, &name, arity);
        let Some(entry) = hits.first().copied() else {
            // Unknown method: infer the args anyway (for nested diagnostics),
            // then leave the result as a fresh var; the HIR lowerer emits the
            // Y110 diagnostic (it has the method-name span).
            for arg in &arg_exprs {
                self.infer_expr(arg);
            }
            return self.db.fresh_var();
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

/// Whether `t` is a type arithmetic is defined on: `Int` or `Float` (§4.12).
/// There is no other numeric scalar, and no composite is numeric.
fn is_numeric(db: &TypeDb, t: Type) -> bool {
    matches!(
        db.data(db.follow(t)),
        praxis_types::TypeData::Scalar(ScalarType::Int | ScalarType::Float)
    )
}

/// Whether `t` is still an unbound variable — a type nothing has pinned yet, so
/// there is no answer to give about it and no mistake to report.
fn is_unconstrained(db: &TypeDb, t: Type) -> bool {
    matches!(db.data(db.follow(t)), praxis_types::TypeData::Var(_))
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
    }
}
