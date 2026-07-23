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

use std::collections::HashMap;

use praxis_ast::{
    ArgList, AssignStmt, AstNode, BinExpr, BlockExpr, CallExpr, ElseBranch, Expr, ExprStmt, FnItem,
    IfExpr, LetStmt, Param, ParamList, SourceFile, VarStmt, WhileExpr,
};
use praxis_source::{BytePos, Diagnostic, FileId, FileSpan, Span};
use praxis_syntax::SyntaxKind;
use rowan::{NodeOrToken, TextRange};

use crate::diagnostics::{unknown_type, unresolved_name};
use crate::name_table::NameTable;
use crate::scope::{ScopeId, ScopeTree};
use crate::symbol::{Symbol, SymbolId, SymbolKind};

/// The built-in scalar type names that a type annotation may legitimately name
/// (§4.3). Reserved-but-unimplemented scalars (`Float`, `UInt`, `Byte`) are
/// deliberately absent: using them yields `N002 unknown type`. `Char` is wired
/// end-to-end in M6 (the input parser produces it).
const KNOWN_TYPE_NAMES: &[&str] = &["Int", "Text", "Bool", "Char", "Unit", "Never"];

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
/// functions and the type-name set) into the root scope first.
#[must_use]
pub fn resolve(file: FileId, root: &SourceFile) -> NameResolution {
    let mut r = Resolver::new(file);
    r.seed_prelude();
    let root_scope = r.out.scopes.root();
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
    /// to them).
    fn seed_prelude(&mut self) {
        let root = self.out.scopes.root();
        for entry in praxis_stdlib::PRELUDE {
            let id = self.mint(SymbolKind::Builtin, entry.name, None);
            self.out.scopes.bind(root, entry.name, id);
        }
        for ty in KNOWN_TYPE_NAMES {
            let id = self.mint(SymbolKind::Builtin, (*ty).to_string(), None);
            self.out.scopes.bind(root, (*ty).to_string(), id);
        }
    }

    // --- top-level statements ----------------------------------------------

    fn resolve_top_stmt(&mut self, scope: ScopeId, node: &praxis_syntax::SyntaxNode) {
        if let Some(let_) = LetStmt::cast(node.clone()) {
            self.resolve_let(scope, &let_);
        } else if let Some(var_) = VarStmt::cast(node.clone()) {
            self.resolve_var(scope, &var_);
        } else if let Some(fn_) = FnItem::cast(node.clone()) {
            self.resolve_fn(scope, &fn_);
        } else if let Some(assign) = AssignStmt::cast(node.clone()) {
            self.resolve_assign(scope, &assign);
        } else if let Some(expr) = ExprStmt::cast(node.clone()) {
            if let Some(e) = expr.expr() {
                self.resolve_expr(scope, &e);
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
        // A function's name is visible inside its own body only AFTER the
        // initializer... but functions are special: the name must be visible
        // inside the body for recursion. Per §4.9/§5.3, recursive groups are
        // inferred together and the name is in scope for the body. So bind the
        // name first, then resolve params + body in a child scope.
        let fn_symbol = if let Some(name_tok) = item.name() {
            let id = self.bind(
                scope,
                SymbolKind::Fn,
                name_tok.text().to_string(),
                name_tok.text_range(),
            );
            Some(id)
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

    fn resolve_call(&mut self, scope: ScopeId, c: &CallExpr) {
        // The callee is a name reference.
        if let Some(callee) = c.callee() {
            if let Some(name_tok) = callee.name() {
                self.resolve_name_ref(scope, &name_tok);
            }
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
    /// built-in type. (Structural type nodes — tuples, function types — are
    /// recursed into; the leaves are the `Ident` names.)
    fn check_type_annotation(&mut self, scope: ScopeId, ty: &praxis_ast::TypeRef) {
        let syntax = ty.syntax();
        for tok in syntax.descendants_with_tokens().filter_map(|e| match e {
            NodeOrToken::Token(t) if t.kind() == SyntaxKind::Ident => Some(t),
            _ => None,
        }) {
            let name = tok.text();
            if !KNOWN_TYPE_NAMES.contains(&name) {
                // It might be a user type from a later milestone; for M2 everything
                // non-builtin is unknown.
                let span = range_to_span(tok.text_range());
                self.out
                    .diagnostics
                    .push(unknown_type(self.file_span(span), name));
            }
        }
        let _ = scope;
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
