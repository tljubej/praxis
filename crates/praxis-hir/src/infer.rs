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
    ElseBranch, EnumItem, Expr, ExprStmt, FieldExpr, FnItem, ForExpr, IfExpr, LetStmt, Literal,
    LoopExpr, MethodCallExpr, Param, PathExpr, RecordLitExpr, ReturnExpr, SourceFile, StructItem,
    UnaryExpr, VarStmt, WhileExpr,
};
use praxis_source::{BytePos, Diagnostic, FileId, FileSpan, Span};
use praxis_syntax::SyntaxKind;
use praxis_types::{unify::UnifyError, ScalarType, Scheme, Type, TypeDb};
use rowan::TextRange;

use crate::diagnostics::{infinite_type, not_equatable, type_mismatch, type_mismatch_with_help};
use crate::name_table::NameTable;
use crate::resolve::{NameResolution, ResolvedRef};
use crate::scope::{ScopeId, ScopeTree};
use crate::symbol::{SymbolId, SymbolKind};

/// The output of inference.
pub struct Inference {
    pub db: TypeDb,
    pub names: NameTable,
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
        mut diagnostics,
    } = resolution;

    let mut inferer = Inferer {
        file,
        db: TypeDb::new(),
        names,
        scopes,
        refs,
        decls,
        ref_types: HashMap::new(),
        call_sites: HashMap::new(),
        diagnostics: Vec::new(),
        catalog: builtin_catalog(),
    };
    inferer.seed_builtin_schemes();
    let root_scope = inferer.scopes.root();
    for stmt in root.stmts() {
        inferer.infer_top_stmt(root_scope, &stmt);
    }
    // Merge name-resolution diagnostics with type diagnostics, sorted by span.
    diagnostics.append(&mut inferer.diagnostics);
    diagnostics.sort_by_key(|d| {
        let s = d.primary().span;
        (s.start(), s.end())
    });
    Inference {
        db: inferer.db,
        names: inferer.names,
        scopes: inferer.scopes,
        refs: inferer.refs,
        ref_types: inferer.ref_types,
        decls: inferer.decls,
        call_sites: inferer.call_sites,
        diagnostics,
    }
}

