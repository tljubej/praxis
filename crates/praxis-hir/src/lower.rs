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
    ArgList, AssignStmt, AstNode, BinExpr, BlockExpr, BreakExpr, CallExpr, ContinueExpr,
    ElseBranch, EnumItem, Expr, ExprStmt, FieldExpr, FnItem, ForExpr, IfExpr, LetStmt, Literal,
    LoopExpr, MethodCallExpr, Param, ParamList, PathExpr, RecordLitExpr, ReturnExpr, SourceFile,
    StructItem, TupleExpr, UnaryExpr, VarStmt, WhileExpr,
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
    /// The `var` symbols captured by some closure in the module (escape
    /// analysis, M7-WS7b). The MIR builder boxes these into a `VarCell` at
    /// their binding site and routes reads/writes through the cell, so a
    /// mutation in one frame is visible to every closure sharing the cell.
    pub escaping_vars: std::collections::HashSet<SymbolId>,
}

/// A top-level item.
#[derive(Debug)]
pub enum TypedItem {
    /// A `fn name(params) -> Ret { body }`.
    Fn(TypedFn),
}

/// A lowered function.
#[derive(Clone, Debug)]
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
    /// The function's source span `[start, end)` as byte offsets (§9.3,
    /// M10-WS1). Threaded through to MIR `Function` so the crash debugger's
    /// `source` command can render the faulting function's extent. `(0, 0)`
    /// only when the AST node has no usable span.
    pub span: (u32, u32),
}

/// A parameter `name: Type`.
#[derive(Clone, Debug)]
pub struct TypedParam {
    pub symbol: SymbolId,
    pub name: String,
    pub ty: Type,
}

/// A `{ stmt; …; tail }` block. `tail` is the block's value (`Unit` if absent).
#[derive(Clone, Debug)]
pub struct TypedBlock {
    pub stmts: Vec<TypedStmt>,
    /// The trailing expression, lowered as a statement; `Unit` typed if none.
    pub tail: TypedExpr,
    /// The block's value type (the tail's type, or Unit).
    pub ty: Type,
}

