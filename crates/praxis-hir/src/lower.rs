//! The typed tree: lower `Analysis` + the lossless AST into a tree that carries
//! an inferred [`Type`](praxis_types::Type) on every node (Milestone 4, ADR-014).
//!
//! Why this exists: M2's [`Analysis`](crate::Analysis) attaches types only to
//! *name-reference* ranges (`ref_types`), not to every subexpression. The JIT
//! backend needs the type of every node — literals, binops, calls, … — inline,
//! without re-running unification. This pass re-walks the AST in "read mode"
//! against the finalized [`TypeDb`]: it *derives* each node's type by recursing
//! (mirroring the inference rules in `infer`) and looking up symbol schemes,
//! never by unifying. It also rejects constructs the M4 backend cannot yet
//! lower (generic functions, reserved-but-unimplemented scalars) with `Y1xx`
//! diagnostics so the CLI never feeds malformed input to the JIT.
//!
//! `analyze` is unchanged — this is a pure consumer of its output.

#![allow(dead_code)] // Consumed by the M4 lowering (praxis-mir); not all variants
                     // are matched until M5/M6 features land.

use std::collections::HashMap;

use praxis_ast::{
    ArgList, AssignStmt, AstNode, BinExpr, BlockExpr, CallExpr, ElseBranch, Expr, ExprStmt, FnItem,
    IfExpr, LetStmt, Literal, MethodCallExpr, Param, ParamList, PathExpr, SourceFile, TupleExpr,
    UnaryExpr, VarStmt, WhileExpr,
};
use praxis_source::{Diagnostic, DiagnosticCategory, DiagnosticCode, FileSpan, Severity, Span};
use praxis_stdlib::type_pattern::ScalarType as PatternScalar;
use praxis_stdlib::TypePattern;
use praxis_syntax::{SyntaxKind, SyntaxNode};
use praxis_types::{Type, TypeDb};
use rowan::TextRange;

use crate::{Analysis, ResolvedRef, ScopeTree, SymbolId};

// ---------------------------------------------------------------------------
// The typed tree
// ---------------------------------------------------------------------------

/// A whole lowered file: its top-level items and any lowering diagnostics.
#[derive(Debug)]
pub struct TypedModule {
    /// The top-level function declarations, in source order.
    pub items: Vec<TypedItem>,
    /// `Y1xx` diagnostics emitted during lowering (generic-fn rejection,
    /// unsupported construct, …). Empty for a fully lowerable module.
    pub diagnostics: Vec<Diagnostic>,
}

/// A top-level item.
#[derive(Debug)]
pub enum TypedItem {
    /// A `fn name(params) -> Ret { body }`.
    Fn(TypedFn),
}

/// A lowered function.
#[derive(Debug)]
pub struct TypedFn {
    /// The function's symbol id (so the backend can mint a stable name).
    pub symbol: SymbolId,
    /// The source name as written.
    pub name: String,
    /// The parameters, in order.
    pub params: Vec<TypedParam>,
    /// The declared/inferred return type.
    pub return_type: Type,
    /// The body block.
    pub body: TypedBlock,
    /// The whole function's type `(P0, …) -> R`.
    pub fn_type: Type,
}

/// A parameter `name: Type`.
#[derive(Debug)]
pub struct TypedParam {
    pub symbol: SymbolId,
    pub name: String,
    pub ty: Type,
}

/// A `{ stmt; …; tail }` block. `tail` is the block's value (`Unit` if absent).
#[derive(Debug)]
pub struct TypedBlock {
    pub stmts: Vec<TypedStmt>,
    /// The trailing expression, lowered as a statement; `Unit` typed if none.
    pub tail: TypedExpr,
    /// The block's value type (the tail's type, or Unit).
    pub ty: Type,
}

/// A statement.
#[derive(Debug)]
pub enum TypedStmt {
    /// `let name = expr` (immutable binding).
    Let {
        symbol: SymbolId,
        name: String,
        ty: Type,
        init: TypedExpr,
    },
    /// `var name = expr` (mutable binding).
    Var {
        symbol: SymbolId,
        name: String,
        ty: Type,
        init: TypedExpr,
    },
    /// `name = expr` / `name += expr` / …
    Assign {
        symbol: SymbolId,
        name: String,
        op: AssignOp,
        value: TypedExpr,
    },
    /// A bare expression evaluated for effect.
    Expr(TypedExpr),
}

/// The assignment operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    /// `=` (plain).
    Assign,
    /// `+=`.
    AddAssign,
    /// `-=`.
    SubAssign,
    /// `*=`.
    MulAssign,
    /// `/=`.
    DivAssign,
    /// `%=`.
    RemAssign,
}

