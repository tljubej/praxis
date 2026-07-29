//! Name resolution: walk the typed syntax tree, build the scope tree, mint a
//! [`SymbolId`] per declaration, and resolve every name reference to its symbol
//! (§13.3).
//!
//! Shadowing (§4.2/§5.3): a `let`/`var` declaration's *initializer* is resolved
//! in the preceding environment, and only then does the new symbol enter scope.
//! So `let a = a + 1` resolves the right-hand `a` to the *previous* binding,
//! then introduces the new one. Each shadowing declaration gets a fresh
//! [`SymbolId`].
//!
//! This module does NOT infer types — it only resolves names and produces the
//! symbol table + reference map that inference (Slice 5) consumes. Type
//! annotations are validated for *known-ness* (`N002`) but not yet checked
//! against use.
//!
//! **Two-pass (M7):** resolution runs in two phases so that top-level type names
//! (`struct`/`enum`/`fn`) are visible *before* any type annotation is checked.
//! Pass 1 (`register_top_level`) seeds all top-level names; pass 2
//! (`resolve_top_stmt`) resolves bodies and annotations. For M7-WS1 this is
//! preparatory infrastructure — `struct`/`enum` items are not parsed until WS3,
//! but the closed `KNOWN_TYPE_NAMES` table is replaced with scope-based lookup
//! so WS3 can register user types the same way.

use std::collections::HashMap;

use praxis_ast::{
    ArgList, AssignStmt, AstNode, BinExpr, BlockExpr, BreakExpr, CallExpr, ContinueExpr,
    ElseBranch, EnumItem, Expr, ExprStmt, FieldExpr, FnItem, ForExpr, IfExpr, LetStmt, LoopExpr,
    Param, ParamList, RecordLitExpr, ReturnExpr, SourceFile, StructItem, VarStmt, WhileExpr,
};
use praxis_source::{BytePos, Diagnostic, FileId, FileSpan, Span};
use praxis_syntax::SyntaxKind;
use rowan::{NodeOrToken, TextRange};

use crate::diagnostics::{duplicate_declaration, nested_function, unknown_type, unresolved_name};
use crate::name_table::NameTable;
use crate::scope::{ScopeId, ScopeTree};
use crate::symbol::{Symbol, SymbolId, SymbolKind};

/// The built-in scalar type names that a type annotation may legitimately name
/// (§4.3). Reserved-but-unimplemented scalars (`UInt`, `Byte`) are deliberately
/// absent: using them yields `N002 unknown type`. `Char` is wired end-to-end in
/// M6 (the input parser produces it); `Float` is wired end-to-end (§4.12).
///
/// M7: these are now seeded into the root scope as `Builtin` symbols (see
/// [`Resolver::seed_type_names`]), so type-annotation validation consults the
/// scope tree rather than this constant directly. The constant is retained as
/// the seed source.
const KNOWN_TYPE_NAMES: &[&str] = &["Int", "Text", "Bool", "Char", "Float", "Unit", "Never"];

/// A name reference resolved at a source range. Inference later attaches the
/// inferred type to each.
#[derive(Clone, Debug)]
pub struct NameRef {
    pub symbol: SymbolId,
    pub range: TextRange,
}

/// The scope id active when a name reference was resolved. Needed by inference
/// to know the binding level at which to instantiate the symbol's scheme.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedRef {
    pub symbol: SymbolId,
    pub scope: ScopeId,
    pub range: TextRange,
}

/// The result of name resolution: the symbol table, the scope tree, the resolved
/// references, and any `N0xx` diagnostics. Type inference consumes and extends
/// this.
#[derive(Clone, Debug, Default)]
pub struct NameResolution {
    pub names: NameTable,
    pub scopes: ScopeTree,
    /// Each name *reference*, keyed by its source range. A reference that failed
    /// to resolve is simply absent (its diagnostic is in `diagnostics`).
    pub refs: HashMap<TextRange, ResolvedRef>,
    /// Each *declaration* site, keyed by the name token's source range → the
    /// [`SymbolId`] it mints. This lets inference attach the inferred scheme to
    /// the exact symbol even when the same name is shadowed (where a scope lookup
    /// would resolve to the wrong/latest binding).
    pub decls: HashMap<TextRange, SymbolId>,
    pub diagnostics: Vec<Diagnostic>,
}

