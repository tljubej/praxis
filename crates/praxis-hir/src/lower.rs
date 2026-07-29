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
use praxis_source::{DiagCode, Diagnostic, FileSpan, Severity, Span};
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

/// Every variant's children, written **once** (F20).
///
/// Each row is `Variant => exprs: [...], blocks: [...]`, and the macro expands
/// it into the four accessors below. Adding a variant is a compile error here
/// (the match is exhaustive) rather than a silent omission in a walk somewhere
/// else — which is the failure mode this replaces: three hand-written ~29-arm
/// walks over one enum, each independently forgettable. One of them *had*
/// forgotten `Call.callee_expr`, and a mutable capture went unboxed for it
/// (HIR-08).
///
/// `exprs`/`blocks` name fields by shape: a plain field is one child, `opt` is
/// an `Option`, `each` is a sequence, and `field_each`/`arm_each` are the two
/// sequences whose elements are not bare expressions.
macro_rules! typed_expr_children {
    (
        $( $variant:ident { $( $field:ident $(: $shape:ident)? ),* $(,)? } ),* $(,)?
    ) => {
        impl TypedExpr {
            /// This expression's immediate sub-expressions, in evaluation order.
            /// Sub-*blocks* are [`TypedExpr::blocks`]; a walk usually wants both.
            pub fn children(&self) -> impl Iterator<Item = &TypedExpr> {
                let mut out: Vec<&TypedExpr> = Vec::new();
                typed_expr_children!(@arms self, out, push, $( $variant { $( $field $(: $shape)? ),* } ),*);
                out.into_iter()
            }

            /// [`TypedExpr::children`], mutably.
            pub fn children_mut(&mut self) -> impl Iterator<Item = &mut TypedExpr> {
                let mut out: Vec<&mut TypedExpr> = Vec::new();
                typed_expr_children!(@arms self, out, push_mut, $( $variant { $( $field $(: $shape)? ),* } ),*);
                out.into_iter()
            }

            /// This expression's immediate sub-blocks (an `if`'s branches, a
            /// loop's body, a closure's body).
            pub fn blocks(&self) -> impl Iterator<Item = &TypedBlock> {
                let mut out: Vec<&TypedBlock> = Vec::new();
                typed_expr_children!(@arms self, out, push_block, $( $variant { $( $field $(: $shape)? ),* } ),*);
                out.into_iter()
            }

            /// [`TypedExpr::blocks`], mutably.
            pub fn blocks_mut(&mut self) -> impl Iterator<Item = &mut TypedBlock> {
                let mut out: Vec<&mut TypedBlock> = Vec::new();
                typed_expr_children!(@arms self, out, push_block_mut, $( $variant { $( $field $(: $shape)? ),* } ),*);
                out.into_iter()
            }
        }
    };

    // The shared body of all four accessors: one arm per variant, binding every
    // child field and handing each to the collector `$how`.
    (@arms $e:expr, $out:ident, $how:ident, $( $variant:ident { $( $field:ident $(: $shape:ident)? ),* } ),*) => {
        match $e {
            $( TypedExpr::$variant { $( $field, )* .. } => {
                $( typed_expr_children!(@$how $out, $field $(, $shape)?); )*
            } )*
            // `Block` is a tuple variant and the only one; its child is the block.
            TypedExpr::Block(b) => {
                typed_expr_children!(@$how $out, b, block);
            }
        }
    };

    // --- the collectors: one per accessor × field shape --------------------
    // Expression fields (`push`/`push_mut`) ignore block fields, and vice versa.
    (@push $out:ident, $f:ident) => { $out.push(&**$f) };
    (@push $out:ident, $f:ident, opt) => { if let Some(x) = $f { $out.push(&**x) } };
    (@push $out:ident, $f:ident, each) => { $out.extend($f.iter()) };
    (@push $out:ident, $f:ident, field_each) => { $out.extend($f.iter().map(|(_, x)| x)) };
    (@push $out:ident, $f:ident, arm_each) => { $out.extend($f.iter().map(|a| &a.body)) };
    (@push $out:ident, $f:ident, block) => { let _ = $f; };
    (@push $out:ident, $f:ident, block_opt) => { let _ = $f; };

    (@push_mut $out:ident, $f:ident) => { $out.push(&mut **$f) };
    (@push_mut $out:ident, $f:ident, opt) => { if let Some(x) = $f { $out.push(&mut **x) } };
    (@push_mut $out:ident, $f:ident, each) => { $out.extend($f.iter_mut()) };
    (@push_mut $out:ident, $f:ident, field_each) => { $out.extend($f.iter_mut().map(|(_, x)| x)) };
    (@push_mut $out:ident, $f:ident, arm_each) => { $out.extend($f.iter_mut().map(|a| &mut a.body)) };
    (@push_mut $out:ident, $f:ident, block) => { let _ = $f; };
    (@push_mut $out:ident, $f:ident, block_opt) => { let _ = $f; };

    (@push_block $out:ident, $f:ident, block) => { $out.push(&**$f) };
    (@push_block $out:ident, $f:ident, block_opt) => { if let Some(x) = $f { $out.push(&**x) } };
    (@push_block $out:ident, $f:ident $(, $other:ident)?) => { let _ = $f; };

    (@push_block_mut $out:ident, $f:ident, block) => { $out.push(&mut **$f) };
    (@push_block_mut $out:ident, $f:ident, block_opt) => { if let Some(x) = $f { $out.push(&mut **x) } };
    (@push_block_mut $out:ident, $f:ident $(, $other:ident)?) => { let _ = $f; };
}