/// A typed expression. Every variant carries its inferred `ty`.
#[derive(Debug)]
pub enum TypedExpr {
    /// An integer / text / bool literal.
    Lit { value: Lit, ty: Type },
    /// A name reference (variable, parameter, function).
    Path { symbol: SymbolId, ty: Type },
    /// `a op b`.
    Bin {
        op: BinOp,
        lhs: Box<TypedExpr>,
        rhs: Box<TypedExpr>,
        ty: Type,
    },
    /// `op a`.
    Unary {
        op: UnaryOp,
        operand: Box<TypedExpr>,
        ty: Type,
    },
    /// `( a )` — transparent; lowered to the inner expression directly, so this
    /// variant exists only when the inner expr is missing (malformed).
    Paren {
        inner: Option<Box<TypedExpr>>,
        ty: Type,
    },
    /// A block used as an expression.
    Block(Box<TypedBlock>),
    /// `if cond { then } else { else }`.
    If {
        cond: Box<TypedExpr>,
        then_block: Box<TypedBlock>,
        else_block: Option<Box<TypedBlock>>,
        ty: Type,
    },
    /// `while cond { body }` — always `Unit`.
    While {
        cond: Box<TypedExpr>,
        body: Box<TypedBlock>,
        ty: Type,
    },
    /// `callee(args)`.
    Call {
        callee: SymbolId,
        /// The callee's source name, resolved during HIR lowering so the MIR
        /// builder (and the JIT) can name the target without a NameTable.
        callee_name: String,
        args: Vec<TypedExpr>,
        ty: Type,
    },
    /// `receiver.method(args)` (M5, §16.2). `lowering_symbol` is the runtime
    /// wrapper name the catalog resolved (e.g. `praxis_vec_push`), so the MIR
    /// builder emits a direct call without re-resolving the catalog.
    MethodCall {
        receiver: Box<TypedExpr>,
        name: String,
        lowering_symbol: String,
        args: Vec<TypedExpr>,
        ty: Type,
    },
    /// `( a, b, … )` — at least two elements.
    Tuple { elements: Vec<TypedExpr>, ty: Type },
}

/// A literal value. (M4 lowers Int/Bool/Unit; Text materializes via the runtime
/// text descriptor. M6 adds Char for the input parser's `char`/`grid(char)`.)
#[derive(Clone, Debug)]
pub enum Lit {
    Int(i64),
    Text(String),
    Bool(bool),
    /// A Unicode scalar value (the payload of a `Char` object).
    Char(u32),
}

/// Binary operators, carrying the §4.12 semantics the backend needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    // Arithmetic (Int -> Int -> Int), overflow-checked.
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    // Comparison (a -> a -> Bool).
    Eq,
    Neq,
    Lt,
    Gt,
    Le,
    Ge,
    // Logical (Bool -> Bool -> Bool).
    LogicalOr,
}

/// Unary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// Negation (Int -> Int).
    Neg,
    /// Logical not (Bool -> Bool).
    Not,
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

/// Lower a fully analyzed file into a typed tree.
///
/// `analysis` must be the result of [`analyze`](crate::analyze) on `root`; pass
/// the same `file` id so diagnostics carry correct spans. Never panics —
/// unsupported constructs become `Y1xx` diagnostics in the returned module.
///
/// Takes `analysis` mutably because instantiating schemes (to read concrete
/// shapes) allocates fresh type slots in [`TypeDb`]. The analysis's prior
/// results are preserved — only the arena grows.
#[must_use]
pub fn lower(
    file: praxis_source::FileId,
    root: &SourceFile,
    analysis: &mut Analysis,
) -> TypedModule {
    // Split-borrow Analysis so we can mutate `db` while reading the rest.
    let Analysis {
        db,
        names,
        scopes,
        refs,
        decls,
        ref_types: _,
        diagnostics: _,
    } = analysis;
    // Cache the scalar/unit handles once (these methods need &mut db).
    let int = db.int();
    let bool_ = db.bool();
    let text = db.text();
    let unit = db.unit();
    let mut l = Lowerer {
        file,
        db,
        names,
        scopes,
        refs,
        decls,
        diagnostics: Vec::new(),
        catalog: builtin_catalog(),
        int,
        bool_,
        text,
        unit,
    };
    let mut items = Vec::new();
    for node in root.stmts() {
        if let Some(fn_item) = FnItem::cast(node.clone()) {
            if let Some(tfn) = l.lower_fn(&fn_item) {
                items.push(TypedItem::Fn(tfn));
            }
        }
        // Top-level `let`/`var`/`expr`/`assign` are not lowered yet — M4 only
        // JITs `fn` items (the entry point is a `fn main` or similar). They are
        // still type-checked by `analyze`; they simply have no runtime lowering.
    }
    TypedModule {
        items,
        diagnostics: l.diagnostics,
    }
}

struct Lowerer<'a> {
    file: praxis_source::FileId,
    db: &'a mut TypeDb,
    names: &'a crate::NameTable,
    scopes: &'a ScopeTree,
    refs: &'a HashMap<TextRange, ResolvedRef>,
    /// Declaration-site ranges → SymbolId (from resolution; survives shadowing).
    decls: &'a HashMap<TextRange, SymbolId>,
    diagnostics: Vec<Diagnostic>,
    /// The built-in method catalog (§16.2), used to resolve `receiver.method()`
    /// calls to their runtime lowering symbol. Immutable; built once.
    catalog: &'static praxis_stdlib::MethodCatalog,
    /// Cached handles for the common scalar/unit types, so the lowering does not
    /// allocate a fresh slot on every literal/binop (which would need `&mut db`
    /// repeatedly). Populated once at construction.
    int: Type,
    bool_: Type,
    text: Type,
    unit: Type,
}