impl NameResolution {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Resolve names in `file`'s parsed tree. Seeds the prelude (the built-in
/// functions and the type-name set) into the root scope first, then runs the
/// two-pass resolution (M7): pass 1 registers all top-level names so forward
/// references work; pass 2 resolves bodies and annotations.
#[must_use]
pub fn resolve(file: FileId, root: &SourceFile) -> NameResolution {
    let mut r = Resolver::new(file);
    let root_scope = r.out.scopes.root();
    r.seed_prelude(root_scope);
    // Pass 1: register all top-level declaration names (fn/let/var, and in WS3+
    // struct/enum) so they are visible before any annotation is checked.
    for stmt in root.stmts() {
        r.register_top_level(root_scope, &stmt);
    }
    // Pass 2: resolve bodies and annotations.
    for stmt in root.stmts() {
        r.resolve_top_stmt(root_scope, &stmt);
    }
    r.out
}

struct Resolver {
    file: FileId,
    out: NameResolution,
}

impl Resolver {
    fn new(file: FileId) -> Self {
        Resolver {
            file,
            out: NameResolution::default(),
        }
    }

    fn file_span(&self, span: Span) -> FileSpan {
        FileSpan::new(self.file, span)
    }

    // --- symbol + scope helpers --------------------------------------------

    fn mint(
        &mut self,
        kind: SymbolKind,
        name: impl Into<String>,
        span: Option<FileSpan>,
    ) -> SymbolId {
        self.out.names.insert(Symbol {
            id: SymbolId(0), // placeholder; NameTable::insert assigns the real id
            name: name.into(),
            kind,
            decl: span,
            scheme: None,
        })
    }

    fn bind(
        &mut self,
        scope: ScopeId,
        kind: SymbolKind,
        name: String,
        range: TextRange,
    ) -> SymbolId {
        let span = range_to_span(range);
        let id = self.mint(kind, name.clone(), Some(self.file_span(span)));
        self.out.scopes.bind(scope, name, id);
        // Record the declaration so inference can attach a scheme to exactly
        // this symbol (not whatever a name lookup would find under shadowing).
        self.out.decls.insert(range, id);
        id
    }

    fn lookup(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
        self.out.scopes.lookup(scope, name)
    }

    fn record_ref(&mut self, scope: ScopeId, symbol: SymbolId, range: TextRange) {
        self.out.refs.insert(
            range,
            ResolvedRef {
                symbol,
                scope,
                range,
            },
        );
    }

    /// Whether `item` is a direct child of the source file, which is the only
    /// place a `fn` may be declared. Asking the tree rather than "did pass 1
    /// declare it?" keeps this answer independent of TY-24's duplicate case,
    /// which also leaves a `fn` undeclared.
    fn is_top_level(&self, item: &FnItem) -> bool {
        item.syntax()
            .parent()
            .is_some_and(|p| p.kind() == SyntaxKind::SOURCE_FILE)
    }

    /// Record an unresolved name reference and emit `N001`.
    fn unresolved(&mut self, range: TextRange, name: &str) {
        let span = range_to_span(range);
        self.out
            .diagnostics
            .push(unresolved_name(self.file_span(span), name));
    }

    // --- prelude (§16.1) ----------------------------------------------------

    /// Seed the root scope with the prelude's function names and the built-in
    /// type names (the latter as builtin symbols so type annotations can resolve
    /// to them). M7: type names are now seeded here so `check_type_annotation`
    /// can validate them through scope lookup rather than a closed constant.
    fn seed_prelude(&mut self, root: ScopeId) {
        for entry in praxis_stdlib::PRELUDE {
            let id = self.mint(SymbolKind::Builtin, entry.name, None);
            self.out.scopes.bind(root, entry.name, id);
        }
        self.seed_type_names(root);
    }

    /// Seed the built-in scalar type names as `Builtin` symbols. Retained as a
    /// separate method so WS3/WS4 can add user `struct`/`enum` registrations
    /// alongside without touching `seed_prelude`.
    fn seed_type_names(&mut self, root: ScopeId) {
        for ty in KNOWN_TYPE_NAMES {
            let id = self.mint(SymbolKind::Builtin, (*ty).to_string(), None);
            self.out.scopes.bind(root, (*ty).to_string(), id);
        }
    }