typed_expr_children! {
    Lit {},
    Path {},
    Read {},
    Continue {},
    Bin { lhs, rhs },
    Unary { operand },
    Paren { inner: opt },
    If { cond, then_block: block, else_block: block_opt },
    While { cond, body: block },
    For { iter, body: block },
    Loop { body: block },
    Break { value: opt },
    Return { value: opt },
    // `callee_expr` is the field the escape walk forgot (HIR-08).
    Call { args: each, callee_expr: opt },
    MethodCall { receiver, args: each },
    Tuple { elements: each },
    Parse { text },
    RecordLit { fields: field_each },
    FieldGet { receiver },
    EnumVariant { args: each },
    Match { scrutinee, arms: arm_each },
    Closure { body: block },
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
        expr_types,
        method_refs,
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
        expr_types,
        method_refs,
        resolved: HashMap::new(),
        diagnostics: Vec::new(),
        catalog: builtin_catalog(),
        int,
        float,
        bool_,
        text,
        unit,
        closure_counter: 0,
        escaping_vars: std::collections::HashSet::new(),
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
    TypedModule {
        items,
        diagnostics: l.diagnostics,
        // Escape analysis (M7-WS7b): every `var` captured by cell, recorded as
        // each closure was lowered rather than re-derived by a walk afterwards
        // (HIR-08). These are boxed into a `VarCell` at their binding site so
        // the closure shares the cell.
        escaping_vars: l.escaping_vars,
    }
}