/// The built-in method catalog, constructed once and cached for the process
/// lifetime (it is immutable data). Used by every `Lowerer`.
fn builtin_catalog() -> &'static praxis_stdlib::MethodCatalog {
    static CATALOG: std::sync::OnceLock<praxis_stdlib::MethodCatalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(praxis_stdlib::builtin_catalog)
}

impl<'a> Lowerer<'a> {
    fn file_span(&self, range: TextRange) -> FileSpan {
        FileSpan::new(
            self.file,
            Span::new(
                praxis_source::BytePos::from(u32::from(range.start())),
                praxis_source::BytePos::from(u32::from(range.end())),
            ),
        )
    }

    fn diag(&mut self, at: TextRange, number: u32, msg: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(
            Severity::Error,
            DiagnosticCode::new(DiagnosticCategory::Type, number),
            msg.into(),
            self.file_span(at),
        ));
    }

    // --- items -------------------------------------------------------------

    fn lower_fn(&mut self, item: &FnItem) -> Option<TypedFn> {
        let name_tok = item.name()?;
        let name_range = name_tok.text_range();
        let name = name_tok.text().to_string();
        // Resolve the fn symbol via its declaration range (decls map; survives
        // shadowing). Anonymous/builtin decls have no symbol and are skipped.
        let symbol = self.resolve_decl_at(name_range)?;

        // Determine the function's scheme. If it is polymorphic, the M4 backend
        // cannot lower it (monomorphization is a later milestone, ADR-018).
        let scheme = self.names.get(symbol).and_then(|s| s.scheme.clone());
        let fn_type = match &scheme {
            Some(s) => {
                // Instantiate once to read the concrete shape (params/result).
                // For a monomorphic function the instantiation equals the body.
                self.db.instantiate(s)
            }
            None => {
                // No scheme inferred (errored in M2); skip — don't cascade.
                return None;
            }
        };
        if scheme.as_ref().is_some_and(|s| s.is_polymorphic()) {
            self.diag(
                name_range,
                100,
                format!(
                    "`{name}` is generic; monomorphization is not supported yet (M4 is monomorphic)"
                ),
            );
            return None;
        }

        // Body scope: a child of the current scope. We don't track scopes
        // explicitly during lowering (the analysis already did); instead we
        // resolve every name reference by its range through `self.refs`, which
        // is scope-independent and unambiguous.
        let _scope = self.scopes.root();

        let params = self.lower_params(item.param_list().as_ref());
        let return_type = self.fn_result(&fn_type);

        let body = item
            .body()
            .and_then(|b| self.lower_block(&b))
            .unwrap_or_else(|| TypedBlock {
                stmts: Vec::new(),
                tail: TypedExpr::Lit {
                    value: Lit::Int(0),
                    ty: self.unit,
                },
                ty: self.unit,
            });

        Some(TypedFn {
            symbol,
            name,
            params,
            return_type,
            body,
            fn_type,
        })
    }

    /// Extract the result type from a function type; defaults to Unit if the
    /// shape is not a `Func` (defensive — should not happen for well-typed fns).
    fn fn_result(&self, fn_type: &Type) -> Type {
        let resolved = self.db.follow(*fn_type);
        if let praxis_types::TypeData::Func { result, .. } = self.db.data(resolved) {
            *result
        } else {
            self.unit
        }
    }

    fn lower_params(&mut self, pl: Option<&ParamList>) -> Vec<TypedParam> {
        let Some(pl) = pl else {
            return Vec::new();
        };
        pl.params().filter_map(|p| self.lower_param(&p)).collect()
    }

    fn lower_param(&mut self, p: &Param) -> Option<TypedParam> {
        let name_tok = p.name()?;
        let name = name_tok.text().to_string();
        let range = name_tok.text_range();
        let symbol = self.resolve_decl_at(range)?;
        // The param's type is the declared annotation; read it from the symbol's
        // instantiated scheme slot. Params are positions in the Func params vec.
        let ty = self
            .param_type(symbol)
            .unwrap_or_else(|| self.db.fresh_var());
        Some(TypedParam { symbol, name, ty })
    }

    /// Read a parameter's type from its function's scheme. Falls back to a fresh
    /// var if the shape is unexpected (defensive).
    fn param_type(&mut self, symbol: SymbolId) -> Option<Type> {
        let sym = self.names.get(symbol)?;
        // Params don't carry their own scheme; we find them via the enclosing fn.
        // Simpler: re-derive from the fn_type isn't possible per-param here, so
        // we rely on the param symbol being a `Param` whose scheme is its type.
        let scheme = sym.scheme.as_ref()?;
        Some(self.db.instantiate(scheme))
    }

    // --- statements --------------------------------------------------------

    fn lower_block(&mut self, block: &BlockExpr) -> Option<TypedBlock> {
        let mut stmts = Vec::new();
        let mut tail: Option<TypedExpr> = None;
        for child in block.stmts() {
            // A trailing ExprStmt is the block's value. But earlier ExprStmts
            // are effect statements — only the *last* one is the tail. So when a
            // new ExprStmt appears, the previously-recorded tail becomes an
            // effect statement (it wasn't the tail after all).
            if let Some(expr_stmt) = ExprStmt::cast(child.clone()) {
                if let Some(e) = expr_stmt.expr() {
                    if let Some(prev) = tail.take() {
                        stmts.push(TypedStmt::Expr(prev));
                    }
                    tail = Some(self.lower_expr(&e));
                    continue;
                }
            }
            // A non-ExprStmt (let/var/assign) after a pending tail demotes the
            // tail to an effect statement — the tail was not the block's value
            // after all, since more statements follow. This preserves source
            // order: `{ v.push(i); i = i + 1 }` must push *before* incrementing.
            if let Some(prev) = tail.take() {
                stmts.push(TypedStmt::Expr(prev));
            }
            if let Some(s) = self.lower_stmt(&child) {
                stmts.push(s);
            }
        }
        let tail = tail.unwrap_or(TypedExpr::Lit {
            value: Lit::Int(0),
            ty: self.unit,
        });
        let ty = expr_ty(&tail);
        Some(TypedBlock { stmts, tail, ty })
    }

    fn lower_stmt(&mut self, node: &SyntaxNode) -> Option<TypedStmt> {
        if let Some(let_) = LetStmt::cast(node.clone()) {
            return self.lower_let(&let_);
        }
        if let Some(var_) = VarStmt::cast(node.clone()) {
            return self.lower_var(&var_);
        }
        if let Some(assign) = AssignStmt::cast(node.clone()) {
            return self.lower_assign(&assign);
        }
        if let Some(expr_stmt) = ExprStmt::cast(node.clone()) {
            if let Some(e) = expr_stmt.expr() {
                return Some(TypedStmt::Expr(self.lower_expr(&e)));
            }
        }
        None
    }

    fn lower_let(&mut self, stmt: &LetStmt) -> Option<TypedStmt> {
        let name_tok = stmt.name()?;
        let name = name_tok.text().to_string();
        let range = name_tok.text_range();
        let symbol = self.resolve_decl_at(range)?;
        let init = stmt.init()?;
        let init = self.lower_expr(&init);
        let ty = expr_ty(&init);
        Some(TypedStmt::Let {
            symbol,
            name,
            ty,
            init,
        })
    }

    fn lower_var(&mut self, stmt: &VarStmt) -> Option<TypedStmt> {
        let name_tok = stmt.name()?;
        let name = name_tok.text().to_string();
        let range = name_tok.text_range();
        let symbol = self.resolve_decl_at(range)?;
        let init = stmt.init()?;
        let init = self.lower_expr(&init);
        let ty = expr_ty(&init);
        Some(TypedStmt::Var {
            symbol,
            name,
            ty,
            init,
        })
    }

    fn lower_assign(&mut self, stmt: &AssignStmt) -> Option<TypedStmt> {
        let name_tok = stmt.name()?;
        let name = name_tok.text().to_string();
        let range = name_tok.text_range();
        let symbol = self.resolve_symbol_at(range)?;
        let value = stmt.value()?;
        let value = self.lower_expr(&value);
        let op = match stmt.op().map(|t| t.kind()) {
            Some(SyntaxKind::PLUS_EQ) => AssignOp::AddAssign,
            Some(SyntaxKind::MINUS_EQ) => AssignOp::SubAssign,
            Some(SyntaxKind::STAR_EQ) => AssignOp::MulAssign,
            Some(SyntaxKind::SLASH_EQ) => AssignOp::DivAssign,
            Some(SyntaxKind::PERCENT_EQ) => AssignOp::RemAssign,
            _ => AssignOp::Assign,
        };
        Some(TypedStmt::Assign {
            symbol,
            name,
            op,
            value,
        })
    }

    // --- expressions -------------------------------------------------------

    fn lower_expr(&mut self, expr: &Expr) -> TypedExpr {
        match expr {
            Expr::Literal(l) => self.lower_literal(l),
            Expr::Path(p) => self.lower_path(p),
            Expr::Bin(b) => self.lower_bin(b),
            Expr::Unary(u) => self.lower_unary(u),
            Expr::Paren(p) => match p.expr() {
                Some(inner) => self.lower_expr(&inner),
                None => TypedExpr::Paren {
                    inner: None,
                    ty: self.db.fresh_var(),
                },
            },
            Expr::Block(b) => self
                .lower_block(b)
                .map(|b| TypedExpr::Block(Box::new(b)))
                .unwrap_or_else(|| TypedExpr::Lit {
                    value: Lit::Int(0),
                    ty: self.unit,
                }),
            Expr::If(i) => self.lower_if(i),
            Expr::While(w) => self.lower_while(w),
            Expr::Call(c) => self.lower_call(c),
            Expr::MethodCall(m) => self.lower_method_call(m),
            Expr::Tuple(t) => self.lower_tuple(t),
            // M6 WS5 fills these in with real read/parse lowering.
            Expr::Read(_) | Expr::Parse(_) => TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.unit,
            },
            Expr::Error(_) => TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.db.fresh_var(),
            },
        }
    }

    fn lower_literal(&mut self, lit: &Literal) -> TypedExpr {
        let Some(tok) = lit.token() else {
            return TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.db.fresh_var(),
            };
        };
        match tok.kind() {
            SyntaxKind::IntLit => {
                let text = tok.text();
                // Strip any `_` digit separators; parse as i64. On overflow we
                // leave the literal at i64::MAX/MIN — the backend will fault on
                // the actual arithmetic anyway.
                let cleaned: String = text.chars().filter(|c| *c != '_').collect();
                let value = cleaned.parse::<i64>().unwrap_or(i64::MAX);
                TypedExpr::Lit {
                    value: Lit::Int(value),
                    ty: self.int,
                }
            }
            SyntaxKind::TextLit => {
                let raw = tok.text();
                let unquoted = unquote_text(raw);
                TypedExpr::Lit {
                    value: Lit::Text(unquoted),
                    ty: self.text,
                }
            }
            SyntaxKind::BacktickTemplate => {
                let raw = tok.text();
                // Backtick templates are M6; treat the inner text as a Text lit.
                let inner = raw
                    .trim_start_matches('`')
                    .trim_end_matches('`')
                    .to_string();
                TypedExpr::Lit {
                    value: Lit::Text(inner),
                    ty: self.text,
                }
            }
            SyntaxKind::KW_TRUE => TypedExpr::Lit {
                value: Lit::Bool(true),
                ty: self.bool_,
            },
            SyntaxKind::KW_FALSE => TypedExpr::Lit {
                value: Lit::Bool(false),
                ty: self.bool_,
            },
            _ => TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.db.fresh_var(),
            },
        }
    }

    fn lower_path(&mut self, p: &PathExpr) -> TypedExpr {
        let ty = match p.name() {
            Some(tok) => {
                let range = tok.text_range();
                // The inferred type for this reference (filled by inference).
                // We re-instantiate the symbol's scheme to be safe.
                match self.resolve_symbol_at(range) {
                    Some(symbol) => self.symbol_type(symbol),
                    None => self.db.fresh_var(),
                }
            }
            None => self.db.fresh_var(),
        };
        let symbol = p
            .name()
            .and_then(|t| self.resolve_symbol_at(t.text_range()))
            .unwrap_or(SymbolId(u32::MAX));
        TypedExpr::Path { symbol, ty }
    }

    fn lower_bin(&mut self, b: &BinExpr) -> TypedExpr {
        let (lhs, rhs) = b.operands();
        let lhs = lhs.map(|e| Box::new(self.lower_expr(&e)));
        let rhs = rhs.map(|e| Box::new(self.lower_expr(&e)));
        let op_tok = b.op().map(|t| t.kind());
        let (op, ty) = match op_tok {
            Some(
                SyntaxKind::PLUS
                | SyntaxKind::MINUS
                | SyntaxKind::STAR
                | SyntaxKind::SLASH
                | SyntaxKind::PERCENT,
            ) => {
                let op = match op_tok.unwrap() {
                    SyntaxKind::PLUS => BinOp::Add,
                    SyntaxKind::MINUS => BinOp::Sub,
                    SyntaxKind::STAR => BinOp::Mul,
                    SyntaxKind::SLASH => BinOp::Div,
                    SyntaxKind::PERCENT => BinOp::Rem,
                    _ => unreachable!(),
                };
                (op, self.int)
            }
            Some(
                SyntaxKind::EQ2
                | SyntaxKind::NEQ
                | SyntaxKind::LT
                | SyntaxKind::GT
                | SyntaxKind::LTEQ
                | SyntaxKind::GTEQ,
            ) => {
                let op = match op_tok.unwrap() {
                    SyntaxKind::EQ2 => BinOp::Eq,
                    SyntaxKind::NEQ => BinOp::Neq,
                    SyntaxKind::LT => BinOp::Lt,
                    SyntaxKind::GT => BinOp::Gt,
                    SyntaxKind::LTEQ => BinOp::Le,
                    SyntaxKind::GTEQ => BinOp::Ge,
                    _ => unreachable!(),
                };
                (op, self.bool_)
            }
            Some(SyntaxKind::PIPE2) => (BinOp::LogicalOr, self.bool_),
            _ => (BinOp::Add, self.db.fresh_var()),
        };
        TypedExpr::Bin {
            op,
            lhs: lhs.unwrap_or_else(|| Box::new(unit_lit(self.db))),
            rhs: rhs.unwrap_or_else(|| Box::new(unit_lit(self.db))),
            ty,
        }
    }

    fn lower_unary(&mut self, u: &UnaryExpr) -> TypedExpr {
        let operand = u.operand().map(|e| Box::new(self.lower_expr(&e)));
        let (op, ty) = match u.op().map(|t| t.kind()) {
            Some(SyntaxKind::MINUS) => (UnaryOp::Neg, self.int),
            Some(SyntaxKind::BANG) => (UnaryOp::Not, self.bool_),
            _ => (UnaryOp::Neg, self.db.fresh_var()),
        };
        TypedExpr::Unary {
            op,
            operand: operand.unwrap_or_else(|| Box::new(unit_lit(self.db))),
            ty,
        }
    }

    fn lower_if(&mut self, i: &IfExpr) -> TypedExpr {
        let cond = i.cond().map(|c| Box::new(self.lower_expr(&c)));
        let then_block = i
            .then_branch()
            .and_then(|b| self.lower_block(&b))
            .map(Box::new);
        let else_block = i.else_branch().and_then(|e| self.lower_else(&e));
        // The if's type is the then-block's type (unified with else by M2).
        let ty = then_block
            .as_ref()
            .map(|b| b.ty)
            .unwrap_or_else(|| self.unit);
        TypedExpr::If {
            cond: cond.unwrap_or_else(|| Box::new(unit_lit(self.db))),
            then_block: then_block.unwrap_or_else(|| {
                Box::new(TypedBlock {
                    stmts: Vec::new(),
                    tail: unit_lit(self.db),
                    ty: self.unit,
                })
            }),
            else_block: else_block.map(Box::new),
            ty,
        }
    }

    fn lower_else(&mut self, e: &ElseBranch) -> Option<TypedBlock> {
        let body = e.body()?;
        match body {
            Expr::Block(b) => self.lower_block(&b),
            other => {
                // `else if` — wrap the nested if in a synthetic block.
                let inner = self.lower_expr(&other);
                let ty = expr_ty(&inner);
                Some(TypedBlock {
                    stmts: Vec::new(),
                    tail: inner,
                    ty,
                })
            }
        }
    }

    fn lower_while(&mut self, w: &WhileExpr) -> TypedExpr {
        let cond = w.cond().map(|c| Box::new(self.lower_expr(&c)));
        let body = w.body().and_then(|b| self.lower_block(&b)).map(Box::new);
        TypedExpr::While {
            cond: cond.unwrap_or_else(|| Box::new(unit_lit(self.db))),
            body: body.unwrap_or_else(|| {
                Box::new(TypedBlock {
                    stmts: Vec::new(),
                    tail: unit_lit(self.db),
                    ty: self.unit,
                })
            }),
            ty: self.unit,
        }
    }

    fn lower_call(&mut self, c: &CallExpr) -> TypedExpr {
        let callee_tok = c.callee().and_then(|p| p.name());
        let callee = callee_tok
            .as_ref()
            .and_then(|t| self.resolve_symbol_at(t.text_range()))
            .unwrap_or(SymbolId(u32::MAX));
        let callee_name = callee_tok
            .as_ref()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        let args: Vec<TypedExpr> = c
            .arg_list()
            .map(|a| self.lower_args(&a))
            .unwrap_or_default();
        // The call's result type comes from the callee's instantiated scheme.
        let ty = self
            .call_result_type(callee)
            .unwrap_or_else(|| self.db.fresh_var());
        TypedExpr::Call {
            callee,
            callee_name,
            args,
            ty,
        }
    }

    fn lower_args(&mut self, args: &ArgList) -> Vec<TypedExpr> {
        args.args().map(|a| self.lower_expr(&a)).collect()
    }

    /// Lower `receiver.method(args)` (M5, §16.2). Resolves the method against
    /// the built-in catalog via the [`crate::catalog`] bridge, recording the
    /// runtime lowering symbol so the MIR builder emits a direct call.
    fn lower_method_call(&mut self, m: &MethodCallExpr) -> TypedExpr {
        // Lower the receiver (or fall back to a Unit-typed literal if the tree
        // is malformed so the rest of the expression still lowers).
        let receiver = match m.receiver() {
            Some(r) => self.lower_expr(&r),
            None => TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.unit,
            },
        };
        let name = m
            .method_name()
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        let args: Vec<TypedExpr> = m
            .arg_list()
            .map(|a| self.lower_args(&a))
            .unwrap_or_default();
        let arity = args.len();

        // Resolve the method against the catalog, keyed by the receiver's
        // inferred type + name + arity (ADR-010 bridge).
        let hits = crate::catalog::lookup(self.db, self.catalog, expr_ty(&receiver), &name, arity);
        if let Some(entry) = hits.first() {
            let ty = pattern_to_type(self.db, &entry.result);
            let lowering_symbol = match &entry.lowering {
                praxis_stdlib::MethodLowering::RuntimeSymbol(sym) => sym.to_string(),
                praxis_stdlib::MethodLowering::Intrinsic(_) => {
                    // Intrinsics are not yet emitted (M8 pipeline); leave empty
                    // so MIR skips the call for now.
                    String::new()
                }
            };
            TypedExpr::MethodCall {
                receiver: Box::new(receiver),
                name,
                lowering_symbol,
                args,
                ty,
            }
        } else {
            // Unknown method: emit a Y110 diagnostic and lower to Unit so the
            // rest of the tree is still well-formed.
            if let Some(name_tok) = m.method_name() {
                self.diag(
                    name_tok.text_range(),
                    110,
                    format!("no method `{name}` on this type taking {arity} argument(s)"),
                );
            }
            TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.unit,
            }
        }
    }

    fn lower_tuple(&mut self, t: &TupleExpr) -> TypedExpr {
        let elements: Vec<TypedExpr> = t.elements().map(|e| self.lower_expr(&e)).collect();
        let tys: Vec<Type> = elements.iter().map(expr_ty).collect();
        let ty = self.db.tuple(tys);
        TypedExpr::Tuple { elements, ty }
    }

    // --- helpers -----------------------------------------------------------

    /// Resolve the symbol *declared* at `range` (a `let`/`var`/`fn`/param name
    /// token), via the resolution `decls` map. Unambiguous under shadowing.
    fn resolve_decl_at(&self, range: TextRange) -> Option<SymbolId> {
        self.decls.get(&range).copied()
    }

    /// Resolve the symbol *referenced* at `range` (a name use), via the
    /// resolution `refs` map.
    fn resolve_symbol_at(&self, range: TextRange) -> Option<SymbolId> {
        self.refs.get(&range).map(|r| r.symbol)
    }

    /// The instantiated type of a symbol's scheme (a fresh var if unknown).
    fn symbol_type(&mut self, symbol: SymbolId) -> Type {
        self.names
            .get(symbol)
            .and_then(|s| s.scheme.clone())
            .map(|s| self.db.instantiate(&s))
            .unwrap_or_else(|| self.db.fresh_var())
    }

    /// The result type of calling `callee`, from its function scheme.
    fn call_result_type(&mut self, callee: SymbolId) -> Option<Type> {
        let sym = self.names.get(callee)?;
        let scheme = sym.scheme.as_ref()?;
        let inst = self.db.instantiate(scheme);
        let inst = self.db.follow(inst);
        match self.db.data(inst) {
            praxis_types::TypeData::Func { result, .. } => Some(*result),
            _ => None,
        }
    }
}