    // --- pass 1: top-level name registration (M7) --------------------------

    /// Register a top-level declaration's *name* without resolving its body.
    /// This is pass 1 of the two-pass resolution: it makes `fn` (and, in WS3+,
    /// `struct`/`enum`) names visible before any type annotation is checked or
    /// any body is resolved, so forward references and mutual recursion work.
    ///
    /// Only `fn` names are registered here: `let`/`var` follow lexical order
    /// (their shadowing semantics require the initializer to resolve in the
    /// *preceding* environment, §5.3, so pre-registering would break that).
    fn register_top_level(&mut self, scope: ScopeId, node: &praxis_syntax::SyntaxNode) {
        if let Some(fn_) = FnItem::cast(node.clone()) {
            if let Some(name_tok) = fn_.name() {
                // A second `fn` of the same name is a redeclaration, not a
                // shadow: both would reach the backend and be emitted under one
                // JIT symbol (TY-24). Report it and keep the first, so the rest
                // of the file still resolves against something.
                if self.out.scopes.is_bound_here(scope, name_tok.text()) {
                    let span = range_to_span(name_tok.text_range());
                    let at = self.file_span(span);
                    self.out
                        .diagnostics
                        .push(duplicate_declaration(at, name_tok.text()));
                } else {
                    self.bind(
                        scope,
                        SymbolKind::Fn,
                        name_tok.text().to_string(),
                        name_tok.text_range(),
                    );
                }
            }
        }
        // M7-WS3: register struct type names so they're visible as type
        // annotations before any body resolves.
        if let Some(struct_) = StructItem::cast(node.clone()) {
            if let Some(name_tok) = struct_.name() {
                self.bind(
                    scope,
                    SymbolKind::Struct,
                    name_tok.text().to_string(),
                    name_tok.text_range(),
                );
            }
        }
        // M7-WS4: register enum type names + variant constructor names.
        if let Some(enum_) = EnumItem::cast(node.clone()) {
            if let Some(name_tok) = enum_.name() {
                self.bind(
                    scope,
                    SymbolKind::Enum,
                    name_tok.text().to_string(),
                    name_tok.text_range(),
                );
            }
            // Each variant is also a constructor name in scope (§4.6):
            // `Empty`, `Number(5)`, etc. are bare names the user writes.
            for v in enum_.variants() {
                if let Some(vname_tok) = v.name() {
                    self.bind(
                        scope,
                        SymbolKind::Fn, // variant constructors behave like fns
                        vname_tok.text().to_string(),
                        vname_tok.text_range(),
                    );
                }
            }
        }
    }

    // --- pass 2: top-level statements --------------------------------------

    fn resolve_top_stmt(&mut self, scope: ScopeId, node: &praxis_syntax::SyntaxNode) {
        if let Some(let_) = LetStmt::cast(node.clone()) {
            self.resolve_let(scope, &let_);
        } else if let Some(var_) = VarStmt::cast(node.clone()) {
            self.resolve_var(scope, &var_);
        } else if let Some(fn_) = FnItem::cast(node.clone()) {
            self.resolve_fn(scope, &fn_);
        } else if let Some(struct_) = StructItem::cast(node.clone()) {
            self.resolve_struct(scope, &struct_);
        } else if let Some(enum_) = EnumItem::cast(node.clone()) {
            self.resolve_enum(scope, &enum_);
        } else if let Some(assign) = AssignStmt::cast(node.clone()) {
            self.resolve_assign(scope, &assign);
        } else if let Some(expr) = ExprStmt::cast(node.clone()) {
            if let Some(e) = expr.expr() {
                self.resolve_expr(scope, &e);
            }
        }
    }

    /// Resolve a `struct Name { field: Type, … }` declaration (M7, §4.5). The
    /// name was already registered in pass 1; here we validate the field types.
    fn resolve_struct(&mut self, scope: ScopeId, item: &StructItem) {
        if let Some(fields) = item.field_list() {
            for f in fields.fields() {
                if let Some(ty) = f.ty() {
                    self.check_type_annotation(scope, &ty);
                }
            }
        }
    }