struct Inferer {
    file: FileId,
    db: TypeDb,
    names: NameTable,
    scopes: ScopeTree,
    refs: HashMap<TextRange, ResolvedRef>,
    /// Declaration sites → SymbolId (resolution's map). Used to attach schemes
    /// to the exact symbol, surviving shadowing.
    decls: HashMap<TextRange, SymbolId>,
    ref_types: HashMap<TextRange, Type>,
    /// Call sites keyed by the callee name token's range. Populated in
    /// `infer_call`; consumed by the monomorphization pass (WS8, §13.6).
    call_sites: HashMap<TextRange, crate::CallSite>,
    diagnostics: Vec<Diagnostic>,
    /// The built-in method catalog (§16.2), for resolving `receiver.method()`.
    /// Immutable; shared via a process-wide `OnceLock`.
    catalog: &'static praxis_stdlib::MethodCatalog,
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
                        || Self::collection_ctor_for(&s.name).is_some())
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
                    let ctor = Self::collection_ctor_for(&name).expect("ctor name");
                    let coll = db.collection(ctor, vec![v]);
                    db.func(vec![], coll)
                }
                "Map" => {
                    // forall K V. () -> Map[K, V]
                    let k = db.fresh_var();
                    let v = db.fresh_var();
                    let coll = db.collection(praxis_types::CollectionCtor::Map, vec![k, v]);
                    db.func(vec![], coll)
                }
                // BitSet and Range are nullary: () -> BitSet / () -> Range.
                "BitSet" | "Range" => {
                    let ctor = Self::collection_ctor_for(&name).expect("ctor name");
                    let coll = db.collection(ctor, vec![]);
                    db.func(vec![], coll)
                }
                // Optionality (M9). `Some : forall T. (T) -> Option[T]` and
                // `None : forall T. Option[T]`. `None` is a zero-payload variant,
                // so its scheme is the enum type directly (mirroring how
                // user-declared zero-payload variants get a monotype scheme in
                // `infer_enum`, not a `() -> ...` function). The Option def is
                // registered fresh inside the inner scope; instantiation
                // re-registers a fresh def per use, and same-named enums unify
                // structurally (unify.rs M9 arm) so independently-stamped
                // Option[T] copies merge into one type.
                "Some" => {
                    let t = db.fresh_var();
                    let opt = db.register_enum(
                        "Option",
                        vec![("Some".into(), Some(vec![t])), ("None".into(), None)],
                    );
                    db.func(vec![t], opt)
                }
                "None" => {
                    let t = db.fresh_var();
                    db.register_enum(
                        "Option",
                        vec![("Some".into(), Some(vec![t])), ("None".into(), None)],
                    )
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

    fn infer_top_stmt(&mut self, scope: ScopeId, node: &praxis_syntax::SyntaxNode) {
        if let Some(let_) = LetStmt::cast(node.clone()) {
            self.infer_let(scope, &let_);
        } else if let Some(var_) = VarStmt::cast(node.clone()) {
            self.infer_var(scope, &var_);
        } else if let Some(fn_) = FnItem::cast(node.clone()) {
            self.infer_fn(scope, &fn_);
        } else if let Some(struct_) = StructItem::cast(node.clone()) {
            self.infer_struct(scope, &struct_);
        } else if let Some(enum_) = EnumItem::cast(node.clone()) {
            self.infer_enum(scope, &enum_);
        } else if let Some(assign) = AssignStmt::cast(node.clone()) {
            self.infer_assign(scope, &assign);
        } else if let Some(expr) = ExprStmt::cast(node.clone()) {
            if let Some(e) = expr.expr() {
                self.infer_expr(scope, &e);
            }
        }
    }

    /// Register a struct declaration's type (M7, §4.5). Resolves each field's
    /// type annotation, builds a `RecordDef`, and stores the resulting `Type` on
    /// the struct's symbol (as a monomorphic scheme) so type annotations and
    /// record literals can look it up.
    fn infer_struct(&mut self, _scope: ScopeId, item: &StructItem) {
        let Some(name_tok) = item.name() else {
            return;
        };
        let name = name_tok.text().to_string();
        let range = name_tok.text_range();
        let Some(symbol) = self.resolve_decl(range) else {
            return;
        };
        // Resolve each field's type. Unknown types (already reported N002 by the
        // resolver) become fresh vars.
        let mut fields = Vec::new();
        if let Some(fl) = item.field_list() {
            for f in fl.fields() {
                let fname = f.name().map(|t| t.text().to_string()).unwrap_or_default();
                let fty = f
                    .ty()
                    .and_then(|t| self.resolve_type(&t))
                    .unwrap_or_else(|| self.db.fresh_var());
                fields.push((fname, fty));
            }
        }
        let ty = self.db.register_record(name, fields);
        if let Some(sym) = self.names.get_mut(symbol) {
            sym.scheme = Some(Scheme::monotype(ty));
        }
    }

    /// Look up a registered struct type by name, returning its `Type` if the
    /// name resolves to a `SymbolKind::Struct` symbol with an attached scheme.
    fn lookup_struct_type(&self, name: &str) -> Option<Type> {
        let root = self.scopes.root();
        let symbol = self.scopes.lookup(root, name)?;
        let sym = self.names.get(symbol)?;
        if sym.kind != SymbolKind::Struct {
            return None;
        }
        let scheme = sym.scheme.as_ref()?;
        Some(scheme.body)
    }

    /// Register an enum declaration's type (M7, §4.6). Builds the `EnumDef`,
    /// stores the resulting `Type` on the enum symbol, and gives each variant
    /// constructor a function type `(payload…) -> EnumType` (or `() -> EnumType`
    /// for payload-less variants).
    fn infer_enum(&mut self, _scope: ScopeId, item: &EnumItem) {
        let Some(name_tok) = item.name() else {
            return;
        };
        let name = name_tok.text().to_string();
        let range = name_tok.text_range();
        let Some(enum_symbol) = self.resolve_decl(range) else {
            return;
        };
        // Resolve each variant's payload types and build the EnumDef.
        let mut variants = Vec::new();
        // Collect (variant-name, payload-types, declaration-range) for ctor setup.
        let mut variant_info: Vec<(String, Vec<Type>, TextRange)> = Vec::new();
        for v in item.variants() {
            let vname = v.name().map(|t| t.text().to_string()).unwrap_or_default();
            let payload_types: Vec<Type> = v
                .payload_types()
                .unwrap_or_default()
                .into_iter()
                .map(|t| self.resolve_type(&t).unwrap_or_else(|| self.db.fresh_var()))
                .collect();
            let payload = if payload_types.is_empty() {
                None
            } else {
                Some(payload_types.clone())
            };
            variants.push((vname.clone(), payload));
            if let Some(vtok) = v.name() {
                variant_info.push((vname, payload_types, vtok.text_range()));
            }
        }
        let enum_ty = self.db.register_enum(name, variants);
        if let Some(sym) = self.names.get_mut(enum_symbol) {
            sym.scheme = Some(Scheme::monotype(enum_ty));
        }
        // Give each variant constructor a type.
        for (vname, payload_types, vrange) in &variant_info {
            if let Some(vsymbol) = self.resolve_decl(*vrange) {
                // The constructor type. A zero-payload variant (`Empty`) is a
                // bare value of the enum type (used as a path, not a call). A
                // payload variant (`Number(Int)`) is a function `(Int) -> Enum`.
                let ctor_ty = if payload_types.is_empty() {
                    enum_ty
                } else {
                    self.db.func(payload_types.clone(), enum_ty)
                };
                if let Some(sym) = self.names.get_mut(vsymbol) {
                    sym.scheme = Some(Scheme::monotype(ctor_ty));
                }
            }
            let _ = vname;
        }
    }

    /// Look up a registered enum type by name.
    #[allow(dead_code)] // used by WS5 pattern matching
    fn lookup_enum_type(&self, name: &str) -> Option<Type> {
        let root = self.scopes.root();
        let symbol = self.scopes.lookup(root, name)?;
        let sym = self.names.get(symbol)?;
        if sym.kind != SymbolKind::Enum {
            return None;
        }
        let scheme = sym.scheme.as_ref()?;
        Some(scheme.body)
    }

    /// Look up an enum variant by constructor name. Returns the variant's enum
    /// def-id, index, and payload types (empty for payload-less). Works for both
    /// payload variants (scheme is `Func -> Enum`) and zero-payload variants
    /// (scheme is the enum type directly).
    #[allow(dead_code)] // used by WS5 pattern matching
    fn lookup_enum_variant(
        &self,
        name: &str,
    ) -> Option<(praxis_types::EnumDefId, usize, Vec<Type>)> {
        let root = self.scopes.root();
        let symbol = self.scopes.lookup(root, name)?;
        let sym = self.names.get(symbol)?;
        let scheme = sym.scheme.as_ref()?;
        // The scheme body is either a Func returning the enum type (payload
        // variant) or the enum type itself (zero-payload variant).
        let result_ty = match self.db.data(self.db.follow(scheme.body)) {
            praxis_types::TypeData::Func { result, .. } => *result,
            praxis_types::TypeData::Enum { .. } => scheme.body,
            _ => return None,
        };
        let def_id = match self.db.data(self.db.follow(result_ty)) {
            praxis_types::TypeData::Enum { def } => *def,
            _ => return None,
        };
        let edef = self.db.enum_def(def_id);
        let idx = edef.variant(name)?;
        let payload = edef.variants[idx].payload.clone().unwrap_or_default();
        Some((def_id, idx, payload))
    }

    /// Resolve the symbol declared at `range` (from the resolution `decls` map).
    fn resolve_decl(&self, range: TextRange) -> Option<SymbolId> {
        self.decls.get(&range).copied()
    }

    fn infer_let(&mut self, scope: ScopeId, stmt: &LetStmt) {
        // Infer the RHS at an inner level so its vars can be generalized. Manage
        // the level explicitly (not via db.scoped) because the inference borrows
        // `self` mutably alongside `self.db`.
        let prev = self.db.enter_level();
        let rhs = stmt.init();
        let rhs_ty = rhs.as_ref().map(|e| self.infer_expr(scope, e));
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

    fn infer_var(&mut self, scope: ScopeId, stmt: &VarStmt) {
        // `var` RHS is inferred but NOT generalized (§5.3).
        let prev = self.db.enter_level();
        let rhs_ty = stmt.init().map(|e| self.infer_expr(scope, &e));
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

    fn infer_fn(&mut self, scope: ScopeId, item: &FnItem) {
        // Bind the fn name to a monomorphic placeholder var first (for recursion).
        let placeholder = self.db.fresh_var();
        let fn_symbol = if let Some(name_tok) = item.name() {
            let id = *self
                .decls
                .get(&name_tok.text_range())
                .expect("fn name was declared during resolution");
            if let Some(sym) = self.names.get_mut(id) {
                sym.scheme = Some(Scheme::monotype(placeholder));
            }
            Some(id)
        } else {
            None
        };

        // Body scope at an inner level: params get fresh vars, body is inferred.
        let body_scope = self.scopes.push_child(scope);
        let prev = self.db.enter_level();

        // Bind each param's symbol to its type (annotated, or a fresh var).
        let mut param_types: Vec<Type> = Vec::new();
        if let Some(pl) = item.param_list() {
            for p in pl.params() {
                let pty = self.infer_param(body_scope, &p);
                param_types.push(pty);
            }
        }
        // The declared return type, if any; else a fresh var inferred from body.
        let ret_annot = item.return_type().and_then(|t| self.resolve_type(&t));
        let (body_ty, tail_range) = match item.body() {
            Some(b) => {
                let (ty, range) = self.infer_block_with_tail(body_scope, &b);
                (Some(ty), range)
            }
            None => (None, None),
        };
        // Unify declared return with the body's type (if both present). Point the
        // mismatch at the offending tail expression (e.g. the trailing
        // `out(...)`) rather than the whole `fn`, falling back to the block's
        // range, then the whole item, so precision never regresses.
        let result_ty = match (ret_annot, body_ty) {
            (Some(a), Some(b)) => {
                if let Err(e) = self.db.unify(a, b) {
                    let at = tail_range.unwrap_or_else(|| item.syntax().text_range());
                    self.diag_unify_hinted(self.file_span(at), e, "the function body");
                }
                a
            }
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => self.db.fresh_var(),
        };
        let fn_ty = self.db.func(param_types, result_ty);
        // Unify the placeholder with the derived function type.
        if let Err(e) = self.db.unify(placeholder, fn_ty) {
            let at = item.syntax().text_range();
            self.diag_unify(self.file_span(at), e);
        }
        self.db.exit_level(prev);
        // Generalize the fn after its body is checked (§5.3).
        let scheme = self.db.generalize(placeholder);
        if let Some(id) = fn_symbol {
            if let Some(sym) = self.names.get_mut(id) {
                sym.scheme = Some(scheme);
            }
        }
    }

    fn infer_param(&mut self, _scope: ScopeId, p: &Param) -> Type {
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

    fn infer_assign(&mut self, scope: ScopeId, stmt: &AssignStmt) {
        // `x = e`: unify e's type with the var's established (monomorphic) type.
        let rhs_ty = stmt.value().map(|e| self.infer_expr(scope, &e));
        if let (Some(name_tok), Some(rhs)) = (stmt.name(), rhs_ty) {
            if let Some(id) = self.scopes.lookup(scope, name_tok.text().as_ref()) {
                if let Some(sym) = self.names.get(id) {
                    if let Some(scheme) = &sym.scheme {
                        let existing = self.db.instantiate(scheme);
                        if let Err(e) = self.db.unify(existing, rhs) {
                            let at = name_tok.text_range();
                            self.diag_unify(self.file_span(at), e);
                        }
                    }
                }
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

    fn infer_expr(&mut self, scope: ScopeId, expr: &Expr) -> Type {
        match expr {
            Expr::Literal(l) => self.infer_literal(l),
            Expr::Path(p) => self.infer_path(scope, p),
            Expr::Bin(b) => self.infer_bin(scope, b),
            Expr::Unary(u) => self.infer_unary(scope, u),
            Expr::Paren(p) => p
                .expr()
                .map(|e| self.infer_expr(scope, &e))
                .unwrap_or_else(|| self.db.fresh_var()),
            Expr::Tuple(t) => {
                let els: Vec<Type> = t.elements().map(|e| self.infer_expr(scope, &e)).collect();
                self.db.tuple(els)
            }
            Expr::Block(b) => self.infer_block(scope, b),
            Expr::If(i) => self.infer_if(scope, i),
            Expr::While(w) => self.infer_while(scope, w),
            Expr::For(f) => self.infer_for(scope, f),
            Expr::Loop(l) => self.infer_loop(scope, l),
            Expr::Break(b) => self.infer_break(scope, b),
            Expr::Continue(c) => self.infer_continue(scope, c),
            Expr::Return(r) => self.infer_return(scope, r),
            Expr::Call(c) => self.infer_call(scope, c),
            Expr::MethodCall(m) => self.infer_method_call(scope, m),
            Expr::Read(r) => self.infer_read(r),
            Expr::Parse(p) => self.infer_parse(scope, p),
            Expr::RecordLit(r) => self.infer_record_lit(scope, r),
            Expr::FieldGet(f) => self.infer_field_get(scope, f),
            Expr::Match(m) => self.infer_match(scope, m),
            // M7-WS7: closure — type is `Func`; params bind in a child scope.
            Expr::Closure(c) => self.infer_closure(scope, c),
            Expr::Error(_) => self.db.fresh_var(),
        }
    }

    /// Infer the type of a `|params| expr` closure (M7, §4.10). The result is a
    /// `Func` type `(P0, …) -> R` built from the param and body types. Free
    /// variables in the body resolve to outer-scope bindings (captures); the
    /// capture environment is a runtime concern, not a type-system one (§4.10).
    fn infer_closure(&mut self, scope: ScopeId, c: &praxis_ast::ClosureExpr) -> Type {
        let body_scope = self.scopes.push_child(scope);
        let mut param_types = Vec::new();
        for p in c.params() {
            param_types.push(self.infer_param(body_scope, &p));
        }
        let result_ty = c
            .body()
            .map_or(self.db.unit(), |b| self.infer_expr(body_scope, &b));
        self.db.func(param_types, result_ty)
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
    fn infer_closure_expected(
        &mut self,
        scope: ScopeId,
        c: &praxis_ast::ClosureExpr,
        expected: Type,
    ) -> Type {
        // Read the expected Func's params/result (after following). If it is not
        // a Func, or the param count differs, defer to the bottom-up path.
        let (exp_params, exp_result) = match self.db.data(self.db.follow(expected)) {
            praxis_types::TypeData::Func { params, result } => (params.clone(), *result),
            _ => return self.infer_closure(scope, c),
        };
        let closure_params: Vec<_> = c.params().collect();
        if exp_params.len() != closure_params.len() {
            return self.infer_closure(scope, c);
        }
        let body_scope = self.scopes.push_child(scope);
        let mut param_types = Vec::new();
        for (p, exp_pt) in closure_params.into_iter().zip(exp_params.iter()) {
            let pt = self.infer_param(body_scope, &p);
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
        let result_ty = c
            .body()
            .map_or(self.db.unit(), |b| self.infer_expr(body_scope, &b));
        self.db.func(param_types, result_ty)
    }

    /// Infer an expression with an expected type pushed down from context
    /// (bidirectional inference, M8 §3). Currently only closures take the hint;
    /// every other expression ignores `expected` and infers bottom-up (a fresh
    /// var unifies no-op, so this is safe).
    fn infer_expr_expected(&mut self, scope: ScopeId, expr: &Expr, expected: Type) -> Type {
        match expr {
            Expr::Closure(c) => self.infer_closure_expected(scope, c, expected),
            _ => self.infer_expr(scope, expr),
        }
    }

    /// Infer the type of a record literal `Name { field: expr, … }` (M7, §4.5).
    /// Looks up the struct type, unifies each field initializer with the declared
    /// field type, and returns the struct type.
    fn infer_record_lit(&mut self, scope: ScopeId, r: &RecordLitExpr) -> Type {
        let struct_ty = r.name().and_then(|p| p.name()).and_then(|tok| {
            let name = tok.text().to_string();
            self.lookup_struct_type(&name)
        });
        let Some(struct_ty) = struct_ty else {
            // Unknown struct: infer each field for diagnostics, return a fresh var.
            if let Some(fl) = r.field_list() {
                for f in fl.fields() {
                    if let Some(e) = f.expr() {
                        self.infer_expr(scope, &e);
                    }
                }
            }
            return self.db.fresh_var();
        };
        // Get the record def to look up declared field types.
        let def_id = match self.db.data(self.db.follow(struct_ty)) {
            praxis_types::TypeData::Record { def } => *def,
            _ => return struct_ty,
        };
        if let Some(fl) = r.field_list() {
            for f in fl.fields() {
                let Some(fname_tok) = f.name() else { continue };
                let fname = fname_tok.text().to_string();
                let rdef = self.db.record_def(def_id);
                if let Some((_, declared_ty)) = rdef.field(&fname) {
                    let init_ty = match &f.expr() {
                        Some(e) => self.infer_expr(scope, e),
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
    fn infer_field_get(&mut self, scope: ScopeId, f: &FieldExpr) -> Type {
        let receiver_ty = f
            .receiver()
            .map(|r| self.infer_expr(scope, &r))
            .unwrap_or_else(|| self.db.fresh_var());
        let Some(field_tok) = f.field_name() else {
            return receiver_ty;
        };
        let fname = field_tok.text().to_string();
        let resolved = self.db.follow(receiver_ty);
        match self.db.data(resolved) {
            praxis_types::TypeData::Record { def } => {
                let rdef = self.db.record_def(*def);
                rdef.field(&fname)
                    .map(|(_, t)| t)
                    .unwrap_or_else(|| self.db.fresh_var())
            }
            _ => self.db.fresh_var(),
        }
    }

    /// Infer the type of a `match scrutinee { pattern => body, … }` expression
    /// (M7, §4.6). Unifies the scrutinee with each pattern, then unifies all
    /// arm body types to determine the match's result type.
    fn infer_match(&mut self, scope: ScopeId, m: &praxis_ast::MatchExpr) -> Type {
        let scrutinee_ty = m
            .scrutinee()
            .map(|s| self.infer_expr(scope, &s))
            .unwrap_or_else(|| self.db.fresh_var());
        let arms: Vec<_> = m.arms().collect();
        let result = self.db.fresh_var();
        for arm in &arms {
            let arm_scope = self.scopes.push_child(scope);
            if let Some(pat) = arm.pattern() {
                self.infer_pattern(arm_scope, &pat, scrutinee_ty);
            }
            if let Some(body) = arm.body() {
                let body_ty = self.infer_expr(arm_scope, &body);
                if let Err(e) = self.db.unify(result, body_ty) {
                    let at = self.file_span(arm.syntax().text_range());
                    self.diag_unify(at, e);
                }
            }
        }
        result
    }

    /// Infer a pattern against an expected type (M7, §4.6). Binds pattern
    /// variables and unifies variant payloads.
    #[allow(clippy::only_used_in_recursion)]
    fn infer_pattern(&mut self, scope: ScopeId, pat: &praxis_ast::Pattern, expected: Type) {
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
                // An enum variant pattern. Look up the variant to get payload types.
                if let Some((enum_def_id, variant_idx, payload_types)) =
                    self.lookup_enum_variant(&vname)
                {
                    let enum_ty = self.db.enum_type(enum_def_id);
                    if let Err(e) = self.db.unify(expected, enum_ty) {
                        if let Some(tok) = pat.name_token() {
                            self.diag_unify(self.file_span(tok.text_range()), e);
                        }
                    }
                    // Unify sub-patterns with payload types.
                    let sub_pats: Vec<_> = pat.sub_patterns().collect();
                    for (i, sub) in sub_pats.iter().enumerate() {
                        if let Some(&payload_ty) = payload_types.get(i) {
                            self.infer_pattern(scope, sub, payload_ty);
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
    fn infer_parse(&mut self, scope: ScopeId, p: &praxis_ast::ParseExpr) -> Type {
        // The text argument is an ordinary expression; resolve it.
        if let Some(text_expr) = p.text_expr() {
            self.infer_expr(scope, &text_expr);
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
            SyntaxKind::TextLit => self.db.text(),
            SyntaxKind::KW_TRUE | SyntaxKind::KW_FALSE => self.db.bool(),
            // Backtick templates are M6; treat as Text for now (a fresh var would
            // be sounder but Text matches the eventual type).
            SyntaxKind::BacktickTemplate => self.db.text(),
            _ => self.db.fresh_var(),
        }
    }

    fn infer_path(&mut self, scope: ScopeId, p: &PathExpr) -> Type {
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
        let _ = scope;
        // Unresolved names were already reported by resolution; return a fresh var
        // so downstream inference does not cascade spurious errors.
        self.db.fresh_var()
    }

    fn infer_bin(&mut self, scope: ScopeId, b: &BinExpr) -> Type {
        let (lhs, rhs) = b.operands();
        // Keep each operand's node so a type mismatch can point at the specific
        // bad operand rather than the whole binary expression (the earlier
        // behavior underlined `a + b` even when only `a` was at fault).
        let lhs_range = lhs.as_ref().map(|e| e.syntax().text_range());
        let rhs_range = rhs.as_ref().map(|e| e.syntax().text_range());
        let lt = lhs.map(|e| self.infer_expr(scope, &e));
        let rt = rhs.map(|e| self.infer_expr(scope, &e));
        let op_kind = b.op().map(|t| t.kind());
        match op_kind {
            // Arithmetic: both operands and result are Int (§4.12).
            Some(
                SyntaxKind::PLUS
                | SyntaxKind::MINUS
                | SyntaxKind::STAR
                | SyntaxKind::SLASH
                | SyntaxKind::PERCENT,
            ) => {
                let int = self.db.int();
                if let (Some(l), Some(r)) = (lt, rt) {
                    let whole = b.syntax().text_range();
                    let lhs_at = lhs_range.unwrap_or(whole);
                    let rhs_at = rhs_range.unwrap_or(whole);
                    if let Err(e) = self.db.unify(l, int) {
                        self.diag_unify(self.file_span(lhs_at), e);
                    }
                    if let Err(e) = self.db.unify(r, int) {
                        self.diag_unify(self.file_span(rhs_at), e);
                    }
                }
                int
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
                    // equatable. Ordering comparisons (`<`, `>`, …) are still
                    // Int-only (§4.12), so they keep the native IntCmp path;
                    // only `==`/`!=` admit composite operands. Emit Y004 for a
                    // type that cannot be compared with `==`.
                    if matches!(op_kind, Some(SyntaxKind::EQ2 | SyntaxKind::NEQ)) {
                        let operand_ty = self.db.follow(l);
                        if !crate::capability::supports_eq(&self.db, operand_ty) {
                            let rendered = self.db.render(operand_ty);
                            self.diagnostics
                                .push(not_equatable(self.file_span(at), &rendered));
                        }
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

    fn infer_unary(&mut self, scope: ScopeId, u: &UnaryExpr) -> Type {
        let operand_node = u.operand();
        let operand = operand_node.as_ref().map(|e| self.infer_expr(scope, e));
        let result = match u.op().map(|t| t.kind()) {
            Some(SyntaxKind::MINUS) => self.db.int(),
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

    fn infer_block(&mut self, scope: ScopeId, block: &BlockExpr) -> Type {
        self.infer_block_inner(scope, block).0
    }

    /// Like [`infer_block`](Self::infer_block), but also returns the source
    /// range of the expression that produced the block's value (its trailing
    /// expression), or the block's own range when there is no trailing
    /// expression (the block's value is then `Unit`). Used by `infer_fn` so a
    /// return-type mismatch can point at the offending tail expression rather
    /// than the whole `fn`.
    fn infer_block_with_tail(
        &mut self,
        scope: ScopeId,
        block: &BlockExpr,
    ) -> (Type, Option<TextRange>) {
        self.infer_block_inner(scope, block)
    }

    /// Shared body: infer every statement, returning the block's value type and
    /// the range that produced it (the trailing expression, or the block itself
    /// when the value is the implicit trailing `Unit`).
    fn infer_block_inner(
        &mut self,
        scope: ScopeId,
        block: &BlockExpr,
    ) -> (Type, Option<TextRange>) {
        let inner = self.scopes.push_child(scope);
        let mut last = self.db.unit();
        let mut tail_range: Option<TextRange> = None;
        for child in block.stmts() {
            // A trailing expression statement is the block's value.
            if let Some(expr_stmt) = ExprStmt::cast(child.clone()) {
                if let Some(e) = expr_stmt.expr() {
                    last = self.infer_expr(inner, &e);
                    tail_range = Some(e.syntax().text_range());
                    continue;
                }
            }
            self.infer_top_stmt(inner, &child);
        }
        // No trailing expression: the value is Unit; point at the whole block so
        // the reader still sees where the implicit Unit comes from.
        let tail_range = tail_range.or_else(|| Some(block.syntax().text_range()));
        (last, tail_range)
    }

    fn infer_if(&mut self, scope: ScopeId, i: &IfExpr) -> Type {
        if let Some(cond) = i.cond() {
            let ct = self.infer_expr(scope, &cond);
            let bool = self.db.bool();
            // Condition must be Bool: point at the condition, not the whole `if`.
            let at = cond.syntax().text_range();
            if let Err(e) = self.db.unify(bool, ct) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        let then_ty = i.then_branch().map(|b| self.infer_block(scope, &b));
        let else_ty = i.else_branch().and_then(|e| self.infer_else(scope, &e));
        match (then_ty, else_ty) {
            (Some(t), Some(e)) => {
                if let Err(err) = self.db.unify(t, e) {
                    // Branches disagree: point at the `else` branch (the one that
                    // diverges from the `then` branch's established type).
                    let at = i
                        .else_branch()
                        .and_then(|eb| eb.body())
                        .map(|body| body.syntax().text_range())
                        .unwrap_or_else(|| i.syntax().text_range());
                    self.diag_unify(self.file_span(at), err);
                }
                t
            }
            (Some(t), None) => t,
            (None, Some(e)) => e,
            (None, None) => self.db.fresh_var(),
        }
    }

    fn infer_else(&mut self, scope: ScopeId, e: &ElseBranch) -> Option<Type> {
        e.body().map(|body| match body {
            Expr::Block(b) => self.infer_block(scope, &b),
            other => {
                let inner = self.scopes.push_child(scope);
                self.infer_expr(inner, &other)
            }
        })
    }

    fn infer_while(&mut self, scope: ScopeId, w: &WhileExpr) -> Type {
        if let Some(cond) = w.cond() {
            let ct = self.infer_expr(scope, &cond);
            let bool = self.db.bool();
            // Condition must be Bool: point at the condition, not the whole `while`.
            let at = cond.syntax().text_range();
            if let Err(e) = self.db.unify(bool, ct) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        if let Some(body) = w.body() {
            self.infer_block(scope, &body);
        }
        // `while` yields Unit.
        self.db.unit()
    }

    /// `for binding in iter { body }` (M8, §4.11). The iterator must be iterable
    /// (§5.4); the binding gets the element type. `for` yields Unit.
    fn infer_for(&mut self, scope: ScopeId, f: &ForExpr) -> Type {
        let iter_ty = f
            .iter()
            .map(|i| self.infer_expr(scope, &i))
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
            self.infer_block(scope, &body);
        }
        self.db.unit()
    }

    /// `loop { body }` (M8, §4.11). Yields Unit (a value-producing loop via
    /// `break expr` refines this; the HIR conservatively reports Unit).
    fn infer_loop(&mut self, scope: ScopeId, l: &LoopExpr) -> Type {
        if let Some(body) = l.body() {
            self.infer_block(scope, &body);
        }
        self.db.unit()
    }

    /// `break [expr]` (M8, §4.11). Diverges; type `Never`.
    fn infer_break(&mut self, scope: ScopeId, b: &BreakExpr) -> Type {
        if let Some(v) = b.value() {
            self.infer_expr(scope, &v);
        }
        self.db.never()
    }

    /// `continue` (M8, §4.11). Diverges; type `Never`.
    fn infer_continue(&mut self, _scope: ScopeId, _c: &ContinueExpr) -> Type {
        self.db.never()
    }

    /// `return [expr]` (M8, §4.11). Diverges; type `Never`.
    fn infer_return(&mut self, scope: ScopeId, r: &ReturnExpr) -> Type {
        if let Some(v) = r.value() {
            self.infer_expr(scope, &v);
        }
        self.db.never()
    }

    fn infer_call(&mut self, scope: ScopeId, c: &CallExpr) -> Type {
        // Collect argument types.
        let arg_types: Vec<Type> = c
            .arg_list()
            .map(|a| self.collect_args(scope, &a))
            .unwrap_or_default();
        // Postfix call on an arbitrary expression (`expr(args)`, M8 §4.10):
        // when there is no named callee, the callee is an expression (e.g.
        // `fs.get(0)`). Infer its type, unify it against `(arg_types) -> result`
        // (which pins a fresh closure param to a Func and checks the arity), and
        // return `result`. The lowered callee_expr carries this Func type, so the
        // HIR lowerer reads its result type for the call's type.
        if c.callee().is_none() {
            if let Some(callee_expr) = c.callee_expr() {
                let callee_ty = self.infer_expr(scope, &callee_expr);
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

    fn collect_args(&mut self, scope: ScopeId, args: &ArgList) -> Vec<Type> {
        args.args().map(|a| self.infer_expr(scope, &a)).collect()
    }

    /// Infer `receiver.method(args)` (M5, §16.2). Resolves the method against
    /// the built-in catalog by receiver type + name + arity, unifies the
    /// element-type variable with the receiver's element type, checks arg types,
    /// and returns the result type. Records the method-name range in
    /// `ref_types` for hover.
    fn infer_method_call(&mut self, scope: ScopeId, m: &MethodCallExpr) -> Type {
        // Infer the receiver's type.
        let receiver_ty = match m.receiver() {
            Some(r) => self.infer_expr(scope, &r),
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
                self.infer_expr(scope, arg);
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
                Some(et) => self.infer_expr_expected(scope, arg, et),
                None => self.infer_expr(scope, arg),
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
    /// annotation names an unknown type (already reported as N002 by resolution);
    /// the caller then falls back to inference.
    fn resolve_type(&mut self, ty: &praxis_ast::TypeRef) -> Option<Type> {
        let syntax = ty.syntax();
        // Dispatch on the underlying node kind: TYPE_REF, TUPLE_TYPE, or FN_TYPE.
        // The TypeRef wrapper's own kind is TYPE_REF; for tuple/fn we must look at
        // whether the node *contains* the structural children. Simplest: walk the
        // node and reconstruct. Type resolution is scope-independent (it reads
        // only the written type names, which are all built-in scalars).
        self.resolve_type_node(syntax)
    }

    fn resolve_type_node(&mut self, node: &praxis_syntax::SyntaxNode) -> Option<Type> {
        match node.kind() {
            SyntaxKind::TYPE_REF => {
                // A scalar name, unless it wraps a tuple/fn (parser may nest).
                // If it has child type nodes, recurse; else it is a scalar.
                let has_struct_child = node
                    .children()
                    .any(|c| matches!(c.kind(), SyntaxKind::TUPLE_TYPE | SyntaxKind::FN_TYPE));
                if has_struct_child {
                    return node.children().find_map(|c| self.resolve_type_node(&c));
                }
                // Collection type: `Vec[T]`, `Map[K, V]`, … The parser emits the
                // ctor name as its own nested TYPE_REF child (the `start_node` +
                // `finish_node` for the name, before the `start_node_at(cp)` wrap
                // reopens to cover the bracketed args), with the bracket args as
                // further TYPE_REF siblings. So the *first* TYPE_REF child is the
                // ctor name; the rest are the type arguments.
                let type_ref_children: Vec<_> = node
                    .children()
                    .filter(|c| c.kind() == SyntaxKind::TYPE_REF)
                    .collect();
                if type_ref_children.len() >= 2 {
                    // The ctor name is the first TYPE_REF child's Ident token;
                    // the remaining TYPE_REF children are the type args.
                    let name =
                        type_ref_children[0]
                            .children_with_tokens()
                            .find_map(|e| match e {
                                rowan::NodeOrToken::Token(t) if t.kind() == SyntaxKind::Ident => {
                                    Some(t.text().to_string())
                                }
                                _ => None,
                            })?;
                    let type_args: Vec<Type> = type_ref_children[1..]
                        .iter()
                        .map(|c| {
                            self.resolve_type_node(c)
                                .unwrap_or_else(|| self.db.fresh_var())
                        })
                        .collect();
                    return self.collection_from_name(&name, type_args);
                }
                // Scalar: the name is a direct Ident token of this node.
                let name = node.children_with_tokens().find_map(|e| match e {
                    rowan::NodeOrToken::Token(t) if t.kind() == SyntaxKind::Ident => {
                        Some(t.text().to_string())
                    }
                    _ => None,
                })?;
                self.scalar_from_name(&name)
            }
            SyntaxKind::TUPLE_TYPE => {
                let els: Vec<Type> = node
                    .children()
                    .filter(|c| {
                        matches!(
                            c.kind(),
                            SyntaxKind::TYPE_REF | SyntaxKind::TUPLE_TYPE | SyntaxKind::FN_TYPE
                        )
                    })
                    .map(|c| {
                        self.resolve_type_node(&c)
                            .unwrap_or_else(|| self.db.fresh_var())
                    })
                    .collect();
                Some(self.db.tuple(els))
            }
            SyntaxKind::FN_TYPE => {
                // An FN_TYPE node has a param-type group (TYPE_REF or TUPLE_TYPE)
                // and a result type, separated by `->`.
                let mut parts: Vec<Type> = node
                    .children()
                    .filter(|c| {
                        matches!(
                            c.kind(),
                            SyntaxKind::TYPE_REF | SyntaxKind::TUPLE_TYPE | SyntaxKind::FN_TYPE
                        )
                    })
                    .map(|c| {
                        self.resolve_type_node(&c)
                            .unwrap_or_else(|| self.db.fresh_var())
                    })
                    .collect();
                if parts.len() >= 2 {
                    let result = parts.pop().expect(">=2 elements");
                    // The first element is the param group: if it is a TUPLE_TYPE,
                    // its elements are the params; else it is a single param.
                    let params = self.flatten_param_group(parts.pop().expect(">=2 elements"));
                    Some(self.db.func(params, result))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Given a type that represents a parameter group, return the parameter
    /// types. `(A, B)` (a tuple type) flattens to `[A, B]`; a single type stays
    /// `[itself]`.
    fn flatten_param_group(&mut self, ty: Type) -> Vec<Type> {
        match self.db.data(self.db.follow(ty)).clone() {
            praxis_types::TypeData::Tuple(els) => els,
            other => vec![self.db.intern(other)],
        }
    }

    fn scalar_from_name(&mut self, name: &str) -> Option<Type> {
        let scalar = match name {
            "Int" => ScalarType::Int,
            "Text" => ScalarType::Text,
            "Bool" => ScalarType::Bool,
            "Char" => ScalarType::Char,
            "Never" => ScalarType::Never,
            "Unit" => return Some(self.db.unit()),
            _ => {
                // M7: user-declared struct types. If `name` is a registered
                // struct, return its type.
                return self.lookup_struct_type(name);
            }
        };
        Some(self.db.scalar(scalar))
    }

    /// Resolve a collection ctor name (e.g. `"Deque"`) to its [`CollectionCtor`].
    /// Used by builtin scheme seeding so each constructor name is callable as
    /// `Name()`. Returns `None` for non-collection names (including `Seq`, which
    /// is compiler-internal and never user-named).
    fn collection_ctor_for(name: &str) -> Option<praxis_types::CollectionCtor> {
        use praxis_types::CollectionCtor;
        Some(match name {
            "Vec" => CollectionCtor::Vec,
            "Deque" => CollectionCtor::Deque,
            "Map" => CollectionCtor::Map,
            "Set" => CollectionCtor::Set,
            "Counter" => CollectionCtor::Counter,
            "MinHeap" => CollectionCtor::MinHeap,
            "MaxHeap" => CollectionCtor::MaxHeap,
            "BitSet" => CollectionCtor::BitSet,
            "Grid" => CollectionCtor::Grid,
            "Range" => CollectionCtor::Range,
            _ => return None,
        })
    }

    /// Resolve a collection type name + args to a [`Type`] (§4.4, §11.2). M8
    /// opens the full collection set: every §6.1 ctor resolves to its
    /// [`CollectionCtor`]. `Seq` is compiler-internal (M8 WS8, §6.3) and is never
    /// user-named — it is rejected here so `Seq[T]` in source surfaces as an
    /// unknown type. Construction (`Vec[T]()`) is wired per workstream as each
    /// collection's runtime payload lands; the *type* resolves for all ctors so
    /// annotations and signatures can name them ahead of construction support.
    fn collection_from_name(&mut self, name: &str, args: Vec<Type>) -> Option<Type> {
        // `Seq` is compiler-internal (§6.3, M8 WS8); never user-named.
        if name == "Seq" {
            return None;
        }
        // `Option[T]` (M9): a type-arg application of the prelude `Option`
        // resolves to a fresh Option def carrying the element type. Registered
        // here (not via `register_enum` in the prelude) so each annotation site
        // gets its own def, which then unifies with any other Option[T] by name
        // (unify.rs M9 arm) and with `Some`/`None`-constructed values.
        if name == "Option" {
            let elem = args
                .into_iter()
                .next()
                .unwrap_or_else(|| self.db.fresh_var());
            return Some(self.db.register_enum(
                "Option",
                vec![("Some".into(), Some(vec![elem])), ("None".into(), None)],
            ));
        }
        let ctor = Self::collection_ctor_for(name)?;
        // Arity check: the ctor declares how many type args it takes. A wrong
        // arity is a type error surfaced as a unification failure downstream
        // (the args vec length won't match), so just pass them through here.
        Some(self.db.collection(ctor, args))
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