/// The type carried by a typed expression.
fn expr_ty(e: &TypedExpr) -> Type {
    match e {
        TypedExpr::Lit { ty, .. } => *ty,
        TypedExpr::Path { ty, .. } => *ty,
        TypedExpr::Bin { ty, .. } => *ty,
        TypedExpr::Unary { ty, .. } => *ty,
        TypedExpr::Paren { ty, .. } => *ty,
        TypedExpr::Block(b) => b.ty,
        TypedExpr::If { ty, .. } => *ty,
        TypedExpr::While { ty, .. } => *ty,
        TypedExpr::Call { ty, .. } => *ty,
        TypedExpr::MethodCall { ty, .. } => *ty,
        TypedExpr::Tuple { ty, .. } => *ty,
    }
}

/// Convert a catalog [`TypePattern`] (the schema-level result type of a method)
/// into a real inferred [`Type`]. `Var("T")` becomes a fresh unbound var (the
/// caller unifies it with the receiver's element type if needed); concrete
/// scalars/collections map directly. This is the reverse of
/// [`crate::catalog::type_to_pattern`] for result types.
///
/// Public so the inference pass can instantiate param/result patterns before
/// unification (both passes share the one conversion).
pub fn pattern_to_type_pub(db: &mut TypeDb, p: &TypePattern) -> Type {
    pattern_to_type(db, p)
}