    /// Resolve an `enum Name { Variant, Variant(Type), … }` declaration (M7,
    /// §4.6). The name + variant constructors were registered in pass 1; here we
    /// validate the variant payload types.
    fn resolve_enum(&mut self, scope: ScopeId, item: &EnumItem) {
        for v in item.variants() {
            if let Some(payload_types) = v.payload_types() {
                for ty in payload_types {
                    self.check_type_annotation(scope, &ty);
                }
            }
        }
    }

    fn resolve_let(&mut self, scope: ScopeId, stmt: &LetStmt) {
        // The initializer resolves in the PRECEDING environment (shadowing rule).
        if let Some(ty) = stmt.ty() {
            self.check_type_annotation(scope, &ty);
        }
        if let Some(init) = stmt.init() {
            self.resolve_expr(scope, &init);
        }
        // Only now does the new binding enter scope.
        if let Some(name_tok) = stmt.name() {
            self.bind(
                scope,
                SymbolKind::Let,
                name_tok.text().to_string(),
                name_tok.text_range(),
            );
        } else {
            // Malformed `let` (no name) — the parser already diagnosed it.
        }
    }

    fn resolve_var(&mut self, scope: ScopeId, stmt: &VarStmt) {
        if let Some(ty) = stmt.ty() {
            self.check_type_annotation(scope, &ty);
        }
        if let Some(init) = stmt.init() {
            self.resolve_expr(scope, &init);
        }
        if let Some(name_tok) = stmt.name() {
            self.bind(
                scope,
                SymbolKind::Var,
                name_tok.text().to_string(),
                name_tok.text_range(),
            );
        }
    }

    fn resolve_fn(&mut self, scope: ScopeId, item: &FnItem) {
        // Only a top-level `fn` was registered in pass 1. One inside a block was
        // parsed but never declared, and inference then reached an `expect` on
        // the missing declaration and panicked (TY-23). Report it here — where
        // the nesting is visible — and carry on resolving the body, so the rest
        // of the file still reports.
        if !self.is_top_level(item) {
            if let Some(name_tok) = item.name() {
                let span = range_to_span(name_tok.text_range());
                let at = self.file_span(span);
                self.out
                    .diagnostics
                    .push(nested_function(at, name_tok.text()));
            }
        }
        // A function's name was registered in pass 1 (`register_top_level`) so
        // it is visible for forward references and mutual recursion. Reuse that
        // symbol rather than re-binding (which would create a duplicate).
        let fn_symbol = if let Some(name_tok) = item.name() {
            self.out.decls.get(&name_tok.text_range()).copied()
        } else {
            None
        };
        // Validate annotations on params and return type.
        if let Some(pl) = item.param_list() {
            for p in pl.params() {
                if let Some(ty) = p.ty() {
                    self.check_type_annotation(scope, &ty);
                }
            }
        }
        if let Some(ret) = item.return_type() {
            self.check_type_annotation(scope, &ret);
        }
        // Body scope: params are bound here, and the fn name is reachable (for
        // recursion) via the enclosing scope.
        let body_scope = self.out.scopes.push_child(scope);
        if let Some(pl) = item.param_list() {
            self.bind_params(body_scope, &pl);
        }
        if let Some(body) = item.body() {
            self.resolve_block_inner(body_scope, &body);
        }
        let _ = fn_symbol;
    }

    fn bind_params(&mut self, scope: ScopeId, pl: &ParamList) {
        for p in pl.params() {
            self.bind_param(scope, &p);
        }
    }

    fn bind_param(&mut self, scope: ScopeId, p: &Param) {
        if let Some(name_tok) = p.name() {
            self.bind(
                scope,
                SymbolKind::Param,
                name_tok.text().to_string(),
                name_tok.text_range(),
            );
        }
    }

    fn resolve_assign(&mut self, scope: ScopeId, stmt: &AssignStmt) {
        // The lhs name is a reference (not a declaration).
        if let Some(name_tok) = stmt.name() {
            self.resolve_name_ref(scope, &name_tok);
        }
        if let Some(value) = stmt.value() {
            self.resolve_expr(scope, &value);
        }
    }

    // --- expressions -------------------------------------------------------