/// A statement. Every variant carries its source `span` `[start, end)` (byte
/// offsets into the program source), threaded from the rowan AST node during
/// lowering. `TypedStmt::Expr` carries the span on its inner expression.
#[derive(Clone, Debug)]
pub enum TypedStmt {
    /// `let name = expr` (immutable binding).
    Let {
        symbol: SymbolId,
        name: String,
        ty: Type,
        init: TypedExpr,
        span: (u32, u32),
    },
    /// `var name = expr` (mutable binding).
    Var {
        symbol: SymbolId,
        name: String,
        ty: Type,
        init: TypedExpr,
        span: (u32, u32),
    },
    /// `name = expr` / `name += expr` / …
    Assign {
        symbol: SymbolId,
        name: String,
        op: AssignOp,
        value: TypedExpr,
        span: (u32, u32),
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
#[derive(Clone, Debug)]
pub enum TypedExpr {
    /// An integer / text / bool literal.
    Lit {
        value: Lit,
        ty: Type,
        span: (u32, u32),
    },
    /// A name reference (variable, parameter, function).
    Path {
        symbol: SymbolId,
        ty: Type,
        span: (u32, u32),
    },
    /// `a op b`.
    Bin {
        op: BinOp,
        lhs: Box<TypedExpr>,
        rhs: Box<TypedExpr>,
        ty: Type,
        span: (u32, u32),
    },
    /// `op a`.
    Unary {
        op: UnaryOp,
        operand: Box<TypedExpr>,
        ty: Type,
        span: (u32, u32),
    },
    /// `( a )` — transparent; lowered to the inner expression directly, so this
    /// variant exists only when the inner expr is missing (malformed).
    Paren {
        inner: Option<Box<TypedExpr>>,
        ty: Type,
        span: (u32, u32),
    },
    /// A block used as an expression.
    Block(Box<TypedBlock>),
    /// `if cond { then } else { else }`.
    If {
        cond: Box<TypedExpr>,
        then_block: Box<TypedBlock>,
        else_block: Option<Box<TypedBlock>>,
        ty: Type,
        span: (u32, u32),
    },
    /// `while cond { body }` — always `Unit`.
    While {
        cond: Box<TypedExpr>,
        body: Box<TypedBlock>,
        ty: Type,
        span: (u32, u32),
    },
    /// `for binding in iter { body }` (M8, §4.11). `binding` is the loop
    /// variable's symbol; `item_ty` is the iterator's element type. Yields Unit.
    For {
        binding: SymbolId,
        iter: Box<TypedExpr>,
        body: Box<TypedBlock>,
        item_ty: Type,
        ty: Type,
        span: (u32, u32),
    },
    /// `loop { body }` (M8, §4.11). An infinite loop terminated by `break`;
    /// its type is the break-value type (Unit if no break carries a value).
    Loop {
        body: Box<TypedBlock>,
        ty: Type,
        span: (u32, u32),
    },
    /// `break [expr]` (M8, §4.11). Diverges from the enclosing loop. `value` is
    /// the optional break value; `ty` is `Never`.
    Break {
        value: Option<Box<TypedExpr>>,
        ty: Type,
        span: (u32, u32),
    },
    /// `continue` (M8, §4.11). Diverges; `ty` is `Never`.
    Continue { ty: Type, span: (u32, u32) },
    /// `return [expr]` (M8, §4.11). Diverges from the enclosing function.
    Return {
        value: Option<Box<TypedExpr>>,
        ty: Type,
        span: (u32, u32),
    },
    /// `callee(args)`.
    Call {
        callee: SymbolId,
        /// The callee's source name, resolved during HIR lowering so the MIR
        /// builder (and the JIT) can name the target without a NameTable.
        callee_name: String,
        args: Vec<TypedExpr>,
        /// The concrete argument types at this call site (WS8, §13.6). The mono
        /// pass uses these to instantiate a polymorphic callee; the MIR builder
        /// ignores them (calls are by name). For a closure-value callee, empty.
        arg_types: Vec<Type>,
        /// For a postfix call on an arbitrary expression (`expr(args)`, M8
        /// §4.10) — e.g. calling a closure retrieved from a collection
        /// (`fs.get(0)(100)`) or the result of another call (`f(1)(2)`) — the
        /// lowered callee expression. `None` for an ordinary named call (the
        /// callee is `callee`/`callee_name`); `Some` for a closure-value callee
        /// that the MIR builder lowers to `Inst::CallIndirect`.
        callee_expr: Option<Box<TypedExpr>>,
        ty: Type,
        span: (u32, u32),
    },
    /// `receiver.method(args)` (M5, §16.2). `lowering_symbol` is the runtime
    /// wrapper the catalog resolved (e.g. `RuntimeSymbol::VecPush`), so the MIR
    /// builder emits a direct call without re-resolving the catalog; `None`
    /// means the method is an intrinsic and MIR lowers it itself. `purity`
    /// (M10b-WS4) is the catalog's purity tag, so the crash debugger's read-only
    /// `p EXPR` evaluator can reject impure calls (§9.5, §19.10 "no command can
    /// mutate").
    MethodCall {
        receiver: Box<TypedExpr>,
        name: String,
        lowering_symbol: Option<praxis_stdlib::abi::RuntimeSymbol>,
        args: Vec<TypedExpr>,
        purity: praxis_stdlib::Purity,
        ty: Type,
        span: (u32, u32),
    },
    /// `( a, b, … )` — at least two elements.
    Tuple {
        elements: Vec<TypedExpr>,
        ty: Type,
        span: (u32, u32),
    },
    /// `read parser_expression` (§7.1, M6). `plan` identifies the compiled
    /// [`ParserPlan`] in the process-wide arena; the runtime interpreter looks
    /// it up.
    Read {
        plan: praxis_input_parser::PlanId,
        ty: Type,
        span: (u32, u32),
    },
    /// `parse(text, parser_expression)` (§7.1, M6). The `text` arg is lowered as
    /// an ordinary expression; `plan` identifies the parser plan.
    Parse {
        text: Box<TypedExpr>,
        plan: praxis_input_parser::PlanId,
        ty: Type,
        span: (u32, u32),
    },
    /// `Name { field: expr, … }` record literal (M7, §4.5). `record_def_id`
    /// identifies the struct type (index into `TypeDb::record_defs`); `fields`
    /// are the lowered initializers in declaration order, each paired with its
    /// field index.
    RecordLit {
        record_def_id: praxis_types::RecordDefId,
        fields: Vec<(u32, TypedExpr)>,
        ty: Type,
        span: (u32, u32),
    },
    /// `receiver.field` field access (M7, §4.5). `field_idx` is the field's
    /// index in the record's `RecordDef`.
    FieldGet {
        receiver: Box<TypedExpr>,
        field_idx: u32,
        ty: Type,
        span: (u32, u32),
    },
    /// An enum variant construction (M7, §4.6): `Number(5)` or bare `Empty`.
    /// `enum_def_id` identifies the enum, `variant_idx` the variant, and `args`
    /// are the payload values (empty for a payload-less variant).
    EnumVariant {
        enum_def_id: praxis_types::EnumDefId,
        variant_idx: u32,
        args: Vec<TypedExpr>,
        ty: Type,
        span: (u32, u32),
    },
    /// `match scrutinee { pattern => body, … }` (M7, §4.6).
    Match {
        scrutinee: Box<TypedExpr>,
        arms: Vec<TypedMatchArm>,
        ty: Type,
        span: (u32, u32),
    },
    /// `|params| body` closure (M7, §4.10). `fn_name` is a synthesized unique
    /// name for the closure's synthetic MIR function; `fn_type` is the inferred
    /// `Func` type; `captures` is the ordered capture list (env slot order).
    Closure {
        params: Vec<TypedParam>,
        body: Box<TypedBlock>,
        captures: Vec<crate::capture::Capture>,
        fn_type: Type,
        fn_name: String,
        ty: Type,
        span: (u32, u32),
    },
}

/// A recursive pattern, the M7-Part-2 replacement for the flat
/// `(variant_idx, bindings)` representation. Models the full pattern grammar:
/// wildcard, literal, variable bind, and enum variant with nested sub-patterns
/// (§4.6). The exhaustiveness checker and the MIR decision-tree lowering both
/// recurse over this.
#[derive(Clone, Debug)]
pub enum TypedPattern {
    /// `_` — matches anything, binds nothing.
    Wildcard,
    /// A literal `Int`/`Bool`/`Text` value to test against (§4.6). The MIR emits
    /// an equality compare against the scrutinee for these.
    Lit { value: Lit, ty: Type },
    /// `x` — binds the whole scrutinee to `symbol` (always matches).
    Bind { symbol: SymbolId, ty: Type },
    /// `Variant` or `Variant(sub, …)` — matches an enum variant by tag, then
    /// matches each sub-pattern against the corresponding payload slot.
    /// `enum_def_id`/`variant_idx` identify the variant; `subpatterns` is
    /// positional (one per payload type, possibly `Wildcard`).
    EnumVariant {
        enum_def_id: praxis_types::EnumDefId,
        variant_idx: u32,
        subpatterns: Vec<TypedPattern>,
        ty: Type,
    },
}

/// One arm of a lowered `match` expression (M7, §4.6). The pattern is recursive
/// (see [`TypedPattern`]); the MIR lowering emits a decision tree over it.
#[derive(Clone, Debug)]
pub struct TypedMatchArm {
    /// The arm's pattern (recursive: may nest sub-patterns in variant payloads).
    pub pattern: TypedPattern,
    /// The arm body expression.
    pub body: TypedExpr,
}

/// A literal value. (M4 lowers Int/Bool/Unit; Text materializes via the runtime
/// text descriptor. M6 adds Char for the input parser's `char`/`grid(char)`.
/// Float literals land here too, §4.12.)
#[derive(Clone, Debug)]
pub enum Lit {
    Int(i64),
    /// An IEEE-754 binary64 value (the payload of a `Float` object, §4.12).
    Float(f64),
    Text(String),
    Bool(bool),
    /// A Unicode scalar value (the payload of a `Char` object).
    Char(u32),
    /// The `Unit` value — the sole inhabitant of the `Unit` type (§4.3). This is
    /// never produced by the parser (there is no `Unit` literal syntax); it is
    /// synthesized wherever an expression of type `Unit` is needed (empty block
    /// tails, `while`/`for`/`loop` results, bare `return`, a missing `else`
    /// branch, and malformed-subtree fallbacks). Lowered to an `AllocKind::Unit`
    /// allocation so a `Unit`-typed expression holds a genuine Unit value, not
    /// an `Int(0)` masquerading as one.
    Unit,
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
        ref_types,
        call_sites,
        diagnostics: _,
    } = analysis;
    // Cache the scalar/unit handles once (these methods need &mut db).
    let int = db.int();
    let float = db.float();
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
        ref_types,
        call_sites,
        diagnostics: Vec::new(),
        catalog: builtin_catalog(),
        int,
        float,
        bool_,
        text,
        unit,
        closure_counter: 0,
    };
    let mut items = Vec::new();
    for node in root.stmts() {
        if let Some(fn_item) = FnItem::cast(node.clone()) {
            if let Some(tfn) = l.lower_fn(&fn_item) {
                items.push(TypedItem::Fn(tfn));
            }
        }
        // Struct declarations (M7, §4.5) are type-only: they register a record
        // type during inference but produce no runtime item. Skip them here.
        if StructItem::cast(node.clone()).is_some() {
            // No codegen for the declaration itself.
        }
        // Enum declarations (M7, §4.6) are likewise type-only.
        if EnumItem::cast(node.clone()).is_some() {
            // No codegen for the declaration itself.
        }
        // Top-level `let`/`var`/`expr`/`assign` are not lowered yet — M4 only
        // JITs `fn` items (the entry point is a `fn main` or similar). They are
        // still type-checked by `analyze`; they simply have no runtime lowering.
    }
    // Escape analysis (M7-WS7b): collect every `var` symbol captured by some
    // closure in the module. These are boxed into a `VarCell` at their binding
    // site so the closure shares the cell.
    let escaping_vars = collect_escaping_vars(&items);
    TypedModule {
        items,
        diagnostics: l.diagnostics,
        escaping_vars,
    }
}

/// Walk the module's fn bodies collecting every `var` symbol that appears as a
/// `ByCell` capture in any closure. The result is the set of escaping `var`s
/// the MIR builder must box into a `VarCell`.
fn collect_escaping_vars(items: &[TypedItem]) -> std::collections::HashSet<SymbolId> {
    let mut out = std::collections::HashSet::new();
    for item in items {
        let TypedItem::Fn(f) = item;
        collect_escaping_block(&f.body, &mut out);
    }
    out
}

fn collect_escaping_block(block: &TypedBlock, out: &mut std::collections::HashSet<SymbolId>) {
    for stmt in &block.stmts {
        collect_escaping_stmt(stmt, out);
    }
    collect_escaping_expr(&block.tail, out);
}

fn collect_escaping_stmt(stmt: &TypedStmt, out: &mut std::collections::HashSet<SymbolId>) {
    match stmt {
        TypedStmt::Let { init, .. } | TypedStmt::Var { init, .. } => {
            collect_escaping_expr(init, out)
        }
        TypedStmt::Assign { value, .. } => collect_escaping_expr(value, out),
        TypedStmt::Expr(e) => collect_escaping_expr(e, out),
    }
}