fn pattern_to_type(db: &mut TypeDb, p: &TypePattern) -> Type {
    match p {
        TypePattern::Scalar(s) => db.scalar(map_pattern_scalar(*s)),
        TypePattern::Unit => db.unit(),
        TypePattern::Var(_) => db.fresh_var(),
        TypePattern::Collection { ctor, args } => {
            let arg_tys: Vec<Type> = args.iter().map(|a| pattern_to_type(db, a)).collect();
            db.collection(*ctor, arg_tys)
        }
        TypePattern::Function { params, result } => {
            let ps: Vec<Type> = params.iter().map(|p| pattern_to_type(db, p)).collect();
            let r = pattern_to_type(db, result);
            db.func(ps, r)
        }
        TypePattern::Opaque => db.fresh_var(),
    }
}

/// Map a stdlib pattern scalar to the inference scalar (they share the enum via
/// `praxis_types::ScalarType`, so this is identity).
fn map_pattern_scalar(s: PatternScalar) -> praxis_types::ScalarType {
    s
}

/// A `Unit`-typed literal placeholder (for malformed subtrees).
fn unit_lit(db: &mut TypeDb) -> TypedExpr {
    TypedExpr::Lit {
        value: Lit::Int(0),
        ty: db.unit(),
    }
}

/// Strip surrounding quotes and unescape simple escapes from a `"…"` literal.
fn unquote_text(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return raw.to_string();
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('0') => out.push('\0'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxis_parser::parse;
    use praxis_stdlib::type_pattern::ScalarType;

    /// Parse + analyze + lower a one-file program. Returns the typed module
    /// alongside the analysis (whose `db` is needed to read any `Type` handle).
    struct Lowered {
        module: TypedModule,
        analysis: crate::Analysis,
    }

    fn lower_src(src: &str) -> Lowered {
        let map = praxis_source::SourceMap::new();
        let file = map.intern("lower_test.px", src);
        let parsed = parse(file, src);
        let mut analysis = crate::analyze_root(file, &parsed.tree);
        // `lower` takes `&SourceFile`; cast the parsed root (analyze_root does
        // the same internally).
        let root = praxis_ast::SourceFile::cast(parsed.tree.clone())
            .expect("parse produced a SOURCE_FILE");
        let module = lower(file, &root, &mut analysis);
        Lowered { module, analysis }
    }

    fn fn0(l: &Lowered) -> &TypedFn {
        match &l.module.items[0] {
            TypedItem::Fn(f) => f,
        }
    }

    fn is_int(l: &Lowered, ty: Type) -> bool {
        matches!(
            l.analysis.db.data(ty),
            praxis_types::TypeData::Scalar(ScalarType::Int)
        )
    }

    fn is_bool(l: &Lowered, ty: Type) -> bool {
        matches!(
            l.analysis.db.data(ty),
            praxis_types::TypeData::Scalar(ScalarType::Bool)
        )
    }

    #[test]
    fn lowers_a_simple_int_fn() {
        let l = lower_src("fn id(x: Int) -> Int { x }");
        assert!(
            l.module.diagnostics.is_empty(),
            "{:?}",
            l.module.diagnostics
        );
        assert_eq!(l.module.items.len(), 1);
        let f = fn0(&l);
        assert_eq!(f.name, "id");
        assert_eq!(f.params.len(), 1);
        assert_eq!(f.params[0].name, "x");
    }

    #[test]
    fn lowers_arithmetic_binop_as_int() {
        let l = lower_src("fn f() -> Int { 1 + 2 }");
        let f = fn0(&l);
        let TypedExpr::Bin { op, ty, .. } = &f.body.tail else {
            panic!("expected bin");
        };
        assert_eq!(*op, BinOp::Add);
        assert!(is_int(&l, *ty));
    }

    #[test]
    fn lowers_comparison_as_bool() {
        let l = lower_src("fn f() -> Bool { 1 < 2 }");
        let f = fn0(&l);
        let TypedExpr::Bin { op, ty, .. } = &f.body.tail else {
            panic!("expected bin");
        };
        assert_eq!(*op, BinOp::Lt);
        assert!(is_bool(&l, *ty));
    }

    #[test]
    fn lowers_integer_literal_value() {
        let l = lower_src("fn f() -> Int { 42 }");
        let f = fn0(&l);
        let TypedExpr::Lit {
            value: Lit::Int(v),
            ty,
        } = &f.body.tail
        else {
            panic!("expected int lit");
        };
        assert_eq!(*v, 42);
        assert!(is_int(&l, *ty));
    }

    #[test]
    fn lowers_text_literal_unquoted() {
        let l = lower_src("fn f() -> Text { \"a\\tb\" }");
        let f = fn0(&l);
        let TypedExpr::Lit {
            value: Lit::Text(s),
            ..
        } = &f.body.tail
        else {
            panic!("expected text lit");
        };
        assert_eq!(s, "a\tb");
    }

    #[test]
    fn lowers_bool_literals() {
        let l = lower_src("fn f() -> Bool { true }");
        let f = fn0(&l);
        let TypedExpr::Lit {
            value: Lit::Bool(b),
            ..
        } = &f.body.tail
        else {
            panic!();
        };
        assert!(*b);
    }

    #[test]
    fn rejects_generic_fn_with_y100() {
        // `id` is polymorphic (`a -> a`); M4 cannot lower it.
        let l = lower_src("fn id(x) { x }");
        assert_eq!(l.module.diagnostics.len(), 1);
        assert_eq!(
            l.module.diagnostics[0].code().number(),
            100,
            "should be Y100 (generic not supported)"
        );
    }

    #[test]
    fn lowers_if_while_and_assign() {
        let l = lower_src(
            "fn f(n: Int) -> Int {\n  var i = 0\n  while i < n { i = i + 1 }\n  if i > 0 { 1 } else { 2 }\n}\n",
        );
        assert!(
            l.module.diagnostics.is_empty(),
            "{:?}",
            l.module.diagnostics
        );
        let f = fn0(&l);
        // stmt 0: Var { i }; then a While; then an If as tail.
        assert!(matches!(f.body.stmts.first(), Some(TypedStmt::Var { name, .. }) if name == "i"));
        assert!(matches!(&f.body.tail, TypedExpr::If { .. }));
    }

    #[test]
    fn lowers_compound_assignment_op() {
        let l = lower_src("fn f() -> Int { var i = 0; i += 1; i }");
        let f = fn0(&l);
        let assign = f
            .body
            .stmts
            .iter()
            .find_map(|s| match s {
                TypedStmt::Assign { op, .. } => Some(*op),
                _ => None,
            })
            .expect("found an assign");
        assert_eq!(assign, AssignOp::AddAssign);
    }

    #[test]
    fn lowers_call_with_args() {
        let l = lower_src("fn a(x: Int) -> Int { x }\nfn b() -> Int { a(7) }");
        assert_eq!(l.module.items.len(), 2);
        // `TypedItem` currently has a single variant, so this bind is infallible.
        let TypedItem::Fn(b) = &l.module.items[1];
        let TypedExpr::Call { args, ty, .. } = &b.body.tail else {
            panic!("expected call");
        };
        assert_eq!(args.len(), 1);
        assert!(is_int(&l, *ty));
    }

    #[test]
    fn lowers_tuple() {
        let l = lower_src("fn f() { (1, 2) }");
        let f = fn0(&l);
        let TypedExpr::Tuple { elements, .. } = &f.body.tail else {
            panic!("expected tuple");
        };
        assert_eq!(elements.len(), 2);
    }

    #[test]
    fn unquote_text_strips_quotes_and_unescapes() {
        assert_eq!(unquote_text("\"plain\""), "plain");
        assert_eq!(unquote_text("\"a\\nb\""), "a\nb");
        assert_eq!(unquote_text("\"\\\\\""), "\\");
        assert_eq!(unquote_text("nope"), "nope");
    }
}