    fn resolve_expr(&mut self, scope: ScopeId, expr: &Expr) {
        match expr {
            Expr::Literal(_) => {}
            Expr::Path(p) => {
                if let Some(name_tok) = p.name() {
                    self.resolve_name_ref(scope, &name_tok);
                }
            }
            Expr::Bin(b) => self.resolve_bin(scope, b),
            Expr::Unary(u) => {
                if let Some(operand) = u.operand() {
                    self.resolve_expr(scope, &operand);
                }
            }
            Expr::Paren(p) => {
                if let Some(inner) = p.expr() {
                    self.resolve_expr(scope, &inner);
                }
            }
            Expr::Tuple(t) => {
                for el in t.elements() {
                    self.resolve_expr(scope, &el);
                }
            }
            Expr::Block(b) => self.resolve_block(scope, b),
            Expr::If(i) => self.resolve_if(scope, i),
            Expr::While(w) => self.resolve_while(scope, w),
            Expr::For(f) => self.resolve_for(scope, f),
            Expr::Loop(l) => self.resolve_loop(scope, l),
            Expr::Break(b) => self.resolve_break(scope, b),
            Expr::Continue(c) => self.resolve_continue(scope, c),
            Expr::Return(r) => self.resolve_return(scope, r),
            Expr::Call(c) => self.resolve_call(scope, c),
            Expr::MethodCall(m) => {
                // Resolve names in the receiver and the arguments. Method names
                // themselves are resolved against the catalog during inference,
                // not name resolution (they are not scope bindings).
                if let Some(receiver) = m.receiver() {
                    self.resolve_expr(scope, &receiver);
                }
                if let Some(args) = m.arg_list() {
                    for arg in args.args() {
                        self.resolve_expr(scope, &arg);
                    }
                }
            }
            Expr::Error(_) => {}
            // M6 WS5: resolve read/parse sub-expressions (parser_expr has no
            // ordinary names to resolve; parse's text arg does).
            Expr::Read(_) => {}
            Expr::Parse(p) => {
                if let Some(text_expr) = p.text_expr() {
                    self.resolve_expr(scope, &text_expr);
                }
            }
            // M7-WS3: record literal — resolve the struct name (a type reference)
            // and each field initializer.
            Expr::RecordLit(r) => self.resolve_record_lit(scope, r),
            // M7-WS3: field access — resolve the receiver. The field name is not
            // a scope binding; it's resolved against the struct type during
            // inference.
            Expr::FieldGet(f) => self.resolve_field_get(scope, f),
            // M7-WS5: match — resolve the scrutinee and each arm's body. Pattern
            // variable bindings enter a child scope.
            Expr::Match(m) => self.resolve_match(scope, m),
            // M7-WS7: closure — params bind in a child scope; the body resolves
            // there (capturing outer-scope names naturally through the scope chain).
            Expr::Closure(c) => self.resolve_closure(scope, c),
        }
    }

    /// Resolve a `|params| expr` closure (M7, §4.10). Params bind in a child
    /// scope; the body resolves in that scope. Outer names are captured
    /// automatically through the scope chain (a free var in the body resolves to
    /// an enclosing binding).
    fn resolve_closure(&mut self, scope: ScopeId, c: &praxis_ast::ClosureExpr) {
        let body_scope = self.out.scopes.push_child(scope);
        for p in c.params() {
            self.bind_param(body_scope, &p);
        }
        if let Some(body) = c.body() {
            self.resolve_expr(body_scope, &body);
        }
    }

    /// Resolve a `match scrutinee { pattern => body, … }` expression (M7, §4.6).
    fn resolve_match(&mut self, scope: ScopeId, m: &praxis_ast::MatchExpr) {
        // Resolve the scrutinee.
        if let Some(scrutinee) = m.scrutinee() {
            self.resolve_expr(scope, &scrutinee);
        }
        // Each arm opens a child scope for its pattern bindings.
        for arm in m.arms() {
            let arm_scope = self.out.scopes.push_child(scope);
            if let Some(pat) = arm.pattern() {
                self.resolve_pattern_bindings(arm_scope, &pat);
            }
            if let Some(body) = arm.body() {
                self.resolve_expr(arm_scope, &body);
            }
        }
    }