fn collect_escaping_expr(e: &TypedExpr, out: &mut std::collections::HashSet<SymbolId>) {
    match e {
        TypedExpr::Closure { captures, body, .. } => {
            for cap in captures {
                if matches!(cap.kind, crate::capture::CaptureKind::ByCell) {
                    out.insert(cap.symbol);
                }
            }
            collect_escaping_block(body, out);
        }
        TypedExpr::Bin { lhs, rhs, .. } => {
            collect_escaping_expr(lhs, out);
            collect_escaping_expr(rhs, out);
        }
        TypedExpr::Unary { operand, .. } => collect_escaping_expr(operand, out),
        TypedExpr::Paren { inner, .. } => {
            if let Some(inner) = inner {
                collect_escaping_expr(inner, out);
            }
        }
        TypedExpr::Block(b) => collect_escaping_block(b, out),
        TypedExpr::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            collect_escaping_expr(cond, out);
            collect_escaping_block(then_block, out);
            if let Some(eb) = else_block.as_deref() {
                collect_escaping_block(eb, out);
            }
        }
        TypedExpr::While { cond, body, .. } => {
            collect_escaping_expr(cond, out);
            collect_escaping_block(body, out);
        }
        TypedExpr::For { iter, body, .. } => {
            collect_escaping_expr(iter, out);
            collect_escaping_block(body, out);
        }
        TypedExpr::Loop { body, .. } => collect_escaping_block(body, out),
        TypedExpr::Break { value, .. } => {
            if let Some(v) = value {
                collect_escaping_expr(v, out);
            }
        }
        TypedExpr::Continue { .. } => {}
        TypedExpr::Return { value, .. } => {
            if let Some(v) = value {
                collect_escaping_expr(v, out);
            }
        }
        TypedExpr::Call { args, .. } => {
            for a in args {
                collect_escaping_expr(a, out);
            }
        }
        TypedExpr::MethodCall { receiver, args, .. } => {
            collect_escaping_expr(receiver, out);
            for a in args {
                collect_escaping_expr(a, out);
            }
        }
        TypedExpr::Tuple { elements, .. } => {
            for el in elements {
                collect_escaping_expr(el, out);
            }
        }
        TypedExpr::Parse { text, .. } => collect_escaping_expr(text, out),
        TypedExpr::RecordLit { fields, .. } => {
            for (_, init) in fields {
                collect_escaping_expr(init, out);
            }
        }
        TypedExpr::FieldGet { receiver, .. } => collect_escaping_expr(receiver, out),
        TypedExpr::EnumVariant { args, .. } => {
            for a in args {
                collect_escaping_expr(a, out);
            }
        }
        TypedExpr::Match {
            scrutinee, arms, ..
        } => {
            collect_escaping_expr(scrutinee, out);
            for arm in arms {
                collect_escaping_expr(&arm.body, out);
            }
        }
        TypedExpr::Lit { .. } | TypedExpr::Path { .. } | TypedExpr::Read { .. } => {}
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
    /// The inferred type for each name reference's range (filled by inference).
    /// Used to read a captured binding's type off the reference site.
    ref_types: &'a HashMap<TextRange, Type>,
    /// Each call site's monomorphization witness (WS8, §13.6), keyed by the
    /// callee name token's range. Read in `lower_call` to attach concrete arg
    /// types to each `TypedExpr::Call`.
    call_sites: &'a HashMap<TextRange, crate::CallSite>,
    diagnostics: Vec<Diagnostic>,
    /// The built-in method catalog (§16.2), used to resolve `receiver.method()`
    /// calls to their runtime lowering symbol. Immutable; built once.
    catalog: &'static praxis_stdlib::MethodCatalog,
    /// Cached handles for the common scalar/unit types, so the lowering does not
    /// allocate a fresh slot on every literal/binop (which would need `&mut db`
    /// repeatedly). Populated once at construction.
    int: Type,
    float: Type,
    bool_: Type,
    text: Type,
    unit: Type,
    /// A monotonically increasing counter for synthesizing unique closure MIR
    /// function names (e.g. `__closure_0`). Each closure literal in the module
    /// gets a distinct name.
    closure_counter: u32,
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

    /// The byte span `[start, end)` of a rowen syntax node, as a `(u32, u32)`
    /// pair threaded onto the typed tree for debugger provenance (per-temp
    /// `@ "expr"` rendering) and future diagnostics. Used at every `TypedExpr`/
    /// `TypedStmt` construction site.
    fn node_span(&self, node: &praxis_ast::SyntaxNode) -> (u32, u32) {
        let r = node.text_range();
        (u32::from(r.start()), u32::from(r.end()))
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

        // Determine the function's scheme and instantiate it once to read the
        // concrete shape (params/result). For a monomorphic function the
        // instantiation equals the body; for a polymorphic function the mono
        // pass (WS8, §13.6) clones and specializes this TypedFn per call site.
        let scheme = self.names.get(symbol).and_then(|s| s.scheme.clone());
        let fn_type = match &scheme {
            Some(s) => self.db.instantiate(s),
            None => {
                // No scheme inferred (errored in M2); skip — don't cascade.
                return None;
            }
        };

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
                    value: Lit::Unit,
                    ty: self.unit,
                    span: self.node_span(item.syntax()),
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
            // The whole `fn ... { ... }` declaration's byte span (§9.3, M10-WS1).
            // Threaded to MIR `Function` → backend → debug frame so the `source`
            // REPL command can render the faulting function's extent.
            span: {
                let r = item.syntax().text_range();
                (u32::from(r.start()), u32::from(r.end()))
            },
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
            value: Lit::Unit,
            ty: self.unit,
            span: self.node_span(block.syntax()),
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
            span: self.node_span(stmt.syntax()),
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
            span: self.node_span(stmt.syntax()),
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
            span: self.node_span(stmt.syntax()),
        })
    }

    // --- expressions -------------------------------------------------------

    fn lower_expr(&mut self, expr: &Expr) -> TypedExpr {
        let span = self.node_span(expr.syntax());
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
                    span,
                },
            },
            Expr::Block(b) => self
                .lower_block(b)
                .map(|b| TypedExpr::Block(Box::new(b)))
                .unwrap_or_else(|| TypedExpr::Lit {
                    value: Lit::Unit,
                    ty: self.unit,
                    span,
                }),
            Expr::If(i) => self.lower_if(i),
            Expr::While(w) => self.lower_while(w),
            Expr::For(f) => self.lower_for(f),
            Expr::Loop(l) => self.lower_loop(l),
            Expr::Break(b) => self.lower_break(b),
            Expr::Continue(c) => self.lower_continue(c),
            Expr::Return(r) => self.lower_return(r),
            Expr::Call(c) => self.lower_call(c),
            Expr::MethodCall(m) => self.lower_method_call(m),
            Expr::Tuple(t) => self.lower_tuple(t),
            Expr::Read(r) => self.lower_read(r),
            Expr::Parse(p) => self.lower_parse(p),
            Expr::RecordLit(r) => self.lower_record_lit(r),
            Expr::FieldGet(f) => self.lower_field_get(f),
            Expr::Match(m) => self.lower_match(m),
            // M7-WS7: closure parsing, resolution, and inference are complete;
            // the runtime lowering (synthetic MIR function, capture environment,
            // indirect call) is the remaining WS7 work. For now the lowerer
            // produces a placeholder so type-checking works end-to-end.
            Expr::Closure(c) => self.lower_closure(c),
            Expr::Error(_) => TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.db.fresh_var(),
                span,
            },
        }
    }

    /// Lower a closure expression (M7-WS7, §4.10). Runs capture analysis to find
    /// the free variables (each becomes one env slot), lowers the params and body,
    /// and produces a [`TypedExpr::Closure`] carrying a synthesized unique MIR
    /// function name. The closure's *type* (`fn_type`) is the inferred `Func`.
    ///
    /// The capture environment is a runtime concern (§4.10): the type system does
    /// not model it. Immutable (`let`/`param`) captures copy the value into the
    /// env; mutable (`var`) captures share a `VarCell` (WS7b) — the env holds the
    /// cell, and the binding site boxes the `var` so writes are visible across
    /// frames.
    fn lower_closure(&mut self, c: &praxis_ast::ClosureExpr) -> TypedExpr {
        let span = self.node_span(c.syntax());
        // The closure's inferred Func type comes from inference: re-derive it by
        // reading the body's type and the param types. Inference already pinned
        // these; we read them off the lowered params/body rather than re-querying
        // (the lowerer is a pure consumer of the finalized TypeDb).
        let params: Vec<TypedParam> = c.params().filter_map(|p| self.lower_param(&p)).collect();
        // The body is an expression. If it is a block, lower it as one; otherwise
        // wrap the single expression as a block whose tail is that expression.
        let body = match c.body() {
            Some(praxis_ast::Expr::Block(b)) => {
                self.lower_block(&b).unwrap_or_else(|| TypedBlock {
                    stmts: Vec::new(),
                    tail: TypedExpr::Lit {
                        value: Lit::Unit,
                        ty: self.unit,
                        span,
                    },
                    ty: self.unit,
                })
            }
            Some(other) => {
                let tail = self.lower_expr(&other);
                let ty = expr_ty(&tail);
                TypedBlock {
                    stmts: Vec::new(),
                    tail,
                    ty,
                }
            }
            None => TypedBlock {
                stmts: Vec::new(),
                tail: TypedExpr::Lit {
                    value: Lit::Unit,
                    ty: self.unit,
                    span,
                },
                ty: self.unit,
            },
        };
        let result_ty = body.ty;
        let param_types: Vec<Type> = params.iter().map(|p| p.ty).collect();
        let fn_type = self.db.func(param_types, result_ty);

        // Capture analysis: walk the closure body for free variables. The
        // "inside the closure" boundary is the whole closure node (params + body)
        // so params and closure-local bindings are recognized as locals.
        let closure_range = c.syntax().text_range();
        let body_expr = match c.body() {
            Some(b) => b,
            None => {
                return TypedExpr::Closure {
                    params,
                    body: Box::new(body),
                    captures: Vec::new(),
                    fn_type,
                    fn_name: self.fresh_closure_name(),
                    ty: fn_type,
                    span,
                };
            }
        };
        let analysis = crate::capture::analyze(
            &body_expr,
            closure_range,
            self.refs,
            |sym| self.decls.iter().find(|(_, s)| **s == sym).map(|(r, _)| *r),
            |sym| self.names.get(sym).map(|s| s.kind),
        );
        // Resolve each capture's name and type. The type is read from the
        // reference site's inferred type (`ref_types`); the name from the symbol.
        let captures: Vec<crate::capture::Capture> = analysis
            .captures
            .iter()
            .map(|fv| {
                let name = self
                    .names
                    .get(fv.symbol)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                let ty = self
                    .ref_types
                    .get(&fv.ref_range)
                    .copied()
                    .unwrap_or(self.db.fresh_var());
                let kind = if matches!(fv.kind, crate::symbol::SymbolKind::Var) {
                    crate::capture::CaptureKind::ByCell
                } else {
                    crate::capture::CaptureKind::ByValue
                };
                crate::capture::Capture {
                    symbol: fv.symbol,
                    name,
                    ty,
                    kind,
                }
            })
            .collect();

        let fn_name = self.fresh_closure_name();
        TypedExpr::Closure {
            params,
            body: Box::new(body),
            captures,
            fn_type,
            fn_name,
            ty: fn_type,
            span,
        }
    }

    /// Mint a fresh, unique synthetic MIR function name for a closure.
    fn fresh_closure_name(&mut self) -> String {
        let n = self.closure_counter;
        self.closure_counter += 1;
        format!("__closure_{n}")
    }

    fn lower_literal(&mut self, lit: &Literal) -> TypedExpr {
        let span = self.node_span(lit.syntax());
        let Some(tok) = lit.token() else {
            return TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.db.fresh_var(),
                span,
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
                    span,
                }
            }
            SyntaxKind::FloatLit => {
                let text = tok.text();
                // Parse the lexed float token (`3.14`, `1e10`, …) as f64. The
                // lexer guarantees a valid float syntax, so parse failure is a
                // defensive fallback (substitute 0.0) rather than a panic.
                let value = text.parse::<f64>().unwrap_or(0.0);
                TypedExpr::Lit {
                    value: Lit::Float(value),
                    ty: self.float,
                    span,
                }
            }
            SyntaxKind::TextLit => {
                let raw = tok.text();
                let unquoted = unquote_text(raw);
                TypedExpr::Lit {
                    value: Lit::Text(unquoted),
                    ty: self.text,
                    span,
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
                    span,
                }
            }
            SyntaxKind::KW_TRUE => TypedExpr::Lit {
                value: Lit::Bool(true),
                ty: self.bool_,
                span,
            },
            SyntaxKind::KW_FALSE => TypedExpr::Lit {
                value: Lit::Bool(false),
                ty: self.bool_,
                span,
            },
            _ => TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.db.fresh_var(),
                span,
            },
        }
    }

    fn lower_path(&mut self, p: &PathExpr) -> TypedExpr {
        let span = self.node_span(p.syntax());
        // M7-WS4: detect a zero-payload enum variant used as a bare path (`Empty`).
        if let Some(tok) = p.name() {
            let name = tok.text().to_string();
            if let Some((enum_def_id, variant_idx, _)) = self.lookup_enum_variant_by_name(&name) {
                // Only treat as a variant if the payload is empty (a zero-payload
                // variant). Payload variants are handled in lower_call.
                let edef = self.db.enum_def(enum_def_id);
                if !edef.variants[variant_idx].has_payload() {
                    let enum_ty = self.db.enum_type(enum_def_id);
                    return TypedExpr::EnumVariant {
                        enum_def_id,
                        variant_idx: variant_idx as u32,
                        args: Vec::new(),
                        ty: enum_ty,
                        span,
                    };
                }
            }
        }
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
        TypedExpr::Path { symbol, ty, span }
    }

    fn lower_bin(&mut self, b: &BinExpr) -> TypedExpr {
        let span = self.node_span(b.syntax());
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
                // The result type follows the operands under the strict
                // per-literal model (§4.12): Float if either operand is a float
                // literal or has a Float-typed lowering (e.g. a method call
                // result); Int otherwise. This mirrors the inference-time check.
                let lhs_is_float = matches!(
                    lhs.as_deref(),
                    Some(TypedExpr::Lit {
                        value: Lit::Float(_),
                        ..
                    })
                );
                let rhs_is_float = matches!(
                    rhs.as_deref(),
                    Some(TypedExpr::Lit {
                        value: Lit::Float(_),
                        ..
                    })
                );
                // Determine float-ness from the operands' resolved TypeData, not
                // by Type-index equality: two independently-interned `Scalar(Float)`
                // types are structurally equal but may have distinct indices, so a
                // `Type == Type` check is unreliable. Match on the followed data.
                let is_float_scalar = |e: &TypedExpr| {
                    matches!(
                        self.db.data(self.db.follow(expr_ty(e))),
                        praxis_types::TypeData::Scalar(praxis_types::ScalarType::Float)
                    )
                };
                let any_float_ty = [lhs.as_deref(), rhs.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(is_float_scalar);
                let is_float = lhs_is_float || rhs_is_float || any_float_ty;
                (op, if is_float { self.float } else { self.int })
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
            span,
        }
    }

    fn lower_unary(&mut self, u: &UnaryExpr) -> TypedExpr {
        let span = self.node_span(u.syntax());
        let operand = u.operand().map(|e| Box::new(self.lower_expr(&e)));
        let (op, ty) = match u.op().map(|t| t.kind()) {
            // Negation follows the operand's literal kind (§4.12): `-3.5` is
            // Float, `-3` is Int. A float literal lowers to `Lit::Float`.
            Some(SyntaxKind::MINUS) => {
                let is_float = matches!(
                    operand.as_deref(),
                    Some(TypedExpr::Lit {
                        value: Lit::Float(_),
                        ..
                    })
                );
                (UnaryOp::Neg, if is_float { self.float } else { self.int })
            }
            Some(SyntaxKind::BANG) => (UnaryOp::Not, self.bool_),
            _ => (UnaryOp::Neg, self.db.fresh_var()),
        };
        TypedExpr::Unary {
            op,
            operand: operand.unwrap_or_else(|| Box::new(unit_lit(self.db))),
            ty,
            span,
        }
    }

    fn lower_if(&mut self, i: &IfExpr) -> TypedExpr {
        let span = self.node_span(i.syntax());
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
            span,
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
        let span = self.node_span(w.syntax());
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
            span,
        }
    }

    /// `for binding in iter { body }` (M8, §4.11).
    fn lower_for(&mut self, f: &ForExpr) -> TypedExpr {
        let span = self.node_span(f.syntax());
        let iter = f
            .iter()
            .map(|i| Box::new(self.lower_expr(&i)))
            .unwrap_or_else(|| Box::new(unit_lit(self.db)));
        let body = f
            .body()
            .and_then(|b| self.lower_block(&b))
            .map(Box::new)
            .unwrap_or_else(|| {
                Box::new(TypedBlock {
                    stmts: Vec::new(),
                    tail: unit_lit(self.db),
                    ty: self.unit,
                })
            });
        // Resolve the binding symbol from the name token's declaration site
        // (`decls`, not `refs` — the binding token is a declaration, and the
        // body's references to it resolve to this same symbol via `refs`).
        let binding = f
            .binding()
            .and_then(|t| self.decls.get(&t.text_range()).copied())
            .unwrap_or(SymbolId(0));
        // The item type is read from the iterator's inferred element type; the
        // inference pass records it on the binding's declaration range. Fall
        // back to a fresh var if unavailable (malformed tree).
        let item_ty = f
            .binding()
            .and_then(|t| self.ref_types.get(&t.text_range()).copied())
            .unwrap_or_else(|| self.db.fresh_var());
        TypedExpr::For {
            binding,
            iter,
            body,
            item_ty,
            ty: self.unit,
            span,
        }
    }

    /// `loop { body }` (M8, §4.11). The type is Unit for now (value-producing
    /// loops via `break expr` refine this in the MIR; the HIR conservatively
    /// reports Unit so a `loop` used as a value still type-checks broadly).
    fn lower_loop(&mut self, l: &LoopExpr) -> TypedExpr {
        let span = self.node_span(l.syntax());
        let body = l
            .body()
            .and_then(|b| self.lower_block(&b))
            .map(Box::new)
            .unwrap_or_else(|| {
                Box::new(TypedBlock {
                    stmts: Vec::new(),
                    tail: unit_lit(self.db),
                    ty: self.unit,
                })
            });
        TypedExpr::Loop {
            body,
            ty: self.unit,
            span,
        }
    }

    /// `break [expr]` (M8, §4.11). Diverges; the optional value is lowered but
    /// the expression's type is `Never`.
    fn lower_break(&mut self, b: &BreakExpr) -> TypedExpr {
        let span = self.node_span(b.syntax());
        let value = b.value().map(|v| Box::new(self.lower_expr(&v)));
        TypedExpr::Break {
            value,
            ty: self.db.never(),
            span,
        }
    }

    /// `continue` (M8, §4.11). Diverges; type `Never`.
    fn lower_continue(&mut self, c: &ContinueExpr) -> TypedExpr {
        TypedExpr::Continue {
            ty: self.db.never(),
            span: self.node_span(c.syntax()),
        }
    }

    /// `return [expr]` (M8, §4.11). Diverges; type `Never`.
    fn lower_return(&mut self, r: &ReturnExpr) -> TypedExpr {
        let span = self.node_span(r.syntax());
        let value = r.value().map(|v| Box::new(self.lower_expr(&v)));
        TypedExpr::Return {
            value,
            ty: self.db.never(),
            span,
        }
    }

    fn lower_call(&mut self, c: &CallExpr) -> TypedExpr {
        let span = self.node_span(c.syntax());
        let callee_tok = c.callee().and_then(|p| p.name());
        // Postfix call on an arbitrary expression (`expr(args)`, M8 §4.10):
        // when there is no named (PathExpr) callee, the callee is an expression
        // (e.g. `fs.get(0)` in `fs.get(0)(100)`). Lower it; the MIR builder
        // emits an indirect call through its closure fn_ptr.
        let callee_expr: Option<Box<TypedExpr>> = if callee_tok.is_none() {
            c.callee_expr().map(|e| Box::new(self.lower_expr(&e)))
        } else {
            None
        };
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
        // M7-WS4: detect enum variant construction. If the callee's scheme is a
        // Func returning an enum type, this is a payload variant like `Number(5)`.
        if let Some(name) = callee_tok.as_ref().map(|t| t.text().to_string()) {
            if let Some((enum_def_id, variant_idx, _payload)) =
                self.lookup_enum_variant_by_name(&name)
            {
                let enum_ty = self.db.enum_type(enum_def_id);
                return TypedExpr::EnumVariant {
                    enum_def_id,
                    variant_idx: variant_idx as u32,
                    args,
                    ty: enum_ty,
                    span,
                };
            }
        }
        // The call's result type comes from the callee's instantiated scheme.
        // For a postfix (expression) callee, the result type is the Func type's
        // result, inferred and recorded on the callee_expr by inference.
        let ty = if let Some(ce) = callee_expr.as_deref() {
            // The callee expression's inferred type is a Func (params) -> result;
            // follow it to read the result. If it is not yet a Func (inference
            // could not pin it), fall back to a fresh var.
            func_result_type(self.db, expr_ty(ce)).unwrap_or_else(|| self.db.fresh_var())
        } else {
            self.call_result_type(callee)
                .unwrap_or_else(|| self.db.fresh_var())
        };
        // The concrete arg types at this call site (WS8, §13.6). Recorded by
        // inference in `analysis.call_sites`, keyed by the callee name token's
        // range. The mono pass reads these off the typed tree to instantiate a
        // polymorphic callee. Empty if the call site wasn't recorded (e.g. an
        // unresolved callee, or a postfix expression callee) — the mono pass
        // treats an empty vec as monomorphic.
        let arg_types = callee_tok
            .as_ref()
            .and_then(|t| self.call_sites.get(&t.text_range()))
            .map(|cs| cs.arg_types.clone())
            .unwrap_or_default();
        TypedExpr::Call {
            callee,
            callee_name,
            args,
            arg_types,
            callee_expr,
            ty,
            span,
        }
    }

    /// Look up an enum variant by constructor name (for lowering). Returns the
    /// enum def-id, variant index, and payload types.
    fn lookup_enum_variant_by_name(
        &self,
        name: &str,
    ) -> Option<(praxis_types::EnumDefId, usize, Vec<Type>)> {
        let root = self.scopes.root();
        let symbol = self.scopes.lookup(root, name)?;
        let sym = self.names.get(symbol)?;
        let scheme = sym.scheme.as_ref()?;
        let result_ty = match self.db.data(self.db.follow(scheme.body())) {
            praxis_types::TypeData::Func { result, .. } => *result,
            praxis_types::TypeData::Enum { .. } => scheme.body(),
            _ => return None,
        };
        let def_id = match self.db.data(self.db.follow(result_ty)) {
            praxis_types::TypeData::Enum { def } => *def,
            _ => return None,
        };
        let edef = self.db.enum_def(def_id);
        let idx = edef.variant(name)?;
        let payload = edef.variants[idx].payload.clone();
        Some((def_id, idx, payload))
    }

    fn lower_args(&mut self, args: &ArgList) -> Vec<TypedExpr> {
        args.args().map(|a| self.lower_expr(&a)).collect()
    }

    /// Lower `receiver.method(args)` (M5, §16.2). Resolves the method against
    /// the built-in catalog via the [`crate::catalog`] bridge, recording the
    /// runtime lowering symbol so the MIR builder emits a direct call.
    fn lower_method_call(&mut self, m: &MethodCallExpr) -> TypedExpr {
        let span = self.node_span(m.syntax());
        // Lower the receiver (or fall back to a Unit-typed literal if the tree
        // is malformed so the rest of the expression still lowers).
        let receiver = match m.receiver() {
            Some(r) => self.lower_expr(&r),
            None => TypedExpr::Lit {
                value: Lit::Unit,
                ty: self.unit,
                span,
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
        // Resolve the method against the catalog, keyed by the receiver's
        // inferred type + name + arity (ADR-010 bridge).
        let receiver_ty = expr_ty(&receiver);
        let hits = crate::catalog::lookup(self.db, self.catalog, receiver_ty, &name, arity);
        if let Some(entry) = hits.first() {
            let ty = pattern_to_type(self.db, &entry.result);
            let lowering_symbol = match &entry.lowering {
                praxis_stdlib::MethodLowering::RuntimeSymbol(sym) => Some(*sym),
                // An intrinsic has no runtime symbol: the MIR builder lowers it
                // (the M8 pipeline combinators) rather than emitting a call.
                praxis_stdlib::MethodLowering::Intrinsic(_) => None,
            };
            TypedExpr::MethodCall {
                receiver: Box::new(receiver),
                name,
                lowering_symbol,
                args,
                purity: entry.purity,
                ty,
                span,
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
                value: Lit::Unit,
                ty: self.unit,
                span,
            }
        }
    }

    fn lower_tuple(&mut self, t: &TupleExpr) -> TypedExpr {
        let span = self.node_span(t.syntax());
        let elements: Vec<TypedExpr> = t.elements().map(|e| self.lower_expr(&e)).collect();
        let tys: Vec<Type> = elements.iter().map(expr_ty).collect();
        let ty = tuple_or_degenerate(self.db, tys);
        TypedExpr::Tuple { elements, ty, span }
    }

    /// Lower a `read parser_expression` (§7.1, M6). Analyzes the parser expr
    /// (validate + synthesize type + lower to plan), then produces a `TypedExpr`
    /// carrying the plan index and synthesized result type.
    fn lower_read(&mut self, r: &praxis_ast::ReadExpr) -> TypedExpr {
        let span = self.node_span(r.syntax());
        let Some(parser_expr) = r.parser_expr() else {
            return self.error_expr();
        };
        match crate::parser_lower::analyze_parser_expr(
            &parser_expr,
            self.file,
            self.db,
            &mut self.diagnostics,
        ) {
            Some(analysis) => TypedExpr::Read {
                plan: analysis.plan,
                ty: analysis.result_type,
                span,
            },
            None => self.error_expr(),
        }
    }

    /// Lower a `parse(text, parser_expression)` call (§7.1, M6).
    fn lower_parse(&mut self, p: &praxis_ast::ParseExpr) -> TypedExpr {
        let span = self.node_span(p.syntax());
        let text_expr = p
            .text_expr()
            .map(|e| self.lower_expr(&e))
            .unwrap_or_else(|| self.error_expr());
        let ty = match &text_expr {
            TypedExpr::Lit { ty, .. } => *ty,
            _ => self.db.fresh_var(),
        };
        match p.parser_expr() {
            Some(parser_expr) => {
                match crate::parser_lower::analyze_parser_expr(
                    &parser_expr,
                    self.file,
                    self.db,
                    &mut self.diagnostics,
                ) {
                    Some(analysis) => TypedExpr::Parse {
                        text: Box::new(text_expr),
                        plan: analysis.plan,
                        ty: analysis.result_type,
                        span,
                    },
                    // Analysis failed and has already pushed a diagnostic. This
                    // used to emit `plan_index: 0`, which is a perfectly valid
                    // index — the first plan any program registers — so a
                    // broken `parse(...)` ran somebody else's parser. There is
                    // no longer a `PlanId` that means "none"; an error
                    // expression is the honest lowering (IP-12).
                    None => self.error_expr(),
                }
            }
            // No parser expression at all: the same "there is no plan" case as
            // above, and the same answer.
            None => {
                let _ = ty;
                self.error_expr()
            }
        }
    }

    /// A typed expression representing a lowering error (Unit-typed literal).
    /// No source span is available (this is a synthetic fallback).
    fn error_expr(&self) -> TypedExpr {
        TypedExpr::Lit {
            value: Lit::Unit,
            ty: self.unit,
            span: (0, 0),
        }
    }

    /// Lower a `Name { field: expr, … }` record literal (M7, §4.5). Looks up the
    /// struct type from the symbol table, pairs each initializer with its field
    /// index, and produces a `TypedExpr::RecordLit`.
    fn lower_record_lit(&mut self, r: &RecordLitExpr) -> TypedExpr {
        let span = self.node_span(r.syntax());
        let struct_ty = r
            .name()
            .and_then(|p| p.name())
            .and_then(|tok| self.resolve_symbol_at(tok.text_range()))
            .and_then(|sym| self.names.get(sym))
            .and_then(|s| s.scheme.as_ref().map(|sc| self.db.instantiate(sc)));
        let Some(struct_ty) = struct_ty else {
            return self.error_expr();
        };
        let record_def_id = match self.db.data(self.db.follow(struct_ty)) {
            praxis_types::TypeData::Record { def } => *def,
            _ => return self.error_expr(),
        };
        let rdef = self.db.record_def(record_def_id).clone();
        let mut fields = Vec::new();
        if let Some(fl) = r.field_list() {
            for f in fl.fields() {
                let Some(name_tok) = f.name() else { continue };
                let fname = name_tok.text().to_string();
                let Some((idx, _)) = rdef.field(&fname) else {
                    continue;
                };
                let init = match &f.expr() {
                    Some(e) => self.lower_expr(e),
                    // Punned field `{ x }` — lower as a path reference to `x`,
                    // using the variable's actual type (so the record field gets
                    // the right value).
                    None => {
                        let range = name_tok.text_range();
                        let span = (u32::from(range.start()), u32::from(range.end()));
                        self.resolve_symbol_at(range)
                            .map(|symbol| TypedExpr::Path {
                                symbol,
                                ty: self.symbol_type(symbol),
                                span,
                            })
                            .unwrap_or_else(|| self.error_expr())
                    }
                };
                fields.push((idx as u32, init));
            }
        }
        // Sort fields into declaration order so the runtime allocates them in
        // the schema's field order.
        fields.sort_by_key(|(idx, _)| *idx);
        TypedExpr::RecordLit {
            record_def_id,
            fields,
            ty: struct_ty,
            span,
        }
    }

    /// Lower a `receiver.field` field access (M7, §4.5). Looks up the field's
    /// index and type from the receiver's record type.
    fn lower_field_get(&mut self, f: &FieldExpr) -> TypedExpr {
        let span = self.node_span(f.syntax());
        let receiver = match f.receiver() {
            Some(r) => self.lower_expr(&r),
            None => return self.error_expr(),
        };
        let receiver_ty = expr_ty(&receiver);
        let resolved = self.db.follow(receiver_ty);
        let Some((field_idx, field_ty)) = (match self.db.data(resolved) {
            praxis_types::TypeData::Record { def } => {
                let rdef = self.db.record_def(*def);
                f.field_name().and_then(|tok| rdef.field(tok.text()))
            }
            _ => None,
        }) else {
            // Not a record type, or unknown field — emit a Y1xx diagnostic.
            if let Some(tok) = f.field_name() {
                self.diag(
                    tok.text_range(),
                    112,
                    format!("no field `{}` on this type", tok.text()),
                );
            }
            return self.error_expr();
        };
        TypedExpr::FieldGet {
            receiver: Box::new(receiver),
            field_idx: field_idx as u32,
            ty: field_ty,
            span,
        }
    }

    /// Lower a `match scrutinee { pattern => body, … }` expression (M7, §4.6).
    /// Each arm is converted to a [`TypedMatchArm`] with the variant index (or
    /// `None` for wildcard) and payload bindings. The MIR builder lowers this to
    /// a tag-compare branch chain.
    fn lower_match(&mut self, m: &praxis_ast::MatchExpr) -> TypedExpr {
        let span = self.node_span(m.syntax());
        let scrutinee = match m.scrutinee() {
            Some(s) => self.lower_expr(&s),
            None => return self.error_expr(),
        };
        let scrutinee_ty = expr_ty(&scrutinee);
        let mut arms = Vec::new();
        let mut arm_spans = Vec::new();
        for arm in m.arms() {
            let pattern = match arm.pattern() {
                Some(pat) => self.lower_pattern(&pat, scrutinee_ty),
                None => TypedPattern::Wildcard,
            };
            let body = match arm.body() {
                Some(b) => self.lower_expr(&b),
                None => self.error_expr(),
            };
            arm_spans.push(self.file_span(arm.syntax().text_range()));
            arms.push(TypedMatchArm { pattern, body });
        }
        // Check exhaustiveness and unreachable arms (§4.6, the WS5 follow-up).
        crate::exhaustive::check(
            self.db,
            self.file,
            scrutinee_ty,
            &arms,
            &arm_spans,
            &mut self.diagnostics,
        );
        // The match's type is the unified body type (inference already unified
        // them); use the first arm's body type.
        let ty = arms.first().map(|a| expr_ty(&a.body)).unwrap_or(self.unit);
        TypedExpr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            ty,
            span,
        }
    }

    /// Lower a pattern into a recursive [`TypedPattern`]. This fixes two M7-Part-1
    /// gaps: nested sub-patterns are now recursed into (previously silently
    /// dropped), and literal patterns carry their value (previously treated as
    /// catch-all wildcards, so `match n { 1 => a, 2 => b }` always took the first
    /// arm).
    ///
    /// A bare `Name` is ambiguous (variable bind vs payload-less variant) and is
    /// disambiguated against the scrutinee's enum type, as in WS5.
    fn lower_pattern(&mut self, pat: &praxis_ast::Pattern, scrutinee_ty: Type) -> TypedPattern {
        use praxis_ast::PatternKind;
        match pat.kind() {
            PatternKind::Wildcard => TypedPattern::Wildcard,
            PatternKind::Literal => {
                // Read the literal value from the pattern's token (the WS5 bug
                // was that literals were dropped to a catch-all wildcard).
                let Some(tok) = pat.literal_token() else {
                    return TypedPattern::Wildcard;
                };
                let value = match tok.kind() {
                    SyntaxKind::IntLit => {
                        let cleaned: String = tok.text().chars().filter(|c| *c != '_').collect();
                        Lit::Int(cleaned.parse::<i64>().unwrap_or(i64::MAX))
                    }
                    SyntaxKind::FloatLit => Lit::Float(tok.text().parse::<f64>().unwrap_or(0.0)),
                    SyntaxKind::TextLit => Lit::Text(unquote_text(tok.text())),
                    SyntaxKind::KW_TRUE => Lit::Bool(true),
                    SyntaxKind::KW_FALSE => Lit::Bool(false),
                    _ => return TypedPattern::Wildcard,
                };
                let ty = match &value {
                    Lit::Int(_) => self.int,
                    Lit::Float(_) => self.float,
                    Lit::Bool(_) => self.bool_,
                    Lit::Text(_) => self.text,
                    // Char literals don't appear in patterns (no char-literal
                    // pattern syntax); use the scrutinee type as a fallback.
                    Lit::Char(_) => scrutinee_ty,
                    // `Unit` literals are synthesized internally; the parser
                    // produces no Unit pattern, so this arm is defensive.
                    Lit::Unit => self.unit,
                };
                TypedPattern::Lit { value, ty }
            }
            PatternKind::Name(name) => {
                // Disambiguate payload-less variant from variable bind by checking
                // the scrutinee's enum type (the WS5 fix).
                let resolved = self.db.follow(scrutinee_ty);
                if let praxis_types::TypeData::Enum { def } = self.db.data(resolved) {
                    let edef = self.db.enum_def(*def);
                    if let Some(idx) = edef.variant(&name) {
                        return TypedPattern::EnumVariant {
                            enum_def_id: *def,
                            variant_idx: idx as u32,
                            subpatterns: Vec::new(),
                            ty: scrutinee_ty,
                        };
                    }
                }
                // Not a variant: a variable bind. Resolve the declared symbol.
                if let Some(tok) = pat.name_token() {
                    if let Some(symbol) = self.resolve_decl_at(tok.text_range()) {
                        return TypedPattern::Bind {
                            symbol,
                            ty: scrutinee_ty,
                        };
                    }
                }
                // Fallback: treat as wildcard if the symbol is unresolved.
                TypedPattern::Wildcard
            }
            PatternKind::Variant(vname) => {
                let resolved = self.db.follow(scrutinee_ty);
                let enum_def_id = match self.db.data(resolved) {
                    praxis_types::TypeData::Enum { def } => *def,
                    _ => return TypedPattern::Wildcard,
                };
                let edef = self.db.enum_def(enum_def_id);
                let Some(idx) = edef.variant(&vname) else {
                    return TypedPattern::Wildcard;
                };
                // Recurse into sub-patterns against payload types — the WS5 bug
                // was that only flat Name sub-patterns were collected and nested
                // variant patterns were silently dropped.
                let variant = &edef.variants[idx];
                let payload_types: Vec<Type> = variant.payload.clone();
                let sub_pats: Vec<_> = pat.sub_patterns().collect();
                let mut subpatterns = Vec::new();
                for (i, sub) in sub_pats.iter().enumerate() {
                    let sub_ty = payload_types.get(i).copied().unwrap_or(scrutinee_ty);
                    subpatterns.push(self.lower_pattern(sub, sub_ty));
                }
                TypedPattern::EnumVariant {
                    enum_def_id,
                    variant_idx: idx as u32,
                    subpatterns,
                    ty: scrutinee_ty,
                }
            }
        }
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

/// The source span `[start, end)` (byte offsets) carried by a typed expression.
/// Public so the MIR builder can thread each expression's provenance into the
/// debug-frame locals without re-matching the whole enum. `TypedExpr::Block`
/// has no top-level span field (it carries the inner block's tail span); this
/// returns `(0, 0)` for it.
pub fn expr_span(e: &TypedExpr) -> (u32, u32) {
    match e {
        TypedExpr::Lit { span, .. }
        | TypedExpr::Path { span, .. }
        | TypedExpr::Bin { span, .. }
        | TypedExpr::Unary { span, .. }
        | TypedExpr::Paren { span, .. }
        | TypedExpr::If { span, .. }
        | TypedExpr::While { span, .. }
        | TypedExpr::For { span, .. }
        | TypedExpr::Loop { span, .. }
        | TypedExpr::Break { span, .. }
        | TypedExpr::Continue { span, .. }
        | TypedExpr::Return { span, .. }
        | TypedExpr::Call { span, .. }
        | TypedExpr::MethodCall { span, .. }
        | TypedExpr::Tuple { span, .. }
        | TypedExpr::Read { span, .. }
        | TypedExpr::Parse { span, .. }
        | TypedExpr::RecordLit { span, .. }
        | TypedExpr::FieldGet { span, .. }
        | TypedExpr::EnumVariant { span, .. }
        | TypedExpr::Match { span, .. }
        | TypedExpr::Closure { span, .. } => *span,
        TypedExpr::Block(b) => expr_span(&b.tail),
    }
}

/// The source span `[start, end)` (byte offsets) carried by a typed statement.
/// `TypedStmt::Expr` carries the span on its inner expression.
pub fn stmt_span(s: &TypedStmt) -> (u32, u32) {
    match s {
        TypedStmt::Let { span, .. }
        | TypedStmt::Var { span, .. }
        | TypedStmt::Assign { span, .. } => *span,
        TypedStmt::Expr(e) => expr_span(e),
    }
}

/// The type carried by a typed expression. Public so the crash debugger (M10b)
/// and the LSP can read an expression's inferred type without re-matching the
/// whole enum.
pub fn expr_ty(e: &TypedExpr) -> Type {
    match e {
        TypedExpr::Lit { ty, .. } => *ty,
        TypedExpr::Path { ty, .. } => *ty,
        TypedExpr::Bin { ty, .. } => *ty,
        TypedExpr::Unary { ty, .. } => *ty,
        TypedExpr::Paren { ty, .. } => *ty,
        TypedExpr::Block(b) => b.ty,
        TypedExpr::If { ty, .. } => *ty,
        TypedExpr::While { ty, .. } => *ty,
        TypedExpr::For { ty, .. } => *ty,
        TypedExpr::Loop { ty, .. } => *ty,
        TypedExpr::Break { ty, .. } => *ty,
        TypedExpr::Continue { ty, .. } => *ty,
        TypedExpr::Return { ty, .. } => *ty,
        TypedExpr::Call { ty, .. } => *ty,
        TypedExpr::MethodCall { ty, .. } => *ty,
        TypedExpr::Tuple { ty, .. } => *ty,
        TypedExpr::Read { ty, .. } => *ty,
        TypedExpr::Parse { ty, .. } => *ty,
        TypedExpr::RecordLit { ty, .. } => *ty,
        TypedExpr::FieldGet { ty, .. } => *ty,
        TypedExpr::EnumVariant { ty, .. } => *ty,
        TypedExpr::Match { ty, .. } => *ty,
        TypedExpr::Closure { ty, .. } => *ty,
    }
}

/// The result type of a function-typed value, if `t` (after following) is a
/// `Func`. Used to read a postfix call's result type off its callee expression.
fn func_result_type(db: &TypeDb, t: Type) -> Option<Type> {
    match db.data(db.follow(t)) {
        praxis_types::TypeData::Func { result, .. } => Some(*result),
        _ => None,
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

/// Public name-aware variant of [`pattern_to_type`] for the inference pass
/// (bidirectional method-call inference, M8 §3). Shares one type variable per
/// `Var(name)` within one instantiation via the supplied `names` map.
pub fn pattern_to_type_named(
    db: &mut TypeDb,
    p: &TypePattern,
    names: &mut HashMap<String, Type>,
) -> Type {
    pattern_to_type_named_impl(db, p, names)
}

fn pattern_to_type(db: &mut TypeDb, p: &TypePattern) -> Type {
    match p {
        TypePattern::Scalar(s) => db.scalar(map_pattern_scalar(*s)),
        TypePattern::Unit => db.unit(),
        TypePattern::Var(_) => db.fresh_var(),
        TypePattern::Collection { ctor, args } => {
            let arg_tys: Vec<Type> = args.iter().map(|a| pattern_to_type(db, a)).collect();
            collection_from_pattern(db, *ctor, arg_tys)
        }
        TypePattern::Function { params, result } => {
            let ps: Vec<Type> = params.iter().map(|p| pattern_to_type(db, p)).collect();
            let r = pattern_to_type(db, result);
            db.func(ps, r)
        }
        TypePattern::Tuple(els) => {
            let tys: Vec<Type> = els.iter().map(|e| pattern_to_type(db, e)).collect();
            tuple_or_degenerate(db, tys)
        }
        TypePattern::Opaque => db.fresh_var(),
    }
}

/// Like [`pattern_to_type`], but shares a single type variable for each named
/// `Var(name)` within one instantiation. This is what the bidirectional method-
/// call inference needs: a combinator signature like `fold`'s
/// `(Acc, (Acc, T) -> Acc) -> Acc` names the accumulator `Acc` in three places,
/// and those must be the *same* type variable so the accumulator type threads
/// from the init argument through the closure params to the result. The
/// `names` map carries the per-instantiation sharing; pass a fresh map for each
/// combinator call site.
fn pattern_to_type_named_impl(
    db: &mut TypeDb,
    p: &TypePattern,
    names: &mut HashMap<String, Type>,
) -> Type {
    match p {
        TypePattern::Scalar(s) => db.scalar(map_pattern_scalar(*s)),
        TypePattern::Unit => db.unit(),
        TypePattern::Var(n) => {
            if let Some(&t) = names.get(*n) {
                t
            } else {
                let t = db.fresh_var();
                names.insert(n.to_string(), t);
                t
            }
        }
        TypePattern::Collection { ctor, args } => {
            let arg_tys: Vec<Type> = args
                .iter()
                .map(|a| pattern_to_type_named_impl(db, a, names))
                .collect();
            collection_from_pattern(db, *ctor, arg_tys)
        }
        TypePattern::Function { params, result } => {
            let ps: Vec<Type> = params
                .iter()
                .map(|p| pattern_to_type_named_impl(db, p, names))
                .collect();
            let r = pattern_to_type_named_impl(db, result, names);
            db.func(ps, r)
        }
        TypePattern::Tuple(els) => {
            let tys: Vec<Type> = els
                .iter()
                .map(|e| pattern_to_type_named_impl(db, e, names))
                .collect();
            tuple_or_degenerate(db, tys)
        }
        TypePattern::Opaque => db.fresh_var(),
    }
}

/// A collection type from a *catalog* [`TypePattern`], whose arity is
/// compiler-authored data rather than user input.
///
/// A row whose argument count disagrees with `ctor.arity()` is a bug in the
/// method catalog, not a program error, and F5 is the first thing that would
/// notice one — so it fails loudly here rather than interning a type nothing
/// can unify with. The standing sweep over the catalog's invariants is S18's
/// (RT-14/RT-15).
fn collection_from_pattern(
    db: &mut TypeDb,
    ctor: praxis_types::CollectionCtor,
    args: Vec<Type>,
) -> Type {
    let args = praxis_types::CollectionArgs::new(ctor, args)
        .unwrap_or_else(|e| panic!("method catalog row: {e}"));
    db.collection(ctor, args)
        .unwrap_or_else(|e| panic!("method catalog row: {e}"))
}

/// A tuple type, honouring F5's arity invariant: `()` is `Unit` and a lone
/// element is that element, because neither is a tuple.
fn tuple_or_degenerate(db: &mut TypeDb, mut els: Vec<Type>) -> Type {
    match els.len() {
        0 => db.unit(),
        1 => els.remove(0),
        _ => {
            let elems = praxis_types::TupleElems::new(els).expect("two or more elements");
            db.tuple(elems)
        }
    }
}

/// Map a stdlib pattern scalar to the inference scalar (they share the enum via
/// `praxis_types::ScalarType`, so this is identity).
fn map_pattern_scalar(s: PatternScalar) -> praxis_types::ScalarType {
    s
}

/// A `Unit`-typed literal placeholder (for malformed subtrees). No source span
/// is available (synthetic fallback).
fn unit_lit(db: &mut TypeDb) -> TypedExpr {
    TypedExpr::Lit {
        value: Lit::Unit,
        ty: db.unit(),
        span: (0, 0),
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