// The escape-analysis set (M7-WS7b) is accumulated by the lowerer itself, in
// `Lowerer::lower_closure`, where every closure's capture list is already in
// hand — see `Lowerer::escaping_vars`.
//
// It used to be a *fourth* walk over the typed tree, run after lowering, and it
// omitted `TypedExpr::Call.callee_expr`: an immediately invoked closure
// (`(|n| { count = count + n })(1)`) was never visited, so its `ByCell` capture
// never reached the set and the mutation was written to a copy (HIR-08).
// `CaptureKind::ByCell` and membership in `escaping_vars` are two
// representations of one fact; only one of them is computed now.

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
    /// Every inferred expression's type, keyed by its node (F15). **This is
    /// where a lowered node's type comes from.** Lowering used to derive one of
    /// its own — instantiating schemes a second time, re-resolving methods, and
    /// falling back to a fresh variable nineteen times over — which is why
    /// `id(1.5)` could lower as `?a` and `values.get(0)` as `?T`. It reads now.
    expr_types: &'a HashMap<crate::NodeKey, Type>,
    /// Each method call's resolved catalog entry and inferred result, keyed by
    /// the method-name token's range (F15). Lowering reads the entry rather than
    /// repeating the catalog lookup against a receiver type of its own.
    method_refs: &'a HashMap<TextRange, crate::MethodRef>,
    /// Memo for [`Lowerer::deep`]: a recorded type, fully resolved to its
    /// leaves. `follow` only resolves the top level, so a `Vec[?T]` whose `?T` a
    /// later `push` pinned would reach codegen with no element descriptor.
    resolved: HashMap<Type, Type>,
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
    /// Every `var` symbol some closure captures **by cell** (M7-WS7b, HIR-08).
    /// Recorded in `lower_closure`, where the capture list is in hand, so the
    /// set cannot disagree with the `CaptureKind::ByCell` decisions that produce
    /// it. The MIR builder boxes each of these into a `VarCell` at its binding
    /// site; a `var` missing here is one whose mutation a closure would write to
    /// a copy.
    escaping_vars: std::collections::HashSet<SymbolId>,
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

    fn diag(&mut self, at: TextRange, code: DiagCode, msg: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(
            Severity::Error,
            code,
            msg.into(),
            self.file_span(at),
        ));
    }

    // --- reading what inference decided (F15) --------------------------------

    /// The type inference recorded for `node`.
    ///
    /// A miss is `Y099`, never a fresh variable: inference visits every
    /// expression through one recording entry point, so a node it lowered and
    /// inference did not see is a compiler bug, and a fresh variable is exactly
    /// the silent lie this whole repair is tracking — it agrees with whatever
    /// the next use wants and the mistake surfaces as a missing descriptor
    /// three passes later.
    fn node_ty(&mut self, node: &SyntaxNode) -> Type {
        match self.expr_types.get(&crate::NodeKey::of(node)).copied() {
            Some(t) => self.deep(t),
            None => {
                self.diag(
                    node.text_range(),
                    DiagCode::InternalMissingType,
                    format!(
                        "internal: inference recorded no type for this {:?} expression",
                        node.kind()
                    ),
                );
                self.unit
            }
        }
    }

    /// `t` with links followed at **every** level, memoized.
    ///
    /// `follow` answers the top-level representative only, so a `Vec[?T]` whose
    /// element a later `push` pinned stays `Vec[?T]` — and the backend, asked
    /// for an element descriptor, finds a variable. Lowering runs after every
    /// link is final, so resolving here is safe and the typed tree is concrete.
    fn deep(&mut self, t: Type) -> Type {
        if let Some(&hit) = self.resolved.get(&t) {
            return hit;
        }
        let r = self.db.deep_resolve(t);
        self.resolved.insert(t, r);
        r
    }

    // --- items -------------------------------------------------------------

    fn lower_fn(&mut self, item: &FnItem) -> Option<TypedFn> {
        let name_tok = item.name()?;
        let name_range = name_tok.text_range();
        let name = name_tok.text().to_string();
        // Resolve the fn symbol via its declaration range (decls map; survives
        // shadowing). Anonymous/builtin decls have no symbol and are skipped.
        let symbol = self.resolve_decl_at(name_range)?;

        // The function's type is its scheme's **body** — the type inference
        // arrived at, binders and all. It used to be a fresh *instantiation* of
        // that scheme, which is a set of variables nothing else in this tree
        // mentions: the params below read their own monotypes and the body read
        // the inferred ones, so a generic fn's `fn_type` disagreed with its own
        // parameters, and `mono::specialize` — which unified against yet a third
        // instantiation and then followed the original — pinned variables that
        // appeared nowhere in the clone it was specializing (MONO-01).
        let scheme = self.names.get(symbol).and_then(|s| s.scheme.clone());
        let fn_type = match &scheme {
            Some(s) => {
                let body = s.body();
                self.deep(body)
            }
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
        // A parameter is not an expression, so it has no entry in `expr_types`;
        // its type is the monotype inference attached to its symbol. That is a
        // read, not a re-derivation — `infer_param` writes `Scheme::monotype`,
        // which has no binders, so the `instantiate` this used to do was a
        // no-op with a fresh-variable fallback hiding behind it.
        let ty = self.symbol_type(symbol);
        Some(TypedParam { symbol, name, ty })
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
        let Some(name_tok) = stmt.name() else {
            return self.lower_discarding_binding(stmt.init());
        };
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

    /// A binding with no name — `let _ = f()` (D7) — still runs its
    /// initializer; it just keeps nothing. Lowering it to a statement
    /// expression is what makes the discard idiom a *discard* rather than a
    /// deletion: dropping the whole statement (which is what returning `None`
    /// here does) silently removed the call.
    fn lower_discarding_binding(&mut self, init: Option<Expr>) -> Option<TypedStmt> {
        let init = init?;
        Some(TypedStmt::Expr(self.lower_expr(&init)))
    }

    fn lower_var(&mut self, stmt: &VarStmt) -> Option<TypedStmt> {
        let Some(name_tok) = stmt.name() else {
            return self.lower_discarding_binding(stmt.init());
        };
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
                    ty: self.node_ty(p.syntax()),
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
            // An error node has no shape of its own; inference recorded a
            // variable for it and the program is already reported.
            Expr::Error(n) => TypedExpr::Lit {
                value: Lit::Int(0),
                ty: self.node_ty(n),
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
        // The closure's `Func` type is inference's, not one rebuilt from the
        // lowered params and body: a closure whose body diverges has a `Never`
        // block tail, and inference joins that with the result the `return`s
        // pinned rather than taking it literally.
        let fn_type = self.node_ty(c.syntax());

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
                // The type at the reference that discovered the capture, when
                // there is one. There is not always: inference records a type
                // for a name it *reads*, and a capture first seen on an
                // assignment target is a write — `|n| { total = n }` found
                // `total` at a range with no recorded type and invented a fresh
                // variable for it, losing the binding's type entirely (HIR-09).
                // The binding's own scheme is the answer in that case; a
                // polymorphic one is skipped, because a capture of a generic
                // binding needs the *use site*'s instantiation, not the scheme.
                let recorded = match self.ref_types.get(&fv.ref_range).copied() {
                    Some(t) => t,
                    None => self
                        .names
                        .get(fv.symbol)
                        .and_then(|s| s.scheme.as_ref())
                        .map(praxis_types::Scheme::body)
                        .unwrap_or(self.unit),
                };
                let ty = self.deep(recorded);
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
        // A `var` captured by cell escapes its frame, and this is where that is
        // decided — so this is where it is recorded (HIR-08). Deriving the set
        // from a later walk over the tree meant any expression position the walk
        // forgot silently produced an unboxed capture.
        self.escaping_vars.extend(
            captures
                .iter()
                .filter(|c| matches!(c.kind, crate::capture::CaptureKind::ByCell))
                .map(|c| c.symbol),
        );

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
        // The *value* is read off the token; the *type* is what inference gave
        // this node. They agree for every well-formed literal — the point of
        // reading is the malformed ones, which used to become a fresh variable.
        let ty = self.node_ty(lit.syntax());
        let Some(tok) = lit.token() else {
            return TypedExpr::Lit {
                value: Lit::Int(0),
                ty,
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
                    ty,
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
                    ty,
                    span,
                }
            }
            SyntaxKind::TextLit => {
                let raw = tok.text();
                let unquoted = unquote_text(raw);
                TypedExpr::Lit {
                    value: Lit::Text(unquoted),
                    ty,
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
                    ty,
                    span,
                }
            }
            SyntaxKind::KW_TRUE => TypedExpr::Lit {
                value: Lit::Bool(true),
                ty,
                span,
            },
            SyntaxKind::KW_FALSE => TypedExpr::Lit {
                value: Lit::Bool(false),
                ty,
                span,
            },
            _ => TypedExpr::Lit {
                value: Lit::Int(0),
                ty,
                span,
            },
        }
    }

    fn lower_path(&mut self, p: &PathExpr) -> TypedExpr {
        let span = self.node_span(p.syntax());
        // The type is the one inference instantiated *at this reference*. Every
        // other answer this used to compute — re-instantiating the symbol's
        // scheme, or the enum def's own type below — is a second instantiation
        // whose variables nothing else pinned.
        let ty = self.node_ty(p.syntax());
        // M7-WS4: a zero-payload enum variant used as a bare path (`Empty`) —
        // decided by the symbol this name resolves to, not by its text.
        if let Some(symbol) = p
            .name()
            .and_then(|tok| self.resolve_symbol_at(tok.text_range()))
        {
            if let Some((enum_def_id, variant_idx)) = self.enum_variant_of(symbol) {
                // Only treat as a variant if the payload is empty (a zero-payload
                // variant). Payload variants are handled in lower_call.
                let edef = self.db.enum_def(enum_def_id);
                if !edef.variants[variant_idx].has_payload() {
                    return TypedExpr::EnumVariant {
                        enum_def_id,
                        variant_idx: variant_idx as u32,
                        args: Vec::new(),
                        ty,
                        span,
                    };
                }
            }
        }
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
        // The *operator* is read off the token; the *type* is inference's. The
        // Int-or-Float heuristic this replaces re-decided a question inference
        // had already answered, from a strictly narrower view of the operands.
        let ty = self.node_ty(b.syntax());
        let op_tok = b.op().map(|t| t.kind());
        let op = match op_tok {
            Some(
                SyntaxKind::PLUS
                | SyntaxKind::MINUS
                | SyntaxKind::STAR
                | SyntaxKind::SLASH
                | SyntaxKind::PERCENT,
            ) => match op_tok.unwrap() {
                SyntaxKind::PLUS => BinOp::Add,
                SyntaxKind::MINUS => BinOp::Sub,
                SyntaxKind::STAR => BinOp::Mul,
                SyntaxKind::SLASH => BinOp::Div,
                SyntaxKind::PERCENT => BinOp::Rem,
                _ => unreachable!(),
            },
            Some(
                SyntaxKind::EQ2
                | SyntaxKind::NEQ
                | SyntaxKind::LT
                | SyntaxKind::GT
                | SyntaxKind::LTEQ
                | SyntaxKind::GTEQ,
            ) => match op_tok.unwrap() {
                SyntaxKind::EQ2 => BinOp::Eq,
                SyntaxKind::NEQ => BinOp::Neq,
                SyntaxKind::LT => BinOp::Lt,
                SyntaxKind::GT => BinOp::Gt,
                SyntaxKind::LTEQ => BinOp::Le,
                SyntaxKind::GTEQ => BinOp::Ge,
                _ => unreachable!(),
            },
            Some(SyntaxKind::PIPE2) => BinOp::LogicalOr,
            _ => BinOp::Add,
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
        let ty = self.node_ty(u.syntax());
        let op = match u.op().map(|t| t.kind()) {
            Some(SyntaxKind::BANG) => UnaryOp::Not,
            _ => UnaryOp::Neg,
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
        // The `if`'s type is inference's join of its branches (TY-19), read
        // rather than recomputed. Repeating the join here was a third pass
        // computing one answer; reading the then-block alone, which is what it
        // replaced, made `if flag { panic("x") } else { 1 }` a `Never`.
        let ty = self.node_ty(i.syntax());
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
        let ty = self.node_ty(w.syntax());
        TypedExpr::While {
            cond: cond.unwrap_or_else(|| Box::new(unit_lit(self.db))),
            body: body.unwrap_or_else(|| {
                Box::new(TypedBlock {
                    stmts: Vec::new(),
                    tail: unit_lit(self.db),
                    ty: self.unit,
                })
            }),
            ty,
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
        // inference pass records it on the binding's declaration range. A
        // `for` binding is not an expression, so this is a `ref_types` read, not
        // an `expr_types` one.
        let item_ty = f
            .binding()
            .and_then(|t| self.ref_types.get(&t.text_range()).copied())
            .map(|t| self.deep(t))
            .unwrap_or(self.unit);
        let ty = self.node_ty(f.syntax());
        TypedExpr::For {
            binding,
            iter,
            body,
            item_ty,
            ty,
            span,
        }
    }

    /// `loop { body }` (M8, §4.11). The type is what its `break`s carry (TY-21)
    /// — `Never` when no `break` leaves the loop, `Unit` when they leave it with
    /// nothing. Inference computed that join; lowering reads it. Keeping a
    /// second stack of loop frames here to recompute it was the third pass over
    /// one answer, and F15 is what retires it.
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
        let ty = self.node_ty(l.syntax());
        TypedExpr::Loop { body, ty, span }
    }

    /// `break [expr]` (M8, §4.11). Diverges; the expression's type is `Never`.
    /// The *value* it carries reaches the loop through inference's join, which
    /// [`lower_loop`](Self::lower_loop) reads.
    fn lower_break(&mut self, b: &BreakExpr) -> TypedExpr {
        let span = self.node_span(b.syntax());
        let value = b.value().map(|v| Box::new(self.lower_expr(&v)));
        let ty = self.node_ty(b.syntax());
        TypedExpr::Break { value, ty, span }
    }

    /// `continue` (M8, §4.11). Diverges; type `Never`.
    fn lower_continue(&mut self, c: &ContinueExpr) -> TypedExpr {
        let ty = self.node_ty(c.syntax());
        TypedExpr::Continue {
            ty,
            span: self.node_span(c.syntax()),
        }
    }

    /// `return [expr]` (M8, §4.11). Diverges; type `Never`.
    fn lower_return(&mut self, r: &ReturnExpr) -> TypedExpr {
        let span = self.node_span(r.syntax());
        let value = r.value().map(|v| Box::new(self.lower_expr(&v)));
        let ty = self.node_ty(r.syntax());
        TypedExpr::Return { value, ty, span }
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
        // The call's result type is the one inference gave **this call site**
        // (MONO-01). It used to be a fresh instantiation of the callee's scheme,
        // so `id(1.5)` lowered as an unbound variable and `let v: Vec[Float] =
        // Vec()` reached codegen with no element type — the annotation had
        // arrived, at the call site inference recorded and lowering ignored.
        let ty = self.node_ty(c.syntax());
        // M7-WS4: enum variant construction — `Number(5)`. Decided by the symbol
        // the callee name resolves to, so a local shadowing a constructor is a
        // call of that local (HIR-03).
        if let Some((enum_def_id, variant_idx)) = self.enum_variant_of(callee) {
            return TypedExpr::EnumVariant {
                enum_def_id,
                variant_idx: variant_idx as u32,
                args,
                ty,
                span,
            };
        }
        // The concrete arg types at this call site (WS8, §13.6). Recorded by
        // inference in `analysis.call_sites`, keyed by the callee name token's
        // range. The mono pass reads these off the typed tree to instantiate a
        // polymorphic callee. Empty if the call site wasn't recorded (e.g. an
        // unresolved callee, or a postfix expression callee) — the mono pass
        // treats an empty vec as monomorphic.
        let arg_types: Vec<Type> = callee_tok
            .as_ref()
            .and_then(|t| self.call_sites.get(&t.text_range()))
            .map(|cs| cs.arg_types.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|t| self.deep(t))
            .collect();
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

    /// The enum variant a **symbol** constructs: its def-id and variant index,
    /// or `None` when the symbol is not a variant constructor (HIR-03).
    ///
    /// It used to look the *text* up in the root scope, before consulting the
    /// symbol resolution had already bound the name to — so `enum E { A }`
    /// followed by `let A = 7` lowered the local `A` as the constructor, and
    /// the binding's value was discarded. Resolution answers which `A` a
    /// reference means; this asks it.
    ///
    /// The kind check is load-bearing on its own: the scheme cannot distinguish
    /// a constructor from a binding that *holds* one, because `let A = Empty`
    /// has the enum type too.
    ///
    /// Unlike inference's counterpart this does **not** instantiate: lowering
    /// takes the use-site type from `expr_types` (F15) and needs only the
    /// variant's identity from here. It does not hand back the payload either —
    /// a generic def's payload is written in terms of its *parameters* (F12),
    /// so reading it off the def would answer `T` rather than the element type.
    fn enum_variant_of(&self, symbol: SymbolId) -> Option<(praxis_types::EnumDefId, usize)> {
        let sym = self.names.get(symbol)?;
        if sym.kind != crate::SymbolKind::EnumVariant {
            return None;
        }
        let scheme = sym.scheme.as_ref()?;
        // A payload variant's scheme is a `Func` returning the enum; a
        // payload-less one's is the enum type itself.
        let result_ty = match self.db.data(self.db.follow(scheme.body())) {
            praxis_types::TypeData::Func { result, .. } => *result,
            praxis_types::TypeData::Enum { .. } => scheme.body(),
            _ => return None,
        };
        let def_id = match self.db.data(self.db.follow(result_ty)) {
            praxis_types::TypeData::Enum { def, .. } => *def,
            _ => return None,
        };
        let idx = self.db.enum_def(def_id).variant(&sym.name)?;
        Some((def_id, idx))
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

        // The catalog entry and the result type are what **inference** resolved
        // at this call site (HIR-02/F15). Repeating the lookup here re-derived
        // the entry from a receiver type of lowering's own and then read the
        // result off the entry's *pattern* — a fresh `?T` for every `Var("T")`
        // in it — so `values.get(0)` on a `Vec[Float]` lowered as `?T` however
        // firmly inference had pinned it to `Float`.
        let resolved = m
            .method_name()
            .and_then(|t| self.method_refs.get(&t.text_range()).copied());
        let Some(resolved) = resolved else {
            // Inference could not resolve the method: report it (this is the
            // one diagnostic lowering owns here, because it has the name span)
            // and keep the receiver and arguments, which are well-formed trees
            // in their own right — discarding them lost every closure and
            // capture inside them.
            if let Some(name_tok) = m.method_name() {
                self.diag(
                    name_tok.text_range(),
                    DiagCode::NoMethodOnType,
                    format!("no method `{name}` on this type taking {arity} argument(s)"),
                );
            }
            let ty = self.node_ty(m.syntax());
            return TypedExpr::MethodCall {
                receiver: Box::new(receiver),
                name,
                lowering_symbol: None,
                args,
                purity: praxis_stdlib::Purity::Impure,
                ty,
                span,
            };
        };
        let ty = self.deep(resolved.result);
        let lowering_symbol = match &resolved.entry.lowering {
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
            purity: resolved.entry.purity,
            ty,
            span,
        }
    }

    fn lower_tuple(&mut self, t: &TupleExpr) -> TypedExpr {
        let span = self.node_span(t.syntax());
        let elements: Vec<TypedExpr> = t.elements().map(|e| self.lower_expr(&e)).collect();
        let ty = self.node_ty(t.syntax());
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
            // The *plan* is lowering's own product; the type is inference's.
            // Both are synthesized from the same parser expression, so they
            // agree — reading keeps them one answer rather than two.
            Some(analysis) => {
                let ty = self.node_ty(r.syntax());
                let _ = analysis.result_type;
                TypedExpr::Read {
                    plan: analysis.plan,
                    ty,
                    span,
                }
            }
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
        match p.parser_expr() {
            Some(parser_expr) => {
                match crate::parser_lower::analyze_parser_expr(
                    &parser_expr,
                    self.file,
                    self.db,
                    &mut self.diagnostics,
                ) {
                    Some(analysis) => {
                        let ty = self.node_ty(p.syntax());
                        let _ = analysis.result_type;
                        TypedExpr::Parse {
                            text: Box::new(text_expr),
                            plan: analysis.plan,
                            ty,
                            span,
                        }
                    }
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
            None => self.error_expr(),
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
        let struct_ty = self.node_ty(r.syntax());
        let record_def_id = match self.db.data(self.db.follow(struct_ty)) {
            praxis_types::TypeData::Record { def, .. } => *def,
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
        let record = match self.db.data(resolved) {
            praxis_types::TypeData::Record { def, args } => Some((*def, args.clone())),
            _ => None,
        };
        let Some((field_idx, _)) = record.and_then(|(def, args)| {
            let name = f.field_name()?;
            self.db.record_field_of(def, &args, name.text())
        }) else {
            // Not a record type, or unknown field — emit a Y1xx diagnostic.
            if let Some(tok) = f.field_name() {
                self.diag(
                    tok.text_range(),
                    DiagCode::NoFieldOnType,
                    format!("no field `{}` on this type", tok.text()),
                );
            }
            return self.error_expr();
        };
        // The *index* comes from the record definition; the *type* from
        // inference, which already substituted the instance's arguments.
        let ty = self.node_ty(f.syntax());
        TypedExpr::FieldGet {
            receiver: Box::new(receiver),
            field_idx: field_idx as u32,
            ty,
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
        // The match's type is inference's join of its arms — not the first
        // arm's, which is `Never` whenever that arm happens to diverge.
        let ty = self.node_ty(m.syntax());
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
                if let praxis_types::TypeData::Enum { def, .. } = self.db.data(resolved) {
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
                let (enum_def_id, enum_args) = match self.db.data(resolved) {
                    praxis_types::TypeData::Enum { def, args } => (*def, args.clone()),
                    _ => return TypedPattern::Wildcard,
                };
                let Some(idx) = self.db.enum_def(enum_def_id).variant(&vname) else {
                    return TypedPattern::Wildcard;
                };
                // Recurse into sub-patterns against payload types — the WS5 bug
                // was that only flat Name sub-patterns were collected and nested
                // variant patterns were silently dropped. The payload comes from
                // the scrutinee's *arguments*, so `Some(n)` against an
                // `Option[Int]` binds `n` at `Int` rather than at the def's own
                // parameter (F12).
                let payload_types: Vec<Type> =
                    self.db.variant_payload_of(enum_def_id, &enum_args, idx);
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

    /// The type of a symbol's binding, resolved to its leaves.
    ///
    /// The scheme's **body**, not an instantiation of it: this is asked only
    /// where there is no expression node to read (a parameter, a punned record
    /// field), and in both cases the binding's own type is the answer. A
    /// polymorphic binding's binders survive into the typed tree, which is what
    /// `mono::specialize` substitutes.
    fn symbol_type(&mut self, symbol: SymbolId) -> Type {
        match self.names.get(symbol).and_then(|s| s.scheme.as_ref()) {
            Some(s) => {
                let body = s.body();
                self.deep(body)
            }
            // A symbol with no scheme errored during inference; it is already
            // reported, and `Unit` cascades nothing.
            None => self.unit,
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