    /// Bind any variable names introduced by a pattern in `scope` (M7, §4.6).
    /// A `Name(x)` pattern introduces a binding `x`; a `Variant(Sub)` recurses
    /// into sub-patterns.
    fn resolve_pattern_bindings(&mut self, scope: ScopeId, pat: &praxis_ast::Pattern) {
        match pat.kind() {
            praxis_ast::PatternKind::Wildcard | praxis_ast::PatternKind::Literal => {}
            praxis_ast::PatternKind::Name(name) => {
                if let Some(tok) = pat.name_token() {
                    self.bind(scope, SymbolKind::Let, name, tok.text_range());
                }
            }
            praxis_ast::PatternKind::Variant(_) => {
                for sub in pat.sub_patterns() {
                    self.resolve_pattern_bindings(scope, &sub);
                }
            }
        }
    }

    /// Resolve names inside a record literal `Name { field: expr, … }`.
    fn resolve_record_lit(&mut self, scope: ScopeId, r: &RecordLitExpr) {
        // The struct name is a type reference — look it up so inference knows
        // which struct. It resolves like any other name.
        if let Some(name) = r.name() {
            if let Some(tok) = name.name() {
                self.resolve_name_ref(scope, &tok);
            }
        }
        if let Some(fields) = r.field_list() {
            for f in fields.fields() {
                match f.expr() {
                    // Explicit field: `{ x: expr }` — resolve the expression.
                    Some(e) => self.resolve_expr(scope, &e),
                    // Punned field: `{ x }` — resolve the field name as a
                    // reference to the binding `x` (like a PathExpr).
                    None => {
                        if let Some(name_tok) = f.name() {
                            self.resolve_name_ref(scope, &name_tok);
                        }
                    }
                }
            }
        }
    }

    /// Resolve names inside a field access `receiver.field`.
    fn resolve_field_get(&mut self, scope: ScopeId, f: &FieldExpr) {
        if let Some(receiver) = f.receiver() {
            self.resolve_expr(scope, &receiver);
        }
    }

    fn resolve_bin(&mut self, scope: ScopeId, b: &BinExpr) {
        let (lhs, rhs) = b.operands();
        if let Some(l) = lhs {
            self.resolve_expr(scope, &l);
        }
        if let Some(r) = rhs {
            self.resolve_expr(scope, &r);
        }
    }

    fn resolve_block(&mut self, scope: ScopeId, block: &BlockExpr) {
        // A block opens a new child scope for its locals.
        let inner = self.out.scopes.push_child(scope);
        self.resolve_block_inner(inner, block);
    }

    fn resolve_block_inner(&mut self, scope: ScopeId, block: &BlockExpr) {
        for child in block.stmts() {
            self.resolve_top_stmt(scope, &child);
        }
    }

    fn resolve_if(&mut self, scope: ScopeId, i: &IfExpr) {
        if let Some(cond) = i.cond() {
            self.resolve_expr(scope, &cond);
        }
        // Each branch is its own block (own scope).
        if let Some(then_b) = i.then_branch() {
            self.resolve_block(scope, &then_b);
        }
        if let Some(else_b) = i.else_branch() {
            self.resolve_else(scope, &else_b);
        }
    }

    fn resolve_else(&mut self, scope: ScopeId, e: &ElseBranch) {
        if let Some(body) = e.body() {
            match body {
                Expr::Block(b) => self.resolve_block(scope, &b),
                other => {
                    // `else if` — the nested if shares the else's scope chain.
                    let inner = self.out.scopes.push_child(scope);
                    self.resolve_expr(inner, &other);
                }
            }
        }
    }

    fn resolve_while(&mut self, scope: ScopeId, w: &WhileExpr) {
        if let Some(cond) = w.cond() {
            self.resolve_expr(scope, &cond);
        }
        if let Some(body) = w.body() {
            self.resolve_block(scope, &body);
        }
    }

    /// `for binding in iter { body }` (M8, §4.11). The iterator resolves in the
    /// current scope; the binding is declared in a child scope that wraps the
    /// body (so the loop variable is visible only inside the body).
    fn resolve_for(&mut self, scope: ScopeId, f: &ForExpr) {
        if let Some(iter) = f.iter() {
            self.resolve_expr(scope, &iter);
        }
        let body_scope = self.out.scopes.push_child(scope);
        if let Some(name_tok) = f.binding() {
            self.bind(
                body_scope,
                SymbolKind::Let,
                name_tok.text().to_string(),
                name_tok.text_range(),
            );
        }
        if let Some(body) = f.body() {
            self.resolve_block_inner(body_scope, &body);
        }
    }

    fn resolve_loop(&mut self, scope: ScopeId, l: &LoopExpr) {
        if let Some(body) = l.body() {
            self.resolve_block(scope, &body);
        }
    }

