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
    ArgList, AssignStmt, AstNode, BinExpr, BlockExpr, CallExpr, ElseBranch, EnumItem, Expr,
    ExprStmt, FieldExpr, FnItem, IfExpr, LetStmt, Literal, MethodCallExpr, Param, PathExpr,
    RecordLitExpr, SourceFile, StructItem, UnaryExpr, VarStmt, WhileExpr,
};
use praxis_source::{BytePos, Diagnostic, FileId, FileSpan, Span};
use praxis_syntax::SyntaxKind;
use praxis_types::{unify::UnifyError, ScalarType, Scheme, Type, TypeDb};
use rowan::TextRange;

use crate::diagnostics::{infinite_type, not_equatable, type_mismatch};
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

    // --- builtin schemes ---------------------------------------------------

    /// Assign polymorphic schemes to the prelude builtins that need them. `out`
    /// is `forall T. (T) -> Unit`; `panic` is `forall T. (T) -> Never`. The rest
    /// of the prelude (numeric helpers, collections) gets no scheme in M2 and
    /// references to them infer a fresh var (their dispatch lands in M5).
    fn seed_builtin_schemes(&mut self) {
        // Collect the ids first to avoid borrowing `self.names` while mutating it.
        let to_seed: Vec<(SymbolId, String)> = self
            .names
            .all()
            .iter()
            .filter(|s| {
                s.kind == SymbolKind::Builtin
                    && (s.name == "out" || s.name == "panic" || s.name == "Vec")
            })
            .map(|s| (s.id, s.name.clone()))
            .collect();
        for (id, name) in to_seed {
            let scheme = self.db.scoped_return(|db| {
                let mono = match name.as_str() {
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
                    "Vec" => {
                        // forall T. () -> Vec[T]
                        let v = db.fresh_var();
                        let vec_ty = db.vec(v);
                        db.func(vec![], vec_ty)
                    }
                    other => panic!("unexpected builtin `{other}` seeded"),
                };
                db.generalize(mono)
            });
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
        let rhs_ty = stmt.init().map(|e| self.infer_expr(scope, &e));
        let annot = stmt.ty().and_then(|t| self.resolve_type(&t));
        // Unify annotation with the inferred RHS, if both are present.
        if let (Some(a), Some(r)) = (annot, rhs_ty) {
            let at = stmt.syntax().text_range();
            if let Err(e) = self.db.unify(a, r) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        let body_ty = annot.or(rhs_ty).unwrap_or_else(|| self.db.fresh_var());
        self.db.exit_level(prev);
        // Generalize `let` bindings (§5.3).
        let scheme = self.db.generalize(body_ty);
        self.attach_scheme(stmt.name(), scheme);
    }

    fn infer_var(&mut self, scope: ScopeId, stmt: &VarStmt) {
        // `var` RHS is inferred but NOT generalized (§5.3).
        let prev = self.db.enter_level();
        let rhs_ty = stmt.init().map(|e| self.infer_expr(scope, &e));
        let annot = stmt.ty().and_then(|t| self.resolve_type(&t));
        if let (Some(a), Some(r)) = (annot, rhs_ty) {
            let at = stmt.syntax().text_range();
            if let Err(e) = self.db.unify(a, r) {
                self.diag_unify(self.file_span(at), e);
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
        let body_ty = item.body().map(|b| self.infer_block(body_scope, &b));
        // Unify declared return with the body's type (if both present).
        let result_ty = match (ret_annot, body_ty) {
            (Some(a), Some(b)) => {
                if let Err(e) = self.db.unify(a, b) {
                    let at = item.syntax().text_range();
                    self.diag_unify(self.file_span(at), e);
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
            Expr::Call(c) => self.infer_call(scope, c),
            Expr::MethodCall(m) => self.infer_method_call(scope, m),
            Expr::Read(r) => self.infer_read(r),
            Expr::Parse(p) => self.infer_parse(scope, p),
            Expr::RecordLit(r) => self.infer_record_lit(scope, r),
            Expr::FieldGet(f) => self.infer_field_get(scope, f),
            Expr::Match(m) => self.infer_match(scope, m),
            Expr::Error(_) => self.db.fresh_var(),
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
                    let at = b.syntax().text_range();
                    if let Err(e) = self.db.unify(l, int) {
                        self.diag_unify(self.file_span(at), e);
                    }
                    if let Err(e) = self.db.unify(r, int) {
                        self.diag_unify(self.file_span(at), e);
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
                    let at = b.syntax().text_range();
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
                    let at = b.syntax().text_range();
                    if let Err(e) = self.db.unify(l, bool) {
                        self.diag_unify(self.file_span(at), e);
                    }
                    if let Err(e) = self.db.unify(r, bool) {
                        self.diag_unify(self.file_span(at), e);
                    }
                }
                bool
            }
            _ => self.db.fresh_var(),
        }
    }

    fn infer_unary(&mut self, scope: ScopeId, u: &UnaryExpr) -> Type {
        let operand = u.operand().map(|e| self.infer_expr(scope, &e));
        let result = match u.op().map(|t| t.kind()) {
            Some(SyntaxKind::MINUS) => self.db.int(),
            Some(SyntaxKind::BANG) => self.db.bool(),
            _ => self.db.fresh_var(),
        };
        if let Some(o) = operand {
            let at = u.syntax().text_range();
            if let Err(e) = self.db.unify(o, result) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        result
    }

    fn infer_block(&mut self, scope: ScopeId, block: &BlockExpr) -> Type {
        let inner = self.scopes.push_child(scope);
        let mut last = self.db.unit();
        for child in block.stmts() {
            // A trailing expression statement is the block's value.
            if let Some(expr_stmt) = ExprStmt::cast(child.clone()) {
                if let Some(e) = expr_stmt.expr() {
                    last = self.infer_expr(inner, &e);
                    continue;
                }
            }
            self.infer_top_stmt(inner, &child);
        }
        last
    }

    fn infer_if(&mut self, scope: ScopeId, i: &IfExpr) -> Type {
        if let Some(cond) = i.cond() {
            let ct = self.infer_expr(scope, &cond);
            let bool = self.db.bool();
            let at = i.syntax().text_range();
            // Condition must be Bool: report "expected Bool, found <ct>".
            if let Err(e) = self.db.unify(bool, ct) {
                self.diag_unify(self.file_span(at), e);
            }
        }
        let then_ty = i.then_branch().map(|b| self.infer_block(scope, &b));
        let else_ty = i.else_branch().and_then(|e| self.infer_else(scope, &e));
        match (then_ty, else_ty) {
            (Some(t), Some(e)) => {
                if let Err(err) = self.db.unify(t, e) {
                    let at = i.syntax().text_range();
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
            let at = w.syntax().text_range();
            // Condition must be Bool.
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

    fn infer_call(&mut self, scope: ScopeId, c: &CallExpr) -> Type {
        // Collect argument types.
        let arg_types: Vec<Type> = c
            .arg_list()
            .map(|a| self.collect_args(scope, &a))
            .unwrap_or_default();
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
                        let expected = self.db.func(arg_types, result);
                        if let Err(e) = self.db.unify(callee_ty, expected) {
                            let at = c.syntax().text_range();
                            self.diag_unify(self.file_span(at), e);
                        }
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
        let arg_types: Vec<Type> = m
            .arg_list()
            .map(|a| self.collect_args(scope, &a))
            .unwrap_or_default();
        let arity = arg_types.len();

        // Record the method-name range for hover (the result type).
        if let Some(tok) = m.method_name() {
            self.ref_types.insert(tok.text_range(), receiver_ty);
        }

        // Look up the method in the catalog via the ADR-010 bridge.
        let hits = crate::catalog::lookup(&self.db, self.catalog, receiver_ty, &name, arity);
        let Some(entry) = hits.first().copied() else {
            // Unknown method: leave the result as a fresh var; the HIR lowerer
            // emits the Y110 diagnostic (it has the method-name span).
            return self.db.fresh_var();
        };

        // Unify the method's parameter types with the argument types. The
        // catalog's param patterns carry `Var("T")` for the element type; we
        // instantiate them as fresh vars, then unify the receiver's element
        // type against the first param's `T` (for `push`) so a `Vec[Int]`
        // only accepts `Int` arguments.
        let param_tys: Vec<Type> = entry
            .params
            .iter()
            .map(|p| crate::lower::pattern_to_type_pub(&mut self.db, p))
            .collect();
        for (pt, at) in param_tys.iter().zip(arg_types.iter()) {
            let _ = self.db.unify(*pt, *at);
        }
        // The result type.
        crate::lower::pattern_to_type_pub(&mut self.db, &entry.result)
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
                let name = node.children_with_tokens().find_map(|e| match e {
                    rowan::NodeOrToken::Token(t) if t.kind() == SyntaxKind::Ident => {
                        Some(t.text().to_string())
                    }
                    _ => None,
                })?;
                // Collection type: `Vec[T]`, `Map[K, V]`, … The parser emits the
                // name plus bracketed child TYPE_REF args as children of one node.
                let type_args: Vec<Type> = node
                    .children()
                    .filter(|c| c.kind() == SyntaxKind::TYPE_REF)
                    .map(|c| {
                        self.resolve_type_node(&c)
                            .unwrap_or_else(|| self.db.fresh_var())
                    })
                    .collect();
                if !type_args.is_empty() {
                    return self.collection_from_name(&name, type_args);
                }
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

    /// Resolve a collection type name + args to a [`Type`] (M5, §4.4). M5
    /// supports `Vec`; other ctors are reserved and return `None` (reported as
    /// an unknown type by resolution).
    fn collection_from_name(&mut self, name: &str, args: Vec<Type>) -> Option<Type> {
        let ctor = match name {
            "Vec" => praxis_types::CollectionCtor::Vec,
            _ => return None,
        };
        Some(self.db.collection(ctor, args))
    }
}