    fn resolve_break(&mut self, scope: ScopeId, b: &BreakExpr) {
        if let Some(v) = b.value() {
            self.resolve_expr(scope, &v);
        }
    }

    fn resolve_continue(&mut self, _scope: ScopeId, _c: &ContinueExpr) {
        // `continue` has no subexpressions or names.
    }

    fn resolve_return(&mut self, scope: ScopeId, r: &ReturnExpr) {
        if let Some(v) = r.value() {
            self.resolve_expr(scope, &v);
        }
    }

    fn resolve_call(&mut self, scope: ScopeId, c: &CallExpr) {
        // The callee is a name reference (named call `f(args)`), or — for a
        // postfix call on an arbitrary expression (`expr(args)`, M8 §4.10) — an
        // expression callee (e.g. `fs.get(0)` in `fs.get(0)(100)`, or a paren'd
        // closure `(|x| x)(14)`). Resolve whichever is present; the expression
        // callee must be resolved so its nested bindings (closure params,
        // captures) are declared.
        if let Some(callee) = c.callee() {
            if let Some(name_tok) = callee.name() {
                self.resolve_name_ref(scope, &name_tok);
            }
        } else if let Some(callee_expr) = c.callee_expr() {
            self.resolve_expr(scope, &callee_expr);
        }
        if let Some(args) = c.arg_list() {
            self.resolve_args(scope, &args);
        }
    }

    fn resolve_args(&mut self, scope: ScopeId, args: &ArgList) {
        for arg in args.args() {
            self.resolve_expr(scope, &arg);
        }
    }

    // --- name references ---------------------------------------------------

    /// Resolve a bare `Ident` token used as a reference. Looks it up, records the
    /// resolved ref, or emits `N001`.
    fn resolve_name_ref(&mut self, scope: ScopeId, tok: &praxis_syntax::SyntaxToken) {
        let range = tok.text_range();
        let name = tok.text().to_string();
        match self.lookup(scope, &name) {
            Some(symbol) => self.record_ref(scope, symbol, range),
            None => self.unresolved(range, &name),
        }
    }

    // --- type annotations --------------------------------------------------

    /// Walk a type annotation and emit `N002` for any name that is not a known
    /// type. M7: known types are resolved through the scope tree (built-in
    /// scalars are seeded as `Builtin` symbols; user `struct`/`enum` names will
    /// be registered in WS3/WS4). Structural type nodes (tuples, function types)
    /// are recursed into; the leaves are the `Ident` names.
    fn check_type_annotation(&mut self, scope: ScopeId, ty: &praxis_ast::TypeRef) {
        let syntax = ty.syntax();
        for tok in syntax.descendants_with_tokens().filter_map(|e| match e {
            NodeOrToken::Token(t) if t.kind() == SyntaxKind::Ident => Some(t),
            _ => None,
        }) {
            let name = tok.text();
            // A type name is valid if it resolves in scope (built-in or
            // user-declared). Collection constructors (Vec, Map, …) are also
            // valid type names — they're handled in inference's resolve_type.
            if self.lookup(scope, name).is_none() && !is_collection_ctor_name(name) {
                let span = range_to_span(tok.text_range());
                self.out
                    .diagnostics
                    .push(unknown_type(self.file_span(span), name));
            }
        }
    }
}

/// Bridge a rowan `TextRange` back into a Praxis [`Span`] (the only place the
/// resolver crosses the rowan/praxis boundary).
fn range_to_span(range: TextRange) -> Span {
    Span::new(
        BytePos::from(u32::from(range.start())),
        BytePos::from(u32::from(range.end())),
    )
}

/// Whether `name` is a built-in collection constructor (§4.4: `Vec`, `Map`,
/// `Set`, …). These are valid type-annotation names that are *not* seeded as
/// scope symbols (they're handled specially in inference's `resolve_type`),
/// so `check_type_annotation` must accept them directly.
fn is_collection_ctor_name(name: &str) -> bool {
    matches!(
        name,
        "Vec"
            | "Deque"
            | "Map"
            | "Set"
            | "Counter"
            | "MinHeap"
            | "MaxHeap"
            | "BitSet"
            | "Grid"
            | "Range"
            | "Option"
    )
}
